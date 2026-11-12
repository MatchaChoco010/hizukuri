//! Measurements of the script binding: the cost of a call, a 4K frame, the
//! garbage collector and a reload.
//!
//! ```sh
//! cargo run --release --bin gfx-bench -- calls|frame4k|gc|reload|all [--quick]
//! ```
//!
//! Times are wall-clock on a machine that also runs other things; the tables
//! report medians (and minima) over many frames.

use std::time::{Duration, Instant};

use proto_gfx_core::*;
use proto_gfx_lua::{HostOptions, ScriptHost, VM_NAME};

fn script_source() -> String {
    let path = format!("{}/scripts/bench.lua", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

fn device_line(host: &ScriptHost) -> String {
    let gpu = host.gpu();
    format!(
        "{} ({}){}",
        gpu.device_name(),
        gpu.driver_name(),
        if std::env::var("GFX_SPIKE_VALIDATION").is_ok_and(|v| v == "1") {
            ", validation layer ON"
        } else {
            ""
        }
    )
}

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// Sorted copy plus the usual percentiles.
struct Dist(Vec<f64>);

impl Dist {
    fn new(mut values: Vec<f64>) -> Self {
        values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Self(values)
    }
    fn at(&self, q: f64) -> f64 {
        let i = ((self.0.len() - 1) as f64 * q).round() as usize;
        self.0[i]
    }
    fn median(&self) -> f64 {
        self.at(0.5)
    }
    fn min(&self) -> f64 {
        self.0[0]
    }
    fn max(&self) -> f64 {
        *self.0.last().unwrap()
    }
}

struct Bench {
    host: ScriptHost,
    target: Handle,
    textures: Vec<Handle>,
}

fn solid_texture(gpu: &mut Gpu, size: u32, color: [u8; 4]) -> Handle {
    let image = gpu
        .create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: size,
            height: size,
            usage: ImageUsage::SAMPLED | ImageUsage::COPY_DST,
        })
        .unwrap();
    gpu.begin_frame().unwrap();
    gpu.upload_image(image, &color.repeat((size * size) as usize))
        .unwrap();
    gpu.end_frame().unwrap();
    image
}

impl Bench {
    fn new(mut options: HostOptions) -> Self {
        options
            .shader_loader
            .get_or_insert_with(proto_gfx_lua::shaders::examples);
        let gpu = Gpu::new(GpuConfig::from_env()).expect("gpu");
        let host = ScriptHost::new(gpu, options).expect("host");
        let textures: Vec<Handle> = (0..16u8)
            .map(|i| solid_texture(&mut host.gpu(), 16, [i * 16, 255 - i * 16, 128, 255]))
            .collect();
        let target = host
            .gpu()
            .create_image(&ImageDesc {
                format: Format::Rgba8Unorm,
                width: 256,
                height: 256,
                usage: ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
            })
            .unwrap();
        host.load("bench.lua", &script_source()).expect("load");
        let list: Vec<i64> = textures.iter().map(|t| t.to_bits()).collect();
        host.call_in_frame("init", list).expect("init");
        Self {
            host,
            target,
            textures,
        }
    }

    /// Runs `frames` frames, timing the recording closure, the submission and
    /// the GPU separately. Returns (record, submit, gpu) in microseconds.
    fn frames(&self, frames: usize, mut record: impl FnMut(usize)) -> (Dist, Dist, Dist) {
        let mut r = Vec::new();
        let mut s = Vec::new();
        let mut g = Vec::new();
        for frame in 0..frames + 5 {
            self.host.gpu().begin_frame().unwrap();
            let t0 = Instant::now();
            record(frame);
            let t1 = Instant::now();
            self.host.gpu().end_frame().unwrap();
            let t2 = Instant::now();
            self.host.gpu().wait_idle().unwrap();
            let t3 = Instant::now();
            if frame >= 5 {
                r.push(micros(t1 - t0));
                s.push(micros(t2 - t1));
                g.push(micros(t3 - t2));
            }
        }
        (Dist::new(r), Dist::new(s), Dist::new(g))
    }
}

fn f32_bytes<const N: usize>(values: [f32; N]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_ne_bytes()).collect()
}

fn calls(quick: bool) {
    let frames = if quick { 10 } else { 200 };
    let with_budget = Bench::new(HostOptions {
        memory_limit: None,
        ..HostOptions::default()
    });
    let without_budget = Bench::new(HostOptions {
        memory_limit: None,
        time_budget: None,
        ..HostOptions::default()
    });
    println!(
        "## calls: {} on {}\n",
        VM_NAME,
        device_line(&with_budget.host)
    );
    println!("One unit = push data + bind a texture slot + draw 6 vertices (3 API calls).");
    println!("CPU time to record one frame, median of {frames} frames.\n");
    println!(
        "| variant | 100 units | 1000 units | 10000 units | per unit at 10000 (median) | (fastest frame) |"
    );
    println!("| --- | --- | --- | --- | --- | --- |");

    let (pipeline, sampler, slots): (i64, i64, Vec<i64>) = {
        let export: mlua::Function = with_budget.host.lua().globals().get("export").unwrap();
        export.call(()).unwrap()
    };
    let (pipeline, sampler) = (Handle::from_bits(pipeline), Handle::from_bits(sampler));
    let slots: Vec<Handle> = slots.into_iter().map(Handle::from_bits).collect();

    let row = |name: &str, bench: &Bench, record: &mut dyn FnMut(&Bench, usize)| {
        let mut cells = Vec::new();
        let mut per_unit = (0.0, 0.0);
        for n in [100usize, 1000, 10_000] {
            let (r, _, _) = bench.frames(frames, |_| record(bench, n));
            cells.push(format!("{:.1} µs", r.median()));
            per_unit = (r.median() * 1000.0 / n as f64, r.min() * 1000.0 / n as f64);
        }
        println!(
            "| {name} | {} | {:.0} ns | {:.0} ns |",
            cells.join(" | "),
            per_unit.0,
            per_unit.1
        );
    };

    let rust_loop = |bench: &Bench, n: usize| {
        let mut gpu = bench.host.gpu();
        gpu.begin_pass(&[ColorAttachment {
            image: bench.target,
            load: LoadOp::Clear([0.0, 0.0, 0.0, 1.0]),
            store: true,
        }])
        .unwrap();
        gpu.set_pipeline(pipeline).unwrap();
        gpu.bind(1, sampler).unwrap();
        gpu.push(
            0,
            &f32_bytes([
                0.0, 0.0, 4.0, 4.0, 256.0, 256.0, 1.0, 0.0, 1.0, 1.0, 1.0, 1.0,
            ]),
        )
        .unwrap();
        for i in 1..=n {
            let x = ((i % 64) * 4) as f32;
            let y = ((i / 64 % 64) * 4) as f32;
            let mut bytes = [0u8; 16];
            for (k, v) in [x, y, 4.0, 4.0].iter().enumerate() {
                bytes[k * 4..k * 4 + 4].copy_from_slice(&v.to_ne_bytes());
            }
            gpu.push(0, &bytes).unwrap();
            gpu.bind(0, slots[i % slots.len()]).unwrap();
            gpu.draw(6, 1, 0, 0).unwrap();
        }
        gpu.end_pass().unwrap();
    };
    let script = |function: &'static str| {
        move |bench: &Bench, n: usize| {
            bench
                .host
                .call(function, (bench.target.to_bits(), n))
                .unwrap();
        }
    };
    let set_checks = |bench: &Bench, on: bool| {
        // SAFETY: the benchmark only makes calls that pass with the checks on.
        unsafe { bench.host.gpu().set_checks(on) };
    };

    row("Rust, checks on", &with_budget, &mut |b, n| rust_loop(b, n));
    set_checks(&with_budget, false);
    row("Rust, checks off", &with_budget, &mut |b, n| {
        rust_loop(b, n)
    });
    set_checks(&with_budget, true);
    row(
        "script, 12 numbers, checks on",
        &with_budget,
        &mut script("frame_f32x12"),
    );
    row(
        "script, 4 numbers, checks on",
        &with_budget,
        &mut script("frame_f32x4"),
    );
    set_checks(&with_budget, false);
    row(
        "script, 4 numbers, checks off",
        &with_budget,
        &mut script("frame_f32x4"),
    );
    set_checks(&with_budget, true);
    row(
        "script, packed bytes, checks on",
        &with_budget,
        &mut script("frame_bytes"),
    );
    row(
        "script, 4 numbers, checks on, no time budget",
        &without_budget,
        &mut script("frame_f32x4"),
    );
    row(
        "script, 3 no-op calls per unit",
        &with_budget,
        &mut script("frame_noop"),
    );
    row(
        "script, 3 no-op calls per unit, no time budget",
        &without_budget,
        &mut script("frame_noop"),
    );
    row(
        "script, loop only",
        &with_budget,
        &mut script("frame_empty"),
    );
    row(
        "script, loop only, no time budget",
        &without_budget,
        &mut script("frame_empty"),
    );

    // One instanced draw: per-quad data goes into a script-side buffer, the
    // texture is chosen in the shader from a texture array. Luau only (uses
    // the `buffer` type).
    #[cfg(feature = "luau")]
    {
        let batched = Bench::new(HostOptions {
            memory_limit: None,
            ..HostOptions::default()
        });
        batched.host.reset().unwrap();
        let path = format!("{}/scripts/batched.luau", env!("CARGO_MANIFEST_DIR"));
        batched
            .host
            .load("batched.luau", &std::fs::read_to_string(path).unwrap())
            .unwrap();
        let textures: Vec<i64> = (0..8u8)
            .map(|i| solid_texture(&mut batched.host.gpu(), 16, [i * 30, 255, 0, 255]).to_bits())
            .collect();
        let target = batched
            .host
            .gpu()
            .create_image(&ImageDesc {
                format: Format::Rgba8Unorm,
                width: 256,
                height: 256,
                usage: ImageUsage::COLOR_TARGET,
            })
            .unwrap();
        batched.host.call_in_frame("init", textures).unwrap();
        let mut cells = Vec::new();
        let mut per_unit = (0.0, 0.0);
        for n in [100usize, 1000, 10_000] {
            let (r, _, _) = batched.frames(frames, |_| {
                batched
                    .host
                    .call("frame_grid", (target.to_bits(), n))
                    .unwrap();
            });
            cells.push(format!("{:.1} µs", r.median()));
            per_unit = (r.median() * 1000.0 / n as f64, r.min() * 1000.0 / n as f64);
        }
        println!(
            "| script, batched: 5 buffer writes per unit, 1 instanced draw per frame | {} | {:.0} ns | {:.0} ns |",
            cells.join(" | "),
            per_unit.0,
            per_unit.1
        );
    }

    // The error path, measured apart: it is far slower than a successful call.
    for n in [10usize, 100, 1000] {
        let (r, _, _) = with_budget.frames(frames.min(10), |_| {
            with_budget
                .host
                .call("frame_errors", (with_budget.target.to_bits(), n))
                .unwrap();
        });
        println!(
            "\nfailing call caught by `pcall`, {n} per frame: {:.1} µs per call",
            r.median() / n as f64
        );
    }

    // Submission and GPU time for the same frames, for scale.
    for n in [100usize, 1000, 10_000] {
        let (r, s, g) = with_budget.frames(frames, |_| {
            with_budget
                .host
                .call("frame_f32x4", (with_budget.target.to_bits(), n))
                .unwrap();
        });
        println!(
            "\n{n} units: record {:.1} µs (min {:.1}), submit {:.1} µs, GPU until idle {:.1} µs",
            r.median(),
            r.min(),
            s.median(),
            g.median()
        );
    }
    let stats = with_budget.host.gpu().stats();
    println!(
        "\npush updates per draw: {:.2}; layout transitions per frame: {:.1}",
        stats.push_updates as f64 / stats.draws as f64,
        stats.barriers as f64 / stats.frames_submitted as f64
    );
    println!();
}

fn frame4k(quick: bool) {
    let frames = if quick { 5 } else { 30 };
    let bench = Bench::new(HostOptions {
        memory_limit: None,
        ..HostOptions::default()
    });
    let (width, height) = (3840u32, 2160u32);
    let output = bench
        .host
        .gpu()
        .create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width,
            height,
            usage: ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
        })
        .unwrap();
    bench
        .host
        .call_in_frame("init_4k", (width, height))
        .unwrap();
    println!("## 4K frame: {} on {}\n", VM_NAME, device_line(&bench.host));
    println!("3840x2160. Pass 1: n textured quads tiling the screen into RGBA16F with blending.");
    println!("Pass 2: full-screen sRGB encode into RGBA8. Median of {frames} frames.\n");
    println!(
        "| windows | script (record) | submit | GPU until idle | total | of a 6.94 ms frame |"
    );
    println!("| --- | --- | --- | --- | --- | --- |");
    for n in [1usize, 10, 100, 1000, 10_000, 30_000] {
        let (r, s, g) = bench.frames(frames, |tick| {
            bench
                .host
                .call("frame_4k", (output.to_bits(), n, width, height, tick))
                .unwrap();
        });
        let total = r.median() + s.median() + g.median();
        println!(
            "| {n} | {:.0} µs | {:.0} µs | {:.0} µs | {:.2} ms | {:.0}% |",
            r.median(),
            s.median(),
            g.median(),
            total / 1000.0,
            total / 6944.0 * 100.0
        );
    }
    let _ = &bench.textures;
    println!();
}

fn gc(quick: bool) {
    let frames = if quick { 300 } else { 3000 };
    println!("## garbage collection: {VM_NAME}\n");
    println!("Each frame allocates 1000 short-lived tables and replaces 8 live nodes.");
    println!("No GPU calls. {frames} frames after warm-up. Times in µs.\n");
    println!(
        "| live nodes | kept in | mode | heap | frame p50 | p99 | p99.9 | max | GC step p50 | max | GC steps/frame |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    // (live nodes, nodes per table)
    let layouts: &[(usize, usize)] = if quick {
        &[(10_000, 10_000)]
    } else {
        &[(10_000, 10_000), (1_000_000, 1_000_000), (1_000_000, 1000)]
    };
    for &(live, chunk) in layouts {
        let kept_in = if chunk == live {
            "one array".to_owned()
        } else {
            format!("{} arrays of {chunk}", live / chunk)
        };
        let mut modes = vec!["automatic", "stepped by the host (300 µs slice)"];
        if cfg!(feature = "lua55") {
            modes.push("generational");
        }
        for mode in modes {
            let gpu = Gpu::new(GpuConfig::from_env()).expect("gpu");
            let host = ScriptHost::new(
                gpu,
                HostOptions {
                    memory_limit: None,
                    time_budget: Some(Duration::from_secs(60)),
                    shader_loader: Some(proto_gfx_lua::shaders::examples()),
                    ..HostOptions::default()
                },
            )
            .unwrap();
            host.load("bench.lua", &script_source()).unwrap();
            host.call("gc_setup", (live, chunk)).unwrap();
            let lua = host.lua();
            lua.gc_collect().unwrap();
            lua.gc_collect().unwrap();
            let stepped = mode.starts_with("stepped");
            #[cfg(feature = "lua55")]
            if mode == "generational" {
                lua.gc_set_mode(mlua::state::GcMode::Generational(
                    mlua::state::GcGenParams::default(),
                ));
            }
            if stepped {
                lua.gc_stop();
            }
            let function: mlua::Function = lua.globals().get("gc_frame").unwrap();
            let mut frame_times = Vec::new();
            let mut step_times = Vec::new();
            let mut steps = 0usize;
            let warmup = frames / 10;
            for frame in 0..frames + warmup {
                let t0 = Instant::now();
                function.call::<()>(1000).unwrap();
                let elapsed = t0.elapsed();
                if stepped {
                    // Advance the collector between frames, in a fixed slice.
                    let slice_start = Instant::now();
                    loop {
                        let s0 = Instant::now();
                        let finished = lua.gc_step().unwrap();
                        lua.gc_stop();
                        if frame >= warmup {
                            step_times.push(micros(s0.elapsed()));
                            steps += 1;
                        }
                        if finished || slice_start.elapsed() > Duration::from_micros(300) {
                            break;
                        }
                    }
                }
                if frame >= warmup {
                    frame_times.push(micros(elapsed));
                }
            }
            let f = Dist::new(frame_times);
            let (step_p50, step_max, per_frame) = if step_times.is_empty() {
                ("-".to_owned(), "-".to_owned(), "-".to_owned())
            } else {
                let s = Dist::new(step_times);
                (
                    format!("{:.0}", s.median()),
                    format!("{:.0}", s.max()),
                    format!("{:.1}", steps as f64 / frames as f64),
                )
            };
            println!(
                "| {live} | {kept_in} | {mode} | {} MiB | {:.0} | {:.0} | {:.0} | {:.0} | {step_p50} | {step_max} | {per_frame} |",
                lua.used_memory() >> 20,
                f.median(),
                f.at(0.99),
                f.at(0.999),
                f.max(),
            );
        }
    }
    println!();
}

fn reload(quick: bool) {
    let rounds = if quick { 3 } else { 15 };
    let bench = Bench::new(HostOptions {
        memory_limit: None,
        ..HostOptions::default()
    });
    println!("## reload: {} on {}\n", VM_NAME, device_line(&bench.host));
    let source = script_source();
    // A long script: the benchmark script followed by 5000 small functions.
    let mut long = source.clone();
    for i in 0..5000 {
        long.push_str(&format!(
            "local function helper_{i}(a, b)\n    local t = {{ a = a, b = b, sum = a + b + {i} }}\n    return t.sum * 2\nend\n"
        ));
        if i % 150 == 149 {
            // Keep the number of locals per scope within the VM's limit.
            long.push_str("do end\n");
        }
    }
    let long = long.replace("local function helper_", "function helper_");
    let textures: Vec<i64> = bench.textures.iter().map(|t| t.to_bits()).collect();
    println!(
        "| script | size | reset | load (compile + run chunk) | init (2 shaders, 1 pipeline, 1 sampler, 16 slots) | first frame | total |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- |");
    for (name, text) in [
        ("bench.lua", &source),
        ("bench.lua + 5000 functions", &long),
    ] {
        let mut reset = Vec::new();
        let mut load = Vec::new();
        let mut init = Vec::new();
        let mut first = Vec::new();
        for _ in 0..rounds {
            // `reset` destroys the host's textures too, so recreate them.
            let t0 = Instant::now();
            bench.host.reset().unwrap();
            let t1 = Instant::now();
            bench.host.load(name, text).unwrap();
            let t2 = Instant::now();
            let fresh: Vec<i64> = (0..textures.len() as u8)
                .map(|i| solid_texture(&mut bench.host.gpu(), 16, [i, i, i, 255]).to_bits())
                .collect();
            let target = bench
                .host
                .gpu()
                .create_image(&ImageDesc {
                    format: Format::Rgba8Unorm,
                    width: 256,
                    height: 256,
                    usage: ImageUsage::COLOR_TARGET,
                })
                .unwrap();
            let t3 = Instant::now();
            bench.host.call_in_frame("init", fresh).unwrap();
            let t4 = Instant::now();
            bench
                .host
                .call_in_frame("frame_f32x4", (target.to_bits(), 100))
                .unwrap();
            bench.host.gpu().wait_idle().unwrap();
            let t5 = Instant::now();
            reset.push(micros(t1 - t0));
            load.push(micros(t2 - t1));
            init.push(micros(t4 - t3));
            first.push(micros(t5 - t4));
        }
        println!(
            "<!-- {name}, first round (cold driver caches): load {:.0} µs, init {:.0} µs, first frame {:.0} µs -->",
            load[0], init[0], first[0]
        );
        let (reset, load, init, first) = (
            Dist::new(reset),
            Dist::new(load),
            Dist::new(init),
            Dist::new(first),
        );
        println!(
            "| {name} | {} lines, {} KiB | {:.0} µs | {:.0} µs | {:.0} µs | {:.0} µs | {:.2} ms |",
            text.lines().count(),
            text.len() >> 10,
            reset.median(),
            load.median(),
            init.median(),
            first.median(),
            (reset.median() + load.median() + init.median() + first.median()) / 1000.0
        );
    }

    // Writing descriptors into the heap, as filling every slot would need.
    {
        let mut gpu = bench.host.gpu();
        let image = solid_texture(&mut gpu, 4, [255; 4]);
        gpu.wait_idle().unwrap();
        let images = vec![image; 2048];
        let t0 = Instant::now();
        let slots = gpu.create_texture_slot_run(&images).unwrap();
        let elapsed = t0.elapsed();
        println!(
            "\nwriting {} sampled-texture descriptors into the heap: {:.0} µs ({:.0} ns each)",
            slots.len(),
            micros(elapsed),
            elapsed.as_nanos() as f64 / slots.len() as f64
        );
        for slot in slots {
            gpu.destroy(slot).unwrap();
        }
    }

    // The parts of `init`, measured alone.
    let mut compile = Vec::new();
    let mut pipeline = Vec::new();
    for _ in 0..rounds {
        let mut gpu = bench.host.gpu();
        let t0 = Instant::now();
        let shader = |gpu: &mut Gpu, stage, entry| {
            let spirv = proto_gfx_lua::shaders::tool("shaders/bench_init.slang", entry);
            gpu.create_shader_spirv(stage, entry, spirv).unwrap()
        };
        let vs = shader(&mut gpu, ShaderStage::Vertex, "vs");
        let fs = shader(&mut gpu, ShaderStage::Fragment, "fs");
        let t1 = Instant::now();
        let p = gpu
            .create_render_pipeline(&RenderPipelineDesc {
                vertex: vs,
                fragment: fs,
                vertex_buffers: vec![],
                targets: vec![ColorTarget {
                    format: Format::Rgba16Float,
                    blend: Some(Blend::PREMULTIPLIED),
                }],
                topology: Topology::TriangleList,
                cull: CullMode::None,
            })
            .unwrap();
        let t2 = Instant::now();
        compile.push(micros(t1 - t0) / 2.0);
        pipeline.push(micros(t2 - t1));
        for h in [p, vs, fs] {
            gpu.destroy(h).unwrap();
        }
    }
    println!(
        "\nshader creation (SPIR-V checks, per shader): {:.0} µs; pipeline creation (driver): first {:.0} µs, then {:.0} µs\n",
        Dist::new(compile).median(),
        pipeline[0],
        Dist::new(pipeline.clone()).median()
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quick = args.iter().any(|a| a == "--quick");
    let what = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(String::as_str)
        .unwrap_or("all");
    match what {
        "calls" => calls(quick),
        "frame4k" => frame4k(quick),
        "gc" => gc(quick),
        "reload" => reload(quick),
        "all" => {
            calls(quick);
            frame4k(quick);
            gc(quick);
            reload(quick);
        }
        other => {
            eprintln!("unknown benchmark `{other}`");
            std::process::exit(2);
        }
    }
}
