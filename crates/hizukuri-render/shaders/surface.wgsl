// Draws one textured quad and converts its texels to premultiplied linear
// values in the compositing space. The fragment stage mirrors
// hizukuri_color::DecodeTransform::apply.

const ALPHA_PREMULTIPLIED_ELECTRICAL: u32 = 0u;
const ALPHA_PREMULTIPLIED_OPTICAL: u32 = 1u;
const ALPHA_STRAIGHT: u32 = 2u;

// The alpha channel of the texture is undefined; treat it as 1.
const FLAG_OPAQUE: u32 = 1u;

// Keep in sync with SurfacePush in src/renderer.rs.
struct Push {
    dst_rect: vec4<f32>, // x, y, width, height in normalized device coordinates
    src_rect: vec4<f32>, // x, y, width, height in texture coordinates
    m0: vec4<f32>,       // rows of the color matrix (xyz)
    m1: vec4<f32>,
    m2: vec4<f32>,
    tf: u32,
    tf_param: f32,
    alpha_mode: u32,
    opacity: f32,
    flags: u32,
}

var<immediate> pc: Push;

@group(0) @binding(0) var u_texture: texture_2d<f32>;
@group(0) @binding(1) var u_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    // 4-vertex triangle strip.
    let corner = vec2<f32>(f32(index & 1u), f32((index >> 1u) & 1u));
    var out: VertexOutput;
    out.uv = pc.src_rect.xy + corner * pc.src_rect.zw;
    out.position = vec4<f32>(pc.dst_rect.xy + corner * pc.dst_rect.zw, 0.0, 1.0);
    return out;
}

fn to_composite(linear: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(dot(pc.m0.xyz, linear), dot(pc.m1.xyz, linear), dot(pc.m2.xyz, linear));
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSampleLevel(u_texture, u_sampler, in.uv, 0.0);
    var alpha = texel.a;
    var alpha_mode = pc.alpha_mode;
    if (pc.flags & FLAG_OPAQUE) != 0u {
        alpha = 1.0;
        alpha_mode = ALPHA_STRAIGHT;
    }

    var premultiplied: vec3<f32>;
    if alpha_mode == ALPHA_PREMULTIPLIED_ELECTRICAL {
        if alpha <= 0.0 {
            premultiplied = vec3<f32>(0.0);
        } else {
            premultiplied = to_composite(tf_decode(texel.rgb / alpha, pc.tf, pc.tf_param)) * alpha;
        }
    } else if alpha_mode == ALPHA_PREMULTIPLIED_OPTICAL {
        premultiplied = to_composite(tf_decode(texel.rgb, pc.tf, pc.tf_param));
    } else {
        premultiplied = to_composite(tf_decode(texel.rgb, pc.tf, pc.tf_param)) * alpha;
    }

    return vec4<f32>(premultiplied, alpha) * pc.opacity;
}
