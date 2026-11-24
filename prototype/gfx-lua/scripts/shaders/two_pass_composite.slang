// two_pass.luau: windows composited into a linear buffer, then the buffer
// encoded for the output (sRGB or PQ).

struct CompositePush {
    float4 rect;
    float2 target_size;
};
[[vk::push_constant]] ConstantBuffer<CompositePush> composite_pc;

[[vk::binding(0, 0)]] Texture2D<float4> window_texture;
[[vk::binding(1, 0)]] SamplerState window_sampler;

struct CompositeOut {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 uv : TEXCOORD0;
};

[shader("vertex")]
CompositeOut composite_vs(uint index : SV_VulkanVertexID) {
    float2 corners[6] = {
        float2(0.0, 0.0), float2(1.0, 0.0), float2(0.0, 1.0),
        float2(0.0, 1.0), float2(1.0, 0.0), float2(1.0, 1.0),
    };
    float2 corner = corners[index];
    float2 pixel = composite_pc.rect.xy + composite_pc.rect.zw * corner;
    CompositeOut o;
    o.position = float4(pixel / composite_pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    o.uv = corner;
    return o;
}

[shader("fragment")]
float4 composite_fs(CompositeOut i) : SV_Target0 {
    return window_texture.Sample(window_sampler, i.uv);
}
