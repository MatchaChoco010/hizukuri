// bench.lua: tinted, rotated textured quads into a linear buffer, then the
// buffer encoded as sRGB.

[[vk::binding(0, 0)]] Texture2D<float4> linear_target;

[shader("vertex")]
float4 encode_vs(uint index : SV_VulkanVertexID) : SV_Position {
    float2 corners[3] = { float2(-1.0, -1.0), float2(3.0, -1.0), float2(-1.0, 3.0) };
    return float4(corners[index], 0.0, 1.0);
}

float3 srgb_oetf(float3 c) {
    float3 low = c * 12.92;
    float3 high = 1.055 * pow(c, float3(1.0 / 2.4)) - 0.055;
    return select(c <= 0.0031308, low, high);
}

[shader("fragment")]
float4 encode_fs(float4 position : SV_Position) : SV_Target0 {
    float4 linear = linear_target.Load(int3(int2(position.xy), 0));
    return float4(srgb_oetf(clamp(linear.rgb, 0.0, 1.0)), linear.a);
}
