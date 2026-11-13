// Transfer functions, as a module the core provides: `import color;`.
//
// The identifiers match what `hz.decode_transform` and `hz.encode_transform`
// return as `transfer_id`, and `param` is their `transfer_param`
// (`TransferFunction::param` in `proto-color`).
module color;

public static const uint TF_LINEAR = 0;
public static const uint TF_SRGB = 1;
public static const uint TF_POWER = 2;
public static const uint TF_PQ = 3;
public static const uint TF_BT1886 = 4;
public static const uint TF_HLG = 5;

static const float PQ_M1 = 2610.0 / 16384.0;
static const float PQ_M2 = 2523.0 / 4096.0 * 128.0;
static const float PQ_C1 = 3424.0 / 4096.0;
static const float PQ_C2 = 2413.0 / 4096.0 * 32.0;
static const float PQ_C3 = 2392.0 / 4096.0 * 32.0;

static const float HLG_A = 0.17883277;
static const float HLG_B = 0.28466892;
static const float HLG_C = 0.55991073;

// Encoded value -> normalized linear value (EOTF). For HLG: scene light (the
// inverse OETF); the OOTF is the caller's, since it mixes the channels.
public float3 tf_decode(float3 e, uint tf, float param) {
    switch (tf) {
    case TF_BT1886: {
        // param: (black / white)^(1/2.4). 0 is black, 1 is white.
        float black = pow(param, 2.4);
        float3 v = max((1.0 - param) * e + param, 0.0);
        return (pow(v, float3(2.4)) - black) / (1.0 - black);
    }
    case TF_HLG: {
        float3 v = clamp(e, 0.0, 1.0);
        float3 lo = v * v / 3.0;
        float3 hi = (exp((v - HLG_C) / HLG_A) + HLG_B) / 12.0;
        return select(v <= 0.5, lo, hi);
    }
    case TF_SRGB: {
        float3 a = abs(e);
        float3 lo = a / 12.92;
        float3 hi = pow((a + 0.055) / 1.055, float3(2.4));
        return sign(e) * select(a <= 0.04045, lo, hi);
    }
    case TF_POWER:
        return sign(e) * pow(abs(e), float3(param));
    case TF_PQ: {
        float3 p = pow(clamp(e, 0.0, 1.0), float3(1.0 / PQ_M2));
        float3 num = max(p - PQ_C1, 0.0);
        float3 den = PQ_C2 - PQ_C3 * p;
        return pow(num / den, float3(1.0 / PQ_M1));
    }
    default:
        return e;
    }
}

// Normalized linear value -> encoded value (inverse EOTF). HLG is not
// encoded here (no output is driven with it).
public float3 tf_encode(float3 l, uint tf, float param) {
    switch (tf) {
    case TF_BT1886: {
        float black = pow(param, 2.4);
        float3 v = max(l * (1.0 - black) + black, 0.0);
        return (pow(v, float3(1.0 / 2.4)) - param) / (1.0 - param);
    }
    case TF_SRGB: {
        float3 a = abs(l);
        float3 lo = a * 12.92;
        float3 hi = 1.055 * pow(a, float3(1.0 / 2.4)) - 0.055;
        return sign(l) * select(a <= 0.0031308, lo, hi);
    }
    case TF_POWER:
        return sign(l) * pow(abs(l), float3(1.0 / param));
    case TF_PQ: {
        float3 y = pow(clamp(l, 0.0, 1.0), float3(PQ_M1));
        float3 num = PQ_C1 + PQ_C2 * y;
        float3 den = 1.0 + PQ_C3 * y;
        return pow(num / den, float3(PQ_M2));
    }
    default:
        return l;
    }
}

// ----- window contents ------------------------------------------------------
//
// A window's texture holds the values its client stored. Reading it through
// `WindowTexture` gives linear light in the working space, with alpha
// premultiplied, whatever the client's image description and alpha mode are.
// The numbers (`WindowParams`) are filled in by the script side
// (`write_window_params` of the built-in `color` module).

// How alpha is stored in a buffer (`alpha_mode` of `hz.decode_transform`).
public static const uint ALPHA_PREMULTIPLIED_ELECTRICAL = 0;
public static const uint ALPHA_PREMULTIPLIED_OPTICAL = 1;
public static const uint ALPHA_STRAIGHT = 2;

// What it takes to read one window surface: 96 bytes, laid out the same in
// push data, uniform buffers and storage buffers (so an array of them works
// in any of the three).
public struct WindowParams {
    // Surface coordinates (the unit square over the surface) to texture
    // coordinates: u = dot(uv_u.xy, st) + uv_u.z, v likewise. The shown part
    // of the buffer and how the client turned it are folded in.
    public float4 uv_u;
    public float4 uv_v;
    // Rows of the matrix from decoded linear values to the working space
    // (xyz), including the brightness. w: the luminance coefficients of the
    // HLG OOTF (zero otherwise).
    public float4 m0;
    public float4 m1;
    public float4 m2;
    public float tf_param;
    public float reserved;
    public uint tf;
    public uint alpha_mode;
};

// Surface coordinates (0..1 over the surface) to texture coordinates.
public float2 window_uv(WindowParams p, float2 st) {
    return float2(dot(p.uv_u.xy, st) + p.uv_u.z, dot(p.uv_v.xy, st) + p.uv_v.z);
}

float3 window_matrix(WindowParams p, float3 linear) {
    return float3(dot(p.m0.xyz, linear), dot(p.m1.xyz, linear), dot(p.m2.xyz, linear));
}

// Encoded values -> display light. HLG decodes to scene light, which its
// OOTF turns into display light by the luminance of the scene light (the
// system gamma is `tf_param`).
float3 window_decode(WindowParams p, float3 e) {
    float3 linear = tf_decode(e, p.tf, p.tf_param);
    if (p.tf == TF_HLG) {
        float y = dot(float3(p.m0.w, p.m1.w, p.m2.w), linear);
        if (y <= 0.0) {
            return float3(0.0);
        }
        return linear * pow(y, p.tf_param - 1.0);
    }
    return linear;
}

// One texel as stored in a window's buffer -> premultiplied linear light in
// the working space. For a texel fetched some other way than through
// `WindowTexture` (an array of textures, a gather).
public float4 window_to_working(float4 texel, WindowParams p) {
    float alpha = texel.a;
    float3 premultiplied;
    if (p.alpha_mode == ALPHA_PREMULTIPLIED_ELECTRICAL) {
        if (alpha <= 0.0) {
            premultiplied = float3(0.0);
        } else {
            premultiplied = window_matrix(p, window_decode(p, texel.rgb / alpha)) * alpha;
        }
    } else if (p.alpha_mode == ALPHA_PREMULTIPLIED_OPTICAL) {
        premultiplied = window_matrix(p, window_decode(p, texel.rgb));
    } else {
        premultiplied = window_matrix(p, window_decode(p, texel.rgb)) * alpha;
    }
    return float4(premultiplied, alpha);
}

// A premultiplied color with its light scaled: brighter or darker, as
// opaque as before.
public float4 scale_light(float4 premultiplied, float factor) {
    return float4(premultiplied.rgb * factor, premultiplied.a);
}

// The contents of a window surface. Every read returns premultiplied linear
// light in the working space. `st` is in surface coordinates: (0, 0) is the
// top left of the surface and (1, 1) its bottom right, however the buffer is
// cropped or turned.
//
// Filtering happens on the stored values, before the conversion.
public struct WindowTexture {
    Texture2D<float4> texture;
    SamplerState sampler;
    public WindowParams params;

    public __init(Texture2D<float4> texture, SamplerState sampler, WindowParams params) {
        this.texture = texture;
        this.sampler = sampler;
        this.params = params;
    }

    // Fragment shaders only (implicit derivatives).
    public float4 sample(float2 st) {
        return window_to_working(texture.Sample(sampler, window_uv(params, st)), params);
    }

    public float4 sample_level(float2 st, float lod) {
        return window_to_working(texture.SampleLevel(sampler, window_uv(params, st), lod), params);
    }

    // One texel of the buffer, by its integer position in the buffer (not
    // in the surface: the crop and the turn are not applied).
    public float4 load(int2 texel) {
        return window_to_working(texture.Load(int3(texel, 0)), params);
    }

    // The size of the buffer in texels.
    public float2 buffer_size() {
        uint w, h;
        texture.GetDimensions(w, h);
        return float2(float(w), float(h));
    }
};
