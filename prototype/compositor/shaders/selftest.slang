// The shader of the KMS self test (src/selftest.rs): solid rectangles of the
// test pattern. build.rs compiles it to SPIR-V, and the compositor carries
// the result.

struct Push {
    float4 rect;        // x, y, width, height in target pixels
    float4 color;
    float4 target_size; // width, height, unused, unused
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

// 4-vertex triangle strip.
[shader("vertex")]
float4 vs(uint index : SV_VulkanVertexID) : SV_Position {
    float2 corner = float2(float(index & 1), float((index >> 1) & 1));
    return float4((pc.rect.xy + corner * pc.rect.zw) / pc.target_size.xy * 2.0 - 1.0, 0.0, 1.0);
}

[shader("fragment")]
float4 fs() : SV_Target0 {
    return pc.color;
}
