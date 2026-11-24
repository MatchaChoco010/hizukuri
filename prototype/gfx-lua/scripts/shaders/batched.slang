// batched.luau: every window in one draw, its texture picked from a texture
// array by an index that comes with the instance data.

struct Push {
    float2 target_size;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> window_textures[8];
[[vk::binding(1, 0)]] SamplerState window_sampler;

struct VsOut {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 uv : TEXCOORD0;
    [[vk::location(1)]] nointerpolation uint texture_index : TEXCOORD1;
};

[shader("vertex")]
VsOut vs(
    uint index : SV_VulkanVertexID,
    [[vk::location(0)]] float4 rect : RECT,
    [[vk::location(1)]] uint texture_index : TEXTURE_INDEX)
{
    float2 corners[6] = {
        float2(0.0, 0.0), float2(1.0, 0.0), float2(0.0, 1.0),
        float2(0.0, 1.0), float2(1.0, 0.0), float2(1.0, 1.0),
    };
    float2 corner = corners[index];
    float2 pixel = rect.xy + rect.zw * corner;
    VsOut o;
    o.position = float4(pixel / pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    o.uv = corner;
    o.texture_index = texture_index;
    return o;
}

[shader("fragment")]
float4 fs(VsOut i) : SV_Target0 {
    // The index differs between the windows of one draw.
    return window_textures[NonUniformResourceIndex(i.texture_index)].SampleLevel(window_sampler, i.uv, 0.0);
}
