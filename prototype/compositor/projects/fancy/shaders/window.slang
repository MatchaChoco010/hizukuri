// A surface of a window: a quad that shows the surface's contents, read
// through the core's `WindowTexture` (linear light in the working space,
// alpha premultiplied), cut to the rounded rectangle of its window.
import color;

struct Push {
    WindowParams window;    // what it takes to read the surface (96 bytes)
    float4 ax;              // a, b, tx, quad width   (pixel = a*x + b*y + tx)
    float4 ay;              // c, d, ty, quad height  (pixel = c*x + d*y + ty)
    float2 origin;          // where the quad starts, in the window's coordinates
    float2 target_size;
    float4 mask;            // the window's rectangle as drawn: half width, half
                            // height, corner radius; w > 0 turns the cut on
    float4 fit;             // xy: the window's coordinates to that rectangle's
                            // (contents stretched to the rectangle); z: opacity
    float4 glow;            // the color the contents glow in (rgb), how much (a):
                            // a window fresh out of the fire, or going out
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_texture;
[[vk::binding(1, 0)]] SamplerState u_sampler;

struct VertexOutput {
    float4 position : SV_Position;
    // The point on the surface (0..1).
    [[vk::location(0)]] float2 st : TEXCOORD0;
    // The point in the rectangle's coordinates, from its middle.
    [[vk::location(1)]] float2 at : TEXCOORD1;
};

// 4-vertex triangle strip.
[shader("vertex")]
VertexOutput vs_main(uint index : SV_VulkanVertexID) {
    float2 corner = float2(float(index & 1), float((index >> 1) & 1));
    float2 local = corner * float2(pc.ax.w, pc.ay.w);
    float2 pixel = float2(dot(pc.ax.xy, local) + pc.ax.z, dot(pc.ay.xy, local) + pc.ay.z);
    VertexOutput o;
    o.st = corner;
    o.at = (pc.origin + local) * pc.fit.xy - pc.mask.xy;
    o.position = float4(pixel / pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

// Signed distance to a rounded rectangle around the origin.
float rounded(float2 p, float2 half_size, float radius) {
    float2 q = abs(p) - half_size + radius;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - radius;
}

[shader("fragment")]
float4 fs_main(VertexOutput i) : SV_Target0 {
    WindowTexture window = WindowTexture(u_texture, u_sampler, pc.window);
    float4 color = window.sample_level(i.st, 0.0);
    float cut = 1.0;
    // How much of the glow there is here: all of it at the edge of the
    // window, less towards its middle (the edges of a plate stay hot longest).
    float glowing = pc.glow.a;
    if (pc.mask.w > 0.0) {
        float d = rounded(i.at, pc.mask.xy, pc.mask.z);
        cut = clamp(0.5 - d / max(fwidth(d), 1e-5), 0.0, 1.0);
        float reach = 0.35 * min(pc.mask.x, pc.mask.y);
        glowing *= 0.4 + 0.6 * smoothstep(-reach, 0.0, d);
    }
    // Glowing: its own light on top of what it shows, which pales.
    color.rgb = color.rgb * (1.0 - 0.6 * glowing) + pc.glow.rgb * (color.a * glowing);
    return color * (pc.fit.z * cut);
}
