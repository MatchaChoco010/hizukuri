// Pass 1 of render.luau: every surface as a textured quad, converted from
// what its values mean to linear light in the working primaries, with alpha
// premultiplied.
import color;

static const uint ALPHA_PREMULTIPLIED_ELECTRICAL = 0;
static const uint ALPHA_PREMULTIPLIED_OPTICAL = 1;

struct Push {
    float4 ax;          // a, b, tx, quad width   (pixel = a*x + b*y + tx)
    float4 ay;          // c, d, ty, quad height  (pixel = c*x + d*y + ty)
    float4 uv_u;        // a, b, tx: u = a*s + b*t + tx over the unit square
    float4 uv_v;        // c, d, ty: v = c*s + d*t + ty
    float4 m0;          // rows of the color matrix (xyz); w: the luminance
    float4 m1;          // coefficients of the HLG OOTF (zero otherwise)
    float4 m2;
    float2 target_size;
    float tf_param;
    float opacity;
    uint tf;
    uint alpha_mode;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_texture;
[[vk::binding(1, 0)]] SamplerState u_sampler;

struct VertexOutput {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 uv : TEXCOORD0;
};

// 4-vertex triangle strip.
[shader("vertex")]
VertexOutput vs_main(uint index : SV_VulkanVertexID) {
    float2 corner = float2(float(index & 1), float((index >> 1) & 1));
    float2 local = corner * float2(pc.ax.w, pc.ay.w);
    float2 pixel = float2(dot(pc.ax.xy, local) + pc.ax.z, dot(pc.ay.xy, local) + pc.ay.z);
    VertexOutput o;
    o.uv = float2(dot(pc.uv_u.xy, corner) + pc.uv_u.z, dot(pc.uv_v.xy, corner) + pc.uv_v.z);
    o.position = float4(pixel / pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

float3 to_working(float3 linear) {
    return float3(dot(pc.m0.xyz, linear), dot(pc.m1.xyz, linear), dot(pc.m2.xyz, linear));
}

// Encoded values -> display light. HLG decodes to scene light, which its
// OOTF turns into display light by the luminance of the scene light (the
// system gamma is `tf_param`).
float3 decode(float3 e) {
    float3 linear = tf_decode(e, pc.tf, pc.tf_param);
    if (pc.tf == TF_HLG) {
        float y = dot(float3(pc.m0.w, pc.m1.w, pc.m2.w), linear);
        if (y <= 0.0) {
            return float3(0.0);
        }
        return linear * pow(y, pc.tf_param - 1.0);
    }
    return linear;
}

[shader("fragment")]
float4 fs_main(VertexOutput i) : SV_Target0 {
    float4 texel = u_texture.SampleLevel(u_sampler, i.uv, 0.0);
    float alpha = texel.a;
    float3 premultiplied;
    if (pc.alpha_mode == ALPHA_PREMULTIPLIED_ELECTRICAL) {
        if (alpha <= 0.0) {
            premultiplied = float3(0.0);
        } else {
            premultiplied = to_working(decode(texel.rgb / alpha)) * alpha;
        }
    } else if (pc.alpha_mode == ALPHA_PREMULTIPLIED_OPTICAL) {
        premultiplied = to_working(decode(texel.rgb));
    } else {
        premultiplied = to_working(decode(texel.rgb)) * alpha;
    }
    return float4(premultiplied, alpha) * pc.opacity;
}
