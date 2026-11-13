// The shaders of the example `cube.luau`: a lit, textured mesh.
//
// Matrices multiply column vectors: `mul(matrix, vector)`. In a buffer, a
// matrix is laid out column by column (Slang's default), which is how the
// script writes them.

struct Scene {
    float4x4 view_proj;
    // Direction the light travels in, in world space.
    float4 light_dir;
};

struct Push {
    float4x4 model;
    float4 tint;
};

[[vk::push_constant]] ConstantBuffer<Push> pc;
[[vk::binding(0, 0)]] ConstantBuffer<Scene> scene;
[[vk::binding(1, 0)]] Texture2D<float4> albedo;
[[vk::binding(2, 0)]] SamplerState smp;

struct VsIn {
    [[vk::location(0)]] float3 position : POSITION;
    [[vk::location(1)]] float3 normal : NORMAL;
    [[vk::location(2)]] float2 uv : TEXCOORD0;
};

struct VsOut {
    float4 position : SV_Position;
    [[vk::location(0)]] float3 normal : NORMAL;
    [[vk::location(1)]] float2 uv : TEXCOORD0;
};

[shader("vertex")]
VsOut vs(VsIn i) {
    VsOut o;
    float4 world = mul(pc.model, float4(i.position, 1.0));
    o.position = mul(scene.view_proj, world);
    // The model matrices of the example only rotate and move.
    o.normal = mul(pc.model, float4(i.normal, 0.0)).xyz;
    o.uv = i.uv;
    return o;
}

[shader("fragment")]
float4 fs(VsOut i) : SV_Target0 {
    float3 n = normalize(i.normal);
    float diffuse = max(dot(n, -scene.light_dir.xyz), 0.0) * 0.8 + 0.2;
    // The level of detail comes from the derivatives of `uv`; the sampler
    // of the example filters between mip levels and anisotropically.
    float4 texel = albedo.Sample(smp, i.uv);
    return float4(texel.rgb * pc.tint.rgb * diffuse, 1.0);
}
