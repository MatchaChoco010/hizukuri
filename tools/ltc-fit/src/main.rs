//! Makes and checks the tables of the area light shader (linearly
//! transformed cosines fitted to the GGX BRDF).
//!
//! To make the tables again (about a minute on six threads):
//!
//! ```sh
//! cargo run --release -p proto-ltc-fit -- fit prototype/compositor/projects/room/assets
//! ```
//!
//! To measure them against the BRDF, and the method against a brute-force
//! integration of a few lights:
//!
//! ```sh
//! cargo run --release -p proto-ltc-fit -- check prototype/compositor/projects/room/assets
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use proto_ltc_fit::brute;
use proto_ltc_fit::cases;
use proto_ltc_fit::fit::{self, Grid};
use proto_ltc_fit::ggx;
use proto_ltc_fit::ltc::Ltc;
use proto_ltc_fit::reads;
use proto_ltc_fit::shade::{self, Horizon};
use proto_ltc_fit::table::Tables;
use proto_ltc_fit::vec::{V3, inverse};

const USAGE: &str = "usage:
  ltc-fit fit <directory> [--size N] [--samples N] [--threads N]
                          [--power P] [--mean-axis]
      Fit the tables (N × N cells, default 64; N × N samples per evaluation,
      default 32) and write ltc_mat.f32 and ltc_amp.f32 into the directory.
      --power: the power of the distance that is minimized (default 3).
      --mean-axis: keep the lobe's axis at the BRDF's mean direction.
  ltc-fit check <directory>
      Measure the tables in the directory against the BRDF, and the shading
      with them against a brute-force integration.
  ltc-fit reads <directory>
      Measure where on a light a lobe's weight is, against the candidates
      for the one point where a shader reads the light's texture.
  ltc-fit shadows <directory>
      Measure the shadow of a box (the part of each lobe that the box's
      cover leaves) against a brute-force integration with the box.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("fit") => run_fit(&args[1..]),
        Some("check") => run_check(&args[1..]),
        Some("reads") => run_reads(&args[1..]),
        Some("shadows") => run_shadows(&args[1..]),
        _ => Err(USAGE.to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run_fit(args: &[String]) -> Result<(), String> {
    let mut dir = None;
    let mut size = 64;
    let mut options = fit::Options::default();
    let mut threads = std::thread::available_parallelism().map_or(1, |n| n.get().div_ceil(2));
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut number = |name: &str| -> Result<usize, String> {
            rest.next()
                .and_then(|value| value.parse().ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("{name} takes a positive number"))
        };
        match arg.as_str() {
            "--size" => size = number("--size")?,
            "--samples" => options.samples = number("--samples")?,
            "--power" => options.power = number("--power")? as i32,
            "--mean-axis" => options.free_axis = false,
            "--threads" => threads = number("--threads")?,
            other if dir.is_none() && !other.starts_with('-') => dir = Some(PathBuf::from(other)),
            _ => return Err(USAGE.to_owned()),
        }
    }
    let dir = dir.ok_or(USAGE)?;
    if size < 2 {
        return Err("--size must be at least 2".into());
    }
    let grid = Grid { size };
    let started = std::time::Instant::now();
    let entries = fit::fit_grid(grid, options, threads, &|rows| {
        eprintln!(
            "fitted {rows} of {size} view angles ({:.0} s)",
            started.elapsed().as_secs_f64()
        );
    });
    let tables = Tables::from_entries(grid, &entries);
    tables
        .write(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    println!(
        "wrote {size} × {size} tables to {} in {:.0} s",
        dir.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// ∫ |BRDF × cos / norm − distribution| over the sphere: 0 when they are the
/// same, 2 when they do not overlap.
fn l1_distance(ltc: &Ltc, v: V3, alpha: f64, norm: f64, n: usize) -> f64 {
    let mut sum = 0.0;
    let mut at = |l: V3| {
        let brdf = ggx::eval(v, l, alpha);
        let d = ltc.eval(l);
        if brdf.pdf + d > 0.0 {
            sum += (brdf.value / norm - d).abs() / (brdf.pdf + d);
        }
    };
    for a in 0..n {
        for b in 0..n {
            let (u1, u2) = ((a as f64 + 0.5) / n as f64, (b as f64 + 0.5) / n as f64);
            at(ltc.sample(u1, u2));
            at(ggx::sample(v, alpha, u1, u2));
        }
    }
    sum / (n * n) as f64
}

/// For a screen standing on a floor, over a grid of floor points and a few
/// roughnesses: how far each candidate read point is from the mean of the
/// lobe's weight on the screen, and how the weight's spread relates to the
/// distance of the read.
fn run_reads(args: &[String]) -> Result<(), String> {
    let [dir] = args else {
        return Err(USAGE.to_owned());
    };
    let dir = PathBuf::from(dir);
    let tables = Tables::read(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let screen = cases::lights()[1].1;
    let names = reads::CANDIDATES;
    // Per lobe (diffuse, specular) and candidate: the summed squared
    // distance of the read from the weight's mean, weighted by the lobe's
    // share of the light.
    let mut squared = [[0.0_f64; 4]; 2];
    let mut weight = [0.0_f64; 2];
    // Per lobe and candidate: spread / (distance of the read / √area), where
    // the light is large against the lobe (the spread is the lobe's own).
    let mut ratios: [[Vec<f64>; 4]; 2] = Default::default();
    println!(
        "| point | roughness | lobe | share | mean of the weight | spread | perpendicular | peak | mean direction | lifted mean |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for xi in -3..=3 {
        for yi in [-1.0, -0.4, 0.2, 0.6, 0.85] {
            for roughness in [0.15, 0.3, 0.6] {
                let position = [f64::from(xi) * 0.5, yi, 0.0];
                let view = proto_ltc_fit::vec::normalize([0.0, -1.0, 1.0]);
                let surface = shade::surface(&tables, position, [0.0, 0.0, 1.0], view, roughness);
                let Some((diffuse, specular, facing)) = shade::corners(&surface, &screen) else {
                    continue;
                };
                let (shares_d, shares_s) = shade::shade(&tables, &surface, &screen, Horizon::Clip);
                for (lobe, corners, share) in [
                    (0, diffuse, shares_d.form_factor),
                    (1, specular, shares_s.form_factor),
                ] {
                    if lobe == 0 && roughness != 0.3 {
                        // The diffuse lobe does not depend on the roughness.
                        continue;
                    }
                    let Some(moments) = reads::moments(corners, 256) else {
                        continue;
                    };
                    let candidates = reads::candidates(corners, facing);
                    let area = {
                        let e1 = proto_ltc_fit::vec::sub(corners[1], corners[0]);
                        let e2 = proto_ltc_fit::vec::sub(corners[3], corners[0]);
                        proto_ltc_fit::vec::length(proto_ltc_fit::vec::cross(e1, e2))
                    };
                    let mut cells = Vec::new();
                    for (k, direction) in candidates.into_iter().enumerate() {
                        let (uv, distance) = reads::along(corners, direction);
                        let uv = uv.map(|c| c.clamp(0.0, 1.0));
                        let d2 =
                            (uv[0] - moments.mean[0]).powi(2) + (uv[1] - moments.mean[1]).powi(2);
                        squared[lobe][k] += share * d2;
                        if moments.blur() < 0.12 {
                            ratios[lobe][k].push(moments.blur() / (distance / area.sqrt()));
                        }
                        cells.push(format!("{:.2} {:.2}", uv[0], uv[1]));
                    }
                    weight[lobe] += share;
                    if xi % 3 == 0 && share > 0.002 {
                        println!(
                            "| {:.1} {:.1} | {roughness} | {} | {share:.3} | {:.2} {:.2} | {:.3} | {} |",
                            position[0],
                            position[1],
                            ["diffuse", "specular"][lobe],
                            moments.mean[0],
                            moments.mean[1],
                            moments.blur(),
                            cells.join(" | "),
                        );
                    }
                }
            }
        }
    }
    println!();
    println!(
        "Distance of the read from the mean of the weight, in texture coordinates (root mean square, weighted by the lobe's share of the light):"
    );
    println!();
    println!("| lobe | perpendicular | peak | mean direction | lifted mean |");
    println!("| --- | --- | --- | --- | --- |");
    for (lobe, name) in ["diffuse", "specular"].iter().enumerate() {
        let rms: Vec<String> = Vec::from_fn(4, |k| {
            format!("{:.3}", (squared[lobe][k] / weight[lobe]).sqrt())
        });
        println!("| {name} | {} |", rms.join(" | "));
    }
    println!();
    println!(
        "The weight's spread over (the read's distance / √area), where the spread is below 0.12 (the light is large against the lobe): median, and the range of the middle half:"
    );
    println!();
    for (lobe, name) in ["diffuse", "specular"].iter().enumerate() {
        for (k, candidate) in names.iter().enumerate() {
            let values = &mut ratios[lobe][k];
            if values.is_empty() {
                continue;
            }
            values.sort_by(f64::total_cmp);
            let at = |q: f64| values[((values.len() - 1) as f64 * q) as usize];
            println!(
                "- {name}, {candidate}: {:.2} ({:.2} to {:.2}; {} reads)",
                at(0.5),
                at(0.25),
                at(0.75),
                values.len()
            );
        }
    }
    Ok(())
}

/// A box standing on a floor in front of a screen: for floor points around
/// it, how much of the diffuse and of the specular term the box leaves, by
/// brute force and by the cover's approximation.
fn run_shadows(args: &[String]) -> Result<(), String> {
    let [dir] = args else {
        return Err(USAGE.to_owned());
    };
    let dir = PathBuf::from(dir);
    let tables = Tables::read(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    println!("| point | roughness | diffuse: brute | cover | specular: brute | cover |");
    println!("| --- | --- | --- | --- | --- | --- |");
    let (mut count, mut sum, mut largest) = (0.0, [0.0_f64; 2], [0.0_f64; 2]);
    for case in cases::shadowed() {
        let surface = shade::surface(
            &tables,
            case.position,
            case.normal,
            case.view,
            case.roughness,
        );
        let brute = |occluders: &[shade::Box3]| {
            brute::integrate_occluded(
                case.position,
                &surface.frame,
                case.view,
                case.roughness,
                &case.quad,
                &|_, _| [1.0; 3],
                occluders,
                512,
            )
        };
        let (open, shadowed) = (brute(&[]), brute(&[cases::OCCLUDER]));
        let (diffuse, specular) = shade::shade(&tables, &surface, &case.quad, Horizon::Sphere);
        let cover = shade::box_cover(&surface, &case.quad, &cases::OCCLUDER);
        let left = [
            shadowed.diffuse[0] / open.diffuse[0].max(1e-9),
            shade::uncovered(&diffuse, cover),
            shadowed.specular[0] / open.specular[0].max(1e-9),
            shade::uncovered(&specular, cover),
        ];
        // The error of the term itself: what is left times the term.
        let errors = [
            (left[1] - left[0]) * open.diffuse[0],
            (left[3] - left[2]) * open.specular[0],
        ];
        for k in 0..2 {
            sum[k] += errors[k].abs();
            largest[k] = largest[k].max(errors[k].abs());
        }
        count += 1.0;
        println!(
            "| {:.2} {:.2} | {} | {:.3} | {:.3} | {:.3} | {:.3} |",
            case.position[0], case.position[1], case.roughness, left[0], left[1], left[2], left[3]
        );
    }
    println!();
    println!("Absolute error of the term (what the box leaves, times the term without the box):");
    println!();
    println!("| | largest | mean |");
    println!("| --- | --- | --- |");
    for (k, name) in ["diffuse", "specular"].iter().enumerate() {
        println!("| {name} | {:.4} | {:.4} |", largest[k], sum[k] / count);
    }
    Ok(())
}

fn run_check(args: &[String]) -> Result<(), String> {
    let [dir] = args else {
        return Err(USAGE.to_owned());
    };
    let dir = PathBuf::from(dir);
    let tables = Tables::read(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    println!("tables: {0} × {0}", tables.size);

    println!();
    println!("## The distribution against the BRDF");
    println!();
    println!(
        "L1: the integral of |BRDF × cos / norm − distribution| (0: the same, 2: disjoint). \
         The table is read with linear filtering, as the shader reads it."
    );
    println!();
    println!(
        "| roughness | view angle | L1 | norm (table) | norm (BRDF) | Fresnel (table) | Fresnel (BRDF) |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- |");
    for roughness in [0.05, 0.1, 0.2, 0.35, 0.5, 0.75, 1.0] {
        for degrees in [0.0_f64, 30.0, 60.0, 75.0, 85.0] {
            let cos_theta = degrees.to_radians().cos();
            let v = fit::view(cos_theta);
            let alpha = roughness * roughness;
            let exact = fit::averages(v, alpha, 512);
            let t = tables.mat_at(roughness, cos_theta);
            let ltc = Ltc::new(inverse(&[
                [t[0], 0.0, t[1]],
                [0.0, 1.0, 0.0],
                [t[2], 0.0, t[3]],
            ]));
            let l1 = l1_distance(&ltc, v, alpha, exact.norm, 256);
            let (norm, fresnel) = tables.amp_at(roughness, cos_theta);
            println!(
                "| {roughness} | {degrees}° | {l1:.3} | {norm:.4} | {:.4} | {fresnel:.4} | {:.4} |",
                exact.norm, exact.fresnel
            );
        }
    }

    println!();
    println!("## The smoothness of the matrix table");
    println!();
    println!(
        "A texel against the mean of its four neighbours, over the texel's largest \
         component: a search that ended in another minimum than its neighbours shows here."
    );
    println!();
    let size = tables.size;
    let mut rough: Vec<(f64, usize, usize)> = Vec::new();
    for row in 1..size - 1 {
        for column in 1..size - 1 {
            let at = |r: usize, c: usize| tables.mat[r * size + c];
            let texel = at(row, column);
            let largest = texel.iter().fold(0.0_f32, |m, v| m.max(v.abs())) as f64;
            let mut worst: f64 = 0.0;
            for (k, value) in texel.iter().enumerate() {
                let mean = (at(row - 1, column)[k]
                    + at(row + 1, column)[k]
                    + at(row, column - 1)[k]
                    + at(row, column + 1)[k]) as f64
                    / 4.0;
                worst = worst.max((*value as f64 - mean).abs() / largest);
            }
            rough.push((worst, row, column));
        }
    }
    rough.sort_by(|a, b| b.0.total_cmp(&a.0));
    let median = rough[rough.len() / 2].0;
    println!("median {median:.4}; the five largest:");
    println!();
    for (value, row, column) in rough.iter().take(5) {
        println!("- {value:.4} at row {row} (view angle), column {column} (roughness)");
    }

    println!();
    println!("## A quad light against brute force");
    println!();
    println!(
        "The light has the radiance 1. Diffuse: the form factor. Specular: with F = 1. \
         `clip`: the polygon is clipped at the horizon. `sphere`: the horizon term of the table."
    );
    println!();
    println!(
        "| case | diffuse: brute | clip | sphere | specular: brute | clip | sphere | error (sphere) |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- |");
    let mut worst: f64 = 0.0;
    for case in cases::all() {
        let surface = shade::surface(
            &tables,
            case.position,
            case.normal,
            case.view,
            case.roughness,
        );
        let reference = brute::integrate(
            case.position,
            &surface.frame,
            case.view,
            case.roughness,
            &case.quad,
            &|_, _| [1.0; 3],
            1024,
        );
        let (diffuse_clip, specular_clip) =
            shade::shade(&tables, &surface, &case.quad, Horizon::Clip);
        let (diffuse_sphere, specular_sphere) =
            shade::shade(&tables, &surface, &case.quad, Horizon::Sphere);
        let specular = specular_sphere.form_factor * surface.norm;
        let error = specular - reference.specular[0];
        worst = worst.max(error.abs());
        println!(
            "| {} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {:+.4} |",
            case.name,
            reference.diffuse[0],
            diffuse_clip.form_factor,
            diffuse_sphere.form_factor,
            reference.specular[0],
            specular_clip.form_factor * surface.norm,
            specular,
            error,
        );
    }
    println!();
    println!("largest absolute error of the specular term (sphere): {worst:.4}");
    Ok(())
}
