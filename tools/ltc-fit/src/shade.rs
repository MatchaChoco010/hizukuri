//! Shading a point lit by a quad light with the tables, on the CPU, step for
//! step as the Slang module `ltc` does it: what the shader's output is
//! compared with, and where the method's own error (against [`crate::brute`])
//! is measured without a GPU.

use std::f64::consts::PI;

use crate::reads::LIFT_COSINE_FLOOR;
use crate::table::Tables;
use crate::vec::{M3, V3, add, cross, dot, length, mul, normalize, scale, sub};

/// A quad light. The corners go around it: `p[0]` is the texture's (0, 0),
/// `p[1]` its (1, 0), `p[2]` its (1, 1), `p[3]` its (0, 1). It is a
/// parallelogram. Its front is the side that sees the corners clockwise
/// (the texture unmirrored, v pointing down).
#[derive(Copy, Clone, Debug)]
pub struct Quad {
    pub p: [V3; 4],
    pub two_sided: bool,
}

impl Quad {
    /// The normal of the front side.
    pub fn front(&self) -> V3 {
        normalize(cross(sub(self.p[3], self.p[0]), sub(self.p[1], self.p[0])))
    }
}

/// How the part of the light below the horizon is left out.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Horizon {
    /// The polygon is clipped at the horizon.
    Clip,
    /// The polygon's vector form factor is taken for a sphere cap's, and
    /// the cap's clipped form factor is looked up.
    Sphere,
}

/// What depends on the shaded point only: the frame and the fitted matrix.
#[derive(Copy, Clone, Debug)]
pub struct Surface {
    pub position: V3,
    /// Rows: the view direction projected onto the surface, the direction
    /// across it, the normal.
    pub frame: M3,
    pub inv: M3,
    pub norm: f64,
    pub fresnel: f64,
}

pub fn surface(tables: &Tables, position: V3, normal: V3, view: V3, roughness: f64) -> Surface {
    let cos_theta = dot(normal, view).clamp(0.0, 1.0);
    let tangent = sub(view, scale(normal, dot(normal, view)));
    let t1 = if dot(tangent, tangent) > 1e-12 {
        normalize(tangent)
    } else {
        // The view is along the normal: any direction on the surface.
        let other = if normal[0].abs() < 0.9 {
            [1.0, 0.0, 0.0]
        } else {
            [0.0, 1.0, 0.0]
        };
        normalize(cross(other, normal))
    };
    let t2 = cross(normal, t1);
    let t = tables.mat_at(roughness, cos_theta);
    let (norm, fresnel) = tables.amp_at(roughness, cos_theta);
    Surface {
        position,
        frame: [t1, t2, normal],
        inv: [[t[0], 0.0, t[1]], [0.0, 1.0, 0.0], [t[2], 0.0, t[3]]],
        norm,
        fresnel,
    }
}

/// θ / sin θ for the angle whose cosine is `x`: with it, the integral along
/// an edge between the unit vectors a and b is this times a × b, and needs
/// no normalization of the cross product.
pub fn theta_over_sin(x: f64) -> f64 {
    let e = 1.0 - x;
    if e < 0.02 {
        // The series about θ = 0, where acos loses its digits.
        1.0 + e * (1.0 / 3.0 + e * (2.0 / 15.0 + e * (2.0 / 35.0)))
    } else {
        let x = x.max(-0.9999);
        x.acos() / (1.0 - x * x).sqrt()
    }
}

/// The sum of the edge integrals of a polygon of unit vectors, over 2π: its
/// vector form factor, pointing at the polygon when the origin sees the
/// vertices clockwise.
pub fn edge_sum(vertices: &[V3]) -> V3 {
    let mut sum = [0.0; 3];
    for (k, &a) in vertices.iter().enumerate() {
        let b = vertices[(k + 1) % vertices.len()];
        sum = add(sum, scale(cross(a, b), theta_over_sin(dot(a, b))));
    }
    scale(sum, 1.0 / (2.0 * PI))
}

/// The part of a polygon above the horizon (z ≥ 0).
fn clip(polygon: &[V3]) -> Vec<V3> {
    let mut out = Vec::with_capacity(polygon.len() + 1);
    for (k, &a) in polygon.iter().enumerate() {
        let b = polygon[(k + 1) % polygon.len()];
        if a[2] >= 0.0 {
            out.push(a);
        }
        if (a[2] >= 0.0) != (b[2] >= 0.0) {
            let t = a[2] / (a[2] - b[2]);
            out.push(add(a, scale(sub(b, a), t)));
        }
    }
    out
}

/// One lobe's share of the light: how much of the lobe the light covers,
/// and where on the light, how blurred, its colour is read.
#[derive(Copy, Clone, Debug)]
pub struct Lobe {
    /// The integral of the lobe's distribution over the light: 0 to 1.
    pub form_factor: f64,
    /// Where the light's texture is read.
    pub uv: [f64; 2],
    /// The standard deviation of the blur it is read with, in units of the
    /// square root of the light's area.
    pub blur: f64,
    /// The same along the texture's u and v, in texture coordinates.
    pub spread: [f64; 2],
}

impl Lobe {
    /// What a point the light does not light gets, and what a lobe starts
    /// from.
    fn none(form_factor: f64) -> Self {
        Self {
            form_factor,
            uv: [0.5, 0.5],
            blur: 1.0,
            spread: [1.0, 1.0],
        }
    }
}

/// The blur's standard deviation over the distance to the point of the
/// light that is read. A clamped cosine lobe falls on a plane facing it
/// with the weight 1 / (1 + r²/d²)², which a Gaussian of 0.55 d matches at
/// half its height; the weight's tail is heavier, which the factor leans
/// towards.
pub const BLUR_PER_DISTANCE: f64 = 0.6;

/// `corners`: the light's corners seen from the shaded point, in a space
/// where the lobe is the clamped cosine about +z. `facing` is 1 when the
/// point sees the corners clockwise and −1 otherwise.
fn lobe(tables: &Tables, corners: [V3; 4], facing: f64, horizon: Horizon) -> Lobe {
    // The light's vector form factor: it points at the light.
    let f = scale(edge_sum(&corners.map(normalize)), facing);
    let len = length(f);
    let form_factor = match horizon {
        Horizon::Clip => {
            let clipped: Vec<V3> = clip(&corners).into_iter().map(normalize).collect();
            if clipped.len() < 3 {
                0.0
            } else {
                (edge_sum(&clipped)[2] * facing).max(0.0)
            }
        }
        Horizon::Sphere => {
            if len > 0.0 {
                len * tables.clipped_sphere_at(f[2] / len, len)
            } else {
                0.0
            }
        }
    };

    // The light is read where the mean direction of the lobe's weight over
    // it meets it: the vector form factor's direction, lifted towards the
    // lobe's axis (`reads::lift`).
    let e1 = sub(corners[1], corners[0]);
    let e2 = sub(corners[3], corners[0]);
    let raw = cross(e1, e2);
    let area = length(raw);
    let mean = scale(f, 1.0 / len.max(1e-30));
    let across = sub([0.0, 0.0, 1.0], scale(mean, mean[2]));
    let direction = add(
        mean,
        scale(across, len / (4.0 * mean[2].max(LIFT_COSINE_FLOOR))),
    );
    let along = dot(raw, direction);
    let t = if along.abs() > 1e-30 {
        dot(raw, corners[0]) / along
    } else {
        0.0
    };
    if t <= 0.0 {
        // No such point: the light as a whole.
        return Lobe::none(form_factor);
    }
    let point = scale(direction, t);
    let w = sub(point, corners[0]);
    let uv = [
        dot(cross(w, e2), raw) / (area * area),
        dot(cross(e1, w), raw) / (area * area),
    ];
    let sigma = BLUR_PER_DISTANCE * length(point);
    Lobe {
        form_factor,
        uv,
        blur: sigma / area.sqrt(),
        spread: [sigma / length(e1), sigma / length(e2)],
    }
}

/// A box that keeps a light's light from a point: its centre and its three
/// half extents (vectors at right angles).
#[derive(Copy, Clone, Debug)]
pub struct Box3 {
    pub center: V3,
    pub half: [V3; 3],
}

impl Box3 {
    /// Whether the segment from `from` to `to` passes through the box.
    pub fn blocks(&self, from: V3, to: V3) -> bool {
        let (mut enter, mut leave) = (0.0_f64, 1.0_f64);
        for axis in self.half {
            let scale = 1.0 / dot(axis, axis);
            let origin = dot(sub(from, self.center), axis) * scale;
            let direction = dot(sub(to, from), axis) * scale;
            if direction.abs() < 1e-12 {
                if origin.abs() > 1.0 {
                    return false;
                }
                continue;
            }
            let (a, b) = ((-1.0 - origin) / direction, (1.0 - origin) / direction);
            enter = enter.max(a.min(b));
            leave = leave.min(a.max(b));
        }
        enter < leave
    }
}

/// The rectangle of the light that a box hides from the shaded point, in
/// texture coordinates: (least u, least v, greatest u, greatest v). It
/// bounds the box's outline. `None` when the box hides nothing.
pub fn box_cover(surface: &Surface, quad: &Quad, occluder: &Box3) -> Option<[f64; 4]> {
    let origin = sub(quad.p[0], surface.position);
    let e1 = sub(quad.p[1], quad.p[0]);
    let e2 = sub(quad.p[3], quad.p[0]);
    let raw = cross(e1, e2);
    let mut towards = normalize(raw);
    let mut distance = dot(towards, origin);
    if distance < 0.0 {
        towards = scale(towards, -1.0);
        distance = -distance;
    }
    let normal = surface.frame[2];
    let above_from = 1e-3 * occluder.half.iter().map(|h| length(*h)).sum::<f64>();
    let centre = sub(occluder.center, surface.position);
    let mut cover = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    let (mut above, mut front, mut beyond) = (false, false, true);
    for corner in 0..8 {
        let mut rel = centre;
        for (axis, half) in occluder.half.iter().enumerate() {
            let sign = if corner >> axis & 1 == 1 { 1.0 } else { -1.0 };
            rel = add(rel, scale(*half, sign));
        }
        let height = dot(normal, rel);
        above |= height > above_from;
        // What is below the surface is taken up to it.
        rel = sub(rel, scale(normal, height.min(0.0)));
        let depth = dot(towards, rel);
        front |= depth > 1e-3 * distance;
        beyond &= depth >= distance;
        let w = sub(scale(rel, distance / depth.max(1e-3 * distance)), origin);
        let uv = [
            dot(cross(w, e2), raw) / dot(raw, raw),
            dot(cross(e1, w), raw) / dot(raw, raw),
        ];
        for k in 0..2 {
            cover[k] = cover[k].min(uv[k]);
            cover[k + 2] = cover[k + 2].max(uv[k]);
        }
    }
    (above && front && !beyond).then_some(cover)
}

/// Where the cubic step that stands for the normal distribution's
/// cumulative function reaches 0 and 1, in standard deviations.
pub const CUMULATIVE_REACH: f64 = 2.0;

/// The part of a lobe's weight on the light that a cover leaves: the weight
/// is taken for a Gaussian about the point where the lobe reads the light.
pub fn uncovered(lobe: &Lobe, cover: Option<[f64; 4]>) -> f64 {
    let Some(cover) = cover else {
        return 1.0;
    };
    // The normal distribution's cumulative function, by a cubic step: no
    // exponential, and within 0.03 of it.
    let cumulative = |x: f64| {
        let t = (x / (2.0 * CUMULATIVE_REACH) + 0.5).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    let mut part = 1.0;
    for k in 0..2 {
        let (least, greatest) = (cover[k].clamp(0.0, 1.0), cover[k + 2].clamp(0.0, 1.0));
        if least >= greatest {
            return 1.0;
        }
        let centre = lobe.uv[k].clamp(0.0, 1.0);
        let sigma = lobe.spread[k].max(1e-3);
        let hidden = cumulative((greatest - centre) / sigma) - cumulative((least - centre) / sigma);
        let whole = cumulative((1.0 - centre) / sigma) - cumulative(-centre / sigma);
        part *= hidden / whole.max(1e-4);
    }
    1.0 - part.clamp(0.0, 1.0)
}

/// The light's corners seen from the shaded point, in the space of the
/// diffuse lobe and in that of the specular lobe (each the clamped cosine
/// about +z there), and 1 when the point sees the corners clockwise, −1
/// when counterclockwise. `None` when the light does not light the point.
pub fn corners(surface: &Surface, quad: &Quad) -> Option<([V3; 4], [V3; 4], f64)> {
    let side = dot(quad.front(), sub(surface.position, quad.p[0]));
    if side <= 0.0 && !quad.two_sided {
        return None;
    }
    let local = quad
        .p
        .map(|corner| mul(&surface.frame, sub(corner, surface.position)));
    Some((
        local,
        local.map(|corner| mul(&surface.inv, corner)),
        if side > 0.0 { 1.0 } else { -1.0 },
    ))
}

/// The diffuse and the specular lobe's share of the light, in this order.
pub fn shade(tables: &Tables, surface: &Surface, quad: &Quad, horizon: Horizon) -> (Lobe, Lobe) {
    let Some((diffuse, specular, facing)) = corners(surface, quad) else {
        return (Lobe::none(0.0), Lobe::none(0.0));
    };
    (
        lobe(tables, diffuse, facing, horizon),
        lobe(tables, specular, facing, horizon),
    )
}
