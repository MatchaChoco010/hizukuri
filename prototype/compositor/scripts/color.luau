-- Transfer functions for the shaders of the built-in drawing.
--
-- The identifiers match what `hz.decode_transform` and `hz.encode_transform`
-- return as `transfer_id`, and `param` is their `transfer_param`
-- (`TransferFunction::param` in `proto-color`).

return [[
const TF_LINEAR: u32 = 0u;
const TF_SRGB: u32 = 1u;
const TF_POWER: u32 = 2u;
const TF_PQ: u32 = 3u;
const TF_BT1886: u32 = 4u;
const TF_HLG: u32 = 5u;

const PQ_M1: f32 = 2610.0 / 16384.0;
const PQ_M2: f32 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f32 = 3424.0 / 4096.0;
const PQ_C2: f32 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f32 = 2392.0 / 4096.0 * 32.0;

const HLG_A: f32 = 0.17883277;
const HLG_B: f32 = 0.28466892;
const HLG_C: f32 = 0.55991073;

// Encoded value -> normalized linear value (EOTF). For HLG: scene light (the
// inverse OETF); the OOTF is the caller's, since it mixes the channels.
fn tf_decode(e: vec3<f32>, tf: u32, param: f32) -> vec3<f32> {
    switch tf {
        case TF_BT1886 {
            // param: (black / white)^(1/2.4). 0 is black, 1 is white.
            let black = pow(param, 2.4);
            let v = max((1.0 - param) * e + vec3<f32>(param), vec3<f32>(0.0));
            return (pow(v, vec3<f32>(2.4)) - vec3<f32>(black)) / (1.0 - black);
        }
        case TF_HLG {
            let v = clamp(e, vec3<f32>(0.0), vec3<f32>(1.0));
            let lo = v * v / 3.0;
            let hi = (exp((v - vec3<f32>(HLG_C)) / HLG_A) + vec3<f32>(HLG_B)) / 12.0;
            return select(hi, lo, v <= vec3<f32>(0.5));
        }
        case TF_SRGB {
            let a = abs(e);
            let lo = a / 12.92;
            let hi = pow((a + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
            return sign(e) * select(hi, lo, a <= vec3<f32>(0.04045));
        }
        case TF_POWER {
            return sign(e) * pow(abs(e), vec3<f32>(param));
        }
        case TF_PQ {
            let p = pow(clamp(e, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / PQ_M2));
            let num = max(p - vec3<f32>(PQ_C1), vec3<f32>(0.0));
            let den = vec3<f32>(PQ_C2) - PQ_C3 * p;
            return pow(num / den, vec3<f32>(1.0 / PQ_M1));
        }
        default {
            return e;
        }
    }
}

// Normalized linear value -> encoded value (inverse EOTF). HLG is not
// encoded here (no output is driven with it).
fn tf_encode(l: vec3<f32>, tf: u32, param: f32) -> vec3<f32> {
    switch tf {
        case TF_BT1886 {
            let black = pow(param, 2.4);
            let v = max(l * (1.0 - black) + vec3<f32>(black), vec3<f32>(0.0));
            return (pow(v, vec3<f32>(1.0 / 2.4)) - vec3<f32>(param)) / (1.0 - param);
        }
        case TF_SRGB {
            let a = abs(l);
            let lo = a * 12.92;
            let hi = 1.055 * pow(a, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
            return sign(l) * select(hi, lo, a <= vec3<f32>(0.0031308));
        }
        case TF_POWER {
            return sign(l) * pow(abs(l), vec3<f32>(1.0 / param));
        }
        case TF_PQ {
            let y = pow(clamp(l, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(PQ_M1));
            let num = vec3<f32>(PQ_C1) + PQ_C2 * y;
            let den = vec3<f32>(1.0) + PQ_C3 * y;
            return pow(num / den, vec3<f32>(PQ_M2));
        }
        default {
            return l;
        }
    }
}
]]
