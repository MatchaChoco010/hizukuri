// What surrounds a rounded rectangle: its shadow, its rim and the glow of
// the rim, and what fills it. One quad, larger than the rectangle by
// `margin`. Windows, the plates of the workspaces and the workspace marks
// are all drawn with it.
//
// The fill can be frosted glass: what is behind it, blurred, tinted with the
// fill's color. What is behind comes as a copy of the picture so far, at
// half the size and with smaller levels (`paint.luau` makes it).
//
// Colors arrive as linear light in the working primaries. The output is
// premultiplied. The glow is light: it adds color with little coverage.

struct Push {
    float4 ax;          // a, b, tx, -   (pixel = a*x + b*y + tx; x, y from the
    float4 ay;          // c, d, ty, -    middle of the rectangle)
    float4 rect;        // half width, half height, corner radius, margin
    float4 fill;        // premultiplied color inside the rectangle
    float4 rim_a;       // rim color (rgb), strength of the line (a)
    float4 rim_b;       // second rim color (rgb), strength of the glow (a)
    float4 shadow;      // strength, softness, drop, opacity of everything
    float4 params;      // line width, glow width, phase of the flow, how
                        // much the two rim colors flow (0: only rim_a)
    float2 target_size;
    float2 frost;       // how much the fill is frosted glass (its alpha is
                        // then how strongly the glass is tinted), and how
                        // blurred: the level of the copy that is read
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_behind;
[[vk::binding(1, 0)]] SamplerState u_sampler;

struct VertexOutput {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 at : TEXCOORD0;
};

// 4-vertex triangle strip.
[shader("vertex")]
VertexOutput vs_main(uint index : SV_VulkanVertexID) {
    float2 corner = float2(float(index & 1), float((index >> 1) & 1));
    float2 local = (corner * 2.0 - 1.0) * (pc.rect.xy + pc.rect.w);
    float2 pixel = float2(dot(pc.ax.xy, local) + pc.ax.z, dot(pc.ay.xy, local) + pc.ay.z);
    VertexOutput o;
    o.at = local;
    o.position = float4(pixel / pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

float rounded(float2 p, float2 half_size, float radius) {
    float2 q = abs(p) - half_size + radius;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - radius;
}

// What is behind a point of the screen, blurred: nine taps of one level of
// the copy, a texel of that level apart, weighted like a tent.
float3 behind(float2 uv, float level) {
    float2 step = exp2(level + 1.0) / pc.target_size;
    float3 sum = u_behind.SampleLevel(u_sampler, uv, level).rgb * 4.0;
    sum += u_behind.SampleLevel(u_sampler, uv + float2(step.x, 0.0), level).rgb * 2.0;
    sum += u_behind.SampleLevel(u_sampler, uv - float2(step.x, 0.0), level).rgb * 2.0;
    sum += u_behind.SampleLevel(u_sampler, uv + float2(0.0, step.y), level).rgb * 2.0;
    sum += u_behind.SampleLevel(u_sampler, uv - float2(0.0, step.y), level).rgb * 2.0;
    sum += u_behind.SampleLevel(u_sampler, uv + step, level).rgb;
    sum += u_behind.SampleLevel(u_sampler, uv - step, level).rgb;
    sum += u_behind.SampleLevel(u_sampler, uv + float2(step.x, -step.y), level).rgb;
    sum += u_behind.SampleLevel(u_sampler, uv + float2(-step.x, step.y), level).rgb;
    return sum / 16.0;
}

[shader("fragment")]
float4 fs_main(VertexOutput i) : SV_Target0 {
    float2 half_size = pc.rect.xy;
    float radius = min(pc.rect.z, min(half_size.x, half_size.y));
    float d = rounded(i.at, half_size, radius);
    float aa = max(fwidth(d), 1e-5);
    float inside = clamp(0.5 - d / aa, 0.0, 1.0);

    // The shadow: the same shape, dropped a little, with a soft edge.
    float softness = max(pc.shadow.y, 1e-3);
    float ds = rounded(i.at - float2(0.0, pc.shadow.z), half_size, radius);
    float s = clamp(0.5 - ds / (2.0 * softness), 0.0, 1.0);
    float shade = pc.shadow.x * s * s * (3.0 - 2.0 * s);
    // Nothing of it under the rectangle itself (the fill may be clear),
    // and nothing left of shadow or glow where the quad ends.
    float reach = clamp((pc.rect.w - d) / max(pc.rect.w * 0.5, 1e-3), 0.0, 1.0);
    reach = reach * reach * (3.0 - 2.0 * reach);
    shade *= (1.0 - inside) * reach;

    // The rim: a line on the edge, and a glow that falls off outwards.
    float line_width = max(pc.params.x, 1e-3);
    float line = clamp((line_width * 0.5 - abs(d)) / aa + 0.5, 0.0, 1.0) * pc.rim_a.a;
    float glow_width = max(pc.params.y, 1e-3);
    float glow = exp(-max(d, 0.0) / glow_width) * (1.0 - inside) * reach * pc.rim_b.a;

    // The two rim colors travel round the rectangle.
    float turn = atan2(i.at.y * half_size.x, i.at.x * half_size.y);
    float wave = 0.5 + 0.5 * sin(turn * 2.0 - pc.params.z * 2.2)
               * (0.6 + 0.4 * sin(turn * 3.0 + pc.params.z * 1.3));
    float3 rim = lerp(pc.rim_a.rgb, pc.rim_b.rgb, wave * pc.params.w);

    float4 color = float4(0.0, 0.0, 0.0, shade);
    color = color * (1.0 - glow * 0.35) + float4(rim * glow, glow * 0.35);
    float4 fill = pc.fill;
    if (pc.frost.x > 0.0 && inside > 0.0) {
        float3 own = pc.fill.rgb / max(pc.fill.a, 1e-4);
        float3 seen = behind(i.position.xy / pc.target_size, pc.frost.y);
        fill = lerp(fill, float4(lerp(seen, own, pc.fill.a), 1.0), pc.frost.x);
    }
    color = color * (1.0 - inside * fill.a) + fill * inside;
    color = color * (1.0 - line) + float4(rim * line, line);
    return color * pc.shadow.w;
}
