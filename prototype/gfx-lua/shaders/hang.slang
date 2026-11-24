// gfx-probe shader-hang: a fragment shader that does not return until push
// data says so, which it never does.

struct Push {
    uint stop;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[shader("vertex")]
float4 vs(uint index : SV_VulkanVertexID) : SV_Position {
    float2 corners[3] = { float2(-1.0, -1.0), float2(3.0, -1.0), float2(-1.0, 3.0) };
    return float4(corners[index], 0.0, 1.0);
}

[shader("fragment")]
float4 fs() : SV_Target0 {
    uint spins = 0;
    while (true) {
        if (pc.stop != 0) {
            break;
        }
        spins += 1;
    }
    return float4(float(spins & 1), 1.0, 0.0, 1.0);
}
