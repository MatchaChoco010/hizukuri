// The shader of the fallback (src/fallback/draw.rs): textured or solid
// quads, with no color conversion. build.rs compiles it to SPIR-V, and the
// compositor carries the result, so the fallback draws even when no Slang
// compiler is around.

struct Push {
    float4 rect;        // x, y, width, height in target pixels
    float4 uv_u;        // a, b, tx: u = a*s + b*t + tx over the unit square
    float4 uv_v;        // c, d, ty: v = c*s + d*t + ty
    float4 color;       // used when solid != 0
    float2 target_size;
    float opacity;
    float solid;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> surface_texture;
[[vk::binding(1, 0)]] SamplerState surface_sampler;

struct VsOut {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 uv : TEXCOORD0;
};

// 4-vertex triangle strip.
[shader("vertex")]
VsOut vs(uint index : SV_VulkanVertexID) {
    float2 corner = float2(float(index & 1), float((index >> 1) & 1));
    VsOut o;
    o.position = float4((pc.rect.xy + corner * pc.rect.zw) / pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    o.uv = float2(dot(pc.uv_u.xy, corner) + pc.uv_u.z, dot(pc.uv_v.xy, corner) + pc.uv_v.z);
    return o;
}

[shader("fragment")]
float4 fs(VsOut i) : SV_Target0 {
    float4 texel = surface_texture.SampleLevel(surface_sampler, i.uv, 0.0);
    if (pc.solid != 0.0) {
        return pc.color;
    }
    return texel * pc.opacity;
}
