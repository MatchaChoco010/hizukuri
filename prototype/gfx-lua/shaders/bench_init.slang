// gfx-bench init: the shader whose creation the benchmark times on its own.

struct Push {
    float4 rect;
    float4 tint;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> t;
[[vk::binding(1, 0)]] SamplerState s;

struct VsOut {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 uv : TEXCOORD0;
};

[shader("vertex")]
VsOut vs(uint i : SV_VulkanVertexID) {
    float2 c[3] = { float2(-1.0, -1.0), float2(3.0, -1.0), float2(-1.0, 3.0) };
    VsOut o;
    o.position = float4(c[i] * pc.rect.zw + pc.rect.xy, 0.0, 1.0);
    o.uv = c[i];
    return o;
}

[shader("fragment")]
float4 fs(VsOut i) : SV_Target0 {
    return t.Sample(s, i.uv) * pc.tint;
}
