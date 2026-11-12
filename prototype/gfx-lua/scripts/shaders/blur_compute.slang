// blur.luau: a box blur in a compute shader, then the result drawn to the
// target.

struct BlurPush {
    int radius;
};
[[vk::push_constant]] ConstantBuffer<BlurPush> blur_pc;

[[vk::binding(0, 0)]] Texture2D<float4> source;
[[vk::binding(1, 0)]] [format("rgba16f")] RWTexture2D<float4> destination;

[shader("compute")]
[numthreads(8, 8, 1)]
void blur(uint3 id : SV_DispatchThreadID) {
    uint width, height;
    source.GetDimensions(width, height);
    int2 size = int2(width, height);
    int2 p = int2(id.xy);
    if (p.x >= size.x || p.y >= size.y) {
        return;
    }
    float4 sum = float4(0.0);
    for (int dy = -blur_pc.radius; dy <= blur_pc.radius; dy++) {
        for (int dx = -blur_pc.radius; dx <= blur_pc.radius; dx++) {
            int2 q = clamp(p + int2(dx, dy), int2(0), size - 1);
            sum += source.Load(int3(q, 0));
        }
    }
    float side = float(2 * blur_pc.radius + 1);
    destination[p] = sum / (side * side);
}
