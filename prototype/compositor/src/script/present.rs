//! `hz.present`: the way out.
//!
//! A renderer draws in the working space. What turns its buffer into what an
//! output takes is the core's: this function. With an OCIO display and view
//! on the output, that is the OCIO pass (`hz.ocio_present`, when the
//! compositor is built with it); otherwise the conversion of `proto-color`
//! (a matrix and a transfer function), drawn with the shader
//! `scripts/shaders/encode.slang` that is in the binary. A project cannot
//! replace either.
//!
//! The core also notices a `frame` that returns without having presented:
//! whatever the script drew into the output's image then went there without
//! a conversion. That is allowed (a script may do the conversion itself),
//! and said once in the log.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Function, Lua, Table};
use proto_color::{CompositeSpace, EncodeTransform};
use proto_gfx_core::{
    ColorAttachment, ColorTarget, CullMode, Format, GfxError, Gpu, Handle, LoadOp,
    RenderPipelineDesc, ShaderStage, Topology,
};

use super::Project;
use super::api::{SharedRef, arg_error};

/// The shader of the conversion, among those of the built-in project.
const SHADER: &str = "shaders/encode.slang";

#[derive(Default)]
pub(crate) struct PresentState {
    shaders: Option<(Handle, Handle)>,
    /// Format of the output's image -> pipeline.
    pipelines: HashMap<Format, Handle>,
    /// Whether `hz.present` was called in the frame being drawn.
    presented: bool,
    /// Frames the script drew without presenting.
    pub frames_without: u64,
    warned: bool,
}

impl PresentState {
    /// A `frame` of the script starts.
    pub(crate) fn begin_frame(&mut self) {
        self.presented = false;
    }

    /// A `frame` of the script returned without failing.
    pub(crate) fn end_frame(&mut self) {
        if self.presented {
            return;
        }
        self.frames_without += 1;
        if !self.warned {
            self.warned = true;
            tracing::warn!(
                "the script's `frame` returned without presenting (`color.present`, \
                 `hz.present`): what it drew into the output's image is shown without the \
                 conversion from the working space to the output"
            );
        }
    }

    /// Another script was loaded: it is told again.
    pub(crate) fn reloaded(&mut self) {
        self.warned = false;
    }

    fn pipeline(&mut self, gpu: &mut Gpu, format: Format) -> Result<Handle, GfxError> {
        if let Some(pipeline) = self.pipelines.get(&format) {
            return Ok(*pipeline);
        }
        let (vertex, fragment) = match self.shaders {
            Some(shaders) => shaders,
            None => {
                let mut shader = |stage, entry: &str| {
                    let spirv = crate::shaders::load(&Project::Builtin, SHADER, entry).map_err(
                        |message| GfxError {
                            code: proto_gfx_core::ErrorCode::InvalidArgument,
                            message,
                        },
                    )?;
                    let handle = gpu.create_shader_spirv(stage, entry, &spirv)?;
                    // The core's: a reload of the script keeps it.
                    gpu.protect(handle);
                    Ok::<_, GfxError>(handle)
                };
                let shaders = (
                    shader(ShaderStage::Vertex, "vs_main")?,
                    shader(ShaderStage::Fragment, "fs_main")?,
                );
                self.shaders = Some(shaders);
                shaders
            }
        };
        let pipeline = gpu.create_render_pipeline(&RenderPipelineDesc {
            vertex,
            fragment,
            vertex_buffers: Vec::new(),
            targets: vec![ColorTarget {
                format,
                blend: None,
            }],
            topology: Topology::TriangleList,
            cull: CullMode::None,
        })?;
        gpu.protect(pipeline);
        self.pipelines.insert(format, pipeline);
        Ok(pipeline)
    }

    /// Draws the buffer behind `slot` into `target`, converted.
    fn draw(
        &mut self,
        gpu: &mut Gpu,
        slot: Handle,
        target: Handle,
        transform: &EncodeTransform,
    ) -> Result<(), GfxError> {
        let format = gpu.image_info(target)?.format;
        let pipeline = self.pipeline(gpu, format)?;
        // The push data of the shader: three rows of the matrix, the
        // transfer function, its parameter, whether to clip.
        let mut push = Vec::with_capacity(64);
        for row in &transform.matrix {
            for value in row.iter().chain(&[0.0]) {
                push.extend_from_slice(&value.to_ne_bytes());
            }
        }
        push.extend_from_slice(&transform.transfer.shader_id().to_ne_bytes());
        push.extend_from_slice(&transform.transfer_param.to_ne_bytes());
        push.extend_from_slice(&u32::from(!transform.transfer.is_extended_range()).to_ne_bytes());
        push.extend_from_slice(&0u32.to_ne_bytes());
        gpu.begin_pass(&[ColorAttachment {
            image: target,
            load: LoadOp::DontCare,
            store: true,
        }])?;
        let drawn = (|| {
            gpu.set_pipeline(pipeline)?;
            gpu.bind(0, slot)?;
            gpu.push(0, &push)?;
            gpu.draw(3, 1, 0, 0)
        })();
        // The pass is closed also when a call in it was refused.
        gpu.end_pass()?;
        drawn
    }
}

pub(super) fn register(
    lua: &Lua,
    hz: &Table,
    shared: &SharedRef,
    gpu: &Rc<RefCell<Gpu>>,
) -> mlua::Result<()> {
    // hz.present(image, slot, output, target): converts the buffer `image`
    // (premultiplied linear light in the working space, the size of the
    // output; `slot` is a texture slot of it) to what the output `output`
    // (its id) takes, and writes it to `target`, the image `frame` was
    // given. A pass of its own: called last, outside a pass.
    let (state, gpu) = (Rc::clone(shared), Rc::clone(gpu));
    let table = hz.clone();
    hz.set(
        "present",
        lua.create_function(
            move |_, (image, slot, output, target): (i64, i64, u64, i64)| {
                let (space, description) = {
                    let mut shared = state.borrow_mut();
                    let description = shared
                        .world
                        .outputs
                        .iter()
                        .find(|o| o.id.0 == output)
                        .ok_or_else(|| arg_error(format!("unknown output {output}")))?
                        .image_description;
                    // For verification captures.
                    shared.debug_composite = Some((Handle::from_bits(image), shared.working));
                    shared.present.presented = true;
                    (
                        CompositeSpace {
                            primaries: shared.working,
                        },
                        description,
                    )
                };
                // An output with an OCIO display and view goes out through
                // the OCIO pass. It answers false when the output has none,
                // or when none is ready yet.
                if let Some(ocio) = table.get::<Option<Function>>("ocio_present")?
                    && ocio.call::<bool>((slot, output, target))?
                {
                    return Ok(());
                }
                let transform = EncodeTransform::new(&space, &description)
                    .ok_or_else(|| arg_error("degenerate primaries or luminances"))?;
                let mut shared = state.borrow_mut();
                shared
                    .present
                    .draw(
                        &mut gpu.borrow_mut(),
                        Handle::from_bits(slot),
                        Handle::from_bits(target),
                        &transform,
                    )
                    .map_err(mlua::Error::external)
            },
        )?,
    )?;
    Ok(())
}
