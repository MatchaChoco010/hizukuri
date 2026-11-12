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
