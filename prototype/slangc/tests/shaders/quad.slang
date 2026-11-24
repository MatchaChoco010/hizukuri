// The QUAD shader of prototype/gfx-core/tests/misuse.rs, in Slang, with the
// same set 0 / binding N layout the GPU API maps to the descriptor heap.
struct Push { float4 color; };
[[vk::push_constant]] ConstantBuffer<Push> pc;
struct Globals { float4 gain; };
[[vk::binding(0, 0)]] Texture2D<float4> tex;
[[vk::binding(1, 0)]] SamplerState smp;
[[vk::binding(2, 0)]] ConstantBuffer<Globals> globals;
[[vk::binding(4, 0)]] Texture2D<uint4> unused_tex;

struct VsOut {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 uv : TEXCOORD0;
};

[shader("vertex")]
VsOut vs([[vk::location(0)]] float2 position : POSITION) {
    VsOut o;
    o.position = float4(position, 0.0, 1.0);
    o.uv = position * 0.5 + 0.5;
    return o;
}

[shader("fragment")]
float4 fs(VsOut i) : SV_Target0 {
    return tex.Sample(smp, i.uv) * pc.color * globals.gain;
}
