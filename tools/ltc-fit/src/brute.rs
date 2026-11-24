//! The reference the tables' method is measured against: the light of a
//! quad on a surface point, integrated by brute force over the quad's area
//! with the BRDF itself.

use std::f64::consts::PI;

use crate::ggx;
use crate::shade::{Box3, Quad};
use crate::vec::{M3, V3, add, cross, dot, length, mul, scale, sub};

/// The integrals over the light, per colour channel of its radiance.
#[derive(Copy, Clone, Debug, Default)]
pub struct Reference {
    /// ∫ L cos θ / π: what a Lambert surface of albedo 1 reflects.
    pub diffuse: [f64; 3],
    /// ∫ L × GGX × cos θ with F = 1.
    pub specular: [f64; 3],
    /// The same, weighted by Schlick's (1 − v·h)^5.
    pub specular_fresnel: [f64; 3],
}

/// Integrates with `n` × `n` stratified points on the quad. `frame` has the
/// shading frame as rows (as [`crate::shade::Surface::frame`]); `radiance`
/// gives the light's colour at a texture coordinate.
pub fn integrate(
    position: V3,
    frame: &M3,
    view: V3,
    roughness: f64,
    quad: &Quad,
    radiance: &dyn Fn(f64, f64) -> [f64; 3],
    n: usize,
) -> Reference {
    integrate_occluded(position, frame, view, roughness, quad, radiance, &[], n)
}

/// [`integrate`] with boxes in the way: a point of the light counts when no
/// box is between it and the shaded point.
#[allow(clippy::too_many_arguments)]
pub fn integrate_occluded(
    position: V3,
    frame: &M3,
    view: V3,
    roughness: f64,
    quad: &Quad,
    radiance: &dyn Fn(f64, f64) -> [f64; 3],
    occluders: &[Box3],
    n: usize,
) -> Reference {
    let alpha = roughness * roughness;
    let v = mul(frame, view);
    let e1 = sub(quad.p[1], quad.p[0]);
    let e2 = sub(quad.p[3], quad.p[0]);
    let area = length(cross(e1, e2));
    let front = quad.front();
    let mut out = Reference::default();
    for a in 0..n {
        for b in 0..n {
            let s = (a as f64 + 0.5) / n as f64;
            let t = (b as f64 + 0.5) / n as f64;
            let point = add(quad.p[0], add(scale(e1, s), scale(e2, t)));
            let to_light = sub(point, position);
            let distance2 = dot(to_light, to_light);
            let l_world = scale(to_light, 1.0 / distance2.sqrt());
            // The cosine at the light: positive when the point is in front.
            let mut cos_light = -dot(front, l_world);
            if quad.two_sided {
                cos_light = cos_light.abs();
            }
            let l = mul(frame, l_world);
            if cos_light <= 0.0 || l[2] <= 0.0 {
                continue;
            }
            if occluders.iter().any(|b| b.blocks(position, point)) {
                continue;
            }
            let solid_angle = cos_light * area / distance2 / (n * n) as f64;
            let brdf = ggx::eval(v, l, alpha);
            let colour = radiance(s, t);
            for (c, colour) in colour.into_iter().enumerate() {
                out.diffuse[c] += colour * l[2] / PI * solid_angle;
                out.specular[c] += colour * brdf.value * solid_angle;
                out.specular_fresnel[c] += colour * brdf.value * brdf.schlick * solid_angle;
            }
        }
    }
    out
}
