//! The OCIO pass on the GPU against OCIO on the CPU.
//!
//! Each case converts a set of floating-point colours with the pass (the
//! shader OCIO generates, compiled by `slangc`, drawn on the device under
//! test) and with OCIO's CPU processor without its fast approximations, and
//! compares the two.

use std::path::PathBuf;

use proto_compositor::ocio::pass::{self, DrawParams, Pass, Prepared};
use proto_gfx_core::{Format, Gpu, GpuConfig, Handle, ImageDesc, ImageUsage};
use proto_ocio::{Config, CpuProcessor, DisplayRequest, Dynamic, Processor};
use proto_slangc::Compiler;

const STUDIO: &str = "ocio://studio-config-latest";
/// A built-in config that still has the ACES 1 output transforms.
const STUDIO_ACES1: &str = "ocio://studio-config-v2.2.0_aces-v1.3_ocio-v2.4";

const WIDTH: u32 = 256;
const HEIGHT: u32 = 256;

/// A small deterministic generator (xorshift64*).
struct Random(u64);

impl Random {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let bits = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 40;
        bits as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, low: f32, high: f32) -> f32 {
        low + (high - low) * self.next()
    }
}

/// Scene-linear test colours: ordinary ones, dark ones, highlights far above
/// 1, slightly negative ones (out of gamut) and a wide range around zero.
fn scene_linear_points() -> Vec<[f32; 4]> {
    let total = (WIDTH * HEIGHT) as usize;
    let mut random = Random(0x1234_5678_9abc_def1);
    let mut points = Vec::with_capacity(total);
    for i in 0..total {
        let rgb: [f32; 3] = match i * 16 / total {
            0..=5 => std::array::from_fn(|_| random.range(0.0, 1.0)),
            6..=7 => std::array::from_fn(|_| random.range(0.0, 0.01)),
            // 2^-4 .. 2^6, each channel on its own.
            8..=10 => std::array::from_fn(|_| 2f32.powf(random.range(-4.0, 6.0))),
            11..=12 => std::array::from_fn(|_| random.range(-0.05, 1.0)),
            13 => std::array::from_fn(|_| random.range(-1.0, 2.0)),
            // Greys, from black to very bright.
            _ => {
                let v = 2f32.powf(random.range(-12.0, 7.0));
                [v, v, v]
            }
        };
        points.push([rgb[0], rgb[1], rgb[2], random.range(0.1, 1.0)]);
    }
    points
}

fn cache_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("ocio-pass-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn compiler() -> Compiler {
    Compiler::find().expect("slangc (the Nix dev shell provides it)")
}

fn bytes(points: &[[f32; 4]]) -> Vec<u8> {
    points
        .iter()
        .flatten()
        .flat_map(|v| v.to_le_bytes())
        .collect()
}

fn floats(bytes: &[u8]) -> Vec<[f32; 4]> {
    bytes
        .as_chunks::<16>()
        .0
        .iter()
        .map(|c| {
            std::array::from_fn(|i| f32::from_le_bytes(c[i * 4..i * 4 + 4].try_into().unwrap()))
        })
        .collect()
}

struct Stage {
    gpu: Gpu,
    vertex: Handle,
    input_slot: Handle,
    target: Handle,
}

impl Stage {
    fn new(points: &[[f32; 4]]) -> Self {
        let mut gpu = Gpu::new(GpuConfig::for_tests()).expect("a Vulkan device");
        println!("device: {}", gpu.device_name());
        let vertex = pass::create_vertex_shader(&mut gpu).unwrap();
        let desc = |usage| ImageDesc {
            format: Format::Rgba32Float,
            width: WIDTH,
            height: HEIGHT,
            usage,
        };
        let input = gpu
            .create_image(&desc(ImageUsage::SAMPLED | ImageUsage::COPY_DST))
            .unwrap();
        let target = gpu
            .create_image(&desc(ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC))
            .unwrap();
        gpu.begin_frame().unwrap();
        gpu.upload_image(input, &bytes(points)).unwrap();
        gpu.end_frame().unwrap();
        let input_slot = gpu.create_texture_slot(input).unwrap();
        Self {
            gpu,
            vertex,
            input_slot,
            target,
        }
    }

    fn make(&mut self, prepared: Prepared) -> Pass {
        self.gpu.begin_frame().unwrap();
        let pass = Pass::new(&mut self.gpu, self.vertex, prepared).expect("the pass");
        self.gpu.end_frame().unwrap();
        pass
    }

    fn draw(&mut self, pass: &mut Pass, params: DrawParams) -> Vec<[f32; 4]> {
        self.gpu.begin_frame().unwrap();
        pass.draw(&mut self.gpu, self.input_slot, self.target, params)
            .expect("the draw");
        self.gpu.end_frame().unwrap();
        self.gpu.wait_idle().unwrap();
        floats(&self.gpu.read_image(self.target).unwrap())
    }
}

/// ICtCp of BT.2100 from CIE XYZ (D65) where 1.0 is 100 cd/m², scaled as
/// BT.2124 does for ΔE ITP.
fn itp(xyz: [f32; 3]) -> [f64; 3] {
    let [x, y, z] = xyz.map(|v| f64::from(v) * 100.0);
    // XYZ -> BT.2020 RGB -> LMS (BT.2100).
    let r = 1.716_651_187_971_268 * x - 0.355_670_783_776_392 * y - 0.253_366_281_373_660 * z;
    let g = -0.666_684_351_832_489 * x + 1.616_481_236_634_939 * y + 0.015_768_545_813_911 * z;
    let b = 0.017_639_857_445_311 * x - 0.042_770_613_257_809 * y + 0.942_103_121_235_474 * z;
    let l = (1688.0 * r + 2146.0 * g + 262.0 * b) / 4096.0;
    let m = (683.0 * r + 2951.0 * g + 462.0 * b) / 4096.0;
    let s = (99.0 * r + 309.0 * g + 3688.0 * b) / 4096.0;
    let pq = |v: f64| {
        let y = (v.max(0.0) / 10000.0).powf(2610.0 / 16384.0);
        ((3424.0 / 4096.0 + 2413.0 / 128.0 * y) / (1.0 + 2392.0 / 128.0 * y)).powf(2523.0 / 32.0)
    };
    let (l, m, s) = (pq(l), pq(m), pq(s));
    let i = 0.5 * l + 0.5 * m;
    let ct = (6610.0 * l - 13613.0 * m + 7003.0 * s) / 4096.0;
    let cp = (17933.0 * l - 17390.0 * m - 543.0 * s) / 4096.0;
    [i, 0.5 * ct, cp]
}

#[derive(Debug)]
struct Difference {
    /// The largest difference in a channel. Printed, not checked.
    #[allow(dead_code)]
    max: f64,
    p99: f64,
    #[allow(dead_code)]
    mean: f64,
    /// Points that differ by more than 1e-3 in a channel.
    outliers: usize,
    delta_e_max: f64,
    delta_e_p99: f64,
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * p) as usize]
}

/// Compares what the GPU gave with the reference. `to_xyz` converts the
/// results to CIE XYZ (1.0 is 100 cd/m²) for the colour difference; points
/// whose reference has a negative XYZ component are left out of it (ΔE ITP
/// means nothing there).
fn compare(gpu: &[[f32; 4]], cpu: &[[f32; 4]], to_xyz: Option<&CpuProcessor>) -> Difference {
    assert_eq!(gpu.len(), cpu.len());
    let mut worst: Vec<f64> = gpu
        .iter()
        .zip(cpu)
        .map(|(g, c)| {
            (0..3)
                .map(|i| {
                    assert!(
                        g[i].is_finite() == c[i].is_finite(),
                        "one of {g:?} and {c:?} is not finite"
                    );
                    f64::from((g[i] - c[i]).abs())
                })
                .fold(0.0, f64::max)
        })
        .collect();
    for (g, c) in gpu.iter().zip(cpu) {
        assert!((g[3] - c[3]).abs() < 1e-6, "alpha changed: {g:?} vs {c:?}");
    }
    let outliers = worst.iter().filter(|d| **d > 1e-3).count();
    let mean = worst.iter().sum::<f64>() / worst.len() as f64;
    let max = worst.iter().copied().fold(0.0, f64::max);
    let p99 = percentile(&mut worst, 0.99);
    let (mut delta_e_max, mut delta_e_p99) = (0.0, 0.0);
    if let Some(to_xyz) = to_xyz {
        let (mut g, mut c) = (gpu.to_vec(), cpu.to_vec());
        to_xyz.apply_rgba(&mut g).unwrap();
        to_xyz.apply_rgba(&mut c).unwrap();
        let mut delta_e: Vec<f64> = g
            .iter()
            .zip(&c)
            .filter(|(_, c)| c[..3].iter().all(|v| *v >= 0.0))
            .map(|(g, c)| {
                let (a, b) = (itp([g[0], g[1], g[2]]), itp([c[0], c[1], c[2]]));
                720.0
                    * ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
            })
            .collect();
        delta_e_max = delta_e.iter().copied().fold(0.0, f64::max);
        delta_e_p99 = percentile(&mut delta_e, 0.99);
    }
    Difference {
        max,
        p99,
        mean,
        outliers,
        delta_e_max,
        delta_e_p99,
    }
}

/// The reference: the precise CPU processor on inputs prepared as the
/// pass's wrapper prepares them.
fn reference(cpu: &CpuProcessor, points: &[[f32; 4]], params: DrawParams) -> Vec<[f32; 4]> {
    let mut prepared: Vec<[f32; 4]> = points
        .iter()
        .map(|p| {
            let mut rgb = [p[0], p[1], p[2]];
            if params.premultiplied && p[3] > 0.0 {
                rgb = rgb.map(|v| v / p[3]);
            }
            let m = params.matrix;
            let rgb: [f32; 3] = std::array::from_fn(|r| {
                (m[r][0] * rgb[0] + m[r][1] * rgb[1] + m[r][2] * rgb[2]) * params.scale
            });
            [rgb[0], rgb[1], rgb[2], p[3]]
        })
        .collect();
    cpu.apply_rgba(&mut prepared).unwrap();
    if params.premultiplied {
        for p in &mut prepared {
            for c in 0..3 {
                p[c] *= p[3];
            }
        }
    }
    prepared
}

struct Limits {
    p99: f64,
    outliers: usize,
    delta_e_p99: f64,
    delta_e_max: f64,
}

/// The limits of the checks. The 99th percentile and the number of points
/// that are far off are what is checked, not the largest difference: the
/// ACES 2 output transforms have points where the last bit of the input
/// moves the result by 1e-3.
const LIMITS: Limits = Limits {
    p99: 2e-5,
    outliers: 4,
    delta_e_p99: 0.01,
    delta_e_max: 0.2,
};

fn check(name: &str, difference: &Difference) {
    println!("{name}: {difference:?}");
    assert!(difference.p99 <= LIMITS.p99, "{name}: {difference:?}");
    assert!(
        difference.outliers <= LIMITS.outliers,
        "{name}: {difference:?}"
    );
    assert!(
        difference.delta_e_p99 <= LIMITS.delta_e_p99,
        "{name}: {difference:?}"
    );
    assert!(
        difference.delta_e_max <= LIMITS.delta_e_max,
        "{name}: {difference:?}"
    );
}

fn display_case(config_spec: &str, display: &str, view: &str) {
    proto_ocio::quiet();
    let name = format!("{display} / {view}");
    let config = Config::from_file(config_spec).unwrap();
    assert!(
        config.views(display).iter().any(|v| v == view),
        "{config_spec} has no view {view:?} for {display:?}: {:?}",
        config.views(display)
    );
    let processor = config
        .display_processor(&DisplayRequest::new("ACEScg", display, view))
        .unwrap();
    let to_xyz = config
        .processor(display, &config.role("cie_xyz_d65_interchange").unwrap())
        .unwrap()
        .cpu(true)
        .unwrap();
    let cache = cache_dir(&name.replace(|c: char| !c.is_ascii_alphanumeric(), "_"));
    let prepared = pass::prepare(&processor, Some(&compiler()), Some(&cache)).unwrap();
    assert!(!prepared.cached);
    println!(
        "{name}: {} bytes of SPIR-V, {} lookup tables, OCIO {:?}, slangc {:?}",
        prepared.spirv.len(),
        prepared.textures.len(),
        prepared.timing.extract,
        prepared.timing.compile
    );
    let points = scene_linear_points();
    let mut stage = Stage::new(&points);
    let mut pass = stage.make(prepared);
    let params = DrawParams::default();
    let gpu = stage.draw(&mut pass, params);
    let cpu = reference(&processor.cpu(true).unwrap(), &points, params);
    check(&name, &compare(&gpu, &cpu, Some(&to_xyz)));
    pass.destroy(&mut stage.gpu);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn srgb_without_tone_mapping() {
    display_case(STUDIO, "sRGB - Display", "Un-tone-mapped");
}

#[test]
fn srgb_aces2_sdr() {
    display_case(
        STUDIO,
        "sRGB - Display",
        "ACES 2.0 - SDR 100 nits (Rec.709)",
    );
}

#[test]
fn srgb_aces1_sdr() {
    display_case(STUDIO_ACES1, "sRGB - Display", "ACES 1.0 - SDR Video");
}

#[test]
fn pq_without_tone_mapping() {
    display_case(STUDIO, "Rec.2100-PQ - Display", "Un-tone-mapped");
}

#[test]
fn pq_aces2_hdr() {
    display_case(
        STUDIO,
        "Rec.2100-PQ - Display",
        "ACES 2.0 - HDR 1000 nits (Rec.2020)",
    );
}

#[test]
fn display_p3_aces2_sdr() {
    display_case(
        STUDIO,
        "Display P3 - Display",
        "ACES 2.0 - SDR 100 nits (P3 D65)",
    );
}

/// A conversion between two colour spaces, and what the wrapper adds: the
/// matrix and the scale in front, and premultiplied alpha around it.
#[test]
fn colorspace_conversion_with_matrix_scale_and_alpha() {
    proto_ocio::quiet();
    let config = Config::from_file(STUDIO).unwrap();
    let processor: Processor = config.processor("ACEScg", "ACEScct").unwrap();
    let prepared = pass::prepare(&processor, Some(&compiler()), None).unwrap();
    let points = scene_linear_points();
    let mut stage = Stage::new(&points);
    let mut pass = stage.make(prepared);
    let cpu = processor.cpu(true).unwrap();
    for params in [
        DrawParams::default(),
        DrawParams {
            scale: 2.03,
            premultiplied: true,
            matrix: [[0.9, 0.08, 0.02], [0.05, 0.9, 0.05], [0.01, 0.09, 0.9]],
        },
    ] {
        let gpu = stage.draw(&mut pass, params);
        let difference = compare(&gpu, &reference(&cpu, &points, params), None);
        println!("ACEScg -> ACEScct, {params:?}: {difference:?}");
        // ACEScct is a logarithm: the differences are those of the inputs
        // the wrapper computes (a division and a matrix in 32-bit floats).
        assert!(difference.p99 <= 2e-5, "{difference:?}");
        assert!(difference.outliers == 0, "{difference:?}");
    }
    pass.destroy(&mut stage.gpu);
}

/// Exposure and gamma change what is drawn without a new shader, and
/// follow the CPU processor with the same values.
#[test]
fn dynamic_exposure_and_gamma_follow_without_compiling() {
    proto_ocio::quiet();
    let config = Config::from_file(STUDIO).unwrap();
    let mut request = DisplayRequest::new(
        "ACEScg",
        "sRGB - Display",
        "ACES 2.0 - SDR 100 nits (Rec.709)",
    );
    request.dynamic_exposure = true;
    request.dynamic_gamma = true;
    let processor = config.display_processor(&request).unwrap();
    let to_xyz = config
        .processor(
            "sRGB - Display",
            &config.role("cie_xyz_d65_interchange").unwrap(),
        )
        .unwrap()
        .cpu(true)
        .unwrap();
    let prepared = pass::prepare(&processor, Some(&compiler()), None).unwrap();
    assert_eq!(prepared.uniforms.len(), 2, "{:?}", prepared.uniforms);
    let points = scene_linear_points();
    let mut stage = Stage::new(&points);
    let mut pass = stage.make(prepared);
    assert!(pass.has_dynamic(Dynamic::Exposure) && pass.has_dynamic(Dynamic::Gamma));
    let mut cpu = processor.cpu(true).unwrap();
    let mut previous: Option<Vec<[f32; 4]>> = None;
    for (exposure, gamma) in [(0.0, 1.0), (-2.0, 1.0), (1.5, 0.9), (4.0, 1.2)] {
        pass.set_dynamic(&mut stage.gpu, Dynamic::Exposure, exposure)
            .unwrap();
        pass.set_dynamic(&mut stage.gpu, Dynamic::Gamma, gamma)
            .unwrap();
        cpu.set_dynamic(Dynamic::Exposure, exposure).unwrap();
        cpu.set_dynamic(Dynamic::Gamma, gamma).unwrap();
        let gpu = stage.draw(&mut pass, DrawParams::default());
        let expected = reference(&cpu, &points, DrawParams::default());
        check(
            &format!("exposure {exposure}, gamma {gamma}"),
            &compare(&gpu, &expected, Some(&to_xyz)),
        );
        if let Some(previous) = &previous {
            assert!(previous != &gpu, "the controls changed nothing");
        }
        previous = Some(gpu);
    }
    pass.destroy(&mut stage.gpu);
}

/// How long the pass takes on the device under test, for a few views: the
/// time to make its GPU objects, the first draw (which makes the pipeline)
/// and the following draws of an output-sized image, each waited for. Run
/// by hand (`-- --ignored --nocapture how_long`); the size is
/// `OCIO_TIMING_SIZE` (`WIDTHxHEIGHT`, default 3840x2160). The values of a
/// small image are compared with the CPU on the way.
#[test]
#[ignore = "a measurement, not a check"]
fn how_long_the_pass_takes() {
    use std::time::Instant;

    proto_ocio::quiet();
    let (width, height) = std::env::var("OCIO_TIMING_SIZE")
        .ok()
        .and_then(|text| {
            let (w, h) = text.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((3840u32, 2160u32));
    let points = scene_linear_points();
    let mut stage = Stage::new(&points);
    // An output-sized input (the test colours, repeated) and target.
    let desc = |format, usage| ImageDesc {
        format,
        width,
        height,
        usage,
    };
    let large = stage
        .gpu
        .create_image(&desc(
            Format::Rgba32Float,
            ImageUsage::SAMPLED | ImageUsage::COPY_DST,
        ))
        .unwrap();
    let large_target = stage
        .gpu
        .create_image(&desc(Format::Rgba8Unorm, ImageUsage::COLOR_TARGET))
        .unwrap();
    let pixels: Vec<[f32; 4]> = points
        .iter()
        .copied()
        .cycle()
        .take((width * height) as usize)
        .collect();
    stage.gpu.begin_frame().unwrap();
    stage.gpu.upload_image(large, &bytes(&pixels)).unwrap();
    stage.gpu.end_frame().unwrap();
    stage.gpu.wait_idle().unwrap();
    let large_slot = stage.gpu.create_texture_slot(large).unwrap();

    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
    for (config_spec, display, view) in [
        (STUDIO, "sRGB - Display", "Un-tone-mapped"),
        (
            STUDIO,
            "sRGB - Display",
            "ACES 2.0 - SDR 100 nits (Rec.709)",
        ),
        (STUDIO_ACES1, "sRGB - Display", "ACES 1.0 - SDR Video"),
        (
            STUDIO,
            "Rec.2100-PQ - Display",
            "ACES 2.0 - HDR 1000 nits (Rec.2020)",
        ),
    ] {
        let config = Config::from_file(config_spec).unwrap();
        let processor = config
            .display_processor(&DisplayRequest::new("ACEScg", display, view))
            .unwrap();
        let prepared = pass::prepare(&processor, Some(&compiler()), None).unwrap();
        let timing = prepared.timing;
        let started = Instant::now();
        let mut pass = stage.make(prepared);
        stage.gpu.wait_idle().unwrap();
        let objects = ms(started);

        let gpu = stage.draw(&mut pass, DrawParams::default());
        let cpu = reference(
            &processor.cpu(true).unwrap(),
            &points,
            DrawParams::default(),
        );
        let difference = compare(&gpu, &cpu, None);

        let mut draws = Vec::new();
        for _ in 0..21 {
            let started = Instant::now();
            stage.gpu.begin_frame().unwrap();
            pass.draw(
                &mut stage.gpu,
                large_slot,
                large_target,
                DrawParams::default(),
            )
            .unwrap();
            stage.gpu.end_frame().unwrap();
            stage.gpu.wait_idle().unwrap();
            draws.push(ms(started));
        }
        let first = draws.remove(0);
        draws.sort_by(f64::total_cmp);
        println!(
            "{display} / {view} on {}: OCIO {:.1} ms, slangc {:.1} ms, GPU objects {objects:.1} ms; \
             {width}x{height}: first draw {first:.1} ms, then median {:.2} ms, max {:.2} ms; \
             against the CPU: max {:.2e}, p99 {:.2e}, outliers {}",
            stage.gpu.device_name(),
            timing.extract.as_secs_f64() * 1e3,
            timing.compile.as_secs_f64() * 1e3,
            draws[draws.len() / 2],
            draws[draws.len() - 1],
            difference.max,
            difference.p99,
            difference.outliers,
        );
        pass.destroy(&mut stage.gpu);
    }
}

/// A config of the test's own with a lookup table from a file: the table
/// becomes a 3D texture. With tetrahedral interpolation the shader reads
/// single texels and interpolates itself; with linear interpolation the
/// sampler does, which is less exact.
#[test]
fn a_3d_lookup_table_from_a_file() {
    proto_ocio::quiet();
    let dir = cache_dir("lut3d");
    std::fs::create_dir_all(&dir).unwrap();
    // A 17^3 table in the .cube format (red changes fastest): a smooth
    // change of the colours that no matrix or curve per channel could make.
    const N: usize = 17;
    let mut cube = format!("LUT_3D_SIZE {N}\n");
    for b in 0..N {
        for g in 0..N {
            for r in 0..N {
                let [r, g, b] = [r, g, b].map(|v| v as f32 / (N - 1) as f32);
                let out = [
                    r.powf(0.8) * (1.0 - 0.3 * g * b),
                    (0.9 * g + 0.1 * r * b).sqrt() * 0.95,
                    b * b * (0.6 + 0.4 * r) + 0.05 * g,
                ];
                cube.push_str(&format!("{:.7} {:.7} {:.7}\n", out[0], out[1], out[2]));
            }
        }
    }
    std::fs::write(dir.join("look.cube"), cube).unwrap();
    std::fs::write(
        dir.join("config.ocio"),
        r#"ocio_profile_version: 2

search_path: "."
roles:
  default: raw
  scene_linear: lin
displays:
  monitor:
    - !<View> {name: tetrahedral, colorspace: graded}
    - !<View> {name: linear, colorspace: graded_linear}
colorspaces:
  - !<ColorSpace>
    name: raw
    isdata: true
  - !<ColorSpace>
    name: lin
  - !<ColorSpace>
    name: graded
    from_scene_reference: !<FileTransform> {src: look.cube, interpolation: tetrahedral}
  - !<ColorSpace>
    name: graded_linear
    from_scene_reference: !<FileTransform> {src: look.cube, interpolation: linear}
"#,
    )
    .unwrap();
    let config = Config::from_path(&dir.join("config.ocio")).unwrap();
    assert_eq!(config.views("monitor"), ["tetrahedral", "linear"]);

    let points = scene_linear_points();
    let mut stage = Stage::new(&points);
    for (view, p99_limit) in [("tetrahedral", 2e-5), ("linear", 2e-3)] {
        let processor = config
            .display_processor(&DisplayRequest::new("lin", "monitor", view))
            .unwrap();
        let prepared = pass::prepare(&processor, Some(&compiler()), None).unwrap();
        println!(
            "{view}: {:?}",
            prepared
                .textures
                .iter()
                .map(|t| (
                    t.name.clone(),
                    t.width,
                    t.height,
                    t.depth,
                    t.channels,
                    t.linear
                ))
                .collect::<Vec<_>>()
        );
        assert!(prepared.textures.iter().any(|t| t.is_3d()));
        let mut pass = stage.make(prepared);
        let gpu = stage.draw(&mut pass, DrawParams::default());
        let cpu = reference(
            &processor.cpu(true).unwrap(),
            &points,
            DrawParams::default(),
        );
        let difference = compare(&gpu, &cpu, None);
        println!("3D lookup table, {view}: {difference:?}");
        assert!(difference.p99 <= p99_limit, "{view}: {difference:?}");
        assert!(
            difference.max <= 100.0 * p99_limit,
            "{view}: {difference:?}"
        );
        // The table does something.
        assert!(
            gpu.iter()
                .zip(&points)
                .any(|(g, p)| (g[0] - p[0]).abs() > 0.05)
        );
        pass.destroy(&mut stage.gpu);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The SPIR-V is kept on disk under the conversion's identifier and found
/// there the next time; without `slangc` a conversion is refused, unless it
/// is in the cache.
#[test]
fn compiled_shaders_are_cached_and_a_missing_compiler_is_an_error() {
    proto_ocio::quiet();
    let config = Config::from_file(STUDIO).unwrap();
    let processor = config
        .display_processor(&DisplayRequest::new(
            "ACEScg",
            "sRGB - Display",
            "Un-tone-mapped",
        ))
        .unwrap();
    let cache = cache_dir("cache");
    let compiler = compiler();

    let error = pass::prepare(&processor, None, Some(&cache)).err().unwrap();
    println!("{error}");
    assert!(error.contains("slangc"), "{error}");

    let first = pass::prepare(&processor, Some(&compiler), Some(&cache)).unwrap();
    assert!(!first.cached);
    let files: Vec<_> = std::fs::read_dir(&cache)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    println!("{files:?}");
    assert_eq!(files.len(), 1);
    println!("cache ID: {:?}", first.cache_id);
    assert!(files[0].starts_with(&pass::cache_name(&first.cache_id)) && files[0].ends_with(".spv"));

    let second = pass::prepare(&processor, Some(&compiler), Some(&cache)).unwrap();
    assert!(second.cached);
    assert_eq!(second.spirv, first.spirv);
    assert_eq!(second.cache_id, first.cache_id);

    // Another conversion gets another file.
    let other = config
        .display_processor(&DisplayRequest::new(
            "ACEScg",
            "Display P3 - Display",
            "Un-tone-mapped",
        ))
        .unwrap();
    assert!(
        !pass::prepare(&other, Some(&compiler), Some(&cache))
            .unwrap()
            .cached
    );
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 2);

    // A file that is not SPIR-V is compiled over.
    let file = cache.join(&files[0]);
    std::fs::write(&file, b"not SPIR-V").unwrap();
    let again = pass::prepare(&processor, Some(&compiler), Some(&cache)).unwrap();
    assert!(!again.cached);
    assert_eq!(std::fs::read(&file).unwrap(), first.spirv);

    std::fs::remove_dir_all(&cache).unwrap();
}
