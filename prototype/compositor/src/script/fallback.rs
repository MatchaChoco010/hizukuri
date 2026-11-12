//! The built-in fallback: what draws when the script cannot.
//!
//! This is deliberately small. Windows are drawn flat at their logical
//! position, in stacking order, with their opacity; the cursor goes on top;
//! a colored bar along the top edge shows that the fallback is in charge.
//! There is no color conversion: buffer values are written to the output as
//! they are, which is right when both are the default (BT.709, gamma 2.2)
//! and merely wrong in color otherwise. The point is that the session stays
//! usable while the script is broken, not that it looks right.
//!
//! It uses the same checked GPU API as scripts, on the same device, so no
//! second renderer and no second set of output images is needed to switch
//! to it. Its objects are protected, so replacing the script keeps them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use proto_backend::OutputInfo;
use proto_gfx_core::{
    AddressMode, Blend, ColorAttachment, ColorTarget, CullMode, Filter, Format, GfxError, Gpu,
    Handle, ImageDesc, ImageUsage, LoadOp, RenderPipelineDesc, SamplerDesc, ShaderLanguage,
    ShaderSource, ShaderStage, Topology,
};

use super::{ScriptRuntime, Shared};
use crate::scene::{
    ARROW_HEIGHT, ARROW_WIDTH, CursorImage, CursorView, SurfaceView, WindowView, arrow_image,
};

const SHADER: &str = r#"
struct Push {
    rect: vec4<f32>,        // x, y, width, height in target pixels
    uv_u: vec4<f32>,        // a, b, tx: u = a*s + b*t + tx over the unit square
    uv_v: vec4<f32>,        // c, d, ty: v = c*s + d*t + ty
    color: vec4<f32>,       // used when solid != 0
    target_size: vec2<f32>,
    opacity: f32,
    solid: f32,
}
var<immediate> pc: Push;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs(@builtin(vertex_index) index: u32) -> VsOut {
    let corner = vec2<f32>(f32(index & 1u), f32((index >> 1u) & 1u));
    var out: VsOut;
    out.position = vec4((pc.rect.xy + corner * pc.rect.zw) / pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(dot(pc.uv_u.xy, corner) + pc.uv_u.z, dot(pc.uv_v.xy, corner) + pc.uv_v.z);
    return out;
}

@group(0) @binding(0) var surface_texture: texture_2d<f32>;
@group(0) @binding(1) var surface_sampler: sampler;

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let texel = textureSampleLevel(surface_texture, surface_sampler, in.uv, 0.0);
    if pc.solid != 0.0 {
        return pc.color;
    }
    return texel * pc.opacity;
}
"#;

/// Height of the bar that marks fallback frames, in physical pixels.
pub const MARKER_HEIGHT: u32 = 4;
/// Color of the bar (encoded values).
pub const MARKER_COLOR: [f32; 4] = [1.0, 0.0, 0.5, 1.0];

#[derive(Default)]
pub(crate) struct Fallback {
    shaders: Option<(Handle, Handle)>,
    pipelines: HashMap<Format, Handle>,
    sampler: Option<Handle>,
}

/// The built-in arrow cursor as a texture: (image, slot). Created on first
/// use; needs a GPU frame for the upload and opens one if none is open.
pub(crate) fn arrow(
    shared: &Rc<RefCell<Shared>>,
    gpu: &mut Gpu,
) -> Result<(Handle, Handle), GfxError> {
    if let Some(arrow) = shared.borrow().arrow {
        return Ok(arrow);
    }
    let (width, height, pixels) = arrow_image();
    let image = gpu.create_image(&ImageDesc {
        format: Format::Bgra8Unorm,
        width,
        height,
        usage: ImageUsage::SAMPLED | ImageUsage::COPY_DST,
    })?;
    if !gpu.is_recording() {
        gpu.begin_frame()?;
    }
    gpu.upload_image(image, &pixels)?;
    let slot = gpu.create_texture_slot(image)?;
    gpu.protect(image);
    gpu.protect(slot);
    shared.borrow_mut().arrow = Some((image, slot));
    Ok((image, slot))
}

/// The whole texture, upright.
const WHOLE: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

fn push_bytes(rect: [f32; 4], uv: [f32; 6], color: [f32; 4], tail: [f32; 4]) -> Vec<u8> {
    let uv_u = [uv[0], uv[1], uv[4], 0.0];
    let uv_v = [uv[2], uv[3], uv[5], 0.0];
    rect.iter()
        .chain(&uv_u)
        .chain(&uv_v)
        .chain(&color)
        .chain(&tail)
        .flat_map(|v| v.to_ne_bytes())
        .collect()
}

impl Fallback {
    fn pipeline(&mut self, gpu: &mut Gpu, format: Format) -> Result<Handle, GfxError> {
        if let Some(pipeline) = self.pipelines.get(&format) {
            return Ok(*pipeline);
        }
        let (vertex, fragment) = match self.shaders {
            Some(shaders) => shaders,
            None => {
                let mut shader = |stage, entry| {
                    let handle = gpu.create_shader(&ShaderSource {
                        stage,
                        language: ShaderLanguage::Wgsl,
                        source: SHADER,
                        entry,
                    })?;
                    gpu.protect(handle);
                    Ok::<_, GfxError>(handle)
                };
                let shaders = (
                    shader(ShaderStage::Vertex, "vs")?,
                    shader(ShaderStage::Fragment, "fs")?,
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
                blend: Some(Blend::PREMULTIPLIED),
            }],
            topology: Topology::TriangleStrip,
            cull: CullMode::None,
        })?;
        gpu.protect(pipeline);
        self.pipelines.insert(format, pipeline);
        Ok(pipeline)
    }

    /// Draws the windows and the cursor into `image`. A GPU frame is open.
    pub fn draw(
        &mut self,
        runtime: &ScriptRuntime,
        output: &OutputInfo,
        image: Handle,
        background: [f32; 3],
        windows: &[WindowView],
        cursor: Option<&CursorView>,
    ) -> Result<(), GfxError> {
        let mut gpu = runtime.gpu();
        let gpu = &mut *gpu;
        let format = gpu.image_info(image)?.format;
        let pipeline = self.pipeline(gpu, format)?;
        let sampler = match self.sampler {
            Some(sampler) => sampler,
            None => {
                let sampler = gpu.create_sampler(&SamplerDesc {
                    mag: Filter::Linear,
                    min: Filter::Linear,
                    address_u: AddressMode::ClampToEdge,
                    address_v: AddressMode::ClampToEdge,
                })?;
                gpu.protect(sampler);
                self.sampler = Some(sampler);
                sampler
            }
        };
        // The arrow also serves as the texture bound for solid quads, which
        // read no texel but still need a valid binding.
        let (_, arrow_slot) = arrow(&runtime.shared, gpu)?;

        let scale = output.scale as f32;
        let origin = (output.position.0 as f32, output.position.1 as f32);
        let size = [output.size.0 as f32, output.size.1 as f32];
        let [r, g, b] = background;

        // Collect what to draw before touching the GPU: (slot, rect, uv, opacity).
        let mut quads: Vec<(Handle, [f32; 4], [f32; 6], f32)> = Vec::new();
        {
            let shared = runtime.shared.borrow();
            let mut add = |surface: &SurfaceView, at: (f32, f32), opacity: f32, round: bool| {
                let (Some(texture), Some(source)) =
                    (shared.textures.get(&surface.surface), surface.source)
                else {
                    return;
                };
                let mut x = (at.0 + surface.offset.0 as f32 - origin.0) * scale;
                let mut y = (at.1 + surface.offset.1 as f32 - origin.1) * scale;
                if round {
                    (x, y) = (x.round(), y.round());
                }
                let uv = super::api::uv_transform(texture, source).map(|v| v as f32);
                quads.push((
                    texture.slot,
                    [
                        x,
                        y,
                        surface.size.0 as f32 * scale,
                        surface.size.1 as f32 * scale,
                    ],
                    uv,
                    opacity,
                ));
            };
            for window in windows {
                let Some(placed) = shared
                    .world
                    .windows
                    .iter()
                    .find(|w| w.window == window.window)
                else {
                    continue;
                };
                let opacity = shared.opacity.get(&window.window).copied().unwrap_or(1.0);
                for surface in &window.surfaces {
                    add(
                        surface,
                        (placed.position.0 as f32, placed.position.1 as f32),
                        opacity,
                        false,
                    );
                }
            }
            if let Some(cursor) = cursor {
                let at = (cursor.position.0 as f32, cursor.position.1 as f32);
                match &cursor.image {
                    CursorImage::Default => quads.push((
                        arrow_slot,
                        [
                            ((at.0 - origin.0) * scale).round(),
                            ((at.1 - origin.1) * scale).round(),
                            ARROW_WIDTH as f32 * scale,
                            ARROW_HEIGHT as f32 * scale,
                        ],
                        WHOLE,
                        1.0,
                    )),
                    CursorImage::Surfaces(surfaces) => {
                        for surface in surfaces {
                            add(surface, at, 1.0, true);
                        }
                    }
                }
            }
        }

        gpu.begin_pass(&[ColorAttachment {
            image,
            load: LoadOp::Clear([r, g, b, 1.0]),
            store: true,
        }])?;
        gpu.set_pipeline(pipeline)?;
        gpu.bind(1, sampler)?;
        for (slot, rect, uv, opacity) in quads {
            gpu.bind(0, slot)?;
            gpu.push(
                0,
                &push_bytes(rect, uv, [0.0; 4], [size[0], size[1], opacity, 0.0]),
            )?;
            gpu.draw(4, 1, 0, 0)?;
        }
        // The marker.
        gpu.bind(0, arrow_slot)?;
        gpu.push(
            0,
            &push_bytes(
                [0.0, 0.0, size[0], MARKER_HEIGHT as f32],
                WHOLE,
                MARKER_COLOR,
                [size[0], size[1], 1.0, 1.0],
            ),
        )?;
        gpu.draw(4, 1, 0, 0)?;
        gpu.end_pass()
    }
}
