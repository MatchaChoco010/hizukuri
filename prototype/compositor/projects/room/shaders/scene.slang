// The room, drawn in linear light of the working space (1.0 is reference
// white; nothing is clipped).
//
//   vs_room / fs_room      the geometry of the room, lit by the monitors
//   vs_screen / fs_screen  the screens themselves: they emit what they show
//
// The lights are the screens, as textured area lights (the module `ltc`:
// linearly transformed cosines), the light the desk throws back from them,
// and a very dim ambient term. There are no shadows.
import ltc;

static const uint MAX_MONITORS = 8;

struct Monitor {
    float4 center;  // xyz: the middle of the screen
    float4 right;   // xyz: from the middle to the right edge; w: how far a change of picture is (0: none)
    float4 up;      // xyz: from the middle to the top edge; w: how much it has the focus (0..1)
    float4 normal;  // xyz: unit, towards the viewer; w: how much of its light reaches the desk
};

// Something that stands on the desk and keeps light off it around its
// foot.
static const uint MAX_BLOCKERS = 8;
struct Blocker {
    float4 place;   // x, z of its middle; half sizes of its footprint
    float4 turn;    // cos and sin of its angle about the vertical; its height
};

struct Frame {
    float4 vp0;     // rows of world -> clip
    float4 vp1;
    float4 vp2;
    float4 vp3;
    float4 eye;     // xyz: the camera; w: time in seconds
    float4 params;  // x: number of monitors, y: ambient light, z: gain of diffuse light, w: gain of glossy light
    float4 adapt;   // x: strength (0: none), y: the mean brightness of the screens that needs none, z: floor of that mean; w: how blurred the mip levels of the lights are
    // The top of the desk, as a light: what it throws back from the screens.
    float4 bounce_center;
    float4 bounce_right;
    float4 bounce_up;
    float4 bounce_tint;     // rgb: albedo of the desk times the strength; w: number of blockers
    Monitor monitors[MAX_MONITORS];
    Blocker blockers[MAX_BLOCKERS];
    // Rows of the matrix from linear sRGB to the working space: colors
    // written in this file are linear sRGB, and go through `from_srgb`.
    float4 srgb0;
    float4 srgb1;
    float4 srgb2;
    // xyz: the weights of red, green and blue in the brightness of a color
    // of the working space. w: how bright the output can get, in multiples
    // of reference white; surfaces bend over towards it (0: they do not).
    float4 luma;
};

[[vk::binding(0, 0)]] ConstantBuffer<Frame> frame;
// For a screen as it is seen: anisotropic, with mip levels.
[[vk::binding(1, 0)]] SamplerState u_screen_sampler;
// For a screen as a light: linear, also between the mip levels, clamped.
[[vk::binding(2, 0)]] SamplerState u_light_sampler;
// What each monitor shows, with mip levels.
[[vk::binding(3, 0)]] Texture2D<float4> u_screens[MAX_MONITORS];
[[vk::binding(4, 0)]] Texture2D<float4> u_ltc_mat;
[[vk::binding(5, 0)]] Texture2D<float4> u_ltc_amp;
[[vk::binding(6, 0)]] SamplerState u_ltc_sampler;
// What each monitor shows, as a light reads it: blurred mip levels.
[[vk::binding(7, 0)]] Texture2D<float4> u_lights[MAX_MONITORS];

struct ScreenPush {
    uint monitor;
    uint part;      // PART_SCREEN or PART_LAMP
};
static const uint PART_SCREEN = 0;
static const uint PART_LAMP = 1;
[[vk::push_constant]] ConstantBuffer<ScreenPush> pc;

static const float PI = 3.14159265;

float4 to_clip(float3 p) {
    float4 w = float4(p, 1.0);
    return float4(dot(frame.vp0, w), dot(frame.vp1, w), dot(frame.vp2, w), dot(frame.vp3, w));
}

// ----- the room ---------------------------------------------------------------

struct RoomVertex {
    [[vk::location(0)]] float3 position : POSITION;
    [[vk::location(1)]] float3 normal : NORMAL;
    [[vk::location(2)]] float4 albedo_roughness : COLOR0;
    [[vk::location(3)]] float4 params : COLOR1;     // x: reflectance at normal incidence, y: kind
};

struct RoomVarying {
    float4 clip : SV_Position;
    [[vk::location(0)]] float3 position : POSITION;
    [[vk::location(1)]] float3 normal : NORMAL;
    [[vk::location(2)]] float4 albedo_roughness : COLOR0;
    [[vk::location(3)]] float4 params : COLOR1;
};

[shader("vertex")]
RoomVarying vs_room(RoomVertex v) {
    RoomVarying o;
    o.clip = to_clip(v.position);
    o.position = v.position;
    o.normal = v.normal;
    o.albedo_roughness = v.albedo_roughness;
    o.params = v.params;
    return o;
}

float hash2(float2 p) {
    float3 q = frac(float3(p.xyx) * float3(0.1031, 0.1030, 0.0973));
    q += dot(q, q.yzx + 33.33);
    return frac((q.x + q.y) * q.z);
}

float value_noise(float2 p) {
    float2 i = floor(p);
    float2 f = frac(p);
    f = f * f * (3.0 - 2.0 * f);
    return lerp(lerp(hash2(i), hash2(i + float2(1.0, 0.0)), f.x),
                lerp(hash2(i + float2(0.0, 1.0)), hash2(i + float2(1.0, 1.0)), f.x), f.y);
}

float fbm(float2 p) {
    float sum = 0.0;
    float gain = 0.5;
    for (int i = 0; i < 4; i++) {
        sum += value_noise(p) * gain;
        p = p * 2.03 + 17.1;
        gain *= 0.5;
    }
    return sum;
}

// The pattern of a material: changes the albedo and the roughness.
void pattern(float kind, float3 p, float3 n, inout float3 albedo, inout float roughness) {
    if (kind == 1.0) {
        // Wood: long grain along x.
        float2 q = float2(p.x * 2.5, p.z * 38.0 + p.y * 38.0);
        float grain = fbm(q + fbm(q * 0.5) * 2.0);
        float ring = 0.5 + 0.5 * sin((p.z + p.y) * 90.0 + grain * 9.0);
        albedo *= 0.62 + 0.55 * grain + 0.16 * ring;
        roughness = clamp(roughness + (grain - 0.5) * 0.12, 0.05, 1.0);
    } else if (kind == 2.0) {
        // Plaster: faint blotches.
        float2 q = abs(n.z) > 0.5 ? p.xy : p.zy;
        albedo *= 0.9 + 0.2 * fbm(q * 3.0);
    } else if (kind == 3.0) {
        // Carpet: fine speckle.
        albedo *= 0.75 + 0.5 * hash2(floor(p.xz * 700.0));
    }
}

float luminance(float3 c) {
    return dot(c, frame.luma.xyz);
}

// A color written as linear sRGB, in the working space.
float3 from_srgb(float3 c) {
    return float3(dot(frame.srgb0.xyz, c), dot(frame.srgb1.xyz, c), dot(frame.srgb2.xyz, c));
}

// How much of the light of a rectangle (center, half extents `right` and
// `up`, facing `normal`) falls on a surface point: the sum over 3 x 3
// points of it. Times the radiance of the rectangle, this is the
// irradiance divided by pi.
float rectangle_form(float3 p, float3 n, float3 center, float3 right, float3 up, float3 normal) {
    float area = 4.0 * length(right) * length(up);
    float form = 0.0;
    for (int a = -1; a <= 1; a++) {
        for (int b = -1; b <= 1; b++) {
            float3 d = center + right * (float(a) * 0.667) + up * (float(b) * 0.667) - p;
            float dist2 = dot(d, d);
            float3 l = d / sqrt(max(dist2, 1e-6));
            // A point stands for a ninth of the rectangle: not closer than
            // about its own size.
            form += saturate(dot(n, l)) * saturate(dot(normal, -l)) / max(dist2, area / 9.0);
        }
    }
    return form * area / 9.0 / PI;
}

// How much light reaches a point of the top of the desk past the things
// that stand on it (1: all of it). There are no shadows; this darkens the
// desk close around each thing, the more the taller it is, so that things
// stand on the desk instead of floating over it.
float grounding(float3 p, float3 n) {
    float above = p.y - frame.bounce_center.y;
    if (frame.bounce_right.w > 0.5 || n.y < 0.9 || above < -0.002 || above > 0.008) {
        return 1.0;
    }
    float light = 1.0;
    uint count = min(uint(frame.bounce_tint.w), MAX_BLOCKERS);
    for (uint i = 0; i < count; i++) {
        Blocker b = frame.blockers[i];
        float2 d = p.xz - b.place.xy;
        float2 q = float2(d.x * b.turn.x - d.y * b.turn.y, d.x * b.turn.y + d.y * b.turn.x);
        float2 outside = max(abs(q) - b.place.zw, 0.0);
        float reach = b.turn.z * 0.8 + 0.01;
        light *= 1.0 - 0.72 * exp(-dot(outside, outside) / (reach * reach));
    }
    return light;
}

// The light that leaves a point of a surface towards the camera.
float3 lit(float3 p, float3 n, float3 v, float3 albedo, float roughness, float f0) {
    LtcTables tables = LtcTables(u_ltc_mat, u_ltc_amp, u_ltc_sampler);
    LtcSurface surface = ltc_surface(tables, p, n, v, max(roughness, 0.08));

    float3 diffuse = float3(0.0);
    float3 glossy = float3(0.0);
    float3 thrown = float3(0.0);
    float mean = 0.0;
    float total_area = 0.0;
    uint count = min(uint(frame.params.x), MAX_MONITORS);
    // Shadows only where they are seen: on the top of the desk (and on the
    // flat things that lie on it).
    float3 on_desk = p - frame.bounce_center.xyz;
    bool shadowed = frame.bounce_right.w > 0.5 && n.y > 0.9
        && on_desk.y > -0.002 && on_desk.y < 0.008
        && abs(on_desk.x) < frame.bounce_right.x + 0.02
        && abs(on_desk.z) < -frame.bounce_up.z + 0.02;
    uint blockers = min(uint(frame.bounce_tint.w), MAX_BLOCKERS);
    for (uint i = 0; i < count; i++) {
        Monitor m = frame.monitors[i];
        float3 c = m.center.xyz;
        float3 right = m.right.xyz;
        float3 up = m.up.xyz;

        // The screen as an area light that shows its picture.
        LtcQuad quad = LtcQuad(c - right + up, c + right + up, c + right - up, c - right - up, false);
        LtcLobes lobes = ltc_quad(tables, surface, quad);
        if (shadowed && (lobes.diffuse.form_factor > 0.0 || lobes.specular.form_factor > 0.0)) {
            // What stands on the desk hides a part of the screen.
            for (uint b = 0; b < blockers; b++) {
                Blocker k = frame.blockers[b];
                float half_height = k.turn.z * 0.5;
                LtcBox box = LtcBox(
                    float3(k.place.x, frame.bounce_center.y + half_height, k.place.y),
                    float3(k.turn.x, 0.0, -k.turn.y) * k.place.z,
                    float3(0.0, half_height, 0.0),
                    float3(k.turn.y, 0.0, k.turn.x) * k.place.w);
                float4 cover = ltc_box_cover(surface, quad, box);
                lobes.diffuse.form_factor *= ltc_uncovered(lobes.diffuse, cover);
                lobes.specular.form_factor *= ltc_uncovered(lobes.specular, cover);
            }
        }
        if (lobes.diffuse.form_factor > 0.0) {
            diffuse += ltc_light_colour(u_lights[i], u_light_sampler, lobes.diffuse, frame.adapt.w)
                * lobes.diffuse.form_factor;
        }
        if (lobes.specular.form_factor > 0.0) {
            glossy += ltc_light_colour(u_lights[i], u_light_sampler, lobes.specular, frame.adapt.w)
                * lobes.specular.form_factor;
        }

        // The smallest mip level: the mean of what the screen shows.
        float3 average = u_screens[i].SampleLevel(u_light_sampler, float2(0.5, 0.5), 20.0).rgb;
        float area = 4.0 * length(right) * length(up);
        mean += luminance(average) * area;
        total_area += area;
        thrown += average * m.normal.w;
    }
    // What the desk throws back.
    float3 bn = normalize(cross(frame.bounce_right.xyz, frame.bounce_up.xyz));
    float3 bounce = thrown * frame.bounce_tint.rgb
        * rectangle_form(p, n, frame.bounce_center.xyz, frame.bounce_right.xyz, frame.bounce_up.xyz, bn);

    // With dark screens the eye opens up.
    mean = max(mean / max(total_area, 1e-4), frame.adapt.z);
    float adapt = pow(frame.adapt.y / mean, frame.adapt.x);

    float3 color = albedo * (diffuse * frame.params.z + bounce)
        + ltc_specular_colour(surface, float3(f0)) * glossy * frame.params.w;
    return color * adapt * grounding(p, n) + albedo * frame.params.y;
}

[shader("fragment")]
float4 fs_room(RoomVarying i) : SV_Target0 {
    float3 n = normalize(i.normal);
    float3 v = normalize(frame.eye.xyz - i.position);
    float3 albedo = i.albedo_roughness.rgb;
    float roughness = i.albedo_roughness.a;
    pattern(i.params.y, i.position, n, albedo, roughness);
    float3 color = lit(i.position, n, v, albedo, roughness, i.params.x);
    // The brightest parts of a surface bend over towards the brightest the
    // output can show, instead of being cut off there (what a screen shows
    // is left as it is). The hue is kept: every channel is scaled alike.
    // This is tone mapping inside the renderer, for the surfaces alone; it
    // is turned off when the way out maps tones itself.
    float ceiling = frame.luma.w;
    float peak = max(color.r, max(color.g, color.b));
    float knee = 0.6 * ceiling;
    if (ceiling > 0.0 && peak > knee) {
        float bent = knee + (ceiling - knee) * tanh((peak - knee) / (ceiling - knee));
        color *= bent / peak;
    }
    return float4(color, 1.0);
}

// ----- the screens ------------------------------------------------------------

struct ScreenVarying {
    float4 clip : SV_Position;
    [[vk::location(0)]] float2 uv : TEXCOORD0;
    [[vk::location(1)]] float3 position : POSITION;
};

// 4-vertex triangle strip: top left, top right, bottom left, bottom right.
// The screen itself, or the small lamp in the bar under it.
[shader("vertex")]
ScreenVarying vs_screen(uint index : SV_VulkanVertexID) {
    Monitor m = frame.monitors[min(pc.monitor, MAX_MONITORS - 1)];
    float2 corner = float2(float(index & 1), float((index >> 1) & 1));
    float3 right = m.right.xyz;
    float3 up = m.up.xyz;
    float3 center = m.center.xyz;
    if (pc.part == PART_LAMP) {
        // In the middle of the bar, a little proud of it.
        float3 unit_up = normalize(up);
        center += -up - unit_up * 0.009 + m.normal.xyz * 0.0028;
        right = normalize(right) * 0.03;
        up = unit_up * 0.0011;
    }
    float3 p = center + right * (corner.x * 2.0 - 1.0) + up * (1.0 - corner.y * 2.0);
    ScreenVarying o;
    o.clip = to_clip(p);
    o.uv = corner;
    o.position = p;
    return o;
}

[shader("fragment")]
float4 fs_screen(ScreenVarying i) : SV_Target0 {
    uint index = min(pc.monitor, MAX_MONITORS - 1);
    Monitor m = frame.monitors[index];
    if (pc.part == PART_LAMP) {
        // Lit on the monitor that has the focus, a dim ember on the others.
        float focus = m.up.w;
        return float4(from_srgb(lerp(float3(0.06, 0.012, 0.003), float3(0.45, 1.5, 2.2), focus)), 1.0);
    }
    float2 uv = i.uv;
    float change = m.right.w;
    if (change <= 0.0) {
        return float4(u_screens[index].Sample(u_screen_sampler, uv).rgb, 1.0);
    }
    // The picture is changing: the lines tear sideways, the colors come
    // apart, and a bright bar rolls down, strongest half way through.
    float strength = sin(PI * change);
    float t = frame.eye.w;
    float row = floor(uv.y * 72.0);
    float tear = (hash2(float2(row, floor(t * 50.0))) - 0.5) * 0.07 * strength * strength;
    uv.x += tear;
    float apart = 0.008 * strength;
    float3 shown = float3(
        u_screens[index].SampleLevel(u_screen_sampler, uv + float2(apart, 0.0), 0.5).r,
        u_screens[index].SampleLevel(u_screen_sampler, uv, 0.5).g,
        u_screens[index].SampleLevel(u_screen_sampler, uv - float2(apart, 0.0), 0.5).b);
    float bar = frac(uv.y * 0.75 - change * 1.6);
    shown *= 1.0 + 1.4 * strength * exp(-bar * bar * 60.0);
    shown += (hash2(floor(uv * float2(160.0, 90.0)) + floor(t * 50.0)) - 0.4) * 0.06 * strength;
    return float4(max(shown, 0.0), 1.0);
}
