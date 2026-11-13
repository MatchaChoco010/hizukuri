// bench.lua: tinted, rotated textured quads into a linear buffer, then the
// buffer encoded as sRGB.

struct QuadPush {
    float4 rect;
    float2 target_size;
    float2 rotation;
    float4 tint;
};
[[vk::push_constant]] ConstantBuffer<QuadPush> quad_pc;

[[vk::binding(0, 0)]] Texture2D<float4> window_texture;
[[vk::binding(1, 0)]] SamplerState window_sampler;

struct QuadOut {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 uv : TEXCOORD0;
};

[shader("vertex")]
QuadOut quad_vs(uint index : SV_VulkanVertexID) {
    float2 corners[6] = {
        float2(0.0, 0.0), float2(1.0, 0.0), float2(0.0, 1.0),
        float2(0.0, 1.0), float2(1.0, 0.0), float2(1.0, 1.0),
    };
    float2 corner = corners[index];
    float2 local = (corner - 0.5) * quad_pc.rect.zw;
    float2 rotated = float2(
        local.x * quad_pc.rotation.x - local.y * quad_pc.rotation.y,
        local.x * quad_pc.rotation.y + local.y * quad_pc.rotation.x);
    float2 pixel = quad_pc.rect.xy + quad_pc.rect.zw * 0.5 + rotated;
    QuadOut o;
    o.position = float4(pixel / quad_pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    o.uv = corner;
    return o;
}

[shader("fragment")]
float4 quad_fs(QuadOut i) : SV_Target0 {
    return window_texture.Sample(window_sampler, i.uv) * quad_pc.tint;
}
