//! Drawing in the fallback.
//!
//! Windows are drawn flat at their logical position, in stacking order, with
//! their opacity; the cursor goes on top; a colored bar along the top edge
//! shows that the fallback is in charge. There is no color conversion:
//! buffer values are written to the output as they are, which is right when
//! both are the default (BT.709, gamma 2.2) and merely wrong in color
//! otherwise. The point is that the session stays usable while the script is
//! broken, not that it looks right.
//!
//! It uses the same checked GPU API as scripts, on the same device, so no
//! second renderer and no second set of output images is needed to switch
//! to it. Its objects are protected, so replacing the script keeps them.

use std::collections::HashMap;

use proto_backend::OutputInfo;
use proto_gfx_core::{
    AddressMode, Blend, ColorAttachment, ColorTarget, CullMode, Filter, Format, GfxError, Gpu,
    Handle, LoadOp, RenderPipelineDesc, SamplerDesc, ShaderLanguage, ShaderSource, ShaderStage,
    Topology,
};

use crate::scene::{ARROW_HEIGHT, ARROW_WIDTH, CursorImage, CursorView, SurfaceView};

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

/// A window as the fallback draws it.
pub struct Window<'a> {
    /// The logical position of the window geometry in the global space.
    pub position: (f64, f64),
    pub opacity: f32,
    /// The surfaces of the window, back to front.
    pub surfaces: &'a [SurfaceView],
}

/// What one fallback frame shows, and where it goes.
pub struct Frame<'a> {
    pub output: &'a OutputInfo,
    /// The image of the output to draw into.
    pub image: Handle,
    /// The color the output is cleared to.
    pub background: [f32; 3],
    /// The texture slot of the built-in arrow cursor.
    pub arrow: Handle,
    /// Stacking order, bottom to top.
    pub windows: &'a [Window<'a>],
    pub cursor: Option<&'a CursorView>,
}

/// Where the contents of a surface are: the texture slot to sample, and how
/// the unit square of the surface maps to the texture (`u = a*s + b*t + tx`,
/// `v = c*s + d*t + ty`, as `[a, b, c, d, tx, ty]`). `None` when the surface
/// shows nothing.
pub type SurfaceTexture<'a> = &'a dyn Fn(&SurfaceView) -> Option<(Handle, [f32; 6])>;

/// The GPU objects of the fallback, created on first use.
#[derive(Default)]
pub struct Drawer {
    shaders: Option<(Handle, Handle)>,
    pipelines: HashMap<Format, Handle>,
    sampler: Option<Handle>,
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

impl Drawer {
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

    fn sampler(&mut self, gpu: &mut Gpu) -> Result<Handle, GfxError> {
        if let Some(sampler) = self.sampler {
            return Ok(sampler);
        }
        let sampler = gpu.create_sampler(&SamplerDesc {
            mag: Filter::Linear,
            min: Filter::Linear,
            address_u: AddressMode::ClampToEdge,
            address_v: AddressMode::ClampToEdge,
        })?;
        gpu.protect(sampler);
        self.sampler = Some(sampler);
        Ok(sampler)
    }

    /// Draws the windows and the cursor into `frame.image`. A GPU frame is
    /// open. `texture` tells where the contents of a surface are.
    pub fn draw(
        &mut self,
        gpu: &mut Gpu,
        frame: &Frame<'_>,
        texture: SurfaceTexture<'_>,
    ) -> Result<(), GfxError> {
        let format = gpu.image_info(frame.image)?.format;
        let pipeline = self.pipeline(gpu, format)?;
        let sampler = self.sampler(gpu)?;
        // The arrow also serves as the texture bound for solid quads, which
        // read no texel but still need a valid binding.
        let arrow_slot = frame.arrow;

        let output = frame.output;
        let scale = output.scale as f32;
        let origin = (output.position.0 as f32, output.position.1 as f32);
        let size = [output.size.0 as f32, output.size.1 as f32];
        let [r, g, b] = frame.background;

        // Collect what to draw before touching the GPU: (slot, rect, uv, opacity).
        let mut quads: Vec<(Handle, [f32; 4], [f32; 6], f32)> = Vec::new();
        let mut add = |surface: &SurfaceView, at: (f32, f32), opacity: f32, round: bool| {
            let Some((slot, uv)) = texture(surface) else {
                return;
            };
            let mut x = (at.0 + surface.offset.0 as f32 - origin.0) * scale;
            let mut y = (at.1 + surface.offset.1 as f32 - origin.1) * scale;
            if round {
                (x, y) = (x.round(), y.round());
            }
            quads.push((
                slot,
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
        for window in frame.windows {
            let at = (window.position.0 as f32, window.position.1 as f32);
            for surface in window.surfaces {
                add(surface, at, window.opacity, false);
            }
        }
        if let Some(cursor) = frame.cursor {
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

        gpu.begin_pass(&[ColorAttachment {
            image: frame.image,
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
