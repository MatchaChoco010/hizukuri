//! The two tables a shader looks up, and their files.
//!
//! Both are `size` × `size` texels of four `f32` (little-endian, row by
//! row, no header): what an RGBA32F image is uploaded from.
//!
//! `ltc_mat.f32`, looked up at (roughness, √(1 − cos θ)), θ being the angle
//! between the view direction and the normal: the inverse matrix of the
//! fitted distribution, divided by its middle element. With the texel
//! `(x, y, z, w)`, the matrix (rows) is
//!
//! ```text
//! | x 0 y |
//! | 0 1 0 |
//! | z 0 w |
//! ```
//!
//! in the frame whose z is the normal and whose x is the view direction
//! projected onto the surface.
//!
//! `ltc_amp.f32`:
//! - x, y, looked up as above: the BRDF's integral (the directional albedo
//!   with F = 1), and the integral weighted by Schlick's (1 − v·h)^5. The
//!   specular colour is `f0 * (x − y) + y`.
//! - w, looked up at (z · 0.5 + 0.5, length), for a vector form factor of
//!   that length whose direction has that z: the form factor of the sphere
//!   cap with that vector form factor, cut at the horizon, over the length.
//!   It stands in for clipping the polygon at the horizon.
//! - z is not used.

use std::f64::consts::PI;
use std::path::Path;

use crate::fit::{Entry, Grid};

pub const MAT_FILE: &str = "ltc_mat.f32";
pub const AMP_FILE: &str = "ltc_amp.f32";

#[derive(Clone, Debug)]
pub struct Tables {
    pub size: usize,
    pub mat: Vec<[f32; 4]>,
    pub amp: Vec<[f32; 4]>,
}

/// The form factor of a sphere cap whose vector form factor has `length`
/// and a direction with `z` to the normal, cut at the horizon, over
/// `length`.
pub fn clipped_sphere(z: f64, length: f64) -> f64 {
    if length <= 0.0 {
        return z.max(0.0);
    }
    // A cap of angular radius σ has the vector form factor sin²σ.
    let sigma = length.min(1.0).sqrt().asin();
    let (cos_w, sin_w) = (z, (1.0 - z * z).max(0.0).sqrt());
    // Rings around the cap's axis: the cosine to the normal along a ring at
    // the angle a is A + B cos b; its positive part is integrated over b in
    // closed form, and over a by the midpoint rule.
    let steps = 4096;
    let mut sum = 0.0;
    for k in 0..steps {
        let a = (k as f64 + 0.5) / steps as f64 * sigma;
        let (big_a, big_b) = (cos_w * a.cos(), sin_w * a.sin());
        let ring = if big_a >= big_b {
            2.0 * PI * big_a
        } else if big_a <= -big_b {
            0.0
        } else {
            let b0 = (-big_a / big_b).clamp(-1.0, 1.0).acos();
            2.0 * (big_a * b0 + big_b * b0.sin())
        };
        sum += ring * a.sin();
    }
    sum * sigma / steps as f64 / PI / length
}

impl Tables {
    pub fn from_entries(grid: Grid, entries: &[Entry]) -> Self {
        let size = grid.size;
        let mut mat = Vec::with_capacity(size * size);
        let mut amp = Vec::with_capacity(size * size);
        for (index, entry) in entries.iter().enumerate() {
            let inv = entry.ltc().inv;
            let c = inv[1][1];
            mat.push([
                (inv[0][0] / c) as f32,
                (inv[0][2] / c) as f32,
                (inv[2][0] / c) as f32,
                (inv[2][2] / c) as f32,
            ]);
            let (row, column) = (index / size, index % size);
            let z = column as f64 / (size - 1) as f64 * 2.0 - 1.0;
            let length = row as f64 / (size - 1) as f64;
            amp.push([
                entry.averages.norm as f32,
                entry.averages.fresnel as f32,
                0.0,
                clipped_sphere(z, length) as f32,
            ]);
        }
        Self { size, mat, amp }
    }

    pub fn write(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join(MAT_FILE), bytes(&self.mat))?;
        std::fs::write(dir.join(AMP_FILE), bytes(&self.amp))
    }

    pub fn read(dir: &Path) -> std::io::Result<Self> {
        let mat = texels(&std::fs::read(dir.join(MAT_FILE))?);
        let amp = texels(&std::fs::read(dir.join(AMP_FILE))?);
        let size = mat.len().isqrt();
        if size < 2 || size * size != mat.len() || amp.len() != mat.len() {
            return Err(std::io::Error::other(format!(
                "{}: the tables are not two squares of the same size",
                dir.display()
            )));
        }
        Ok(Self { size, mat, amp })
    }

    /// The texel of `ltc_mat.f32` for a roughness and a view cosine, as a
    /// sampler with linear filtering and clamping gives it.
    pub fn mat_at(&self, roughness: f64, cos_theta: f64) -> [f64; 4] {
        let (u, v) = brdf_coords(roughness, cos_theta);
        bilinear(&self.mat, self.size, u, v)
    }

    /// The norm and the Fresnel-weighted integral.
    pub fn amp_at(&self, roughness: f64, cos_theta: f64) -> (f64, f64) {
        let (u, v) = brdf_coords(roughness, cos_theta);
        let t = bilinear(&self.amp, self.size, u, v);
        (t[0], t[1])
    }

    /// The horizon term for a vector form factor.
    pub fn clipped_sphere_at(&self, z: f64, length: f64) -> f64 {
        bilinear(&self.amp, self.size, z * 0.5 + 0.5, length)[3]
    }
}

/// Where a roughness and a view cosine are in the tables: both in [0, 1],
/// 0 at the first texel's centre and 1 at the last one's.
pub fn brdf_coords(roughness: f64, cos_theta: f64) -> (f64, f64) {
    (
        roughness.clamp(0.0, 1.0),
        (1.0 - cos_theta.clamp(0.0, 1.0)).sqrt(),
    )
}

fn bilinear(texels: &[[f32; 4]], size: usize, u: f64, v: f64) -> [f64; 4] {
    let at = |t: f64| {
        let x = t.clamp(0.0, 1.0) * (size - 1) as f64;
        let i = (x.floor() as usize).min(size - 2);
        (i, x - i as f64)
    };
    let ((i, fu), (j, fv)) = (at(u), at(v));
    std::array::from_fn(|c| {
        let texel = |row: usize, column: usize| texels[row * size + column][c] as f64;
        let top = texel(j, i) * (1.0 - fu) + texel(j, i + 1) * fu;
        let bottom = texel(j + 1, i) * (1.0 - fu) + texel(j + 1, i + 1) * fu;
        top * (1.0 - fv) + bottom * fv
    })
}

fn bytes(texels: &[[f32; 4]]) -> Vec<u8> {
    texels
        .iter()
        .flatten()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn texels(bytes: &[u8]) -> Vec<[f32; 4]> {
    bytes
        .chunks_exact(16)
        .map(|texel| {
            std::array::from_fn(|c| f32::from_le_bytes(texel[c * 4..c * 4 + 4].try_into().unwrap()))
        })
        .collect()
}
