//! Fitting a linearly transformed cosine to the GGX BRDF, for a grid of
//! roughness and view angle.
//!
//! For each cell: the BRDF's integral (its norm), the Fresnel-weighted
//! integral and its mean direction come from sampling the BRDF. The lobe's
//! axis (a turn away from the mean direction, unless `Options::free_axis`
//! is off), the two scales and the shear are found by a Nelder-Mead search
//! that minimizes the L3 distance between the BRDF (divided by its norm) and
//! the distribution. The distance is integrated with samples from both,
//! weighted against each other.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::ggx;
use crate::ltc::{Ltc, Params};
use crate::vec::{V3, normalize};

/// The grid of the tables: `size` × `size` cells.
#[derive(Copy, Clone, Debug)]
pub struct Grid {
    pub size: usize,
}

/// The lowest roughness the tables are fitted for. Column 0 stands for it.
pub const MIN_ROUGHNESS: f64 = 0.01;
/// The lowest cosine of the view angle. The last row stands for it.
pub const MIN_COS_THETA: f64 = 0.004;

impl Grid {
    /// The roughness of column `i`: linear from 0 to 1. α is its square.
    pub fn roughness(&self, i: usize) -> f64 {
        (i as f64 / (self.size - 1) as f64).max(MIN_ROUGHNESS)
    }

    /// The cosine of the view angle of row `j`: `1 − t²`, t from 0 to 1.
    pub fn cos_theta(&self, j: usize) -> f64 {
        let t = j as f64 / (self.size - 1) as f64;
        (1.0 - t * t).max(MIN_COS_THETA)
    }
}

/// The view direction with the given cosine to the normal, in the xz plane.
pub fn view(cos_theta: f64) -> V3 {
    [
        (1.0 - cos_theta * cos_theta).max(0.0).sqrt(),
        0.0,
        cos_theta,
    ]
}

/// Integrals of the BRDF × cosine over the light directions.
#[derive(Copy, Clone, Debug)]
pub struct Averages {
    /// The integral itself: the directional albedo with F = 1.
    pub norm: f64,
    /// The integral weighted by (1 − v·h)^5.
    pub fresnel: f64,
    /// The normalized mean direction.
    pub direction: V3,
}

/// Integrates with `n` × `n` stratified samples of the BRDF.
pub fn averages(v: V3, alpha: f64, n: usize) -> Averages {
    let (mut norm, mut fresnel, mut dir) = (0.0, 0.0, [0.0; 3]);
    for a in 0..n {
        for b in 0..n {
            let u1 = (a as f64 + 0.5) / n as f64;
            let u2 = (b as f64 + 0.5) / n as f64;
            let l = ggx::sample(v, alpha, u1, u2);
            let e = ggx::eval(v, l, alpha);
            if e.pdf > 0.0 {
                let w = e.value / e.pdf;
                norm += w;
                fresnel += w * e.schlick;
                dir[0] += w * l[0];
                dir[2] += w * l[2];
            }
        }
    }
    let count = (n * n) as f64;
    Averages {
        norm: norm / count,
        fresnel: fresnel / count,
        direction: normalize(dir),
    }
}

/// The distance between the BRDF × cosine over `norm` and `ltc`: the
/// integral of the difference's absolute value to the power `power`.
pub fn distance(ltc: &Ltc, v: V3, alpha: f64, norm: f64, n: usize, power: i32) -> f64 {
    let mut sum = 0.0;
    let mut at = |l: V3| {
        let brdf = ggx::eval(v, l, alpha);
        let d = ltc.eval(l);
        let pdf = brdf.pdf + d;
        if pdf > 0.0 {
            sum += (brdf.value / norm - d).abs().powi(power) / pdf;
        }
    };
    for a in 0..n {
        for b in 0..n {
            let u1 = (a as f64 + 0.5) / n as f64;
            let u2 = (b as f64 + 0.5) / n as f64;
            at(ltc.sample(u1, u2));
            at(ggx::sample(v, alpha, u1, u2));
        }
    }
    sum / (n * n) as f64
}

/// Minimizes `f` from `start` with the downhill simplex method. `step` is
/// the size of the first simplex.
pub fn nelder_mead(
    f: &dyn Fn(&[f64]) -> f64,
    start: &[f64],
    step: f64,
    tolerance: f64,
    max_iterations: usize,
) -> (Vec<f64>, f64) {
    let dim = start.len();
    let mut simplex: Vec<(Vec<f64>, f64)> = Vec::from_fn(dim + 1, |k| {
        let mut point = start.to_vec();
        if k > 0 {
            point[k - 1] += step;
        }
        let value = f(&point);
        (point, value)
    });
    let along = |centroid: &[f64], worst: &[f64], t: f64| -> Vec<f64> {
        Vec::from_fn(dim, |d| centroid[d] + t * (worst[d] - centroid[d]))
    };
    for _ in 0..max_iterations {
        simplex.sort_by(|a, b| a.1.total_cmp(&b.1));
        let (best, worst) = (simplex[0].1, simplex[dim].1);
        if (worst - best).abs() <= tolerance * (best.abs() + worst.abs()) * 0.5 {
            break;
        }
        let mut centroid = vec![0.0; dim];
        for (point, _) in &simplex[..dim] {
            for d in 0..dim {
                centroid[d] += point[d] / dim as f64;
            }
        }
        let worst_point = simplex[dim].0.clone();
        let reflected = along(&centroid, &worst_point, -1.0);
        let f_reflected = f(&reflected);
        if f_reflected < simplex[0].1 {
            let expanded = along(&centroid, &worst_point, -2.0);
            let f_expanded = f(&expanded);
            simplex[dim] = if f_expanded < f_reflected {
                (expanded, f_expanded)
            } else {
                (reflected, f_reflected)
            };
        } else if f_reflected < simplex[dim - 1].1 {
            simplex[dim] = (reflected, f_reflected);
        } else {
            let t = if f_reflected < simplex[dim].1 {
                -0.5
            } else {
                0.5
            };
            let contracted = along(&centroid, &worst_point, t);
            let f_contracted = f(&contracted);
            if f_contracted < simplex[dim].1.min(f_reflected) {
                simplex[dim] = (contracted, f_contracted);
            } else {
                let best_point = simplex[0].0.clone();
                for (point, value) in simplex.iter_mut().skip(1) {
                    for d in 0..dim {
                        point[d] = best_point[d] + 0.5 * (point[d] - best_point[d]);
                    }
                    *value = f(point);
                }
            }
        }
    }
    simplex.sort_by(|a, b| a.1.total_cmp(&b.1));
    simplex.swap_remove(0)
}

/// How the search is run.
#[derive(Copy, Clone, Debug)]
pub struct Options {
    /// `samples` × `samples` directions from each of the BRDF and the
    /// distribution, per evaluation of the distance.
    pub samples: usize,
    /// The power of the distance that is minimized.
    pub power: i32,
    /// Whether the lobe's axis is searched as well. Otherwise it is the
    /// BRDF's mean direction.
    pub free_axis: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            samples: 32,
            power: 3,
            free_axis: true,
        }
    }
}

/// One fitted cell.
#[derive(Copy, Clone, Debug)]
pub struct Entry {
    pub params: Params,
    /// The angle of the lobe's axis from the BRDF's mean direction, about
    /// the y axis (radians).
    pub turn: f64,
    pub averages: Averages,
    /// The distance the search ended with.
    pub distance: f64,
}

/// `direction` (in the xz plane) turned by `angle` about y.
fn turned(direction: V3, angle: f64) -> V3 {
    let (s, c) = angle.sin_cos();
    [
        direction[0] * c + direction[2] * s,
        0.0,
        direction[2] * c - direction[0] * s,
    ]
}

impl Entry {
    pub fn ltc(&self) -> Ltc {
        Ltc::from_params(turned(self.averages.direction, self.turn), self.params)
    }
}

/// How many samples per axis the BRDF's integrals are taken with. The part
/// of a sharp lobe that leaves below the horizon is a few thousandths of
/// it: fewer strata than this miss it.
pub const AVERAGE_SAMPLES: usize = 512;

/// Fits one cell, searching from `start`. `averages` are the BRDF's
/// integrals for the cell. With `isotropic` (the view along the normal),
/// the two scales are equal and there is no shear.
pub fn fit_cell(
    v: V3,
    alpha: f64,
    mut averages: Averages,
    start: (Params, f64),
    isotropic: bool,
    options: Options,
) -> Entry {
    let samples = options.samples;
    if isotropic {
        averages.direction = [0.0, 0.0, 1.0];
    }
    // The search runs over the logarithms of the scales, and over the shear
    // in units of the scale it shears along: steps are relative, and the
    // scales stay positive.
    let unpack = |x: &[f64]| -> (Params, f64) {
        if isotropic {
            let m = x[0].exp();
            (
                Params {
                    m11: m,
                    m22: m,
                    m13: 0.0,
                },
                0.0,
            )
        } else {
            let m11 = x[0].exp();
            (
                Params {
                    m11,
                    m22: x[1].exp(),
                    m13: x[2] * m11,
                },
                // The turn is searched in units of the lobe's width.
                if options.free_axis { x[3] * m11 } else { 0.0 },
            )
        }
    };
    let pack = |(p, turn): (Params, f64)| -> Vec<f64> {
        if isotropic {
            vec![(0.5 * (p.m11.ln() + p.m22.ln()))]
        } else if options.free_axis {
            vec![p.m11.ln(), p.m22.ln(), p.m13 / p.m11, turn / p.m11]
        } else {
            vec![p.m11.ln(), p.m22.ln(), p.m13 / p.m11]
        }
    };
    let cost = |x: &[f64]| -> f64 {
        let (params, turn) = unpack(x);
        let ltc = Ltc::from_params(turned(averages.direction, turn), params);
        distance(&ltc, v, alpha, averages.norm, samples, options.power)
    };
    // Twice: the second search starts a fresh simplex where the first ended,
    // which gets it out of a simplex that collapsed early.
    let (x, _) = nelder_mead(&cost, &pack(start), 0.05, 1e-7, 400);
    let (x, d) = nelder_mead(&cost, &x, 0.02, 1e-9, 400);
    let (params, turn) = unpack(&x);
    Entry {
        params,
        turn,
        averages,
        distance: d,
    }
}

/// Fits every cell of `grid`, on `threads` threads. The result is indexed
/// `row * size + column` (row: view angle, column: roughness). `progress` is
/// told how many rows are done.
pub fn fit_grid(
    grid: Grid,
    options: Options,
    threads: usize,
    progress: &(dyn Fn(usize) + Sync),
) -> Vec<Entry> {
    let size = grid.size;
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let rows: Mutex<Vec<Option<Vec<Entry>>>> = Mutex::new(vec![None; size]);
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                loop {
                    let j = next.fetch_add(1, Ordering::Relaxed);
                    if j >= size {
                        break;
                    }
                    let row = fit_row(grid, j, options);
                    rows.lock().unwrap()[j] = Some(row);
                    progress(done.fetch_add(1, Ordering::Relaxed) + 1);
                }
            });
        }
    });
    let rows = rows.into_inner().unwrap();
    rows.into_iter().flat_map(|row| row.unwrap()).collect()
}

/// One view angle, from the roughest cell down: each search starts from the
/// result of the cell before it.
fn fit_row(grid: Grid, j: usize, options: Options) -> Vec<Entry> {
    let size = grid.size;
    let cos_theta = grid.cos_theta(j);
    let v = view(cos_theta);
    let isotropic = j == 0;
    let mut row: Vec<Option<Entry>> = vec![None; size];
    let mut previous: Option<(f64, Params, f64)> = None;
    for i in (0..size).rev() {
        let roughness = grid.roughness(i);
        let alpha = roughness * roughness;
        let averages = averages(v, alpha, AVERAGE_SAMPLES);
        let entry = match previous {
            None => fit_cell(
                v,
                alpha,
                averages,
                (
                    Params {
                        m11: 1.0,
                        m22: 1.0,
                        m13: 0.0,
                    },
                    0.0,
                ),
                isotropic,
                options,
            ),
            Some((previous_alpha, p, turn)) => {
                // Two starts: the neighbour's result as it is, and narrowed
                // by the ratio of α (a sharp lobe's width follows α).
                let ratio = alpha / previous_alpha;
                let narrowed = Params {
                    m11: p.m11 * ratio,
                    m22: p.m22 * ratio,
                    m13: p.m13 * ratio,
                };
                let a = fit_cell(v, alpha, averages, (p, turn), isotropic, options);
                let b = fit_cell(
                    v,
                    alpha,
                    averages,
                    (narrowed, turn * ratio),
                    isotropic,
                    options,
                );
                if b.distance < a.distance { b } else { a }
            }
        };
        previous = Some((alpha, entry.params, entry.turn));
        row[i] = Some(entry);
    }
    row.into_iter().map(Option::unwrap).collect()
}
