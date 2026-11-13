// What is behind everything: a dark wall of brushed steel, lit from below
// by a bed of embers, with a few sparks rising. The fire moves with
// `phase`, which only advances while something on screen happens (`stage`),
// and slides a little against the camera.
//
// Colors arrive as linear light in the working space.

struct Push {
    float4 top;         // color of the wall at the top (rgb)
    float4 bottom;      // color of the wall at the bottom (rgb)
    float4 ember;       // color of the embers (rgb), strength (a)
    float4 spark;       // color of the hottest spots (rgb), strength (a)
    float2 target_size;
    float2 offset;      // camera shift, in pixels
    float phase;
    float zoom;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

// One triangle over the whole target.
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

float noise(float2 p) {
    float2 i = floor(p);
    float2 f = frac(p);
    float2 u = f * f * (3.0 - 2.0 * f);
    float a = hash(i);
    float b = hash(i + float2(1.0, 0.0));
    float c = hash(i + float2(0.0, 1.0));
    float d = hash(i + float2(1.0, 1.0));
    return lerp(lerp(a, b, u.x), lerp(c, d, u.x), u.y);
}

float fbm(float2 p) {
    float sum = 0.0;
    float gain = 0.5;
    for (int octave = 0; octave < 4; octave++) {
        sum += gain * noise(p);
        p = p * 2.03 + float2(17.1, 9.2);
        gain *= 0.5;
    }
    return sum;
}

// Sparks: one per cell of a grid that drifts upwards, in a share of the
// cells; each a small bright point that dims as it rises.
float sparks(float2 p, float height, float phase) {
    float glow = 0.0;
    for (int layer = 0; layer < 2; layer++) {
        float cells = 9.0 + 7.0 * float(layer);
        float column = floor(p.x * cells);
        float speed = 0.25 + 0.5 * hash(float2(column, 3.0 + float(layer)));
        float2 uv = float2(p.x * cells, p.y * cells + phase * speed * 1.6);
        float2 cell = floor(uv);
        float2 f = frac(uv);
        float there = step(0.78, hash(cell + float2(11.0, 5.0) * float(layer + 1)));
        float2 at = 0.5 + 0.5 * (float2(hash(cell + 1.7), hash(cell + 9.1)) - 0.5);
        float d = length(f - at) * (1.0 / cells) * pc.target_size.y;
        float size = 1.1 + 1.6 * hash(cell + 4.3);
        float point = clamp(1.0 - d / size, 0.0, 1.0);
        // The halo ends before the edge of the cell, where the next
        // cell's spark takes over.
        float2 edge = min(f, 1.0 - f);
        float halo = exp(-d / (size * 3.0)) * 0.22 * smoothstep(0.0, 0.25, min(edge.x, edge.y));
        glow += there * (point * point + halo);
    }
    // Brightest just above the fire, gone half way up.
    return glow * smoothstep(0.42, 1.0, height) * smoothstep(0.42, 1.0, height);
}

[shader("fragment")]
float4 fs_main(float4 frag_coord : SV_Position) : SV_Target0 {
    float2 size = pc.target_size;
    // Screen position from the middle, in units of the height; pulled back
    // with the camera, but less.
    float2 p = (frag_coord.xy - 0.5 * size) / size.y;
    p /= lerp(1.0, pc.zoom, 0.35);
    p.x += pc.offset.x / size.y;
    float height = clamp(p.y + 0.5, 0.0, 1.0);

    // The wall: darker at the top, with the grain of brushed metal.
    float3 color = lerp(pc.top.rgb, pc.bottom.rgb, height * height);
    float brush = fbm(float2(p.x * 2.5, p.y * 90.0)) - 0.47;
    float plates = fbm(float2(p.x * 0.9 + 3.0, p.y * 1.3)) - 0.47;
    color *= 1.0 + 0.22 * brush + 0.7 * plates;

    // The fire below the lower edge: its light on the wall falls off
    // upwards, and tongues of it reach up, slowly changing.
    float low = 1.0 - height;
    float light = exp(-low * 4.2);
    float2 q = float2(p.x * 1.3, p.y * 2.0 - pc.phase * 0.10);
    float warp = fbm(q * 0.8 + float2(0.0, pc.phase * 0.04));
    float field = fbm(q + float2(warp * 1.5, -warp * 0.7) + float2(pc.phase * 0.025, 0.0));
    float tongues = smoothstep(0.22, 0.95, field + (height - 0.78) * 1.1);
    float3 fire = pc.ember.rgb * pc.ember.a * (0.5 * light + 1.1 * tongues * light);
    // The hotter, the more towards the spark color.
    float hot = smoothstep(0.5, 1.0, tongues * light * 1.5);
    fire += pc.spark.rgb * pc.spark.a * hot * hot * 0.5;
    color += fire;
    color += pc.spark.rgb * pc.spark.a * 2.2 * sparks(p, height, pc.phase);

    // Darker towards the corners.
    float away = dot(p * float2(0.75, 1.0), p * float2(0.75, 1.0));
    color *= 1.0 - 0.5 * smoothstep(0.2, 1.3, away);

    // A grain finer than a code value, against banding in the dark.
    float grain = hash(frag_coord.xy) - 0.5;
    color += color * grain * 0.05 + grain * 0.0005;
    return float4(max(color, 0.0), 1.0);
}
