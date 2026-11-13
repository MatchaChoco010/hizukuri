// The launcher, drawn over what a monitor shows, into the monitor's own
// image: boxes (rounded, with a rim, and a glow around them), lines of
// text (a texture of coverage, as `hz.render_text` makes it), and letters
// of dots (one texel per dot, drawn as round dots).
//
// Colors come in the push data: linear light in the working space,
// premultiplied (the script converts them). The glow has no alpha: it
// adds light to what is under it.

struct Push {
    float4 rect;        // x, y, width, height, in pixels of the monitor
    float4 fill;        // inside (premultiplied)
    float4 rim;         // the rim inside the edge (premultiplied)
    float4 glow;        // added around the box (rgb), and how far it reaches (w, pixels)
    float4 shape;       // corner radius, rim width, kind (0 box, 1 text, 2 dots), unused
    float2 target_size; // of the monitor, in pixels
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_texture;
[[vk::binding(1, 0)]] SamplerState u_sampler;

struct VertexOutput {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 pixel : TEXCOORD0;
};

// 4-vertex triangle strip over the rectangle and the reach of its glow.
[shader("vertex")]
VertexOutput vs_main(uint index : SV_VulkanVertexID) {
    float2 corner = float2(float(index & 1), float((index >> 1) & 1));
    float reach = pc.shape.z < 0.5 ? pc.glow.w : 0.0;
    float2 pixel = pc.rect.xy - reach + corner * (pc.rect.zw + 2.0 * reach);
    VertexOutput o;
    o.pixel = pixel;
    o.position = float4(pixel / pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

// Signed distance to a rounded rectangle (negative inside).
float rounded_box(float2 p, float2 half_size, float radius) {
    float2 q = abs(p) - half_size + radius;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - radius;
}

[shader("fragment")]
float4 fs_main(VertexOutput i) : SV_Target0 {
    float kind = pc.shape.z;
    float2 uv = (i.pixel - pc.rect.xy) / pc.rect.zw;
    if (kind > 1.5) {
        // Dots: nearest texel, drawn round.
        if (any(uv < 0.0) || any(uv >= 1.0)) {
            return float4(0.0);
        }
        float2 texels;
        u_texture.GetDimensions(texels.x, texels.y);
        float2 cell = uv * texels;
        float on = u_texture.Load(int3(int2(floor(cell)), 0)).r;
        float2 within = frac(cell) - 0.5;
        float round_dot = 1.0 - smoothstep(0.34, 0.5, length(within));
        return pc.fill * (on * round_dot);
    }
    if (kind > 0.5) {
        float coverage = u_texture.SampleLevel(u_sampler, uv, 0.0).a;
        return pc.fill * coverage;
    }
    float2 half_size = pc.rect.zw * 0.5;
    float d = rounded_box(i.pixel - (pc.rect.xy + half_size), half_size, pc.shape.x);
    float inside = clamp(0.5 - d, 0.0, 1.0);
    float rim = pc.shape.y > 0.0 ? clamp(d + pc.shape.y + 0.5, 0.0, 1.0) : 0.0;
    float4 color = lerp(pc.fill, pc.rim, rim) * inside;
    if (pc.glow.w > 0.0) {
        // Gone at the edge of the quad.
        float k = clamp(1.0 - max(d, 0.0) / pc.glow.w, 0.0, 1.0);
        float falloff = k * k * k;
        color.rgb += pc.glow.rgb * falloff * (1.0 - inside);
    }
    return color;
}
