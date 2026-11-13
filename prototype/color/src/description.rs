//! Image descriptions: what the numbers in a buffer mean.

use crate::primaries::Primaries;
use crate::transfer::TransferFunction;

/// Luminances of the primary color volume, in cd/m².
///
/// `min` is the luminance of the linear value 0.0 after
/// [`TransferFunction::decode`] (the black level) and `max` that of 1.0
/// (for PQ, `min + 10000` whatever `max` says). `reference` is the
/// luminance of the reference white ("SDR white"); all content is anchored
/// to it when compositing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Luminance {
    pub min: f32,
    pub max: f32,
    pub reference: f32,
}

impl Luminance {
    /// Default of color-management-v1 for most transfer functions
    /// (from the sRGB characterization data): 0.2 / 80 / 80 cd/m².
    pub const SDR: Luminance = Luminance {
        min: 0.2,
        max: 80.0,
        reference: 80.0,
    };

    /// Default of color-management-v1 for BT.1886 (from BT.2035):
    /// 0.01 / 100 / 100 cd/m².
    pub const BT1886: Luminance = Luminance {
        min: 0.01,
        max: 100.0,
        reference: 100.0,
    };

    /// Default of color-management-v1 for ST 2084 PQ:
    /// 0.005 / 10000 / 203 cd/m².
    pub const PQ: Luminance = Luminance {
        min: 0.005,
        max: 10000.0,
        reference: 203.0,
    };

    /// Default of color-management-v1 for HLG (a 1000 cd/m² display):
    /// 0.005 / 1000 / 203 cd/m².
    pub const HLG: Luminance = Luminance {
        min: 0.005,
        max: 1000.0,
        reference: 203.0,
    };

    /// The default luminances that color-management-v1 associates with a
    /// transfer function.
    pub const fn default_for(transfer: TransferFunction) -> Luminance {
        match transfer {
            TransferFunction::Pq => Luminance::PQ,
            TransferFunction::Hlg => Luminance::HLG,
            TransferFunction::Bt1886 => Luminance::BT1886,
            _ => Luminance::SDR,
        }
    }

    /// The luminance of the decoded value 1.0 above the black level: the
    /// swing of the transfer function. `max - min`, except for PQ, whose
    /// swing is always 10000 cd/m².
    pub fn swing(&self, transfer: TransferFunction) -> f64 {
        match transfer {
            TransferFunction::Pq => 10000.0,
            _ => self.max as f64 - self.min as f64,
        }
    }
}

/// Mastering display information (SMPTE ST 2086) and content light levels.
/// This is metadata about the content; it does not change how values decode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MasteringDisplay {
    pub primaries: Primaries,
    /// Minimum and maximum luminance of the mastering display, cd/m².
    pub min_luminance: f32,
    pub max_luminance: f32,
    /// Maximum content light level, cd/m².
    pub max_cll: Option<f32>,
    /// Maximum frame-average light level, cd/m².
    pub max_fall: Option<f32>,
}

/// A parametric image description.
///
/// Every texture and every render target carries one. There is no type in
/// hizukuri that means "sRGB" implicitly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageDescription {
    pub primaries: Primaries,
    pub transfer: TransferFunction,
    pub luminance: Luminance,
    pub mastering: Option<MasteringDisplay>,
}

impl ImageDescription {
    /// Builds a description with the default luminances of the transfer
    /// function and no mastering information.
    pub const fn new(primaries: Primaries, transfer: TransferFunction) -> Self {
        let luminance = Luminance::default_for(transfer);
        Self {
            primaries,
            transfer,
            luminance,
            mastering: None,
        }
    }

    /// sRGB / BT.709 primaries with a pure 2.2 power function: how an
    /// actual sRGB display responds. This is what content without any color
    /// information is taken to be.
    pub const SRGB_GAMMA22: ImageDescription =
        ImageDescription::new(Primaries::SRGB, TransferFunction::Gamma22);

    /// sRGB primaries with the piecewise encoding function of IEC 61966-2-1
    /// (`compound_power_2_4` in color-management-v1).
    pub const SRGB_PIECEWISE: ImageDescription =
        ImageDescription::new(Primaries::SRGB, TransferFunction::SrgbPiecewise);

    /// Display P3: P3 primaries, D65 white, piecewise sRGB transfer function.
    pub const DISPLAY_P3: ImageDescription =
        ImageDescription::new(Primaries::DISPLAY_P3, TransferFunction::SrgbPiecewise);

    /// Adobe RGB (1998) primaries with a 2.2 power function.
    pub const ADOBE_RGB: ImageDescription =
        ImageDescription::new(Primaries::ADOBE_RGB, TransferFunction::Gamma22);

    /// BT.2020 primaries with a 2.4 power function (SDR wide gamut).
    pub const BT2020_GAMMA24: ImageDescription =
        ImageDescription::new(Primaries::BT2020, TransferFunction::Gamma24);

    /// BT.2100 PQ (HDR10).
    pub const BT2020_PQ: ImageDescription =
        ImageDescription::new(Primaries::BT2020, TransferFunction::Pq);

    /// BT.2020 primaries, linear, 1.0 = 80 cd/m² reference white.
    pub const BT2020_LINEAR: ImageDescription =
        ImageDescription::new(Primaries::BT2020, TransferFunction::Linear);

    /// scRGB: sRGB primaries, linear, extended range, 1.0 = 80 cd/m².
    pub const SCRGB_LINEAR: ImageDescription =
        ImageDescription::new(Primaries::SRGB, TransferFunction::Linear);

    /// Presets with the names used on the command line and in state dumps.
    pub const NAMED: [(&'static str, ImageDescription); 8] = [
        ("srgb-gamma22", ImageDescription::SRGB_GAMMA22),
        ("srgb-piecewise", ImageDescription::SRGB_PIECEWISE),
        ("display-p3", ImageDescription::DISPLAY_P3),
        ("adobe-rgb", ImageDescription::ADOBE_RGB),
        ("bt2020-gamma24", ImageDescription::BT2020_GAMMA24),
        ("bt2020-pq", ImageDescription::BT2020_PQ),
        ("bt2020-linear", ImageDescription::BT2020_LINEAR),
        ("scrgb-linear", ImageDescription::SCRGB_LINEAR),
    ];

    pub fn from_name(name: &str) -> Option<ImageDescription> {
        Self::NAMED
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, d)| *d)
    }

    /// Returns the preset name when this description equals a preset.
    pub fn name(&self) -> Option<&'static str> {
        Self::NAMED
            .iter()
            .find(|(_, d)| d == self)
            .map(|(name, _)| *name)
    }

    pub fn with_luminance(mut self, luminance: Luminance) -> Self {
        self.luminance = luminance;
        self
    }
}

/// How the alpha channel relates to the color channels
/// (color-representation-v1 `alpha_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlphaMode {
    /// Color channels are multiplied by alpha in the encoded (electrical)
    /// domain. This is what `wl_shm` and most toolkits produce.
    #[default]
    PremultipliedElectrical,
    /// Color channels are multiplied by alpha in the linear (optical) domain.
    PremultipliedOptical,
    /// Color channels are not multiplied by alpha.
    Straight,
}

impl AlphaMode {
    pub const NAMED: [(&'static str, AlphaMode); 3] = [
        (
            "premultiplied-electrical",
            AlphaMode::PremultipliedElectrical,
        ),
        ("premultiplied-optical", AlphaMode::PremultipliedOptical),
        ("straight", AlphaMode::Straight),
    ];

    pub fn from_name(name: &str) -> Option<AlphaMode> {
        Self::NAMED
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, m)| *m)
    }

    pub fn name(&self) -> &'static str {
        Self::NAMED
            .iter()
            .find(|(_, m)| m == self)
            .map(|(name, _)| *name)
            .expect("every alpha mode is named")
    }

    /// Identifier of this mode in the shaders
    /// (`prototype/compositor/scripts/render.luau`).
    pub fn shader_id(&self) -> u32 {
        match self {
            AlphaMode::PremultipliedElectrical => 0,
            AlphaMode::PremultipliedOptical => 1,
            AlphaMode::Straight => 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_luminances_follow_the_transfer_function() {
        assert_eq!(ImageDescription::SRGB_GAMMA22.luminance, Luminance::SDR);
        assert_eq!(ImageDescription::BT2020_PQ.luminance, Luminance::PQ);
        assert_eq!(
            Luminance::default_for(TransferFunction::Gamma22),
            Luminance::SDR
        );
        assert_eq!(Luminance::default_for(TransferFunction::Pq), Luminance::PQ);
        assert_eq!(
            Luminance::default_for(TransferFunction::Bt1886),
            Luminance::BT1886
        );
        assert_eq!(
            Luminance::default_for(TransferFunction::Hlg),
            Luminance::HLG
        );
    }

    #[test]
    fn pq_swings_10000_above_its_black() {
        let pq = Luminance {
            min: 0.1,
            max: 1.0,
            reference: 203.0,
        };
        assert_eq!(pq.swing(TransferFunction::Pq), 10000.0);
        assert!((Luminance::SDR.swing(TransferFunction::Gamma22) - 79.8).abs() < 1e-5);
    }

    #[test]
    fn preset_names_round_trip() {
        for (name, desc) in ImageDescription::NAMED {
            assert_eq!(ImageDescription::from_name(name), Some(desc));
            assert_eq!(desc.name(), Some(name));
        }
        let custom = ImageDescription::SRGB_GAMMA22.with_luminance(Luminance {
            min: 0.0,
            max: 200.0,
            reference: 100.0,
        });
        assert_eq!(custom.name(), None);
    }

    #[test]
    fn alpha_mode_names_round_trip() {
        for (name, mode) in AlphaMode::NAMED {
            assert_eq!(AlphaMode::from_name(name), Some(mode));
            assert_eq!(mode.name(), name);
        }
    }
}
