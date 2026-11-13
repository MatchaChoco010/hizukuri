//! Where on a light a lobe's weight is, measured: the mean position and the
//! spread of the clamped cosine's weight over the light's quad, in texture
//! coordinates. The shader reads the light's texture once per lobe, at one
//! point and with one blur; this is what that read stands in for.

use crate::shade::edge_sum;
use crate::vec::{V3, add, cross, dot, length, normalize, scale, sub};

/// The first and second moments of the lobe's weight over the quad.
#[derive(Copy, Clone, Debug)]
pub struct Moments {
    /// The mean texture coordinate.
    pub mean: [f64; 2],
    /// The standard deviations along u and v.
    pub deviation: [f64; 2],
}

impl Moments {
    /// One number for the spread: the geometric mean of the two standard
    /// deviations, in units of the square root of the light's area.
    pub fn blur(&self) -> f64 {
        (self.deviation[0] * self.deviation[1]).sqrt()
    }
}

/// Integrates the weight with `n` × `n` points of the quad. `corners` are in
/// the space where the lobe is the clamped cosine about +z. `None` when no
/// weight falls on the quad.
pub fn moments(corners: [V3; 4], n: usize) -> Option<Moments> {
    let e1 = sub(corners[1], corners[0]);
    let e2 = sub(corners[3], corners[0]);
    let normal = normalize(cross(e1, e2));
    let (mut total, mut mean, mut square) = (0.0, [0.0; 2], [0.0; 2]);
    for a in 0..n {
        for b in 0..n {
            let uv = [(a as f64 + 0.5) / n as f64, (b as f64 + 0.5) / n as f64];
            let p = add(corners[0], add(scale(e1, uv[0]), scale(e2, uv[1])));
            let distance = length(p);
            // The cosine at the lobe, the cosine at the light, over the
            // squared distance: the solid angle of a patch times the lobe.
            let weight = (p[2] / distance).max(0.0) * (dot(normal, p) / distance).abs()
                / (distance * distance);
            total += weight;
            for k in 0..2 {
                mean[k] += weight * uv[k];
                square[k] += weight * uv[k] * uv[k];
            }
        }
    }
    if total <= 0.0 {
        return None;
    }
    let mean = mean.map(|m| m / total);
    Some(Moments {
        mean,
        deviation: std::array::from_fn(|k| (square[k] / total - mean[k] * mean[k]).max(0.0).sqrt()),
    })
}

/// The texture coordinate where the ray from the origin along `direction`
/// meets the quad's plane, and the distance to that point.
pub fn along(corners: [V3; 4], direction: V3) -> ([f64; 2], f64) {
    let e1 = sub(corners[1], corners[0]);
    let e2 = sub(corners[3], corners[0]);
    let raw = cross(e1, e2);
    let t = dot(raw, corners[0]) / dot(raw, direction);
    let point = scale(direction, t);
    let w = sub(point, corners[0]);
    let area2 = dot(raw, raw);
    (
        [
            dot(cross(w, e2), raw) / area2,
            dot(cross(e1, w), raw) / area2,
        ],
        length(point),
    )
}

/// The names of the directions [`candidates`] returns.
pub const CANDIDATES: [&str; 4] = ["perpendicular", "peak", "mean direction", "lifted mean"];

/// Candidate directions for the one read of a lobe:
///
/// - the foot of the perpendicular from the origin to the light's plane;
/// - where the lobe's weight on the whole plane is greatest;
/// - the direction of the light's vector form factor, the mean direction of
///   the light's solid angle;
/// - that direction lifted towards the lobe's axis by [`lift`]: the lobe
///   weighs the light's directions by their cosine to its axis.
pub fn candidates(corners: [V3; 4], facing: f64) -> [V3; 4] {
    let e1 = sub(corners[1], corners[0]);
    let e2 = sub(corners[3], corners[0]);
    let mut n = normalize(cross(e1, e2));
    if dot(n, corners[0]) < 0.0 {
        n = scale(n, -1.0);
    }
    let c = n[2];
    let s = (1.0 - c * c).max(0.0).sqrt();
    let tan_a = (s / (2.0 * c + (3.0 + c * c).sqrt()).max(1e-3)).min(4.0);
    let towards_axis = if s > 1e-6 {
        scale(sub([0.0, 0.0, 1.0], scale(n, c)), 1.0 / s)
    } else {
        [0.0; 3]
    };
    let f = scale(edge_sum(&corners.map(normalize)), facing);
    [
        n,
        normalize(add(n, scale(towards_axis, tan_a))),
        normalize(f),
        lift(f),
    ]
}

/// The mean direction of a light, weighted by the cosine to +z, from its
/// vector form factor `f`: to first order, the unweighted mean direction
/// moved towards +z by the variance of the light's directions over the
/// mean's cosine. A light of the solid angle Ω has about Ω / 4π of variance
/// per axis, and the vector form factor's length is about Ω / π. Near the
/// horizon the first order overshoots: the cosine is not taken below
/// [`LIFT_COSINE_FLOOR`].
pub fn lift(f: V3) -> V3 {
    let len = length(f);
    let mean = scale(f, 1.0 / len);
    let across = sub([0.0, 0.0, 1.0], scale(mean, mean[2]));
    let amount = len / (4.0 * mean[2].max(LIFT_COSINE_FLOOR));
    normalize(add(mean, scale(across, amount)))
}

/// See [`lift`]. With 0.3 instead, the diffuse lobe's reads are slightly
/// nearer to the weight's mean and the specular lobe's reads are farther,
/// most where the light is in the lobe's tail.
pub const LIFT_COSINE_FLOOR: f64 = 0.6;
