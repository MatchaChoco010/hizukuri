// The shaders of the example `shadow.luau`: instanced meshes lit by one
// directional light with a shadow map, drawn into a multisampled HDR
// target, and the pass that writes the result to the output.
//
// Matrices multiply column vectors and are laid out column by column, as in
// `cube.slang`. The three passes share this file, so their bindings do not
// overlap.

struct Scene {
    float4x4 view_proj;
    float4x4 light_view_proj;
    // Direction the light travels in, in world space.
    float4 light_dir;
    // Reciprocal of the size of the shadow map, in x.
    float4 shadow_texel;
};

struct Instance {
    float4x4 model;
    float4 tint;
};

[[vk::binding(0, 0)]] ConstantBuffer<Scene> scene;
[[vk::binding(1, 0)]] StructuredBuffer<Instance> instances;
[[vk::binding(2, 0)]] Texture2D<float4> albedo;
[[vk::binding(3, 0)]] SamplerState albedo_sampler;
[[vk::binding(4, 0)]] Texture2D<float> shadow_map;
[[vk::binding(5, 0)]] SamplerComparisonState shadow_sampler;
[[vk::binding(6, 0)]] Texture2D<float4> hdr;

struct VsIn {
    [[vk::location(0)]] float3 position : POSITION;
    [[vk::location(1)]] float3 normal : NORMAL;
    [[vk::location(2)]] float2 uv : TEXCOORD0;
};

struct VsOut {
    float4 position : SV_Position;
    [[vk::location(0)]] float3 normal : NORMAL;
    [[vk::location(1)]] float2 uv : TEXCOORD0;
    [[vk::location(2)]] float3 world : WORLD;
    [[vk::location(3)]] float3 tint : TINT;
};

// ----- the shadow pass: depth only, seen from the light

[shader("vertex")]
float4 vs_shadow(VsIn i, uint instance : SV_VulkanInstanceID) : SV_Position {
    float4 world = mul(instances[instance].model, float4(i.position, 1.0));
    return mul(scene.light_view_proj, world);
}

// A pass without a colour attachment takes a fragment shader without an
// output.
[shader("fragment")]
void fs_shadow() {}

// ----- the scene pass

[shader("vertex")]
VsOut vs(VsIn i, uint instance : SV_VulkanInstanceID) {
    Instance object = instances[instance];
    float4 world = mul(object.model, float4(i.position, 1.0));
    VsOut o;
    o.position = mul(scene.view_proj, world);
    // The model matrices of the example rotate, move and scale evenly.
    o.normal = mul(object.model, float4(i.normal, 0.0)).xyz;
    o.uv = i.uv;
    o.world = world.xyz;
    o.tint = object.tint.rgb;
    return o;
}

// How much of the light reaches `world`: 3x3 comparisons, each of them
// filtered over 2x2 texels by the sampler.
float shadow(float3 world) {
    float4 clip = mul(scene.light_view_proj, float4(world, 1.0));
    float3 ndc = clip.xyz / clip.w;
    // The light's projection flips y like the camera's, so its clip space
    // maps onto the shadow map without another flip.
    float2 uv = ndc.xy * 0.5 + 0.5;
    float lit = 0.0;
    for (int y = -1; y <= 1; y++) {
        for (int x = -1; x <= 1; x++) {
            float2 offset = float2(x, y) * scene.shadow_texel.x;
            lit += shadow_map.SampleCmpLevelZero(shadow_sampler, uv + offset, ndc.z);
        }
    }
    return lit / 9.0;
}

[shader("fragment")]
float4 fs(VsOut i) : SV_Target0 {
    float3 n = normalize(i.normal);
    float facing = max(dot(n, -scene.light_dir.xyz), 0.0);
    float3 base = albedo.Sample(albedo_sampler, i.uv).rgb * i.tint;
    // Linear light: the lit parts go above 1.
    float3 color = base * (0.12 + 2.6 * facing * shadow(i.world));
    return float4(color, 1.0);
}

// ----- the output pass: the resolved image, tone-mapped and encoded

[shader("vertex")]
float4 vs_present(uint i : SV_VulkanVertexID) : SV_Position {
    float2 p[3] = { float2(-1, -1), float2(3, -1), float2(-1, 3) };
    return float4(p[i], 0, 1);
}

[shader("fragment")]
float4 fs_present(float4 p : SV_Position) : SV_Target0 {
    float3 c = hdr.Load(int3(int2(p.xy), 0)).rgb;
    c = c / (1.0 + c);
    float g = 1.0 / 2.2;
    return float4(pow(c, float3(g, g, g)), 1.0);
}
