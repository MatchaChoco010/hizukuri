// two_pass.luau: windows composited into a linear buffer, then the buffer
// encoded for the output (sRGB or PQ).

struct Globals {
    float exposure;
    uint mode;          // 0: sRGB, 1: PQ (SMPTE ST 2084)
    float white_nits;   // luminance of linear 1.0, for PQ
};
[[vk::binding(0, 0)]] Texture2D<float4> linear_target;
[[vk::binding(2, 0)]] ConstantBuffer<Globals> globals;

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

float3 pq_oetf(float3 nits) {
    float m1 = 0.1593017578125;
    float m2 = 78.84375;
    float c1 = 0.8359375;
    float c2 = 18.8515625;
    float c3 = 18.6875;
    float3 y = pow(clamp(nits / 10000.0, 0.0, 1.0), float3(m1));
    return pow((c1 + c2 * y) / (1.0 + c3 * y), float3(m2));
}

[shader("fragment")]
float4 encode_fs(float4 position : SV_Position) : SV_Target0 {
    float4 linear = linear_target.Load(int3(int2(position.xy), 0));
    float3 color = linear.rgb * globals.exposure;
    if (globals.mode == 0) {
        return float4(srgb_oetf(clamp(color, 0.0, 1.0)), linear.a);
    }
    return float4(pq_oetf(color * globals.white_nits), linear.a);
}
