// The shaders of ltc-check: single points whose shading is read back and
// compared, and a small scene (a floor and boxes under quad lights) to look
// at.
import ltc;

[[vk::binding(0, 0)]] Texture2D<float4> u_mat;
[[vk::binding(1, 0)]] Texture2D<float4> u_amp;
[[vk::binding(2, 0)]] SamplerState u_table_sampler;
[[vk::binding(3, 0)]] Texture2D<float4> u_light0;
[[vk::binding(4, 0)]] Texture2D<float4> u_light1;
[[vk::binding(5, 0)]] Texture2D<float4> u_light2;
[[vk::binding(6, 0)]] Texture2D<float4> u_light3;
[[vk::binding(7, 0)]] SamplerState u_light_sampler;

struct VsOut {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 ndc : TEXCOORD0;
};

// One triangle over the whole target.
[shader("vertex")]
VsOut vs_main(uint index : SV_VulkanVertexID) {
    float2 corner = float2(float((index << 1) & 2), float(index & 2));
    VsOut o;
    o.ndc = corner * 2.0 - 1.0;
    o.position = float4(o.ndc, 0.0, 1.0);
    return o;
}

// A box: its centre and its three half extents.
struct Box {
    float3 center;
    float pad0;
    float3 x;
    float pad1;
    float3 y;
    float pad2;
    float3 z;
    float pad3;
};

LtcBox ltc_box(Box b) {
    return LtcBox(b.center, b.x, b.y, b.z);
}

// ---- single points ----------------------------------------------------------

struct Probe {
    float3 position;
    float roughness;
    float3 normal;
    // What to write: see fs_probe.
    uint mode;
    float3 view;
    float level_sigma;
    // The light: its (0, 0) corner and its two edges.
    float3 p0;
    float two_sided;
    float3 e1;
    float pad1;
    float3 e2;
    float pad2;
    // A box in the light's way.
    Box box;
};
[[vk::push_constant]] ConstantBuffer<Probe> probe;

[shader("fragment")]
float4 fs_probe() : SV_Target0 {
    LtcTables tables = LtcTables(u_mat, u_amp, u_table_sampler);
    LtcSurface s = ltc_surface(tables, probe.position, probe.normal, probe.view, probe.roughness);
    LtcQuad q = LtcQuad(probe.p0, probe.p0 + probe.e1, probe.p0 + probe.e1 + probe.e2, probe.p0 + probe.e2, probe.two_sided != 0.0);
    LtcLobes lobes = ltc_quad(tables, s, q);
    switch (probe.mode) {
    case 0: {
        // The form factors, with the table's horizon term and clipped.
        LtcLobes clipped = ltc_quad_clipped(s, q);
        return float4(lobes.diffuse.form_factor, lobes.specular.form_factor, clipped.diffuse.form_factor, clipped.specular.form_factor);
    }
    case 1:
        return float4(s.norm, s.fresnel, lobes.diffuse.blur, lobes.specular.blur);
    case 2:
        return float4(lobes.diffuse.uv, lobes.specular.uv);
    case 3: {
        LtcLight l = ltc_quad_textured(tables, s, q, u_light0, u_light_sampler, probe.level_sigma);
        return float4(l.diffuse, 1.0);
    }
    case 4: {
        LtcLight l = ltc_quad_textured(tables, s, q, u_light0, u_light_sampler, probe.level_sigma);
        return float4(l.specular, 1.0);
    }
    default: {
        // What the box leaves of each lobe, and the lobes' spreads along u.
        float4 cover = ltc_box_cover(s, q, ltc_box(probe.box));
        return float4(ltc_uncovered(lobes.diffuse, cover), ltc_uncovered(lobes.specular, cover), lobes.diffuse.spread.x, lobes.specular.spread.x);
    }
    }
}

// ---- the scene --------------------------------------------------------------

struct SceneLight {
    float3 p0;
    float intensity;
    float3 e1;
    float two_sided;
    float3 e2;
    float pad0;
    float3 tint;
    float pad1;
};

struct Scene {
    // The camera: a pinhole at `eye`.
    float3 eye;
    float tan_half_fov;
    float3 forward;
    float aspect;
    float3 right;
    // The floor (z = 0) has five bands along x, from this roughness...
    float roughness_low;
    float3 up;
    // ...to this one.
    float roughness_high;
    float3 albedo;
    float light_count;
    float3 f0;
    float level_sigma;
    SceneLight lights[4];
    // The boxes standing on the floor.
    float3 box_albedo;
    float box_count;
    Box boxes[4];
};
[[vk::binding(8, 0)]] ConstantBuffer<Scene> scene;

float4 light_texel(uint index, float2 uv) {
    switch (index) {
    case 0: return u_light0.SampleLevel(u_light_sampler, uv, 0.0);
    case 1: return u_light1.SampleLevel(u_light_sampler, uv, 0.0);
    case 2: return u_light2.SampleLevel(u_light_sampler, uv, 0.0);
    default: return u_light3.SampleLevel(u_light_sampler, uv, 0.0);
    }
}

float3 light_colour(uint index, LtcLobe lobe) {
    switch (index) {
    case 0: return ltc_light_colour(u_light0, u_light_sampler, lobe, scene.level_sigma);
    case 1: return ltc_light_colour(u_light1, u_light_sampler, lobe, scene.level_sigma);
    case 2: return ltc_light_colour(u_light2, u_light_sampler, lobe, scene.level_sigma);
    default: return ltc_light_colour(u_light3, u_light_sampler, lobe, scene.level_sigma);
    }
}

// The light that leaves a point of a surface towards the eye: every light,
// less what the boxes keep from the point.
float3 lit(float3 position, float3 normal, float3 view, float3 albedo, float roughness) {
    LtcTables tables = LtcTables(u_mat, u_amp, u_table_sampler);
    LtcSurface s = ltc_surface(tables, position, normal, view, roughness);
    float3 diffuse = float3(0.0, 0.0, 0.0);
    float3 specular = float3(0.0, 0.0, 0.0);
    uint lights = uint(scene.light_count);
    uint boxes = uint(scene.box_count);
    for (uint i = 0; i < lights; i++) {
        SceneLight light = scene.lights[i];
        LtcQuad q = LtcQuad(light.p0, light.p0 + light.e1, light.p0 + light.e1 + light.e2, light.p0 + light.e2, light.two_sided != 0.0);
        LtcLobes lobes = ltc_quad(tables, s, q);
        for (uint b = 0; b < boxes; b++) {
            float4 cover = ltc_box_cover(s, q, ltc_box(scene.boxes[b]));
            lobes.diffuse.form_factor *= ltc_uncovered(lobes.diffuse, cover);
            lobes.specular.form_factor *= ltc_uncovered(lobes.specular, cover);
        }
        float3 scale = light.tint * light.intensity;
        if (lobes.diffuse.form_factor > 0.0) {
            diffuse += light_colour(i, lobes.diffuse) * lobes.diffuse.form_factor * scale;
        }
        if (lobes.specular.form_factor > 0.0) {
            specular += light_colour(i, lobes.specular) * lobes.specular.form_factor * scale;
        }
    }
    return albedo * diffuse + ltc_specular_colour(s, scene.f0) * specular;
}

// Where the ray enters a box, or a negative number; and the box's normal
// there.
float enter_box(Box b, float3 origin, float3 direction, out float3 normal) {
    float3 o = origin - b.center;
    float3 scales = float3(1.0 / dot(b.x, b.x), 1.0 / dot(b.y, b.y), 1.0 / dot(b.z, b.z));
    float3 lo = float3(dot(o, b.x), dot(o, b.y), dot(o, b.z)) * scales;
    float3 ld = float3(dot(direction, b.x), dot(direction, b.y), dot(direction, b.z)) * scales;
    // A direction along a face still divides: by a small number of its sign.
    ld = select(abs(ld) < 1e-9, float3(1e-9, 1e-9, 1e-9), ld);
    float3 a = (-1.0 - lo) / ld;
    float3 c = (1.0 - lo) / ld;
    float3 first = min(a, c);
    float3 last = max(a, c);
    float enter = max(first.x, max(first.y, first.z));
    float leave = min(last.x, min(last.y, last.z));
    normal = float3(0.0, 0.0, 1.0);
    if (enter >= leave || enter <= 0.0) {
        return -1.0;
    }
    if (enter == first.x) {
        normal = normalize(b.x) * -sign(ld.x);
    } else if (enter == first.y) {
        normal = normalize(b.y) * -sign(ld.y);
    } else {
        normal = normalize(b.z) * -sign(ld.z);
    }
    return enter;
}

[shader("fragment")]
float4 fs_scene(VsOut input) : SV_Target0 {
    float3 direction = normalize(scene.forward
        + scene.right * (input.ndc.x * scene.tan_half_fov * scene.aspect)
        - scene.up * (input.ndc.y * scene.tan_half_fov));
    uint count = uint(scene.light_count);
    float nearest = 1e30;
    float3 colour = float3(0.0, 0.0, 0.0);

    // The lights themselves.
    for (uint i = 0; i < count; i++) {
        SceneLight light = scene.lights[i];
        float3 n = cross(light.e2, light.e1);
        float facing = -dot(direction, n);
        if (facing == 0.0 || (facing < 0.0 && light.two_sided == 0.0)) {
            continue;
        }
        float t = dot(light.p0 - scene.eye, n) / dot(direction, n);
        if (t <= 0.0 || t >= nearest) {
            continue;
        }
        float3 w = scene.eye + direction * t - light.p0;
        float2 uv = float2(dot(w, light.e1) / dot(light.e1, light.e1), dot(w, light.e2) / dot(light.e2, light.e2));
        if (all(uv >= float2(0.0, 0.0)) && all(uv <= float2(1.0, 1.0))) {
            nearest = t;
            colour = light_texel(i, uv).rgb * light.tint * light.intensity;
        }
    }

    // The surfaces: the floor and the boxes.
    bool surface = false;
    float3 normal = float3(0.0, 0.0, 1.0);
    float3 albedo = scene.albedo;
    float roughness = 0.5;
    if (direction.z < 0.0) {
        float t = -scene.eye.z / direction.z;
        if (t < nearest) {
            nearest = t;
            surface = true;
            float band = clamp(floor((scene.eye.x + direction.x * t) * 2.0 + 2.5), 0.0, 4.0);
            roughness = lerp(scene.roughness_low, scene.roughness_high, band / 4.0);
        }
    }
    uint boxes = uint(scene.box_count);
    for (uint b = 0; b < boxes; b++) {
        float3 box_normal;
        float t = enter_box(scene.boxes[b], scene.eye, direction, box_normal);
        if (t > 0.0 && t < nearest) {
            nearest = t;
            surface = true;
            normal = box_normal;
            albedo = scene.box_albedo;
            roughness = 0.6;
        }
    }
    if (surface) {
        colour = lit(scene.eye + direction * nearest, normal, -direction, albedo, roughness);
    }
    return float4(colour, 1.0);
}
