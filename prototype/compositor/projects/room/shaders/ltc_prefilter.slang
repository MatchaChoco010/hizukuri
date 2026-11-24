// The blurred mip levels of an area light's texture (`update_light` of the
// `ltc` script module): each level is the one above it, halved and blurred
// by a Gaussian of one texel of its own. Two passes per level:
//
//   fs_halve: level k − 1 → level k of a scratch image, halved (a 2 × 2 box
//             per tap) and blurred along x.
//   fs_blur:  that → level k of the light's texture, blurred along y.
//
// Outside the texture, the colour of its edge goes on (the sampler clamps).

struct Push {
    // One texel of the level that is written, in texture coordinates.
    float2 texel;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_source;
// Linear, clamped.
[[vk::binding(1, 0)]] SamplerState u_sampler;

struct VertexOutput {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 uv : TEXCOORD0;
};

// One triangle over the whole target.
[shader("vertex")]
VertexOutput vs_main(uint index : SV_VulkanVertexID) {
    float2 corner = float2(float((index << 1) & 2), float(index & 2));
    VertexOutput o;
    o.uv = corner;
    o.position = float4(corner * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

// A Gaussian of standard deviation 1, at the offsets 0 to 3, normalized over
// the seven taps.
static const float W0 = 0.399050;
static const float W1 = 0.242036;
static const float W2 = 0.054005;
static const float W3 = 0.004433;

float4 pair(float2 uv, float2 offset) {
    return u_source.SampleLevel(u_sampler, uv + offset, 0.0) + u_source.SampleLevel(u_sampler, uv - offset, 0.0);
}

float4 blurred(float2 uv, float2 step) {
    return u_source.SampleLevel(u_sampler, uv, 0.0) * W0
        + pair(uv, step) * W1
        + pair(uv, step * 2.0) * W2
        + pair(uv, step * 3.0) * W3;
}

// The source is the level above: at the centre of a written texel, a read
// with linear filtering is the mean of the 2 × 2 texels beneath it.
[shader("fragment")]
float4 fs_halve(VertexOutput i) : SV_Target0 {
    return blurred(i.uv, float2(pc.texel.x, 0.0));
}

// The source is the scratch image's level of the same size.
[shader("fragment")]
float4 fs_blur(VertexOutput i) : SV_Target0 {
    return blurred(i.uv, float2(0.0, pc.texel.y));
}
