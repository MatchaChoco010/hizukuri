//! Transfer functions (EOTFs) and their inverses.
//!
//! [`TransferFunction::decode`] maps an encoded (electrical) value to a
//! normalized linear (optical) value, where 0.0 is the minimum and 1.0 the
//! maximum luminance of the primary color volume. [`TransferFunction::encode`]
//! is the inverse.
//!
//! Two functions depend on the luminances of the image description: BT.1886
//! (through its black level) and HLG (whose system gamma follows the peak
//! luminance). Their dependence is folded into one number,
//! [`TransferFunction::param`], which [`TransferFunction::decode_with`] and
//! [`TransferFunction::encode_with`] take. [`TransferFunction::decode`] and
//! [`TransferFunction::encode`] use the default luminances of
//! color-management-v1.
//!
//! HLG is a scene-referred encoding: decoding it per channel gives scene
//! light. The OOTF that turns scene light into display light mixes the
//! channels, so it is not here but in the decode transform
//! ([`crate::DecodeTransform`]).
//!
//! The same formulas are implemented in the shaders of the script layer
//! (the `color` Slang module, `prototype/compositor/shaders/color.slang`).
//! [`TransferFunction::shader_id`] and [`TransferFunction::param`] are the
//! bridge between the two, handed to scripts by `hz.decode_transform` and
//! `hz.encode_transform`; keep them in sync with that module.

use crate::description::Luminance;

/// A parametric transfer function.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransferFunction {
    /// Identity. Values are already linear and may lie outside `[0, 1]`
    /// (extended range, e.g. scRGB).
    Linear,
    /// The piecewise sRGB function of IEC 61966-2-1 (a linear segment near
    /// black, then a 2.4 power with an offset).
    SrgbPiecewise,
    /// Pure power function with exponent 2.2.
    Gamma22,
    /// Pure power function with exponent 2.4. This is the BT.1886 EOTF for a
    /// display with zero black level.
    Gamma24,
    /// Pure power function with exponent 2.8.
    Gamma28,
    /// Pure power function with an arbitrary exponent.
    Power(f32),
    /// SMPTE ST 2084 perceptual quantizer. 1.0 decodes to 10000 cd/m² above
    /// the black level.
    Pq,
    /// Rec. ITU-R BT.1886: a 2.4 power function that accounts for the
    /// black level of the display (the minimum luminance).
    Bt1886,
    /// Rec. ITU-R BT.2100 hybrid log-gamma. Decoding gives scene light; the
    /// OOTF is applied by the decode transform.
    Hlg,
}

// SMPTE ST 2084 constants.
const PQ_M1: f64 = 2610.0 / 16384.0;
const PQ_M2: f64 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f64 = 3424.0 / 4096.0;
const PQ_C2: f64 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f64 = 2392.0 / 4096.0 * 32.0;

// Rec. ITU-R BT.2100 HLG constants.
const HLG_A: f64 = 0.178_832_77;
const HLG_B: f64 = 1.0 - 4.0 * HLG_A;
// 0.5 - a * ln(4a)
const HLG_C: f64 = 0.559_910_729_529_562_3;

/// Exponent of BT.1886.
const BT1886_GAMMA: f64 = 2.4;

impl TransferFunction {
    /// Names used on the command line and in state dumps.
    pub const NAMED: [(&'static str, TransferFunction); 8] = [
        ("linear", TransferFunction::Linear),
        ("srgb-piecewise", TransferFunction::SrgbPiecewise),
        ("gamma22", TransferFunction::Gamma22),
        ("gamma24", TransferFunction::Gamma24),
        ("gamma28", TransferFunction::Gamma28),
        ("pq", TransferFunction::Pq),
        ("bt1886", TransferFunction::Bt1886),
        ("hlg", TransferFunction::Hlg),
    ];

    pub fn from_name(name: &str) -> Option<TransferFunction> {
        Self::NAMED
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| *t)
    }

    pub fn name(&self) -> Option<&'static str> {
        Self::NAMED
            .iter()
            .find(|(_, t)| t == self)
            .map(|(name, _)| *name)
    }

    /// Whether encoded values outside `[0, 1]` are meaningful.
    pub fn is_extended_range(&self) -> bool {
        matches!(self, TransferFunction::Linear)
    }

    /// The number that carries what the function needs of the luminances:
    ///
    /// - power functions: the exponent,
    /// - BT.1886: `(L_B / L_W)^(1/2.4)`, the black level on the scale of
    ///   encoded values,
    /// - HLG: the system gamma of the OOTF, `1.2 + 0.42 log10(L_W / 1000)`
    ///   (Rec. ITU-R BT.2100, table 5, note 5f),
    /// - others: 0, unused.
    pub fn param(&self, luminance: &Luminance) -> f32 {
        match *self {
            TransferFunction::Gamma22 => 2.2,
            TransferFunction::Gamma24 => 2.4,
            TransferFunction::Gamma28 => 2.8,
            TransferFunction::Power(g) => g,
            TransferFunction::Bt1886 => {
                let ratio = (luminance.min as f64 / luminance.max as f64).clamp(0.0, 1.0);
                ratio.powf(1.0 / BT1886_GAMMA) as f32
            }
            TransferFunction::Hlg => hlg_system_gamma(luminance.max as f64) as f32,
            _ => 0.0,
        }
    }

    /// [`Self::param`] for the default luminances of color-management-v1.
    pub fn default_param(&self) -> f32 {
        self.param(&Luminance::default_for(*self))
    }

    /// EOTF with the default luminances: encoded value → normalized linear
    /// value. See [`Self::decode_with`].
    pub fn decode(&self, encoded: f32) -> f32 {
        self.decode_with(encoded, self.default_param())
    }

    /// Inverse EOTF with the default luminances. See [`Self::encode_with`].
    pub fn encode(&self, linear: f32) -> f32 {
        self.encode_with(linear, self.default_param())
    }

    /// EOTF: encoded value → normalized linear value (0.0 is the black
    /// level, 1.0 the peak). `param` is [`Self::param`].
    ///
    /// Power-like functions are mirrored around zero for negative input, so
    /// out-of-range values survive a round trip. PQ and HLG clamp to
    /// `[0, 1]`. BT.1886 follows its definition below black: the light
    /// stops at zero, which is below the black level.
    pub fn decode_with(&self, encoded: f32, param: f32) -> f32 {
        let e = encoded as f64;
        let out = match *self {
            TransferFunction::Linear => e,
            TransferFunction::SrgbPiecewise => mirrored(e, |v| {
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            }),
            TransferFunction::Gamma22
            | TransferFunction::Gamma24
            | TransferFunction::Gamma28
            | TransferFunction::Power(_) => mirrored(e, |v| v.powf(param as f64)),
            TransferFunction::Pq => {
                let p = e.clamp(0.0, 1.0).powf(1.0 / PQ_M2);
                ((p - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * p)).powf(1.0 / PQ_M1)
            }
            TransferFunction::Bt1886 => {
                // L = a * max(E + b, 0)^2.4 with a and b from L_W and L_B
                // (appendix of color-management-v1), normalized so that
                // L_B is 0 and L_W is 1. With r = (L_B / L_W)^(1/2.4):
                // L / L_W = ((1 - r) E + r)^2.4.
                let r = param as f64;
                let black = r.powf(BT1886_GAMMA);
                (((1.0 - r) * e + r).max(0.0).powf(BT1886_GAMMA) - black) / (1.0 - black)
            }
            TransferFunction::Hlg => {
                // Inverse OETF: scene light in [0, 1].
                let v = e.clamp(0.0, 1.0);
                if v <= 0.5 {
                    v * v / 3.0
                } else {
                    (((v - HLG_C) / HLG_A).exp() + HLG_B) / 12.0
                }
            }
        };
        out as f32
    }

    /// Inverse EOTF: normalized linear value → encoded value. For HLG, the
    /// OETF (scene light → encoded value).
    pub fn encode_with(&self, linear: f32, param: f32) -> f32 {
        let l = linear as f64;
        let out = match *self {
            TransferFunction::Linear => l,
            TransferFunction::SrgbPiecewise => mirrored(l, |v| {
                if v <= 0.0031308 {
                    v * 12.92
                } else {
                    1.055 * v.powf(1.0 / 2.4) - 0.055
                }
            }),
            TransferFunction::Gamma22
            | TransferFunction::Gamma24
            | TransferFunction::Gamma28
            | TransferFunction::Power(_) => mirrored(l, |v| v.powf(1.0 / param as f64)),
            TransferFunction::Pq => {
                let y = l.clamp(0.0, 1.0).powf(PQ_M1);
                ((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y)).powf(PQ_M2)
            }
            TransferFunction::Bt1886 => {
                let r = param as f64;
                let black = r.powf(BT1886_GAMMA);
                let v = (l * (1.0 - black) + black).max(0.0);
                (v.powf(1.0 / BT1886_GAMMA) - r) / (1.0 - r)
            }
            TransferFunction::Hlg => {
                let v = l.clamp(0.0, 1.0);
                if v <= 1.0 / 12.0 {
                    (3.0 * v).sqrt()
                } else {
                    HLG_A * (12.0 * v - HLG_B).ln() + HLG_C
                }
            }
        };
        out as f32
    }

    /// Identifier of this function in the shaders
    /// (the `color` Slang module, `prototype/compositor/shaders/color.slang`).
    pub fn shader_id(&self) -> u32 {
        match self {
            TransferFunction::Linear => 0,
            TransferFunction::SrgbPiecewise => 1,
            TransferFunction::Gamma22
            | TransferFunction::Gamma24
            | TransferFunction::Gamma28
            | TransferFunction::Power(_) => 2,
            TransferFunction::Pq => 3,
            TransferFunction::Bt1886 => 4,
            TransferFunction::Hlg => 5,
        }
    }

    /// Parameter passed to the shaders together with [`Self::shader_id`],
    /// for the default luminances. A decode or encode transform carries the
    /// parameter for the luminances of its image description.
    pub fn shader_param(&self) -> f32 {
        self.default_param()
    }
}

/// The system gamma of the HLG OOTF for a display with peak luminance
/// `peak` (cd/m²): `1.2 + 0.42 log10(peak / 1000)`.
pub(crate) fn hlg_system_gamma(peak: f64) -> f64 {
    1.2 + 0.42 * (peak.max(1.0) / 1000.0).log10()
}

fn mirrored(v: f64, f: impl Fn(f64) -> f64) -> f64 {
    if v < 0.0 { -f(-v) } else { f(v) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f32, expected: f32, tolerance: f32) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} vs {expected} (tolerance {tolerance})"
        );
    }

    #[test]
    fn srgb_known_values() {
        let tf = TransferFunction::SrgbPiecewise;
        assert_close(tf.decode(0.0), 0.0, 0.0);
        assert_close(tf.decode(1.0), 1.0, 1e-7);
        // Boundary of the linear segment.
        assert_close(tf.decode(0.04045), 0.04045 / 12.92, 1e-8);
        // 8 bit 128 and 188, values tabulated in many sRGB references.
        assert_close(tf.decode(128.0 / 255.0), 0.215_860_5, 1e-6);
        assert_close(tf.decode(188.0 / 255.0), 0.502_886_5, 1e-6);
        assert_close(tf.decode(0.5), 0.214_041_14, 1e-6);
        assert_close(tf.encode(0.5), 0.735_357, 1e-6);
        assert_close(tf.encode(0.18), 0.461_356_1, 1e-6);
    }

    #[test]
    fn gamma_known_values() {
        assert_close(TransferFunction::Gamma22.decode(0.5), 0.217_637_64, 1e-6);
        assert_close(TransferFunction::Gamma24.decode(0.5), 0.189_464_57, 1e-6);
        assert_close(TransferFunction::Gamma28.decode(0.5), 0.143_587_29, 1e-6);
        assert_close(TransferFunction::Power(2.0).decode(0.5), 0.25, 1e-7);
        assert_close(TransferFunction::Gamma22.encode(0.5), 0.729_74, 1e-6);
    }

    #[test]
    fn linear_is_identity_and_extended() {
        let tf = TransferFunction::Linear;
        for v in [-0.5, 0.0, 0.25, 1.0, 7.5] {
            assert_eq!(tf.decode(v), v);
            assert_eq!(tf.encode(v), v);
        }
        assert!(tf.is_extended_range());
        assert!(!TransferFunction::Pq.is_extended_range());
    }

    // SMPTE ST 2084 / Rec. ITU-R BT.2100 PQ: well-known code values.
    // 100 cd/m² ↔ 0.5081, 203 cd/m² ↔ 0.5807 (BT.2408 reference white),
    // 1000 cd/m² ↔ 0.7518, 10000 cd/m² ↔ 1.0.
    #[test]
    fn pq_known_values() {
        let tf = TransferFunction::Pq;
        assert_close(tf.encode(0.0), 0.000_000_73, 1e-6);
        assert_close(tf.encode(100.0 / 10000.0), 0.508_078_4, 1e-6);
        assert_close(tf.encode(203.0 / 10000.0), 0.580_688_9, 1e-5);
        assert_close(tf.encode(1000.0 / 10000.0), 0.751_827_1, 1e-6);
        assert_close(tf.encode(1.0), 1.0, 1e-7);
        assert_close(tf.decode(0.508_078_4), 0.01, 1e-7);
        assert_close(tf.decode(0.751_827_1), 0.1, 1e-6);
        assert_close(tf.decode(1.0), 1.0, 1e-6);
        assert_close(tf.decode(0.0), 0.0, 1e-9);
    }

    // BT.1886 against its definition in the appendix of color-management-v1
    // (L = a max(E + b, 0)^2.4, a and b from L_W and L_B), for the default
    // luminances of the protocol (BT.2035: 0.01 and 100 cd/m²) and others.
    #[test]
    fn bt1886_matches_its_definition() {
        let tf = TransferFunction::Bt1886;
        for (black, white) in [
            (0.01f64, 100.0f64),
            (0.1, 100.0),
            (0.5, 300.0),
            (0.0, 100.0),
        ] {
            let luminance = Luminance {
                min: black as f32,
                max: white as f32,
                reference: white as f32,
            };
            let param = tf.param(&luminance);
            let g = 2.4;
            let a = (white.powf(1.0 / g) - black.powf(1.0 / g)).powf(g);
            let b = black.powf(1.0 / g) / (white.powf(1.0 / g) - black.powf(1.0 / g));
            for i in 0..=20 {
                let e = i as f64 / 20.0;
                let expected = a * (e + b).max(0.0).powf(g);
                let normalized = tf.decode_with(e as f32, param) as f64;
                let actual = (white - black) * normalized + black;
                assert!(
                    (actual - expected).abs() <= 1e-4 * white,
                    "E={e} Lb={black} Lw={white}: {actual} vs {expected}"
                );
            }
            // Ends: black at E = 0, the peak at E = 1.
            assert_close(tf.decode_with(0.0, param), 0.0, 1e-6);
            assert_close(tf.decode_with(1.0, param), 1.0, 1e-6);
        }
        // With no black level it is the 2.4 power function.
        let zero = Luminance {
            min: 0.0,
            max: 100.0,
            reference: 100.0,
        };
        assert_eq!(tf.param(&zero), 0.0);
        assert_close(tf.decode_with(0.5, 0.0), 0.189_464_57, 1e-6);
        // The default luminances of the protocol: 0.01 / 100.
        assert_close(tf.default_param(), 0.021_544_35, 1e-7);
    }

    // Rec. ITU-R BT.2100 HLG: the OETF reaches 0.5 at 1/12 and 1.0 at 1.0;
    // Report ITU-R BT.2408 puts the reference white at 75 % of the signal.
    #[test]
    fn hlg_known_values() {
        let tf = TransferFunction::Hlg;
        assert_close(tf.encode(1.0 / 12.0), 0.5, 1e-6);
        assert_close(tf.encode(1.0), 1.0, 1e-6);
        assert_close(tf.encode(0.0), 0.0, 0.0);
        assert_close(tf.decode(0.5), 1.0 / 12.0, 1e-7);
        assert_close(tf.decode(1.0), 1.0, 1e-6);
        // 75 %: scene light 0.26497 (BT.2408, table of HLG signal levels).
        assert_close(tf.decode(0.75), 0.264_97, 2e-5);
        // System gamma: 1.2 at 1000 cd/m², 1.0 at about 334 cd/m².
        assert_close(hlg_system_gamma(1000.0) as f32, 1.2, 1e-7);
        assert_close(hlg_system_gamma(2000.0) as f32, 1.326_43, 1e-5);
        assert_close(tf.default_param(), 1.2, 1e-7);
    }

    #[test]
    fn round_trips() {
        let functions = [
            TransferFunction::Linear,
            TransferFunction::SrgbPiecewise,
            TransferFunction::Gamma22,
            TransferFunction::Gamma24,
            TransferFunction::Gamma28,
            TransferFunction::Power(1.8),
            TransferFunction::Pq,
            TransferFunction::Bt1886,
            TransferFunction::Hlg,
        ];
        for tf in functions {
            for i in 0..=1000 {
                let e = i as f32 / 1000.0;
                assert_close(tf.encode(tf.decode(e)), e, 2e-6);
            }
        }
    }

    #[test]
    fn power_functions_mirror_negative_values() {
        let tf = TransferFunction::SrgbPiecewise;
        assert_eq!(tf.decode(-0.5), -tf.decode(0.5));
        assert_eq!(tf.encode(-0.5), -tf.encode(0.5));
        let tf = TransferFunction::Gamma22;
        assert_eq!(tf.decode(-0.25), -tf.decode(0.25));
    }

    #[test]
    fn names_round_trip() {
        for (name, tf) in TransferFunction::NAMED {
            assert_eq!(tf.name(), Some(name));
            assert_eq!(TransferFunction::from_name(name), Some(tf));
        }
        assert_eq!(TransferFunction::Power(1.8).name(), None);
    }
}
