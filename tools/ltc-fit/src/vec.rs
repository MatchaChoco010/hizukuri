//! The little linear algebra the fit needs, in `f64`.

pub type V3 = [f64; 3];
/// A 3×3 matrix as rows.
pub type M3 = [[f64; 3]; 3];

pub fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

pub fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub fn scale(a: V3, s: f64) -> V3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

pub fn length(a: V3) -> f64 {
    dot(a, a).sqrt()
}

pub fn normalize(a: V3) -> V3 {
    scale(a, 1.0 / length(a))
}

pub fn mul(m: &M3, v: V3) -> V3 {
    [dot(m[0], v), dot(m[1], v), dot(m[2], v)]
}

pub fn mul_mm(a: &M3, b: &M3) -> M3 {
    let mut out = [[0.0; 3]; 3];
    for (r, row) in out.iter_mut().enumerate() {
        for (c, value) in row.iter_mut().enumerate() {
            *value = (0..3).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    out
}

pub fn det(m: &M3) -> f64 {
    dot(m[0], cross(m[1], m[2]))
}

pub fn inverse(m: &M3) -> M3 {
    let d = det(m);
    let c0 = cross(m[1], m[2]);
    let c1 = cross(m[2], m[0]);
    let c2 = cross(m[0], m[1]);
    [
        [c0[0] / d, c1[0] / d, c2[0] / d],
        [c0[1] / d, c1[1] / d, c2[1] / d],
        [c0[2] / d, c1[2] / d, c2[2] / d],
    ]
}
