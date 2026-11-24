// `blit_window` of the `color` module: the contents of one window surface,
// stretched over the whole target, as premultiplied linear light in the
// working space.
import color;

struct Push {
    WindowParams window;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_texture;
[[vk::binding(1, 0)]] SamplerState u_sampler;

struct VertexOutput {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 st : TEXCOORD0;
};

// One triangle over the whole target.
[shader("vertex")]
VertexOutput vs_main(uint index : SV_VulkanVertexID) {
    float2 corner = float2(float((index << 1) & 2), float(index & 2));
    VertexOutput o;
    o.st = corner;
    o.position = float4(corner * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

[shader("fragment")]
float4 fs_main(VertexOutput i) : SV_Target0 {
    WindowTexture window = WindowTexture(u_texture, u_sampler, pc.window);
    return window.sample_level(i.st, 0.0);
}
