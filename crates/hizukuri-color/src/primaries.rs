//! Chromaticities, RGB primaries and the matrices that convert between them.

use crate::math::Mat3;

/// CIE 1931 xy chromaticity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chromaticity {
    pub x: f64,
    pub y: f64,
}

impl Chromaticity {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// CIE standard illuminant D65.
    pub const D65: Chromaticity = Chromaticity::new(0.3127, 0.3290);

    /// XYZ with Y normalized to 1.
    fn to_xyz(self) -> [f64; 3] {
        [self.x / self.y, 1.0, (1.0 - self.x - self.y) / self.y]
    }
}

/// RGB primaries and white point of a color space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Primaries {
    pub red: Chromaticity,
    pub green: Chromaticity,
    pub blue: Chromaticity,
    pub white: Chromaticity,
}

impl Primaries {
    /// sRGB / BT.709.
    pub const SRGB: Primaries = Primaries {
        red: Chromaticity::new(0.64, 0.33),
        green: Chromaticity::new(0.30, 0.60),
        blue: Chromaticity::new(0.15, 0.06),
        white: Chromaticity::D65,
    };

    /// Display P3 (DCI-P3 primaries with a D65 white point).
    pub const DISPLAY_P3: Primaries = Primaries {
        red: Chromaticity::new(0.680, 0.320),
        green: Chromaticity::new(0.265, 0.690),
        blue: Chromaticity::new(0.150, 0.060),
        white: Chromaticity::D65,
    };

    /// BT.2020 / BT.2100.
    pub const BT2020: Primaries = Primaries {
        red: Chromaticity::new(0.708, 0.292),
        green: Chromaticity::new(0.170, 0.797),
        blue: Chromaticity::new(0.131, 0.046),
        white: Chromaticity::D65,
    };

    /// Adobe RGB (1998).
    pub const ADOBE_RGB: Primaries = Primaries {
        red: Chromaticity::new(0.64, 0.33),
        green: Chromaticity::new(0.21, 0.71),
        blue: Chromaticity::new(0.15, 0.06),
        white: Chromaticity::D65,
    };

    /// Named primaries known to this crate, with the names used on the
    /// command line and in state dumps.
    pub const NAMED: [(&'static str, Primaries); 4] = [
        ("srgb", Primaries::SRGB),
        ("display-p3", Primaries::DISPLAY_P3),
        ("bt2020", Primaries::BT2020),
        ("adobe-rgb", Primaries::ADOBE_RGB),
    ];

    /// Returns the name of these primaries when they match a named set.
    pub fn name(&self) -> Option<&'static str> {
        Self::NAMED
            .iter()
            .find(|(_, p)| p == self)
            .map(|(name, _)| *name)
    }

    pub fn from_name(name: &str) -> Option<Primaries> {
        Self::NAMED
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, p)| *p)
    }

    /// Matrix that converts linear RGB in these primaries to CIE XYZ
    /// (Y of the white point is 1).
    ///
    /// Returns `None` when the primaries are degenerate (collinear, or a
    /// chromaticity with `y == 0`).
    pub fn rgb_to_xyz(&self) -> Option<Mat3> {
        if [self.red, self.green, self.blue, self.white]
            .iter()
            .any(|c| c.y == 0.0)
        {
            return None;
        }
        let r = self.red.to_xyz();
        let g = self.green.to_xyz();
        let b = self.blue.to_xyz();
        let m = Mat3([[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]]);
        let s = m.inverse()?.mul_vec(self.white.to_xyz());
        Some(m * Mat3::from_diagonal(s))
    }

    /// Matrix that converts CIE XYZ to linear RGB in these primaries.
    pub fn xyz_to_rgb(&self) -> Option<Mat3> {
        self.rgb_to_xyz()?.inverse()
    }

    /// Matrix that converts linear RGB in `self` to linear RGB in `to`.
    ///
    /// When the white points differ, a Bradford chromatic adaptation is
    /// inserted so that the source white maps to the destination white.
    pub fn conversion_to(&self, to: &Primaries) -> Option<Mat3> {
        if self == to {
            return Some(Mat3::IDENTITY);
        }
        let src = self.rgb_to_xyz()?;
        let dst = to.xyz_to_rgb()?;
        if self.white == to.white {
            Some(dst * src)
        } else {
            Some(dst * bradford_adaptation(self.white, to.white)? * src)
        }
    }
}

/// Bradford chromatic adaptation matrix (XYZ → XYZ) from `from` to `to`.
fn bradford_adaptation(from: Chromaticity, to: Chromaticity) -> Option<Mat3> {
    const BRADFORD: Mat3 = Mat3([
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ]);
    let src = BRADFORD.mul_vec(from.to_xyz());
    let dst = BRADFORD.mul_vec(to.to_xyz());
    if src.contains(&0.0) {
        return None;
    }
    let scale = Mat3::from_diagonal([dst[0] / src[0], dst[1] / src[1], dst[2] / src[2]]);
    Some(BRADFORD.inverse()? * scale * BRADFORD)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_mat_close(actual: &Mat3, expected: [[f64; 3]; 3], tolerance: f64) {
        for (actual_row, expected_row) in actual.0.iter().zip(&expected) {
            for (a, e) in actual_row.iter().zip(expected_row) {
                assert!((a - e).abs() < tolerance, "{a} vs {e} in {actual:?}");
            }
        }
    }

    // Reference matrices: the RGB → XYZ matrices listed in CSS Color Module
    // Level 4, "Sample code for Color Conversions" (given there as exact
    // rationals derived from the same xy chromaticities; D65 = 0.3127, 0.3290).
    #[test]
    fn srgb_to_xyz_matches_reference() {
        let m = Primaries::SRGB.rgb_to_xyz().unwrap();
        assert_mat_close(
            &m,
            [
                [0.4123908, 0.3575843, 0.1804808],
                [0.2126390, 0.7151687, 0.0721923],
                [0.0193308, 0.1191948, 0.9505322],
            ],
            1e-6,
        );
    }

    #[test]
    fn display_p3_to_xyz_matches_reference() {
        let m = Primaries::DISPLAY_P3.rgb_to_xyz().unwrap();
        assert_mat_close(
            &m,
            [
                [0.4865709, 0.2656677, 0.1982173],
                [0.2289746, 0.6917385, 0.0792869],
                [0.0000000, 0.0451134, 1.0439444],
            ],
            1e-6,
        );
    }

    #[test]
    fn bt2020_to_xyz_matches_reference() {
        let m = Primaries::BT2020.rgb_to_xyz().unwrap();
        assert_mat_close(
            &m,
            [
                [0.6369580, 0.1446169, 0.1688810],
                [0.2627002, 0.6779981, 0.0593017],
                [0.0000000, 0.0280727, 1.0609851],
            ],
            1e-6,
        );
    }

    // Rec. ITU-R BT.2087-0, matrix M2 (BT.709 → BT.2020), given to 4 decimals.
    #[test]
    fn bt709_to_bt2020_matches_bt2087() {
        let m = Primaries::SRGB.conversion_to(&Primaries::BT2020).unwrap();
        assert_mat_close(
            &m,
            [
                [0.6274, 0.3293, 0.0433],
                [0.0691, 0.9195, 0.0114],
                [0.0164, 0.0880, 0.8956],
            ],
            5e-5,
        );
    }

    #[test]
    fn conversion_round_trips() {
        for (_, a) in Primaries::NAMED {
            for (_, b) in Primaries::NAMED {
                let m = b.conversion_to(&a).unwrap() * a.conversion_to(&b).unwrap();
                assert_mat_close(&m, Mat3::IDENTITY.0, 1e-12);
            }
        }
    }

    #[test]
    fn white_maps_to_white() {
        for (_, a) in Primaries::NAMED {
            for (_, b) in Primaries::NAMED {
                let w = a.conversion_to(&b).unwrap().mul_vec([1.0, 1.0, 1.0]);
                for v in w {
                    assert!((v - 1.0).abs() < 1e-12, "{w:?}");
                }
            }
        }
    }

    #[test]
    fn bradford_maps_source_white_to_destination_white() {
        // DCI-P3 theatrical white to D65.
        let dci = Primaries {
            white: Chromaticity::new(0.314, 0.351),
            ..Primaries::DISPLAY_P3
        };
        let w = dci
            .conversion_to(&Primaries::SRGB)
            .unwrap()
            .mul_vec([1.0, 1.0, 1.0]);
        for v in w {
            assert!((v - 1.0).abs() < 1e-9, "{w:?}");
        }
    }

    #[test]
    fn names_round_trip() {
        for (name, p) in Primaries::NAMED {
            assert_eq!(p.name(), Some(name));
            assert_eq!(Primaries::from_name(name), Some(p));
        }
        assert_eq!(Primaries::from_name("nope"), None);
    }

    #[test]
    fn degenerate_primaries_are_rejected() {
        let p = Primaries {
            red: Chromaticity::new(0.3, 0.3),
            green: Chromaticity::new(0.3, 0.3),
            blue: Chromaticity::new(0.3, 0.3),
            white: Chromaticity::D65,
        };
        assert!(p.rgb_to_xyz().is_none());
    }
}
