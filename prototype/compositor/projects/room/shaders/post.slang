// What a lens does to the picture of the room: light of the brightest
// parts spreads around them, and the corners are a little darker.
//
//   fs_bright   the bright parts of the picture, at half its size
//   fs_combine  the picture plus those parts, blurred (their mip levels)
//
// All in linear light of the working space.

struct Push {
    float2 size;        // of the target, in pixels
    float strength;     // of the spread light
    float vignette;     // how much darker the corners are (0: not at all)
    float threshold;    // brightness from which light spreads
    float pad0;
    float pad1;
    float pad2;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_picture;
[[vk::binding(1, 0)]] SamplerState u_linear;
// The bright parts, with mip levels.
[[vk::binding(2, 0)]] Texture2D<float4> u_bright;

[shader("vertex")]
float4 vs_main(uint index : SV_VulkanVertexID) : SV_Position {
    float2 corner = float2(float((index << 1) & 2), float(index & 2));
    return float4(corner * 2.0 - 1.0, 0.0, 1.0);
}

[shader("fragment")]
float4 fs_bright(float4 frag_coord : SV_Position) : SV_Target0 {
    float2 uv = frag_coord.xy / pc.size;
    float3 c = max(u_picture.SampleLevel(u_linear, uv, 0.0).rgb, 0.0);
    float peak = max(c.r, max(c.g, c.b));
    float keep = smoothstep(pc.threshold, pc.threshold + 0.6, peak);
    return float4(c * keep, 1.0);
}

[shader("fragment")]
float4 fs_combine(float4 frag_coord : SV_Position) : SV_Target0 {
    float2 uv = frag_coord.xy / pc.size;
    float4 picture = u_picture.Load(int3(int2(frag_coord.xy), 0));
    // A wide, soft spread: several levels, each wider and fainter.
    float3 spread = float3(0.0);
    spread += u_bright.SampleLevel(u_linear, uv, 1.5).rgb * 0.30;
    spread += u_bright.SampleLevel(u_linear, uv, 2.5).rgb * 0.26;
    spread += u_bright.SampleLevel(u_linear, uv, 3.5).rgb * 0.20;
    spread += u_bright.SampleLevel(u_linear, uv, 4.5).rgb * 0.14;
    spread += u_bright.SampleLevel(u_linear, uv, 5.5).rgb * 0.10;
    float3 color = picture.rgb + spread * pc.strength;
    float2 from_center = uv * 2.0 - 1.0;
    float falloff = dot(from_center, from_center) * 0.5;
    color *= 1.0 - pc.vignette * smoothstep(0.15, 1.0, falloff);
    return float4(color, picture.a);
}
