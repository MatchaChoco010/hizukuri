// What a monitor shows without a window: test bars that roll slowly, some
// static, and a box that says NO SIGNAL and drifts around, changing its
// color when it bounces off an edge.
//
// Drawn into the monitor's own image, in linear light of the working
// space (1.0 is reference white). The picture can be shifted sideways, for
// the change from and to a window.

struct Push {
    float2 size;    // of the monitor, in pixels
    float2 shift;   // of the picture, in pixels
    float time;     // seconds
    float seed;     // differs per monitor
    float gain;     // overall brightness
    float pad;
    float4 srgb0;   // rows of the matrix from linear sRGB to the working space
    float4 srgb1;
    float4 srgb2;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

// The words, one texel per dot of a 5x7 font.
[[vk::binding(0, 0)]] Texture2D<float4> u_text;
[[vk::binding(1, 0)]] SamplerState u_nearest;

[shader("vertex")]
float4 vs_main(uint index : SV_VulkanVertexID) : SV_Position {
    float2 corner = float2(float((index << 1) & 2), float(index & 2));
    return float4(corner * 2.0 - 1.0, 0.0, 1.0);
}

float hash(float2 p) {
    float3 q = frac(float3(p.xyx) * float3(0.1031, 0.1030, 0.0973));
    q += dot(q, q.yzx + 33.33);
    return frac((q.x + q.y) * q.z);
}

float3 hue(float h) {
    float3 k = frac(float3(h, h + 2.0 / 3.0, h + 1.0 / 3.0)) * 6.0;
    return clamp(abs(k - 3.0) - 1.0, 0.0, 1.0);
}

// A triangle wave: 0..1..0 over a period of 2.
float bounce(float x) {
    return abs(frac(x * 0.5) * 2.0 - 1.0);
}

float3 bars(float2 uv) {
    static const float3 COLORS[7] = {
        float3(0.75, 0.75, 0.75),
        float3(0.75, 0.75, 0.05),
        float3(0.05, 0.75, 0.75),
        float3(0.05, 0.75, 0.05),
        float3(0.75, 0.05, 0.75),
        float3(0.75, 0.05, 0.05),
        float3(0.05, 0.05, 0.75),
    };
    int index = clamp(int(floor(uv.x * 7.0)), 0, 6);
    float3 c = COLORS[index];
    if (uv.y > 0.72 && uv.y <= 0.8) {
        // The strip under the bars: the same colors, the other way round.
        c = (index % 2 == 0) ? COLORS[6 - index] : float3(0.02);
    } else if (uv.y > 0.8) {
        // A ramp and a few blocks.
        if (uv.x < 0.6) {
            c = float3(pow(uv.x / 0.6, 2.2) * 0.75);
        } else {
            c = (frac(uv.x * 10.0) < 0.5) ? float3(0.02) : float3(0.0, 0.03, 0.09);
        }
    }
    return c;
}

[shader("fragment")]
float4 fs_main(float4 frag_coord : SV_Position) : SV_Target0 {
    float2 pixel = frag_coord.xy - pc.shift;
    if (pixel.x < 0.0 || pixel.y < 0.0 || pixel.x >= pc.size.x || pixel.y >= pc.size.y) {
        discard;
    }
    float2 uv = pixel / pc.size;
    float aspect = pc.size.x / pc.size.y;
    float t = pc.time + pc.seed * 17.0;

    float3 color = bars(uv) * 0.3;
    // A dark band that rolls down the screen, as with a lost sync.
    float roll = frac(uv.y - t * 0.07 + pc.seed);
    color *= 0.55 + 0.45 * smoothstep(0.0, 0.25, abs(roll - 0.5) * 2.0);
    // Scan lines and static.
    float row = floor(uv.y * 240.0);
    color *= 0.9 + 0.1 * step(0.5, frac(uv.y * 120.0));
    float2 cell = floor(float2(uv.x * 240.0 * aspect, row));
    color += (hash(cell + floor(t * 24.0) * 7.31) - 0.5) * 0.022;
    color = max(color, 0.0);

    // The box drifts and bounces off the edges.
    // In units of the shorter side, 0.82 wide and 0.27 high.
    float2 box = aspect >= 1.0 ? float2(0.82 / aspect, 0.267) : float2(0.82, 0.267 * aspect);
    float2 room = 1.0 - box;
    float2 travel = float2(t * 0.045 + pc.seed * 3.1, t * 0.062 * aspect + pc.seed * 1.7) / max(room, 0.001);
    float2 corner = room * float2(bounce(travel.x), bounce(travel.y));
    // The color changes with every bounce.
    float bounces = floor(travel.x) + floor(travel.y);
    float3 tint = lerp(float3(1.0), hue(frac(bounces * 0.381966 + pc.seed)), 0.82);

    float2 in_box = (uv - corner) / box;
    if (in_box.x >= 0.0 && in_box.x <= 1.0 && in_box.y >= 0.0 && in_box.y <= 1.0) {
        float2 edge = min(in_box, 1.0 - in_box) * box * float2(aspect, 1.0);
        float border = step(edge.x, 0.006) + step(edge.y, 0.006);
        color = float3(0.004);
        if (border > 0.0) {
            color = tint * 0.9;
        } else {
            // The words: 53 x 7 dots, in the middle of the box.
            float2 text_uv = (in_box - float2(0.12, 0.3)) / float2(0.76, 0.4);
            if (text_uv.x >= 0.0 && text_uv.x < 1.0 && text_uv.y >= 0.0 && text_uv.y < 1.0) {
                float dot_on = u_text.SampleLevel(u_nearest, text_uv, 0.0).r;
                // Round dots.
                float2 texels;
                u_text.GetDimensions(texels.x, texels.y);
                float2 within = frac(text_uv * texels) - 0.5;
                float round_dot = 1.0 - smoothstep(0.36, 0.48, length(within));
                color = lerp(color, tint * 1.0, dot_on * round_dot);
            }
        }
    }
    // The colors above are written as linear sRGB.
    float3 working = float3(dot(pc.srgb0.xyz, color), dot(pc.srgb1.xyz, color), dot(pc.srgb2.xyz, color));
    return float4(working * pc.gain, 1.0);
}
