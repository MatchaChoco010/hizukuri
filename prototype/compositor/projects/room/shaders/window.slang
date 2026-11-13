// A surface of a window as a flat quad: on a monitor's own image, and the
// cursor over the finished picture. Read through the core's `WindowTexture`,
// which gives linear light in the working space with alpha premultiplied.
import color;

struct Push {
    float4 ax;          // a, b, tx, quad width   (pixel = a*x + b*y + tx)
    float4 ay;          // c, d, ty, quad height  (pixel = c*x + d*y + ty)
    WindowParams window;
    float2 target_size;
    float opacity;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_texture;
[[vk::binding(1, 0)]] SamplerState u_sampler;

struct VertexOutput {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 st : TEXCOORD0;
};

// 4-vertex triangle strip.
[shader("vertex")]
VertexOutput vs_main(uint index : SV_VulkanVertexID) {
    float2 corner = float2(float(index & 1), float((index >> 1) & 1));
    float2 local = corner * float2(pc.ax.w, pc.ay.w);
    float2 pixel = float2(dot(pc.ax.xy, local) + pc.ax.z, dot(pc.ay.xy, local) + pc.ay.z);
    VertexOutput o;
    o.st = corner;
    o.position = float4(pixel / pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

[shader("fragment")]
float4 fs_main(VertexOutput i) : SV_Target0 {
    WindowTexture window = WindowTexture(u_texture, u_sampler, pc.window);
    return window.sample_level(i.st, 0.0) * pc.opacity;
}
