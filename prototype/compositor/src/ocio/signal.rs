//! What the signal of an OCIO display is, told from outside: its primaries
//! and its transfer function, when the display's colour space is a curve
//! per channel followed by a matrix.
//!
//! A config describes a display as a conversion from its display-referred
//! reference space; it does not say "these primaries, this curve". What an
//! output is driven with, and what applications are told about it, needs
//! exactly that. So the conversion from the display's code values to CIE
//! XYZ is probed on the CPU: greys give the curve, the three primaries give
//! the matrix, and other colours tell whether the two explain everything.

use proto_color::{ImageDescription, Primaries, TransferFunction};
use proto_ocio::{Config, ReferenceSpace};

/// The signal of a display.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplaySignal {
    /// The primaries and the white of the code values.
    pub primaries: Primaries,
    /// The transfer function, when it is one this compositor knows.
    pub transfer: Option<TransferFunction>,
    /// The luminance of the code value 1, in cd/m², taking OCIO's
    /// display-referred 1.0 as 100 cd/m².
    pub peak: f64,
}

/// What OCIO answers for a view that ends in the display's own colour space.
const USE_DISPLAY_NAME: &str = "<USE_DISPLAY_NAME>";

/// The colour space a display's view ends in.
pub fn display_color_space(config: &Config, display: &str, view: &str) -> Option<String> {
    let name = config.view_color_space(display, view)?;
    config.resolve(if name == USE_DISPLAY_NAME {
        display
    } else {
        &name
    })
}

/// The transfer functions a curve is compared with.
const CANDIDATES: [TransferFunction; 6] = [
    TransferFunction::SrgbPiecewise,
    TransferFunction::Gamma22,
    TransferFunction::Gamma24,
    TransferFunction::Power(2.6),
    TransferFunction::Gamma28,
    TransferFunction::Pq,
];

/// Probes the colour space `color_space` (display-referred). `None` when it
/// is not display-referred, or not a curve per channel and a matrix.
pub fn probe(config: &Config, color_space: &str) -> Option<DisplaySignal> {
    if config.reference_space(color_space) != Some(ReferenceSpace::Display) {
        return None;
    }
    let xyz = config.role("cie_xyz_d65_interchange")?;
    let cpu = config.processor(color_space, &xyz).ok()?.cpu(true).ok()?;
    let apply = |rgb: [f32; 3]| -> Option<[f64; 3]> {
        let out = cpu.apply([rgb[0], rgb[1], rgb[2], 1.0]).ok()?;
        Some([out[0], out[1], out[2]].map(f64::from))
    };

    // Greys: the curve, as luminance relative to that of the code value 1.
    const STEPS: usize = 64;
    let white = apply([1.0; 3])?;
    let black = apply([0.0; 3])?;
    if white[1].is_nan() || white[1] <= 1e-6 || black.iter().any(|v| v.abs() > 1e-4 * white[1]) {
        return None;
    }
    let mut curve = Vec::with_capacity(STEPS + 1);
    for i in 0..=STEPS {
        let code = i as f32 / STEPS as f32;
        let grey = apply([code; 3])?;
        curve.push((code, grey[1] / white[1]));
        // Every grey has the chromaticity of white.
        for c in [0, 2] {
            if (grey[c] - white[c] / white[1] * grey[1]).abs() > 1e-4 * white[1] {
                return None;
            }
        }
    }
    // The curve at one code value, from the grey of that value.
    let linear = |code: f32| -> Option<f64> { Some(apply([code; 3])?[1] / white[1]) };

    // The primaries: the matrix from linear RGB (1 at the code value 1).
    let columns = [
        apply([1.0, 0.0, 0.0])?,
        apply([0.0, 1.0, 0.0])?,
        apply([0.0, 0.0, 1.0])?,
    ];
    let matrix: [[f64; 3]; 3] =
        std::array::from_fn(|row| std::array::from_fn(|column| columns[column][row]));
    // Other colours: explained by the curve and the matrix, or not.
    for rgb in [
        [0.5, 0.25, 0.75],
        [0.9, 0.1, 0.4],
        [0.2, 0.8, 0.6],
        [0.33, 0.66, 0.99],
    ] {
        let got = apply(rgb)?;
        let lin = [linear(rgb[0])?, linear(rgb[1])?, linear(rgb[2])?];
        for row in 0..3 {
            let expected: f64 = (0..3).map(|k| matrix[row][k] * lin[k]).sum();
            if (expected - got[row]).abs() > 1e-3 * white[1] {
                return None;
            }
        }
    }
    let primaries = super::primaries_of(&matrix)?;

    // The curve against the known ones, at the samples.
    let transfer = CANDIDATES
        .iter()
        .map(|candidate| {
            let peak = f64::from(candidate.decode(1.0));
            let worst = curve
                .iter()
                .map(|(code, value)| {
                    let known = f64::from(candidate.decode(*code)) / peak;
                    // Relative for the dark end of PQ, absolute elsewhere.
                    (known - value).abs() / value.max(0.05)
                })
                .fold(0.0, f64::max);
            (*candidate, worst)
        })
        .filter(|(_, worst)| *worst < 2e-3)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(candidate, _)| candidate);

    Some(DisplaySignal {
        primaries,
        transfer,
        peak: white[1] * 100.0,
    })
}

/// How far two chromaticities may be apart and still be called the same.
const CHROMATICITY_TOLERANCE: f64 = 2e-3;

fn same_primaries(a: &Primaries, b: &Primaries) -> bool {
    [
        (a.red, b.red),
        (a.green, b.green),
        (a.blue, b.blue),
        (a.white, b.white),
    ]
    .iter()
    .all(|(p, q)| {
        (p.x - q.x).abs() <= CHROMATICITY_TOLERANCE && (p.y - q.y).abs() <= CHROMATICITY_TOLERANCE
    })
}

impl DisplaySignal {
    /// A name for the primaries, when they are ones this compositor names.
    pub fn primaries_name(&self) -> Option<&'static str> {
        Primaries::NAMED
            .iter()
            .find(|(_, known)| same_primaries(known, &self.primaries))
            .map(|(name, _)| *name)
    }

    /// Whether an output driven as `description` says carries this signal.
    /// `None`: cannot be told (the display's curve is not a known one).
    pub fn matches(&self, description: &ImageDescription) -> Option<bool> {
        let transfer = self.transfer?;
        Some(
            same_primaries(&self.primaries, &description.primaries)
                && transfer == description.transfer,
        )
    }

    /// The image description of an output driven with this signal: the
    /// named one with these primaries and this transfer function when there
    /// is one, else one made of them, with the luminances that go with the
    /// transfer function. `None` when the transfer function is not known.
    ///
    /// The luminances are not the display's: a config says nothing about
    /// them beyond "1.0 is 100 cd/m²".
    pub fn image_description(&self) -> Option<ImageDescription> {
        let transfer = self.transfer?;
        Some(
            ImageDescription::NAMED
                .iter()
                .map(|(_, known)| *known)
                .find(|known| {
                    known.transfer == transfer && same_primaries(&known.primaries, &self.primaries)
                })
                .unwrap_or_else(|| ImageDescription::new(self.primaries, transfer)),
        )
    }

    pub fn describe(&self) -> String {
        format!(
            "{} primaries, {}, code value 1 at {:.0} cd/m²",
            self.primaries_name().unwrap_or("unnamed"),
            match self.transfer {
                Some(transfer) => transfer.name().map_or_else(
                    || format!("{transfer:?}"),
                    |name| format!("the {name} transfer function"),
                ),
                None => "an unknown transfer function".into(),
            },
            self.peak
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The displays of OCIO's built-in studio config, probed: the SDR ones
    /// and the PQ ones come out as primaries and a known curve; HLG, whose
    /// conversion mixes the channels, does not.
    #[test]
    fn builtin_displays_are_told_apart() {
        proto_ocio::quiet();
        let config = Config::from_file("ocio://studio-config-latest").unwrap();
        let mut found = Vec::new();
        for display in config.displays() {
            let space = display_color_space(&config, &display, "Un-tone-mapped").unwrap();
            let signal = probe(&config, &space);
            println!(
                "{display}: {}",
                signal.map_or("not a curve and a matrix".into(), |s| s.describe())
            );
            found.push((display, signal));
        }
        let of = |name: &str| found.iter().find(|(d, _)| d == name).unwrap().1;

        let srgb = of("sRGB - Display").unwrap();
        assert_eq!(srgb.primaries_name(), Some("srgb"));
        assert_eq!(srgb.transfer, Some(TransferFunction::SrgbPiecewise));
        assert!((srgb.peak - 100.0).abs() < 0.1);
        assert_eq!(srgb.matches(&ImageDescription::SRGB_PIECEWISE), Some(true));
        assert_eq!(srgb.matches(&ImageDescription::SRGB_GAMMA22), Some(false));
        assert_eq!(srgb.matches(&ImageDescription::DISPLAY_P3), Some(false));

        let p3 = of("Display P3 - Display").unwrap();
        assert_eq!(p3.primaries_name(), Some("display-p3"));
        assert_eq!(p3.transfer, Some(TransferFunction::SrgbPiecewise));

        let gamma22 = of("Gamma 2.2 Rec.709 - Display").unwrap();
        assert_eq!(gamma22.transfer, Some(TransferFunction::Gamma22));
        assert_eq!(gamma22.matches(&ImageDescription::SRGB_GAMMA22), Some(true));

        let rec1886 = of("Rec.1886 Rec.709 - Display").unwrap();
        assert_eq!(rec1886.primaries_name(), Some("srgb"));
        assert_eq!(rec1886.transfer, Some(TransferFunction::Gamma24));

        let cinema = of("P3-D65 - Display").unwrap();
        assert_eq!(cinema.transfer, Some(TransferFunction::Power(2.6)));

        let pq = of("Rec.2100-PQ - Display").unwrap();
        assert_eq!(pq.primaries_name(), Some("bt2020"));
        assert_eq!(pq.transfer, Some(TransferFunction::Pq));
        assert!((pq.peak - 10000.0).abs() < 10.0, "{}", pq.peak);
        assert_eq!(pq.matches(&ImageDescription::BT2020_PQ), Some(true));

        let pq_p3 = of("ST2084-P3-D65 - Display").unwrap();
        assert_eq!(pq_p3.primaries_name(), Some("display-p3"));
        assert_eq!(pq_p3.transfer, Some(TransferFunction::Pq));

        assert_eq!(of("Rec.2100-HLG - Display"), None);

        // A view that ends in a colour space that is no display's.
        assert_eq!(
            display_color_space(&config, "sRGB - Display", "Raw")
                .and_then(|space| probe(&config, &space)),
            None
        );
    }
}
