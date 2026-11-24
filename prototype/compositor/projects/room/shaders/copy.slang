// Copies a picture into a target of another size, with linear filtering
// (onto half the size, every texel is the mean of four).

struct Push {
    float2 size;    // of the target, in pixels
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_source;
[[vk::binding(1, 0)]] SamplerState u_linear;

[shader("vertex")]
float4 vs_main(uint index : SV_VulkanVertexID) : SV_Position {
    float2 corner = float2(float((index << 1) & 2), float(index & 2));
    return float4(corner * 2.0 - 1.0, 0.0, 1.0);
}

[shader("fragment")]
float4 fs_main(float4 frag_coord : SV_Position) : SV_Target0 {
    return u_source.SampleLevel(u_linear, frag_coord.xy / pc.size, 0.0);
}
