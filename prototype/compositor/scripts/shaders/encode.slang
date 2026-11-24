// Pass 2 of render.luau: the linear composite to the encoded values of the
// output.
import color;

struct Push {
    float4 m0;
    float4 m1;
    float4 m2;
    uint tf;
    float tf_param;
    uint clip;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_composite;

// One triangle over the whole target.
[shader("vertex")]
float4 vs_main(uint index : SV_VulkanVertexID) : SV_Position {
    float2 corner = float2(float((index << 1) & 2), float(index & 2));
    return float4(corner * 2.0 - 1.0, 0.0, 1.0);
}

[shader("fragment")]
float4 fs_main(float4 frag_coord : SV_Position) : SV_Target0 {
    float4 c = u_composite.Load(int3(int2(frag_coord.xy), 0));
    float3 linear = float3(dot(pc.m0.xyz, c.rgb), dot(pc.m1.xyz, c.rgb), dot(pc.m2.xyz, c.rgb));
    if (pc.clip != 0) {
        linear = clamp(linear, 0.0, 1.0);
    }
    return float4(tf_encode(linear, pc.tf, pc.tf_param), c.a);
}
