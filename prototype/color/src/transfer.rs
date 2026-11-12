//! Transfer functions (EOTFs) and their inverses.
//!
//! [`TransferFunction::decode`] maps an encoded (electrical) value to a
//! normalized linear (optical) value, where 1.0 is the maximum luminance of
//! the primary color volume. [`TransferFunction::encode`] is the inverse.
//!
//! The same formulas are implemented in the shaders of the script layer
//! (`prototype/compositor/scripts/color.luau`). [`TransferFunction::shader_id`]
//! and [`TransferFunction::shader_param`] are the bridge between the two,
//! handed to scripts by `hz.decode_transform` and `hz.encode_transform`;
//! keep them in sync with that module.

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
    /// SMPTE ST 2084 perceptual quantizer. 1.0 decodes to 10000 cd/m².
    Pq,
}

// SMPTE ST 2084 constants.
const PQ_M1: f64 = 2610.0 / 16384.0;
const PQ_M2: f64 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f64 = 3424.0 / 4096.0;
const PQ_C2: f64 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f64 = 2392.0 / 4096.0 * 32.0;

impl TransferFunction {
    /// Names used on the command line and in state dumps.
    pub const NAMED: [(&'static str, TransferFunction); 6] = [
        ("linear", TransferFunction::Linear),
        ("srgb-piecewise", TransferFunction::SrgbPiecewise),
        ("gamma22", TransferFunction::Gamma22),
        ("gamma24", TransferFunction::Gamma24),
        ("gamma28", TransferFunction::Gamma28),
        ("pq", TransferFunction::Pq),
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

    /// EOTF: encoded value → normalized linear value.
    ///
    /// Power-like functions are mirrored around zero for negative input, so
    /// out-of-range values survive a round trip. PQ clamps to `[0, 1]`.
    pub fn decode(&self, encoded: f32) -> f32 {
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
            TransferFunction::Gamma22 => mirrored(e, |v| v.powf(2.2)),
            TransferFunction::Gamma24 => mirrored(e, |v| v.powf(2.4)),
            TransferFunction::Gamma28 => mirrored(e, |v| v.powf(2.8)),
            TransferFunction::Power(g) => mirrored(e, |v| v.powf(g as f64)),
            TransferFunction::Pq => {
                let p = e.clamp(0.0, 1.0).powf(1.0 / PQ_M2);
                ((p - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * p)).powf(1.0 / PQ_M1)
            }
        };
        out as f32
    }

    /// Inverse EOTF: normalized linear value → encoded value.
    pub fn encode(&self, linear: f32) -> f32 {
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
            TransferFunction::Gamma22 => mirrored(l, |v| v.powf(1.0 / 2.2)),
            TransferFunction::Gamma24 => mirrored(l, |v| v.powf(1.0 / 2.4)),
            TransferFunction::Gamma28 => mirrored(l, |v| v.powf(1.0 / 2.8)),
            TransferFunction::Power(g) => mirrored(l, |v| v.powf(1.0 / g as f64)),
            TransferFunction::Pq => {
                let y = l.clamp(0.0, 1.0).powf(PQ_M1);
                ((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y)).powf(PQ_M2)
            }
        };
        out as f32
    }

    /// Identifier of this function in the shaders (`shaders/color.glsl`).
    pub fn shader_id(&self) -> u32 {
        match self {
            TransferFunction::Linear => 0,
            TransferFunction::SrgbPiecewise => 1,
            TransferFunction::Gamma22
            | TransferFunction::Gamma24
            | TransferFunction::Gamma28
            | TransferFunction::Power(_) => 2,
            TransferFunction::Pq => 3,
        }
    }

    /// Parameter passed to the shaders together with [`Self::shader_id`]:
    /// the exponent for power functions, unused otherwise.
    pub fn shader_param(&self) -> f32 {
        match *self {
            TransferFunction::Gamma22 => 2.2,
            TransferFunction::Gamma24 => 2.4,
            TransferFunction::Gamma28 => 2.8,
            TransferFunction::Power(g) => g,
            _ => 0.0,
        }
    }
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
