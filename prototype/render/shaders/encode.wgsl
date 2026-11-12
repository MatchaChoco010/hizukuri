// Converts the compositing buffer to encoded output values with a
// full-screen triangle. The fragment stage mirrors
// proto_color::EncodeTransform::apply.

// Clip linear output values to [0, 1] before encoding.
const FLAG_CLIP: u32 = 1u;

// Keep in sync with EncodePush in src/renderer.rs.
struct Push {
    m0: vec4<f32>, // rows of the color matrix (xyz)
    m1: vec4<f32>,
    m2: vec4<f32>,
    tf: u32,
    tf_param: f32,
    flags: u32,
}

var<immediate> pc: Push;

@group(0) @binding(0) var u_composite: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(corner * 2.0 - vec2<f32>(1.0), 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) frag_coord: vec4<f32>) -> @location(0) vec4<f32> {
    let c = textureLoad(u_composite, vec2<i32>(frag_coord.xy), 0);
    var linear = vec3<f32>(dot(pc.m0.xyz, c.rgb), dot(pc.m1.xyz, c.rgb), dot(pc.m2.xyz, c.rgb));
    if (pc.flags & FLAG_CLIP) != 0u {
        linear = clamp(linear, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    return vec4<f32>(tf_encode(linear, pc.tf, pc.tf_param), c.a);
}
