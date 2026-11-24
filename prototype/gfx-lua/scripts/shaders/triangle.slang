// triangle.luau: vertex positions and colours from a vertex buffer.

struct VsOut {
    float4 position : SV_Position;
    [[vk::location(0)]] float4 color : COLOR0;
};

[shader("vertex")]
VsOut vs([[vk::location(0)]] float2 position : POSITION, [[vk::location(1)]] float4 color : COLOR0) {
    VsOut o;
    o.position = float4(position, 0.0, 1.0);
    o.color = color;
    return o;
}

[shader("fragment")]
float4 fs(VsOut i) : SV_Target0 {
    return i.color;
}
