// windows.luau: a textured quad per window, rotated about its centre and
// tinted.

struct Push {
    float4 rect;        // x, y, width, height in target pixels
    float2 target_size;
    float2 rotation;    // cos, sin
    float4 tint;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> window_texture;
[[vk::binding(1, 0)]] SamplerState window_sampler;

struct VsOut {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 uv : TEXCOORD0;
};

[shader("vertex")]
VsOut vs(uint index : SV_VulkanVertexID) {
    float2 corners[6] = {
        float2(0.0, 0.0), float2(1.0, 0.0), float2(0.0, 1.0),
        float2(0.0, 1.0), float2(1.0, 0.0), float2(1.0, 1.0),
    };
    float2 corner = corners[index];
    float2 local = (corner - 0.5) * pc.rect.zw;
    float2 rotated = float2(
        local.x * pc.rotation.x - local.y * pc.rotation.y,
        local.x * pc.rotation.y + local.y * pc.rotation.x);
    float2 pixel = pc.rect.xy + pc.rect.zw * 0.5 + rotated;
    VsOut o;
    o.position = float4(pixel / pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    o.uv = corner;
    return o;
}

[shader("fragment")]
float4 fs(VsOut i) : SV_Target0 {
    return window_texture.Sample(window_sampler, i.uv) * pc.tint;
}
