//! The compositing pipeline: content → compositing space → output.
//!
//! ```text
//! buffer (encoded, some alpha mode)
//!   └─ DecodeTransform ─▶ compositing space (linear light, premultiplied alpha)
//!                           └─ blend ─▶ EncodeTransform ─▶ output (encoded)
//! ```
//!
//! Both transforms are plain data so they can be passed to a shader
//! unchanged. The `apply` methods here are the CPU reference that what the
//! GPU draws is compared against.
//!
//! # Luminance mapping
//!
//! The compositing space is relative to the reference white: 0.0 means
//! "black", 1.0 means "reference white", larger values are HDR highlights.
//! A decoded linear value `O` stands for the luminance
//! `L = (max - min) O + min` (the appendix of color-management-v1; for PQ
//! the swing `max - min` is 10000 cd/m²). On the way in it becomes
//!
//! ```text
//! (L - min) / (reference - min) = O (max - min) / (reference - min)
//! ```
//!
//! and the way out is the inverse with the output's luminances. This
//! anchors the reference white of every image description to the same
//! level, as color-management-v1 asks compositors to do, and puts the black
//! level of every image description on the black of the output: an SDR
//! surface on an HDR output does not get a raised black. Both are a scale,
//! so they fold into the color matrix.

use crate::description::{AlphaMode, ImageDescription, Luminance};
use crate::math::mul_vec_f32;
use crate::primaries::Primaries;
use crate::transfer::TransferFunction;

/// The space surfaces are blended in: linear light in some primaries, with
/// the reference white at 1.0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompositeSpace {
    pub primaries: Primaries,
}

impl CompositeSpace {
    /// Linear BT.2020.
    pub const BT2020_LINEAR: CompositeSpace = CompositeSpace {
        primaries: Primaries::BT2020,
    };

    /// ACEScg (linear, AP1 primaries): the compositing space unless another
    /// is chosen.
    pub const ACESCG: CompositeSpace = CompositeSpace {
        primaries: Primaries::ACES_AP1,
    };
}

impl Default for CompositeSpace {
    fn default() -> Self {
        Self::ACESCG
    }
}

/// The OOTF of HLG (Rec. ITU-R BT.2100): scene light to display light,
/// `display = scene * Y_s^(gamma - 1)` with `Y_s` the luminance of the
/// scene light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ootf {
    /// The luminance of linear RGB in the primaries of the image
    /// description (the second row of its RGB → XYZ matrix).
    pub coefficients: [f32; 3],
    pub gamma: f32,
}

impl Ootf {
    fn new(description: &ImageDescription) -> Option<Self> {
        let y = description.primaries.rgb_to_xyz()?.0[1];
        Some(Self {
            coefficients: [y[0] as f32, y[1] as f32, y[2] as f32],
            gamma: description.transfer.param(&description.luminance),
        })
    }

    fn luminance(&self, rgb: [f32; 3]) -> f32 {
        let c = self.coefficients;
        (c[0] * rgb[0] + c[1] * rgb[1] + c[2] * rgb[2]).max(0.0)
    }

    /// Scene light → display light.
    pub fn apply(&self, scene: [f32; 3]) -> [f32; 3] {
        let k = self.luminance(scene).powf(self.gamma - 1.0);
        scene.map(|c| c * k)
    }

    /// Display light → scene light.
    pub fn invert(&self, display: [f32; 3]) -> [f32; 3] {
        let y = self.luminance(display);
        if y <= 0.0 {
            return [0.0; 3];
        }
        let k = y.powf((1.0 - self.gamma) / self.gamma);
        display.map(|c| c * k)
    }
}

/// The factor between decoded linear values of `description` and values
/// relative to its reference white (see the module documentation). `None`
/// when the reference white is not above the black level.
fn relative_scale(description: &ImageDescription) -> Option<f64> {
    let l = &description.luminance;
    let headroom = l.reference as f64 - l.min as f64;
    let swing = l.swing(description.transfer);
    (headroom > 0.0 && swing > 0.0 && headroom.is_finite() && swing.is_finite())
        .then_some(swing / headroom)
}

/// Converts buffer values to premultiplied linear values in the compositing
/// space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecodeTransform {
    pub alpha_mode: AlphaMode,
    pub transfer: TransferFunction,
    /// [`TransferFunction::param`] for the luminances of the source.
    pub transfer_param: f32,
    /// For HLG: the OOTF, applied after the transfer function and before
    /// the matrix.
    pub ootf: Option<Ootf>,
    /// Row-major. Maps decoded linear RGB to the compositing space. Includes
    /// the luminance scale.
    pub matrix: [[f32; 3]; 3],
}

impl DecodeTransform {
    /// Returns `None` when the primaries of `source` or `space` are
    /// degenerate, or the luminances of `source` are.
    pub fn new(
        source: &ImageDescription,
        alpha_mode: AlphaMode,
        space: &CompositeSpace,
    ) -> Option<Self> {
        let scale = relative_scale(source)?;
        let matrix = source
            .primaries
            .conversion_to(&space.primaries)?
            .scaled(scale)
            .to_f32();
        let ootf = match source.transfer {
            TransferFunction::Hlg => Some(Ootf::new(source)?),
            _ => None,
        };
        Some(Self {
            alpha_mode,
            transfer: source.transfer,
            transfer_param: source.transfer.param(&source.luminance),
            ootf,
            matrix,
        })
    }

    /// Converts one RGBA value as stored in a buffer to a premultiplied
    /// linear RGBA value in the compositing space.
    pub fn apply(&self, rgba: [f32; 4]) -> [f32; 4] {
        let [r, g, b, a] = rgba;
        let decode = |v: [f32; 3]| {
            let linear = v.map(|c| self.transfer.decode_with(c, self.transfer_param));
            match &self.ootf {
                Some(ootf) => ootf.apply(linear),
                None => linear,
            }
        };
        match self.alpha_mode {
            AlphaMode::PremultipliedElectrical => {
                if a <= 0.0 {
                    return [0.0; 4];
                }
                let linear = mul_vec_f32(&self.matrix, decode([r / a, g / a, b / a]));
                [linear[0] * a, linear[1] * a, linear[2] * a, a]
            }
            AlphaMode::PremultipliedOptical => {
                // The buffer stores encode(linear * alpha), and the matrix is
                // linear, so the product with alpha carries through.
                let linear = mul_vec_f32(&self.matrix, decode([r, g, b]));
                [linear[0], linear[1], linear[2], a]
            }
            AlphaMode::Straight => {
                let linear = mul_vec_f32(&self.matrix, decode([r, g, b]));
                [linear[0] * a, linear[1] * a, linear[2] * a, a]
            }
        }
    }
}

impl DecodeTransform {
    /// Describes the steps of the transform as text, in the order they are
    /// applied. Used by state dumps so that a change of the conversion path
    /// is visible in tests.
    pub fn describe(
        source: &ImageDescription,
        alpha_mode: AlphaMode,
        space: &CompositeSpace,
    ) -> String {
        let mut eotf = vec![format!("eotf:{}", transfer_label(&source.transfer))];
        if source.transfer == TransferFunction::Hlg {
            eotf.push(format!(
                "ootf:gamma({})",
                source.transfer.param(&source.luminance)
            ));
        }
        let primaries = format!(
            "primaries:{}->{}",
            primaries_label(&source.primaries),
            primaries_label(&space.primaries)
        );
        let luminance = luminance_label(&source.luminance, source.transfer, false);
        let mut steps = Vec::new();
        if alpha_mode == AlphaMode::PremultipliedElectrical {
            steps.push("unpremultiply".to_string());
        }
        steps.extend(eotf);
        steps.push(primaries);
        steps.push(luminance);
        if alpha_mode != AlphaMode::PremultipliedOptical {
            steps.push("premultiply".to_string());
        }
        steps.join(" | ")
    }
}

fn transfer_label(transfer: &TransferFunction) -> String {
    match (transfer.name(), transfer) {
        (Some(name), _) => name.to_string(),
        (None, TransferFunction::Power(g)) => format!("power({g})"),
        (None, other) => format!("{other:?}"),
    }
}

/// `luminance:x(swing/headroom)` on the way in, the inverse on the way out:
/// the scale between decoded values and values relative to the reference
/// white, as a fraction of luminances above the black level.
fn luminance_label(luminance: &Luminance, transfer: TransferFunction, out: bool) -> String {
    let swing = luminance.swing(transfer) as f32;
    let headroom = luminance.reference - luminance.min;
    if out {
        format!("luminance:x({headroom}/{swing})")
    } else {
        format!("luminance:x({swing}/{headroom})")
    }
}

fn primaries_label(primaries: &Primaries) -> String {
    match primaries.name() {
        Some(name) => name.to_string(),
        None => format!(
            "custom(r={},{} g={},{} b={},{} w={},{})",
            primaries.red.x,
            primaries.red.y,
            primaries.green.x,
            primaries.green.y,
            primaries.blue.x,
            primaries.blue.y,
            primaries.white.x,
            primaries.white.y
        ),
    }
}

/// How compositing-space values that the output cannot show are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToneMapping {
    /// Clip each channel to the range of the output, after converting to the
    /// output primaries. No roll-off, no gamut mapping.
    #[default]
    Clip,
}

/// Converts values in the compositing space to encoded output values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodeTransform {
    /// Row-major. Maps compositing-space RGB to normalized linear RGB of the
    /// output. Includes the luminance scale.
    pub matrix: [[f32; 3]; 3],
    pub tone_mapping: ToneMapping,
    pub transfer: TransferFunction,
    /// [`TransferFunction::param`] for the luminances of the output.
    pub transfer_param: f32,
    /// For an HLG output: the OOTF, inverted after clipping and before the
    /// OETF. (The renderer's shaders do not encode HLG.)
    pub ootf: Option<Ootf>,
}

impl EncodeTransform {
    /// Returns `None` when the primaries of `output` or `space` are
    /// degenerate, or the luminances of `output` are.
    pub fn new(space: &CompositeSpace, output: &ImageDescription) -> Option<Self> {
        let scale = 1.0 / relative_scale(output)?;
        let matrix = space
            .primaries
            .conversion_to(&output.primaries)?
            .scaled(scale)
            .to_f32();
        let ootf = match output.transfer {
            TransferFunction::Hlg => Some(Ootf::new(output)?),
            _ => None,
        };
        Some(Self {
            matrix,
            tone_mapping: ToneMapping::Clip,
            transfer: output.transfer,
            transfer_param: output.transfer.param(&output.luminance),
            ootf,
        })
    }

    /// Converts one compositing-space RGB value to an encoded output value.
    ///
    /// The result is in `[0, 1]` unless the output transfer function is
    /// extended range ([`TransferFunction::is_extended_range`]).
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let mut linear = mul_vec_f32(&self.matrix, rgb);
        match self.tone_mapping {
            ToneMapping::Clip => {
                if !self.transfer.is_extended_range() {
                    linear = linear.map(|c| c.clamp(0.0, 1.0));
                }
            }
        }
        if let Some(ootf) = &self.ootf {
            linear = ootf.invert(linear);
        }
        linear.map(|c| self.transfer.encode_with(c, self.transfer_param))
    }
}

impl EncodeTransform {
    /// Describes the steps of the transform as text, in the order they are
    /// applied. See [`DecodeTransform::describe`].
    pub fn describe(space: &CompositeSpace, output: &ImageDescription) -> String {
        let mut steps = vec![
            luminance_label(&output.luminance, output.transfer, true),
            format!(
                "primaries:{}->{}",
                primaries_label(&space.primaries),
                primaries_label(&output.primaries)
            ),
        ];
        if !output.transfer.is_extended_range() {
            steps.push("clip[0,1]".to_string());
        }
        if output.transfer == TransferFunction::Hlg {
            steps.push(format!(
                "inverse-ootf:gamma({})",
                output.transfer.param(&output.luminance)
            ));
        }
        steps.push(format!("inverse-eotf:{}", transfer_label(&output.transfer)));
        steps.join(" | ")
    }
}

/// Premultiplied "source over" blend.
pub fn blend_over(destination: [f32; 4], source: [f32; 4]) -> [f32; 4] {
    let k = 1.0 - source[3];
    [
        source[0] + destination[0] * k,
        source[1] + destination[1] * k,
        source[2] + destination[2] * k,
        source[3] + destination[3] * k,
    ]
}

/// Quantizes a value in `[0, 1]` to an unsigned normalized integer of `bits`
/// bits, rounding to nearest (the conversion Vulkan specifies for UNORM
/// formats).
pub fn quantize_unorm(value: f32, bits: u32) -> u32 {
    let max = ((1u64 << bits) - 1) as f32;
    (value.clamp(0.0, 1.0) * max).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPACE: CompositeSpace = CompositeSpace::BT2020_LINEAR;

    fn assert_close3(actual: [f32; 3], expected: [f32; 3], tolerance: f32) {
        for i in 0..3 {
            assert!(
                (actual[i] - expected[i]).abs() <= tolerance,
                "{actual:?} vs {expected:?} (tolerance {tolerance})"
            );
        }
    }

    fn round_trip(source: &ImageDescription, output: &ImageDescription, rgb: [f32; 3]) -> [f32; 3] {
        let decode = DecodeTransform::new(source, AlphaMode::Straight, &SPACE).unwrap();
        let encode = EncodeTransform::new(&SPACE, output).unwrap();
        let c = decode.apply([rgb[0], rgb[1], rgb[2], 1.0]);
        encode.apply([c[0], c[1], c[2]])
    }

    #[test]
    fn same_description_round_trips() {
        for (_, desc) in ImageDescription::NAMED {
            for rgb in [
                [0.0, 0.0, 0.0],
                [1.0, 1.0, 1.0],
                [1.0, 0.0, 0.0],
                [0.2, 0.5, 0.8],
                [0.75, 0.25, 0.5],
            ] {
                // The matrices are f32, so a channel that should be exactly
                // zero comes back as ~1e-5 linear; the steep start of the
                // transfer functions amplifies that. 5e-4 is still 1/8 of
                // an 8 bit step.
                assert_close3(round_trip(&desc, &desc, rgb), rgb, 5e-4);
            }
        }
    }

    #[test]
    fn srgb_white_is_composite_one() {
        let decode = DecodeTransform::new(
            &ImageDescription::SRGB_PIECEWISE,
            AlphaMode::Straight,
            &SPACE,
        )
        .unwrap();
        let c = decode.apply([1.0, 1.0, 1.0, 1.0]);
        assert_close3([c[0], c[1], c[2]], [1.0, 1.0, 1.0], 1e-6);
    }

    // sRGB red expressed in linear BT.2020 is the first column of the
    // BT.709 → BT.2020 matrix of Rec. ITU-R BT.2087-0.
    #[test]
    fn srgb_red_in_composite_space() {
        let decode = DecodeTransform::new(
            &ImageDescription::SRGB_PIECEWISE,
            AlphaMode::Straight,
            &SPACE,
        )
        .unwrap();
        let c = decode.apply([1.0, 0.0, 0.0, 1.0]);
        assert_close3([c[0], c[1], c[2]], [0.6274, 0.0691, 0.0164], 5e-5);
    }

    // sRGB red shown on a Display P3 output: color(display-p3 0.9175 0.2003
    // 0.1386), the value CSS Color 4 tooling reports for sRGB red.
    #[test]
    fn srgb_red_on_display_p3_output() {
        let out = round_trip(
            &ImageDescription::SRGB_PIECEWISE,
            &ImageDescription::DISPLAY_P3,
            [1.0, 0.0, 0.0],
        );
        assert_close3(out, [0.9175, 0.2003, 0.1386], 1e-4);
    }

    #[test]
    fn display_p3_red_clips_on_srgb_output() {
        let out = round_trip(
            &ImageDescription::DISPLAY_P3,
            &ImageDescription::SRGB_PIECEWISE,
            [1.0, 0.0, 0.0],
        );
        // Out of the sRGB gamut: red saturates, green and blue clip at zero.
        assert_close3(out, [1.0, 0.0, 0.0], 1e-6);
    }

    // SDR reference white (sRGB 1.0) on a PQ output lands on the PQ code
    // for 203 cd/m² (0.5807, BT.2408).
    #[test]
    fn sdr_white_on_pq_output_is_203_nits() {
        let out = round_trip(
            &ImageDescription::SRGB_PIECEWISE,
            &ImageDescription::BT2020_PQ,
            [1.0, 1.0, 1.0],
        );
        assert_close3(out, [0.5807, 0.5807, 0.5807], 1e-4);
    }

    #[test]
    fn pq_content_above_reference_white_exceeds_one() {
        let decode =
            DecodeTransform::new(&ImageDescription::BT2020_PQ, AlphaMode::Straight, &SPACE)
                .unwrap();
        // PQ code for 1000 cd/m² → 1000 / 203 in the compositing space.
        let code = TransferFunction::Pq.encode(0.1);
        let c = decode.apply([code, code, code, 1.0]);
        assert_close3([c[0], c[1], c[2]], [1000.0 / 203.0; 3], 2e-3);
    }

    #[test]
    fn hdr_highlight_clips_on_sdr_output() {
        let out = round_trip(
            &ImageDescription::BT2020_PQ,
            &ImageDescription::SRGB_PIECEWISE,
            [0.75, 0.75, 0.75],
        );
        assert_close3(out, [1.0, 1.0, 1.0], 1e-6);
    }

    #[test]
    fn extended_range_output_is_not_clipped() {
        let out = round_trip(
            &ImageDescription::DISPLAY_P3,
            &ImageDescription::SCRGB_LINEAR,
            [1.0, 0.0, 0.0],
        );
        // Display P3 red in linear sRGB (negative green and blue).
        assert_close3(out, [1.2249, -0.0420, -0.0197], 2e-4);
    }

    #[test]
    fn custom_reference_white_scales_content() {
        // Content whose reference white is half of its maximum: full code
        // value is twice the reference white.
        let desc = ImageDescription::SRGB_PIECEWISE.with_luminance(Luminance {
            min: 0.0,
            max: 160.0,
            reference: 80.0,
        });
        let decode = DecodeTransform::new(&desc, AlphaMode::Straight, &SPACE).unwrap();
        let c = decode.apply([1.0, 1.0, 1.0, 1.0]);
        assert_close3([c[0], c[1], c[2]], [2.0, 2.0, 2.0], 1e-5);
    }

    #[test]
    fn alpha_modes_agree_on_the_same_color() {
        let straight = DecodeTransform::new(
            &ImageDescription::SRGB_PIECEWISE,
            AlphaMode::Straight,
            &SPACE,
        )
        .unwrap();
        let electrical = DecodeTransform::new(
            &ImageDescription::SRGB_PIECEWISE,
            AlphaMode::PremultipliedElectrical,
            &SPACE,
        )
        .unwrap();
        let optical = DecodeTransform::new(
            &ImageDescription::SRGB_PIECEWISE,
            AlphaMode::PremultipliedOptical,
            &SPACE,
        )
        .unwrap();

        let (rgb, a) = ([0.8f32, 0.4, 0.2], 0.5f32);
        let expected = straight.apply([rgb[0], rgb[1], rgb[2], a]);

        let pe = electrical.apply([rgb[0] * a, rgb[1] * a, rgb[2] * a, a]);
        let tf = TransferFunction::SrgbPiecewise;
        let po_in = rgb.map(|c| tf.encode(tf.decode(c) * a));
        let po = optical.apply([po_in[0], po_in[1], po_in[2], a]);

        for i in 0..4 {
            assert!((pe[i] - expected[i]).abs() < 1e-6, "{pe:?} vs {expected:?}");
            assert!((po[i] - expected[i]).abs() < 1e-6, "{po:?} vs {expected:?}");
        }
    }

    #[test]
    fn zero_alpha_decodes_to_transparent() {
        let decode = DecodeTransform::new(
            &ImageDescription::SRGB_PIECEWISE,
            AlphaMode::PremultipliedElectrical,
            &SPACE,
        )
        .unwrap();
        assert_eq!(decode.apply([0.0, 0.0, 0.0, 0.0]), [0.0; 4]);
    }

    // Blending 50% white over black in linear light and encoding to sRGB
    // gives 0.7354 (8 bit 188), not 0.5 (8 bit 128) as blending encoded
    // values would.
    #[test]
    fn half_transparent_white_over_black_blends_in_linear_light() {
        let decode = DecodeTransform::new(
            &ImageDescription::SRGB_PIECEWISE,
            AlphaMode::PremultipliedElectrical,
            &SPACE,
        )
        .unwrap();
        let encode = EncodeTransform::new(&SPACE, &ImageDescription::SRGB_PIECEWISE).unwrap();
        let black = decode.apply([0.0, 0.0, 0.0, 1.0]);
        let white = decode.apply([0.5, 0.5, 0.5, 0.5]);
        let blended = blend_over(black, white);
        assert!((blended[3] - 1.0).abs() < 1e-7);
        let out = encode.apply([blended[0], blended[1], blended[2]]);
        assert_close3(out, [0.735_357; 3], 1e-5);
        assert_eq!(quantize_unorm(out[0], 8), 188);
    }

    #[test]
    fn transforms_describe_their_steps() {
        assert_eq!(
            DecodeTransform::describe(
                &ImageDescription::SRGB_GAMMA22,
                AlphaMode::PremultipliedElectrical,
                &SPACE
            ),
            "unpremultiply | eotf:gamma22 | primaries:srgb->bt2020 | luminance:x(79.8/79.8) | premultiply"
        );
        assert_eq!(
            DecodeTransform::describe(&ImageDescription::BT2020_PQ, AlphaMode::Straight, &SPACE),
            "eotf:pq | primaries:bt2020->bt2020 | luminance:x(10000/202.995) | premultiply"
        );
        assert_eq!(
            EncodeTransform::describe(&SPACE, &ImageDescription::DISPLAY_P3),
            "luminance:x(79.8/79.8) | primaries:bt2020->display-p3 | clip[0,1] | inverse-eotf:srgb-piecewise"
        );
        assert_eq!(
            EncodeTransform::describe(&SPACE, &ImageDescription::SCRGB_LINEAR),
            "luminance:x(79.8/79.8) | primaries:bt2020->srgb | inverse-eotf:linear"
        );
    }

    // Content without color information (BT.709 primaries, gamma 2.2) on an
    // output of the same kind comes back unchanged.
    #[test]
    fn unmanaged_content_on_gamma22_output_is_unchanged() {
        for code in [0u32, 1, 2, 17, 64, 128, 200, 254, 255] {
            let v = code as f32 / 255.0;
            let out = round_trip(
                &ImageDescription::SRGB_GAMMA22,
                &ImageDescription::SRGB_GAMMA22,
                [v, v, v],
            );
            assert_eq!(out.map(|c| quantize_unorm(c, 8)), [code; 3]);
        }
    }

    // The same content on a piecewise sRGB output is *not* unchanged: the
    // two curves differ, most visibly in the dark range.
    #[test]
    fn gamma22_and_piecewise_srgb_differ() {
        let out = round_trip(
            &ImageDescription::SRGB_GAMMA22,
            &ImageDescription::SRGB_PIECEWISE,
            [0.1, 0.1, 0.1],
        );
        // 0.1^2.2 = 0.0063096; 1.055 * 0.0063096^(1/2.4) - 0.055 = 0.072816.
        assert_close3(out, [0.072_816; 3], 2e-5);
    }

    #[test]
    fn quantize_rounds_to_nearest_and_clamps() {
        assert_eq!(quantize_unorm(0.0, 8), 0);
        assert_eq!(quantize_unorm(1.0, 8), 255);
        assert_eq!(quantize_unorm(0.5, 8), 128);
        assert_eq!(quantize_unorm(1.5, 8), 255);
        assert_eq!(quantize_unorm(-0.5, 8), 0);
        assert_eq!(quantize_unorm(1.0, 16), 65535);
        assert_eq!(quantize_unorm(0.5, 16), 32768);
    }

    fn decode_straight(desc: &ImageDescription, rgb: [f32; 3]) -> [f32; 3] {
        let decode = DecodeTransform::new(desc, AlphaMode::Straight, &SPACE).unwrap();
        let c = decode.apply([rgb[0], rgb[1], rgb[2], 1.0]);
        [c[0], c[1], c[2]]
    }

    // The signal at the reference white comes out at 1.0 whatever the
    // luminances: here the code value that the appendix maps to 50 cd/m²
    // on a 1 .. 100 cd/m² display whose reference white is 50 cd/m².
    #[test]
    fn reference_white_is_anchored_with_a_black_level() {
        let desc = ImageDescription::new(Primaries::SRGB, TransferFunction::Gamma22)
            .with_luminance(Luminance {
                min: 1.0,
                max: 100.0,
                reference: 50.0,
            });
        // L = 99 O + 1 = 50  →  O = 49 / 99.
        let code = TransferFunction::Gamma22.encode(49.0 / 99.0);
        assert_close3(decode_straight(&desc, [code; 3]), [1.0; 3], 1e-5);
        // The black level is black.
        assert_close3(decode_straight(&desc, [0.0; 3]), [0.0; 3], 0.0);
    }

    // BT.2408: an SDR surface's black stays black on a PQ output (the PQ
    // code of 0 cd/m² above the output's black), and its white sits at
    // 203 cd/m².
    #[test]
    fn sdr_black_stays_black_on_pq_output() {
        let out = round_trip(
            &ImageDescription::SRGB_PIECEWISE,
            &ImageDescription::BT2020_PQ,
            [0.0, 0.0, 0.0],
        );
        assert_close3(out, [TransferFunction::Pq.encode(0.0); 3], 1e-7);
    }

    // Report ITU-R BT.2408: HLG at 75 % of the signal is the reference
    // white (203 cd/m² on the 1000 cd/m² display the protocol assumes).
    #[test]
    fn hlg_reference_white_is_composite_one() {
        let desc = ImageDescription::new(Primaries::BT2020, TransferFunction::Hlg);
        assert_eq!(desc.luminance, Luminance::HLG);
        let c = decode_straight(&desc, [0.75; 3]);
        // 1000 * 0.26496^1.2 = 203.15 cd/m²; relative to 203 above black.
        assert_close3(c, [1.0; 3], 2e-3);
        // HLG peak: 1000 cd/m², about 4.93 times the reference white.
        let c = decode_straight(&desc, [1.0; 3]);
        assert_close3(c, [(1000.0 - 0.005) / (203.0 - 0.005); 3], 1e-3);
    }

    // The OOTF works on the luminance of the scene light: a saturated
    // color is scaled by its own luminance, not channel by channel.
    #[test]
    fn hlg_ootf_scales_by_the_luminance() {
        let desc = ImageDescription::new(Primaries::BT2020, TransferFunction::Hlg);
        let tf = TransferFunction::Hlg;
        let red = 0.75f32;
        let scene = tf.decode(red);
        // Y_s = 0.2627 * scene for pure BT.2020 red.
        let display = scene * (0.262_700_2 * scene).powf(0.2);
        let expected = display * (1000.0 - 0.005) / (203.0 - 0.005);
        let c = decode_straight(&desc, [red, 0.0, 0.0]);
        assert_close3(c, [expected, 0.0, 0.0], 1e-4);
    }

    #[test]
    fn hlg_and_bt1886_outputs_round_trip() {
        let hlg = ImageDescription::new(Primaries::BT2020, TransferFunction::Hlg);
        let bt1886 = ImageDescription::new(Primaries::SRGB, TransferFunction::Bt1886);
        for desc in [hlg, bt1886] {
            for rgb in [
                [0.0, 0.0, 0.0],
                [0.75, 0.75, 0.75],
                [0.2, 0.5, 0.8],
                [1.0, 0.3, 0.1],
            ] {
                assert_close3(round_trip(&desc, &desc, rgb), rgb, 2e-4);
            }
        }
    }

    // BT.1886 content (BT.2035 defaults: 0.01 / 100 / 100 cd/m²) on a
    // gamma 2.2 sRGB output (0.2 / 80 / 80): white is white, black is black.
    #[test]
    fn bt1886_white_and_black_land_on_the_output_white_and_black() {
        let bt1886 = ImageDescription::new(Primaries::SRGB, TransferFunction::Bt1886);
        assert_eq!(bt1886.luminance, Luminance::BT1886);
        let out = round_trip(&bt1886, &ImageDescription::SRGB_GAMMA22, [1.0; 3]);
        assert_close3(out, [1.0; 3], 1e-5);
        let out = round_trip(&bt1886, &ImageDescription::SRGB_GAMMA22, [0.0; 3]);
        assert_close3(out, [0.0; 3], 1e-6);
    }

    // PQ ignores the maximum: 1.0 is always 10000 cd/m² above the black.
    #[test]
    fn pq_maximum_follows_the_minimum() {
        let desc = ImageDescription::BT2020_PQ.with_luminance(Luminance {
            min: 0.0,
            max: 1000.0,
            reference: 100.0,
        });
        let code = TransferFunction::Pq.encode(100.0 / 10000.0);
        assert_close3(decode_straight(&desc, [code; 3]), [1.0; 3], 1e-4);
    }

    #[test]
    fn degenerate_luminances_are_rejected() {
        let desc = ImageDescription::SRGB_GAMMA22.with_luminance(Luminance {
            min: 10.0,
            max: 80.0,
            reference: 10.0,
        });
        assert!(DecodeTransform::new(&desc, AlphaMode::Straight, &SPACE).is_none());
        assert!(EncodeTransform::new(&SPACE, &desc).is_none());
    }

    #[test]
    fn hlg_transforms_describe_the_ootf() {
        let hlg = ImageDescription::new(Primaries::BT2020, TransferFunction::Hlg);
        assert_eq!(
            DecodeTransform::describe(&hlg, AlphaMode::PremultipliedElectrical, &SPACE),
            "unpremultiply | eotf:hlg | ootf:gamma(1.2) | primaries:bt2020->bt2020 \
             | luminance:x(999.995/202.995) | premultiply"
        );
    }
}
