// Copies an image to one of half its size: every pixel the mean of the
// four it covers. The first step of the blur behind glass (`paint.luau`
// then lets the core make the smaller levels).

[[vk::binding(0, 0)]] Texture2D<float4> u_source;

// One triangle over the whole target.
[shader("vertex")]
float4 vs_main(uint index : SV_VulkanVertexID) : SV_Position {
    float2 corner = float2(float((index << 1) & 2), float(index & 2));
    return float4(corner * 2.0 - 1.0, 0.0, 1.0);
}

[shader("fragment")]
float4 fs_main(float4 frag_coord : SV_Position) : SV_Target0 {
    uint width, height;
    u_source.GetDimensions(width, height);
    int2 last = int2(int(width) - 1, int(height) - 1);
    int2 at = int2(frag_coord.xy) * 2;
    float4 sum = u_source.Load(int3(min(at, last), 0))
               + u_source.Load(int3(min(at + int2(1, 0), last), 0))
               + u_source.Load(int3(min(at + int2(0, 1), last), 0))
               + u_source.Load(int3(min(at + int2(1, 1), last), 0));
    return sum * 0.25;
}
