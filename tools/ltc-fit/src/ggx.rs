//! The BRDF the tables are fitted to: the isotropic GGX microfacet model
//! with the height-correlated Smith masking-shadowing term, and no Fresnel
//! term (the tables carry what a shader needs to put Schlick's term back).
//!
//! Directions are in the shading frame: the normal is +z.

use std::f64::consts::PI;

use crate::vec::{V3, add, dot, normalize, scale, sub};

/// Smith's Λ for a direction with `cos_theta` to the normal.
fn lambda(alpha: f64, cos_theta: f64) -> f64 {
    let c2 = cos_theta * cos_theta;
    let tan2 = (1.0 - c2).max(0.0) / c2;
    0.5 * ((1.0 + alpha * alpha * tan2).sqrt() - 1.0)
}

/// The distribution of normals.
fn ndf(alpha: f64, h: V3) -> f64 {
    let a2 = alpha * alpha;
    let t = h[2] * h[2] * (a2 - 1.0) + 1.0;
    a2 / (PI * t * t)
}

/// What [`eval`] returns for one light direction.
#[derive(Copy, Clone, Debug, Default)]
pub struct Eval {
    /// BRDF × cos θ of the light direction.
    pub value: f64,
    /// The density with which [`sample`] draws this direction.
    pub pdf: f64,
    /// (1 − v·h)^5, the weight of Schlick's Fresnel term.
    pub schlick: f64,
}

/// Evaluates the BRDF times the cosine for the view direction `v` and the
/// light direction `l`.
pub fn eval(v: V3, l: V3, alpha: f64) -> Eval {
    let h = add(v, l);
    if v[2] <= 0.0 || dot(h, h) <= 0.0 {
        return Eval::default();
    }
    let h = normalize(h);
    if h[2] <= 0.0 {
        return Eval::default();
    }
    let d = ndf(alpha, h);
    let lambda_v = lambda(alpha, v[2]);
    // `sample` draws visible normals: G1(v) D(h) max(0, v·h) / v.z over the
    // half vectors, which is G1(v) D(h) / (4 v.z) over the light directions.
    let pdf = d / ((1.0 + lambda_v) * 4.0 * v[2]);
    if l[2] <= 0.0 {
        return Eval {
            value: 0.0,
            pdf,
            schlick: 0.0,
        };
    }
    let g2 = 1.0 / (1.0 + lambda_v + lambda(alpha, l[2]));
    Eval {
        value: d * g2 / (4.0 * v[2]),
        pdf,
        schlick: (1.0 - dot(v, h)).clamp(0.0, 1.0).powi(5),
    }
}

/// Draws a light direction by reflecting `v` about a visible normal
/// (`u1`, `u2` in [0, 1)). The direction can be below the horizon.
pub fn sample(v: V3, alpha: f64, u1: f64, u2: f64) -> V3 {
    // The visible normals of the stretched configuration are a spherical
    // cap of reflection directions.
    let vh = normalize([alpha * v[0], alpha * v[1], v[2]]);
    let phi = 2.0 * PI * u1;
    let z = (1.0 - u2) * (1.0 + vh[2]) - vh[2];
    let s = (1.0 - z * z).max(0.0).sqrt();
    let h = add([s * phi.cos(), s * phi.sin(), z], vh);
    let m = normalize([alpha * h[0], alpha * h[1], h[2].max(1e-300)]);
    sub(scale(m, 2.0 * dot(v, m)), v)
}
