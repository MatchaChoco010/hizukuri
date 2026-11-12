//! Small 3x3 matrix type. Derivations are done in `f64`; the result is
//! converted to `f32` for use in the pipeline (the shaders work in `f32`).

use std::ops::Mul;

/// Row-major 3x3 matrix of `f64`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat3(pub [[f64; 3]; 3]);

impl Mat3 {
    pub const IDENTITY: Mat3 = Mat3([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);

    pub fn from_diagonal(d: [f64; 3]) -> Mat3 {
        Mat3([[d[0], 0.0, 0.0], [0.0, d[1], 0.0], [0.0, 0.0, d[2]]])
    }

    pub fn determinant(&self) -> f64 {
        let m = &self.0;
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    }

    /// Returns the inverse, or `None` when the matrix is singular.
    pub fn inverse(&self) -> Option<Mat3> {
        let det = self.determinant();
        if det.abs() < 1e-12 {
            return None;
        }
        let m = &self.0;
        let inv_det = 1.0 / det;
        let cofactor = |r0: usize, r1: usize, c0: usize, c1: usize| {
            (m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0]) * inv_det
        };
        Some(Mat3([
            [
                cofactor(1, 2, 1, 2),
                -cofactor(0, 2, 1, 2),
                cofactor(0, 1, 1, 2),
            ],
            [
                -cofactor(1, 2, 0, 2),
                cofactor(0, 2, 0, 2),
                -cofactor(0, 1, 0, 2),
            ],
            [
                cofactor(1, 2, 0, 1),
                -cofactor(0, 2, 0, 1),
                cofactor(0, 1, 0, 1),
            ],
        ]))
    }

    pub fn mul_vec(&self, v: [f64; 3]) -> [f64; 3] {
        let m = &self.0;
        [
            m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
            m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
            m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
        ]
    }

    pub fn scaled(&self, s: f64) -> Mat3 {
        let mut out = *self;
        for row in &mut out.0 {
            for v in row {
                *v *= s;
            }
        }
        out
    }

    /// Row-major `f32` copy.
    pub fn to_f32(&self) -> [[f32; 3]; 3] {
        let m = &self.0;
        [
            [m[0][0] as f32, m[0][1] as f32, m[0][2] as f32],
            [m[1][0] as f32, m[1][1] as f32, m[1][2] as f32],
            [m[2][0] as f32, m[2][1] as f32, m[2][2] as f32],
        ]
    }
}

impl Mul for Mat3 {
    type Output = Mat3;

    fn mul(self, rhs: Mat3) -> Mat3 {
        let mut out = [[0.0; 3]; 3];
        for (r, out_row) in out.iter_mut().enumerate() {
            for (c, out_v) in out_row.iter_mut().enumerate() {
                *out_v = (0..3).map(|k| self.0[r][k] * rhs.0[k][c]).sum();
            }
        }
        Mat3(out)
    }
}

/// Applies a row-major `f32` matrix to a vector.
pub(crate) fn mul_vec_f32(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_times_original_is_identity() {
        let m = Mat3([[2.0, 0.5, -1.0], [0.25, 3.0, 0.75], [-0.5, 1.5, 4.0]]);
        let p = m * m.inverse().unwrap();
        for r in 0..3 {
            for c in 0..3 {
                let expected = if r == c { 1.0 } else { 0.0 };
                assert!((p.0[r][c] - expected).abs() < 1e-12, "{p:?}");
            }
        }
    }

    #[test]
    fn singular_matrix_has_no_inverse() {
        let m = Mat3([[1.0, 2.0, 3.0], [2.0, 4.0, 6.0], [0.0, 1.0, 0.0]]);
        assert!(m.inverse().is_none());
    }
}
