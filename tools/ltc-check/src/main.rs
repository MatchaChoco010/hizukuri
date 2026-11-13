//! Checks the area light shader of a project (its Slang module `ltc`, its
//! script module `ltc` and the tables in its `assets/`) on a GPU.
//!
//! ```sh
//! cargo run -p proto-ltc-check -- prototype/compositor/projects/room \
//!     [--shots DIRECTORY] [--flicker] [--bench | --bench-only]
//! ```
//!
//! - Without `--bench-only`: shades single points with the shader, reads
//!   them back, and compares them with the same computation on the CPU and
//!   with a brute-force integration of the BRDF over the light. `cargo test`
//!   does this as well.
//! - `--shots`: draws a floor under quad lights and writes PNG files.
//! - `--flicker`: follows the shading of fixed points while the light's
//!   picture scrolls, for the two ways of making its mip levels.
//! - `--bench`: times a 1920 × 1080 frame with up to four lights, and
//!   making the mip levels of a light's texture. `--bench-only`: nothing
//!   but that.
//!
//! The device is the one the tests use (`GpuConfig::for_tests`). With
//! `GFX_SPIKE_VALIDATION=1`, any message of the validation layer fails the
//! run and the test.

mod scene;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use proto_gfx_core::{Gpu, GpuConfig, Handle};
use proto_gfx_lua::{HostOptions, ScriptHost, ShaderLoader};
use proto_ltc_fit::brute;
use proto_ltc_fit::cases::{self, Case};
use proto_ltc_fit::shade::{self, Horizon};
use proto_ltc_fit::table::{AMP_FILE, MAT_FILE, Tables};
use proto_ltc_fit::vec::{V3, sub};
use proto_slangc::{Compiler, Input, Stage};

const USAGE: &str =
    "usage: ltc-check <project> [--shots DIRECTORY] [--flicker] [--bench | --bench-only]";
const SHADER: &str = "shaders/check.slang";
const PREFILTER: &str = "shaders/ltc_prefilter.slang";
/// The shader's `LTC_SIGMA_MIPMAPS` and `LTC_SIGMA_PREFILTER`.
const SIGMA_MIPMAPS: f32 = 0.5;
const SIGMA_PREFILTER: f32 = 1.25;
const SCRIPT: &str = include_str!("../scripts/check.luau");

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

struct Check {
    host: ScriptHost,
    tables: Tables,
}

fn run() -> Result<(), String> {
    let mut project = None;
    let mut shots = None;
    let mut bench = false;
    let mut bench_only = false;
    let mut flicker = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--shots" => shots = Some(PathBuf::from(args.next().ok_or(USAGE)?)),
            "--bench" => bench = true,
            "--bench-only" => bench_only = true,
            "--flicker" => flicker = true,
            other if project.is_none() && !other.starts_with('-') => {
                project = Some(PathBuf::from(other));
            }
            _ => return Err(USAGE.into()),
        }
    }
    let project = project.ok_or(USAGE)?;
    let check = Check::new(&project)?;
    println!("device: {}", check.device());
    if bench_only {
        check.bench()?;
        return no_validation_messages();
    }
    let worst = check.compare_all()?;
    if flicker {
        check.flicker()?;
    }
    if let Some(dir) = shots {
        check.shots(&dir)?;
    }
    if bench {
        check.bench()?;
    }
    no_validation_messages()?;
    worst.within_bounds()
}

/// With `GFX_SPIKE_VALIDATION=1`, the validation layer watches what the
/// project's script module and the check do: it has to stay silent.
fn no_validation_messages() -> Result<(), String> {
    match proto_gfx_core::validation_error_count() {
        0 => Ok(()),
        count => Err(format!(
            "{count} messages of the validation layer: {:#?}",
            proto_gfx_core::validation_messages()
        )),
    }
}

/// The largest differences the comparisons found.
struct Worst {
    /// The shader against the CPU's version of it.
    against_cpu: f64,
    /// Against brute force: the specular term of a light of one colour, the
    /// specular term of a light that shows a picture, and either term with
    /// a box in the light's way.
    uniform: f64,
    textured: f64,
    shadowed: f64,
}

impl Worst {
    fn within_bounds(&self) -> Result<(), String> {
        // The CPU's version follows the shader step for step: more than
        // what 32-bit arithmetic explains means that one of the two was
        // changed without the other.
        if self.against_cpu > 2e-3 {
            return Err(format!(
                "the shader and the CPU's version of it disagree by {:.2e}",
                self.against_cpu
            ));
        }
        // What the method itself is off by, with room: beyond these, the
        // tables or the shader got worse.
        for (name, value, bound) in [
            ("a light of one colour", self.uniform, 0.06),
            ("a light that shows a picture", self.textured, 0.09),
            ("a box in the light's way", self.shadowed, 0.08),
        ] {
            if value > bound {
                return Err(format!(
                    "{name}: {value:.3} from brute force; at most {bound} is expected"
                ));
            }
        }
        Ok(())
    }
}

/// Compiles the entry points of the check's shader, with the project's
/// modules on the search path.
fn compile(project: &Path) -> Result<HashMap<(String, String), Vec<u8>>, String> {
    let mut compiler = Compiler::find().map_err(|e| e.to_string())?;
    compiler.include_dirs.push(project.join("shaders"));
    let sources = [
        (
            SHADER,
            Path::new(env!("CARGO_MANIFEST_DIR")).join(SHADER),
            vec![
                ("vs_main", Stage::Vertex),
                ("fs_probe", Stage::Fragment),
                ("fs_scene", Stage::Fragment),
            ],
        ),
        // The project's own shader, which its script module names.
        (
            PREFILTER,
            project.join(PREFILTER),
            vec![
                ("vs_main", Stage::Vertex),
                ("fs_halve", Stage::Fragment),
                ("fs_blur", Stage::Fragment),
            ],
        ),
    ];
    let mut out = HashMap::new();
    for (name, source, entries) in sources {
        for (entry, stage) in entries {
            let compiled = compiler
                .compile(Input::File(&source), stage, entry)
                .map_err(|e| e.to_string())?;
            for warning in &compiled.warnings {
                eprintln!("{warning}");
            }
            out.insert((name.to_owned(), entry.to_owned()), compiled.spirv);
        }
    }
    Ok(out)
}

fn script_error(what: &str) -> impl Fn(proto_gfx_lua::ScriptError) -> String + '_ {
    move |e| format!("{what}: {e}")
}

impl Check {
    fn new(project: &Path) -> Result<Self, String> {
        let assets = project.join("assets");
        let tables = Tables::read(&assets).map_err(|e| format!("{}: {e}", assets.display()))?;
        let shaders = compile(project)?;
        let module_path = project.join("ltc.luau");
        let module = std::fs::read_to_string(&module_path)
            .map_err(|e| format!("{}: {e}", module_path.display()))?;
        let gpu = Gpu::new(GpuConfig::for_tests()).map_err(|e| format!("no device: {e}"))?;
        let options = HostOptions {
            time_budget: Some(Duration::from_secs(600)),
            shader_loader: Some(ShaderLoader(Rc::new(move |name: &str, entry: &str| {
                shaders
                    .get(&(name.to_owned(), entry.to_owned()))
                    .cloned()
                    .ok_or_else(|| format!("no shader {name} with entry point `{entry}`"))
            }))),
            ..HostOptions::default()
        };
        let host = ScriptHost::with_setup(gpu, options, |lua, _| {
            let value: mlua::Value = lua.load(module.as_str()).set_name("ltc.luau").call(())?;
            lua.globals().set("ltc", value)
        })
        .map_err(script_error("ltc.luau"))?;
        host.load("check.luau", SCRIPT)
            .map_err(script_error("check.luau"))?;
        let file = |name: &str| -> Result<mlua::LuaString, String> {
            let bytes = std::fs::read(assets.join(name)).map_err(|e| format!("{name}: {e}"))?;
            host.lua().create_string(&bytes).map_err(|e| e.to_string())
        };
        host.call_in_frame("setup", (file(MAT_FILE)?, file(AMP_FILE)?))
            .map_err(script_error("setup"))?;
        Ok(Self { host, tables })
    }

    fn device(&self) -> String {
        let gpu = self.host.gpu();
        format!("{} / {}", gpu.device_name(), gpu.driver_name())
    }

    /// Calls a script function that returns an image, inside a frame.
    fn image<A: mlua::IntoLuaMulti>(&self, name: &str, args: A) -> Result<Handle, String> {
        self.host
            .gpu()
            .begin_frame()
            .map_err(|e| format!("{name}: {e}"))?;
        let result = self.host.call_function::<A, i64>(name, args, None);
        let end = self.host.gpu().end_frame();
        let bits = result.map_err(script_error(name))?;
        end.map_err(|e| format!("{name}: {e}"))?;
        Ok(Handle::from_bits(bits))
    }

    /// A light texture from RGBA texels, as 16-bit floats. `levels`: how its
    /// mip levels are made ("none", "mipmaps", "prefilter"). Returns the
    /// script's id for it.
    fn light(
        &self,
        width: u32,
        height: u32,
        texels: &[[f32; 4]],
        levels: &str,
    ) -> Result<i64, String> {
        self.light_in("rgba16float", width, height, texels, levels)
    }

    /// [`Self::light`] in a format: "rgba16float" or "rgba32float".
    fn light_in(
        &self,
        format: &str,
        width: u32,
        height: u32,
        texels: &[[f32; 4]],
        levels: &str,
    ) -> Result<i64, String> {
        let values = texels.iter().flatten();
        let bytes: Vec<u8> = if format == "rgba32float" {
            values.flat_map(|v| v.to_ne_bytes()).collect()
        } else {
            values
                .flat_map(|v| half::f16::from_f32(*v).to_bits().to_ne_bytes())
                .collect()
        };
        let bytes = self
            .host
            .lua()
            .create_string(&bytes)
            .map_err(|e| e.to_string())?;
        self.host
            .gpu()
            .begin_frame()
            .map_err(|e| format!("make_light: {e}"))?;
        let result = self.host.call_function::<_, i64>(
            "make_light",
            (width, height, bytes, levels, format),
            None,
        );
        let end = self.host.gpu().end_frame();
        let id = result.map_err(script_error("make_light"))?;
        end.map_err(|e| format!("make_light: {e}"))?;
        Ok(id)
    }

    /// Shades the cases on the GPU. `mode` is the shader's. One RGBA per
    /// case.
    fn probe(
        &self,
        cases: &[Case],
        mode: u32,
        light: i64,
        level_sigma: f32,
    ) -> Result<Vec<[f32; 4]>, String> {
        // The box in the light's way is the same for every case; only the
        // shader's last mode looks at it.
        let occluder = cases::OCCLUDER;
        let mut pushes = Vec::with_capacity(cases.len() * 160);
        for case in cases {
            let quad = &case.quad;
            let rows: [(V3, f32); 10] = [
                (case.position, case.roughness as f32),
                (case.normal, f32::from_bits(mode)),
                (case.view, level_sigma),
                (quad.p[0], if quad.two_sided { 1.0 } else { 0.0 }),
                (sub(quad.p[1], quad.p[0]), 0.0),
                (sub(quad.p[3], quad.p[0]), 0.0),
                (occluder.center, 0.0),
                (occluder.half[0], 0.0),
                (occluder.half[1], 0.0),
                (occluder.half[2], 0.0),
            ];
            for (v, w) in rows {
                for c in v {
                    pushes.extend_from_slice(&(c as f32).to_ne_bytes());
                }
                pushes.extend_from_slice(&w.to_ne_bytes());
            }
        }
        let pushes = self
            .host
            .lua()
            .create_string(&pushes)
            .map_err(|e| e.to_string())?;
        let image = self.image("probe", (cases.len(), pushes, light))?;
        let bytes = self
            .host
            .gpu()
            .read_image(image)
            .map_err(|e| format!("read the probes: {e}"))?;
        Ok(bytes
            .as_chunks::<16>()
            .0
            .iter()
            .map(|texel| {
                std::array::from_fn(|c| {
                    f32::from_ne_bytes(texel[c * 4..c * 4 + 4].try_into().unwrap())
                })
            })
            .collect())
    }

    fn compare_all(&self) -> Result<Worst, String> {
        let (against_cpu, uniform) = self.compare()?;
        let textured = self.compare_textured()?;
        let (shadows_against_cpu, shadowed) = self.compare_shadows()?;
        Ok(Worst {
            against_cpu: against_cpu.max(shadows_against_cpu),
            uniform,
            textured,
            shadowed,
        })
    }

    /// The shader against the CPU's version of it, and against brute force.
    /// Returns the largest difference between the shader and the CPU, and
    /// the largest error of the specular term against brute force.
    fn compare(&self) -> Result<(f64, f64), String> {
        let white = self.light(1, 1, &[[1.0; 4]], "none")?;
        let cases = cases::all();
        let factors = self.probe(&cases, 0, white, 0.5)?;
        let amplitudes = self.probe(&cases, 1, white, 0.5)?;
        let reads = self.probe(&cases, 2, white, 0.5)?;

        println!();
        println!("## The shader against brute force");
        println!();
        println!(
            "A light of radiance 1. Diffuse: the form factor. Specular: with F = 1. \
             `sphere`: the horizon term of the table. `clip`: the light clipped at the horizon."
        );
        println!();
        println!(
            "| case | diffuse: brute | sphere | clip | specular: brute | sphere | clip | error (sphere) | error (clip) |"
        );
        println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
        // The largest differences: shader − CPU, and shader − brute force.
        let mut against_cpu: f64 = 0.0;
        let mut against_brute = [0.0_f64; 4];
        let mut sum_brute = [0.0_f64; 4];
        for (index, case) in cases.iter().enumerate() {
            let surface = shade::surface(
                &self.tables,
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
            let (diffuse_sphere, specular_sphere) =
                shade::shade(&self.tables, &surface, &case.quad, Horizon::Sphere);
            let (diffuse_clip, specular_clip) =
                shade::shade(&self.tables, &surface, &case.quad, Horizon::Clip);
            let gpu = factors[index].map(f64::from);
            let amplitude = amplitudes[index].map(f64::from);
            let read = reads[index].map(f64::from);
            // Where and how blurred a lobe reads the light counts as much as
            // the lobe's share of the light: where the share is next to
            // nothing, the direction of the vector form factor is what is
            // left of sums that cancel, in 32-bit arithmetic.
            let (d, s) = (diffuse_sphere.form_factor, specular_sphere.form_factor);
            let cpu = [
                (gpu[0], diffuse_sphere.form_factor, 1.0),
                (gpu[1], specular_sphere.form_factor, 1.0),
                (gpu[2], diffuse_clip.form_factor, 1.0),
                (gpu[3], specular_clip.form_factor, 1.0),
                (amplitude[0], surface.norm, 1.0),
                (amplitude[1], surface.fresnel, 1.0),
                (amplitude[2], diffuse_sphere.blur, d),
                (amplitude[3], specular_sphere.blur, s),
                (read[0], diffuse_sphere.uv[0], d),
                (read[1], diffuse_sphere.uv[1], d),
                (read[2], specular_sphere.uv[0], s),
                (read[3], specular_sphere.uv[1], s),
            ];
            for (which, (on_gpu, on_cpu, weight)) in cpu.into_iter().enumerate() {
                // The reads' positions and blurs are not bounded by 1.
                let difference = (on_gpu - on_cpu).abs() / on_cpu.abs().max(1.0) * weight;
                if difference > 2e-3 {
                    eprintln!(
                        "{}: value {which}: {on_gpu} on the GPU, {on_cpu} on the CPU",
                        case.name
                    );
                }
                against_cpu = against_cpu.max(difference);
            }
            let errors = [
                gpu[0] - reference.diffuse[0],
                gpu[2] - reference.diffuse[0],
                gpu[1] * amplitude[0] - reference.specular[0],
                gpu[3] * amplitude[0] - reference.specular[0],
            ];
            for (k, error) in errors.iter().enumerate() {
                against_brute[k] = against_brute[k].max(error.abs());
                sum_brute[k] += error.abs();
            }
            println!(
                "| {} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {:+.4} | {:+.4} |",
                case.name,
                reference.diffuse[0],
                gpu[0],
                gpu[2],
                reference.specular[0],
                gpu[1] * amplitude[0],
                gpu[3] * amplitude[0],
                errors[2],
                errors[3],
            );
        }
        let count = cases.len() as f64;
        println!();
        println!("{} cases. Absolute error against brute force:", cases.len());
        println!();
        println!("| | largest | mean |");
        println!("| --- | --- | --- |");
        for (k, name) in [
            "diffuse, sphere",
            "diffuse, clip",
            "specular, sphere",
            "specular, clip",
        ]
        .iter()
        .enumerate()
        {
            println!(
                "| {name} | {:.4} | {:.4} |",
                against_brute[k],
                sum_brute[k] / count
            );
        }
        println!();
        println!(
            "The shader against the same computation on the CPU (form factors, amplitudes, \
             where and how blurred the light is read): largest difference {against_cpu:.2e}"
        );
        Ok((against_cpu, against_brute[2]))
    }

    /// A light that shows a picture: the shader's read of its blurred
    /// levels against brute force with the picture itself. Returns the
    /// largest error of the specular term with the prefiltered levels.
    fn compare_textured(&self) -> Result<f64, String> {
        let (tw, th) = (256, 144);
        let picture = scene::desktop(tw, th);
        let prefiltered = self.light(tw, th, &picture, "prefilter")?;
        let halved = self.light(tw, th, &picture, "mipmaps")?;
        let cases = scene::textured_cases();
        let read = |light: i64, sigma: f32| -> Result<Vec<[[f32; 4]; 2]>, String> {
            let diffuse = self.probe(&cases, 3, light, sigma)?;
            let specular = self.probe(&cases, 4, light, sigma)?;
            Ok(diffuse
                .into_iter()
                .zip(specular)
                .map(|(d, s)| [d, s])
                .collect())
        };
        let with_prefilter = read(prefiltered, SIGMA_PREFILTER)?;
        let with_mipmaps = read(halved, SIGMA_MIPMAPS)?;
        let amplitudes = self.probe(&cases, 1, prefiltered, SIGMA_PREFILTER)?;

        // The same lights as 32-bit floats (the format of the buffers a
        // compositor draws colour to): drawing to their levels, halving
        // them and reading them with linear filtering has to work, and to
        // give the same.
        let mut between_formats: f32 = 0.0;
        for (levels, sigma, in_half) in [
            ("prefilter", SIGMA_PREFILTER, &with_prefilter),
            ("mipmaps", SIGMA_MIPMAPS, &with_mipmaps),
        ] {
            let light = self.light_in("rgba32float", tw, th, &picture, levels)?;
            for (case, in_half) in read(light, sigma)?.iter().zip(in_half) {
                for term in 0..2 {
                    for c in 0..3 {
                        between_formats =
                            between_formats.max((case[term][c] - in_half[term][c]).abs());
                    }
                }
            }
        }
        // A 16-bit float has 11 bits: each level rounds anew.
        if between_formats > 5e-3 {
            return Err(format!(
                "a light as rgba32float and as rgba16float differ by {between_formats:.2e}"
            ));
        }

        println!();
        println!("## A light that shows a picture, against brute force");
        println!();
        println!(
            "The screen shows the desktop picture ({tw} × {th}). RGB of the diffuse term and of \
             the specular term (F = 1). `prefilter`: the levels of `update_light`. \
             `mipmaps`: the levels of `generate_mipmaps`."
        );
        println!();
        println!("| case | term | brute | prefilter | mipmaps |");
        println!("| --- | --- | --- | --- | --- |");
        let rgb = |v: [f64; 3]| format!("{:.3} {:.3} {:.3}", v[0], v[1], v[2]);
        // Largest and summed absolute error per term (diffuse, specular) and
        // per way of making the levels (prefilter, mipmaps).
        let mut largest = [[0.0_f64; 2]; 2];
        let mut sum = [[0.0_f64; 2]; 2];
        for (index, case) in cases.iter().enumerate() {
            let surface = shade::surface(
                &self.tables,
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
                &|u, v| scene::desktop_at(u, v),
                1024,
            );
            let norm = f64::from(amplitudes[index][0]);
            for (term, brute, scale) in [(0, reference.diffuse, 1.0), (1, reference.specular, norm)]
            {
                let take = |values: &[[[f32; 4]; 2]]| -> [f64; 3] {
                    std::array::from_fn(|c| f64::from(values[index][term][c]) * scale)
                };
                let shaded = [take(&with_prefilter), take(&with_mipmaps)];
                for (way, value) in shaded.iter().enumerate() {
                    for c in 0..3 {
                        let error = (value[c] - brute[c]).abs();
                        largest[term][way] = largest[term][way].max(error);
                        sum[term][way] += error / 3.0;
                    }
                }
                println!(
                    "| {} | {} | {} | {} | {} |",
                    case.name,
                    ["diffuse", "specular"][term],
                    rgb(brute),
                    rgb(shaded[0]),
                    rgb(shaded[1]),
                );
            }
        }
        let count = cases.len() as f64;
        println!();
        println!(
            "{} cases. Absolute error of a channel against brute force:",
            cases.len()
        );
        println!();
        println!("| | largest | mean |");
        println!("| --- | --- | --- |");
        for (term, term_name) in ["diffuse", "specular"].iter().enumerate() {
            for (way, way_name) in ["prefilter", "mipmaps"].iter().enumerate() {
                println!(
                    "| {term_name}, {way_name} | {:.4} | {:.4} |",
                    largest[term][way],
                    sum[term][way] / count
                );
            }
        }
        println!();
        println!(
            "The same lights as rgba32float instead of rgba16float: largest difference \
             {between_formats:.2e}"
        );
        Ok(largest[1][0])
    }

    /// A picture that moves: the light's picture scrolls by a texel per
    /// step, and the shading of fixed points is followed. Light that changes
    /// evenly with the scrolling has small second differences; levels made
    /// of large square texels make it jump.
    fn flicker(&self) -> Result<(), String> {
        let (tw, th) = (256, 144);
        let steps = 48;
        let cases = scene::textured_cases();
        // Per way of making the levels, per step, per case: diffuse and
        // specular RGB.
        let mut series: [Vec<Vec<[[f32; 4]; 2]>>; 2] = Default::default();
        for step in 0..steps {
            let picture = scene::desktop_scrolled(tw, th, step);
            for (way, (levels, sigma)) in
                [("prefilter", SIGMA_PREFILTER), ("mipmaps", SIGMA_MIPMAPS)]
                    .into_iter()
                    .enumerate()
            {
                let light = self.light(tw, th, &picture, levels)?;
                let diffuse = self.probe(&cases, 3, light, sigma)?;
                let specular = self.probe(&cases, 4, light, sigma)?;
                series[way].push(
                    diffuse
                        .into_iter()
                        .zip(specular)
                        .map(|(d, s)| [d, s])
                        .collect(),
                );
            }
        }
        println!();
        println!("## A picture that moves");
        println!();
        println!(
            "The picture ({tw} × {th}) scrolls by one texel per step, {steps} steps. For each \
             point and term: how far the value moves over all steps (the range of a channel), \
             and the mean second difference between steps (what does not follow the scrolling \
             evenly), in thousandths."
        );
        println!();
        println!("| case | term | range | unevenness: prefilter | mipmaps |");
        println!("| --- | --- | --- | --- | --- |");
        let mut totals = [[0.0_f64; 2]; 2];
        for (index, case) in cases.iter().enumerate() {
            for (term, term_name) in ["diffuse", "specular"].iter().enumerate() {
                let mut uneven = [0.0_f64; 2];
                let mut range: f64 = 0.0;
                for (way, steps_of) in series.iter().enumerate() {
                    for c in 0..3 {
                        let values: Vec<f64> = steps_of
                            .iter()
                            .map(|step| f64::from(step[index][term][c]))
                            .collect();
                        let (low, high) = values
                            .iter()
                            .fold((f64::MAX, f64::MIN), |(l, h), v| (l.min(*v), h.max(*v)));
                        range = range.max(high - low);
                        let second: f64 = values
                            .windows(3)
                            .map(|w| (w[2] - 2.0 * w[1] + w[0]).abs())
                            .sum::<f64>()
                            / (values.len() - 2) as f64;
                        uneven[way] += second / 3.0;
                    }
                    totals[term][way] += uneven[way] / cases.len() as f64;
                }
                println!(
                    "| {} | {term_name} | {:.1} | {:.3} | {:.3} |",
                    case.name,
                    range * 1000.0,
                    uneven[0] * 1000.0,
                    uneven[1] * 1000.0
                );
            }
        }
        println!();
        println!("Mean unevenness, in thousandths:");
        println!();
        println!("| term | prefilter | mipmaps |");
        println!("| --- | --- | --- |");
        for (term, term_name) in ["diffuse", "specular"].iter().enumerate() {
            println!(
                "| {term_name} | {:.3} | {:.3} |",
                totals[term][0] * 1000.0,
                totals[term][1] * 1000.0
            );
        }
        Ok(())
    }

    /// A box in the light's way: what the shader's cover leaves of each
    /// lobe, against the CPU's version of it and against brute force with
    /// the box. Returns the largest difference between the shader and the
    /// CPU, and the largest error of a term against brute force.
    fn compare_shadows(&self) -> Result<(f64, f64), String> {
        let white = self.light(1, 1, &[[1.0; 4]], "none")?;
        let cases = cases::shadowed();
        let left = self.probe(&cases, 5, white, SIGMA_MIPMAPS)?;
        println!();
        println!("## The shadow of a box, against brute force");
        println!();
        println!(
            "A box stands on the floor in front of the screen. What it leaves of the diffuse \
             and of the specular term (1: no shadow): by brute force with the box, and by the \
             shader (`ltc_box_cover` and `ltc_uncovered`)."
        );
        println!();
        println!("| case | diffuse: brute | shader | specular: brute | shader |");
        println!("| --- | --- | --- | --- | --- |");
        let mut against_cpu: f64 = 0.0;
        let (mut sum, mut largest) = ([0.0_f64; 2], [0.0_f64; 2]);
        for (index, case) in cases.iter().enumerate() {
            let surface = shade::surface(
                &self.tables,
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
            let (diffuse, specular) =
                shade::shade(&self.tables, &surface, &case.quad, Horizon::Sphere);
            let cover = shade::box_cover(&surface, &case.quad, &cases::OCCLUDER);
            let gpu = left[index].map(f64::from);
            let terms = [
                (
                    open.diffuse[0],
                    shadowed.diffuse[0],
                    gpu[0],
                    shade::uncovered(&diffuse, cover),
                ),
                (
                    open.specular[0],
                    shadowed.specular[0],
                    gpu[1],
                    shade::uncovered(&specular, cover),
                ),
            ];
            let mut cells = Vec::new();
            for (k, (open, shadowed, on_gpu, on_cpu)) in terms.into_iter().enumerate() {
                // As for the reads: where a lobe has next to no share of
                // the light, what is left of it does not count.
                against_cpu = against_cpu.max((on_gpu - on_cpu).abs() * open.min(1.0));
                let brute_left = shadowed / open.max(1e-9);
                let error = ((on_gpu - brute_left) * open).abs();
                sum[k] += error;
                largest[k] = largest[k].max(error);
                cells.push(format!("{brute_left:.3} | {on_gpu:.3}"));
                // A box leaves its own faces all of the light.
                if case.name.starts_with(cases::OWN_FACE) && !(on_gpu > 0.999 && on_gpu <= 1.0) {
                    return Err(format!(
                        "{}: the box leaves {on_gpu} of the light",
                        case.name
                    ));
                }
            }
            println!("| {} | {} |", case.name, cells.join(" | "));
        }
        println!();
        println!(
            "{} cases. Absolute error of the term (what is left, times the term without the box):",
            cases.len()
        );
        println!();
        println!("| | largest | mean |");
        println!("| --- | --- | --- |");
        for (k, name) in ["diffuse", "specular"].iter().enumerate() {
            println!(
                "| {name} | {:.4} | {:.4} |",
                largest[k],
                sum[k] / cases.len() as f64
            );
        }
        println!();
        println!(
            "The shader against the same computation on the CPU: largest difference {against_cpu:.2e}"
        );
        Ok((against_cpu, largest[0].max(largest[1])))
    }

    /// Draws a scene and reads it back as linear RGB.
    fn draw(
        &self,
        scene: &scene::Scene,
        lights: &[i64],
        width: u32,
        height: u32,
    ) -> Result<Vec<[f32; 3]>, String> {
        let data = self
            .host
            .lua()
            .create_string(scene.bytes(width as f32 / height as f32))
            .map_err(|e| e.to_string())?;
        let image = self.image(
            "draw_scene",
            ("rgba16float", width, height, data, lights.to_vec()),
        )?;
        let bytes = self
            .host
            .gpu()
            .read_image(image)
            .map_err(|e| format!("read the scene: {e}"))?;
        Ok(bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|texel| {
                std::array::from_fn(|c| {
                    half::f16::from_bits(u16::from_ne_bytes([texel[c * 2], texel[c * 2 + 1]]))
                        .to_f32()
                })
            })
            .collect())
    }

    fn shots(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let (width, height) = (1280, 720);
        let white = self.light(1, 1, &[[1.0; 4]], "none")?;
        let (tw, th) = (512, 288);
        let picture = scene::desktop(tw, th);
        let ids = scene::Pictures {
            white,
            prefiltered: self.light(tw, th, &picture, "prefilter")?,
            halved: self.light(tw, th, &picture, "mipmaps")?,
            sharp: self.light(tw, th, &picture, "none")?,
        };
        for (name, scene, lights) in scene::shots(&ids) {
            let pixels = self.draw(&scene, &lights, width, height)?;
            let path = dir.join(format!("{name}.png"));
            write_png(&path, width, height, &pixels)?;
            println!("wrote {}", path.display());
        }
        Ok(())
    }

    fn bench(&self) -> Result<(), String> {
        let (width, height) = (1920, 1080);
        let (tw, th) = (512, 288);
        let desktop = self.light(tw, th, &scene::desktop(tw, th), "prefilter")?;
        let idle = || self.host.gpu().wait_idle().map_err(|e| e.to_string());
        // Runs `frame` once to build what a first run builds, then as often
        // as fits into about two seconds (at least 3 times), and returns
        // how often and the milliseconds per run.
        let time = |frame: &dyn Fn() -> Result<(), String>| -> Result<(u32, f64), String> {
            frame()?;
            idle()?;
            let started = Instant::now();
            let mut frames = 0;
            while frames < 3 || (started.elapsed() < Duration::from_secs(2) && frames < 2000) {
                frame()?;
                frames += 1;
            }
            idle()?;
            Ok((
                frames,
                started.elapsed().as_secs_f64() * 1000.0 / f64::from(frames),
            ))
        };

        println!();
        println!("## Making the levels of a light's texture again");
        println!();
        println!("| texture | levels | runs | ms per run |");
        println!("| --- | --- | --- | --- |");
        for (lw, lh) in [(512, 288), (960, 540), (1920, 1080)] {
            let picture = scene::desktop(lw, lh);
            for levels in ["prefilter", "mipmaps"] {
                let id = self.light(lw, lh, &picture, levels)?;
                let (runs, ms) = time(&|| {
                    self.host
                        .call_in_frame("refresh_light", id)
                        .map_err(script_error("refresh_light"))
                })?;
                println!("| {lw} × {lh} | {levels} | {runs} | {ms:.3} |");
            }
        }

        println!();
        println!("## Frame time, {width} × {height}, the lit floor fills the frame");
        println!();
        println!("| lights | boxes in their way | frames | ms per frame |");
        println!("| --- | --- | --- | --- |");
        for (count, boxes) in [(0, 0), (1, 0), (2, 0), (4, 0), (4, 1), (4, 3)] {
            let mut scene = scene::bench(count);
            scene.boxes = scene::boxes().into_iter().take(boxes).collect();
            let lights = vec![desktop; 4];
            let data = self
                .host
                .lua()
                .create_string(scene.bytes(width as f32 / height as f32))
                .map_err(|e| e.to_string())?;
            let (frames, per_frame) = time(&|| {
                self.image(
                    "draw_scene",
                    ("rgba16float", width, height, data.clone(), lights.clone()),
                )
                .map(|_| ())
            })?;
            println!("| {count} | {boxes} | {frames} | {per_frame:.3} |");
        }
        Ok(())
    }
}

/// What the pictures are multiplied with before they are written: the floor
/// under a screen of the value 1 is dark, as it is in a dark room.
const SHOT_EXPOSURE: f32 = 3.0;

/// Writes linear RGB as an 8-bit sRGB PNG: times [`SHOT_EXPOSURE`], clipped
/// at 1, the sRGB curve.
fn write_png(path: &Path, width: u32, height: u32, pixels: &[[f32; 3]]) -> Result<(), String> {
    let encode = |v: f32| -> u8 {
        let v = (v * SHOT_EXPOSURE).clamp(0.0, 1.0);
        let e = if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (e * 255.0).round() as u8
    };
    let data: Vec<u8> = pixels.iter().flatten().map(|v| encode(*v)).collect();
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writer
        .write_image_data(&data)
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The area light shader of the room project, on the tests' device:
    /// it computes what the CPU's version of it computes, and both stay as
    /// near to brute force as they were measured to be.
    #[test]
    fn the_room_projects_area_lights_agree_with_the_cpu_and_with_brute_force() {
        let project =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prototype/compositor/projects/room");
        let check = Check::new(&project).unwrap();
        let worst = check.compare_all().unwrap();
        no_validation_messages().unwrap();
        worst.within_bounds().unwrap();
    }
}
