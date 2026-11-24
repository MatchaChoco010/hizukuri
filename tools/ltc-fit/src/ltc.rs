//! A linearly transformed cosine: the clamped cosine distribution on the
//! hemisphere, with its directions transformed by a 3×3 matrix `M` and
//! normalized again (Heitz, Dupuy, Hill and Neubelt, "Real-Time
//! Polygonal-Light Shading with Linearly Transformed Cosines", 2016).

use std::f64::consts::PI;

use crate::vec::{M3, V3, det, inverse, length, mul, mul_mm, normalize};

/// The three numbers the fit searches for one view angle and roughness.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Params {
    /// The scale along the plane of incidence.
    pub m11: f64,
    /// The scale across it.
    pub m22: f64,
    /// The shear that skews the lobe within the plane of incidence.
    pub m13: f64,
}

#[derive(Copy, Clone, Debug)]
pub struct Ltc {
    pub m: M3,
    pub inv: M3,
    det_inv: f64,
}

impl Ltc {
    pub fn new(m: M3) -> Self {
        let inv = inverse(&m);
        Self {
            m,
            inv,
            det_inv: det(&inv).abs(),
        }
    }

    /// `M = R S`: `R` turns +z to `axis` (a direction in the xz plane, the
    /// plane of incidence), and `S` scales x and y and shears z into x.
    pub fn from_params(axis: V3, p: Params) -> Self {
        let (zx, zz) = (axis[0], axis[2]);
        let r = [[zz, 0.0, zx], [0.0, 1.0, 0.0], [-zx, 0.0, zz]];
        let s = [[p.m11, 0.0, p.m13], [0.0, p.m22, 0.0], [0.0, 0.0, 1.0]];
        Self::new(mul_mm(&r, &s))
    }

    /// The density of the distribution at the direction `l`.
    pub fn eval(&self, l: V3) -> f64 {
        let lo = mul(&self.inv, l);
        let len = length(lo);
        let z = lo[2] / len;
        if z <= 0.0 {
            return 0.0;
        }
        // The clamped cosine at the original direction, times the change of
        // solid angle: |M⁻¹| / ‖M⁻¹ l‖³.
        z / PI * self.det_inv / (len * len * len)
    }

    /// Draws a direction with the density [`Self::eval`].
    pub fn sample(&self, u1: f64, u2: f64) -> V3 {
        let r = u1.sqrt();
        let phi = 2.0 * PI * u2;
        let wo = [r * phi.cos(), r * phi.sin(), (1.0 - u1).max(0.0).sqrt()];
        normalize(mul(&self.m, wo))
    }
}
