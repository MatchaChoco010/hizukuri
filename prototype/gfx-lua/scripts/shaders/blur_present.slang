// blur.luau: a box blur in a compute shader, then the result drawn to the
// target.

[[vk::binding(0, 0)]] Texture2D<float4> image;

[shader("vertex")]
float4 present_vs(uint index : SV_VulkanVertexID) : SV_Position {
    float2 corners[3] = { float2(-1.0, -1.0), float2(3.0, -1.0), float2(-1.0, 3.0) };
    return float4(corners[index], 0.0, 1.0);
}

[shader("fragment")]
float4 present_fs(float4 position : SV_Position) : SV_Target0 {
    return image.Load(int3(int2(position.xy), 0));
}
