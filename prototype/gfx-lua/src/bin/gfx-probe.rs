//! Experiments that may hang or kill the process, run as a child process by
//! `tests/hazards.rs`. Each prints marker lines so that the parent can tell how
//! far it got.

use std::io::Write;
use std::time::{Duration, Instant};

use proto_gfx_core::*;

fn say(line: &str) {
    println!("{line}");
    std::io::stdout().flush().ok();
}

fn software_gpu(timeout: Duration) -> Gpu {
    let mut config = GpuConfig::from_env();
    config.gpu_timeout = timeout;
    let gpu = Gpu::new(config).expect("create gpu");
    if !gpu.device_name().contains("llvmpipe") {
        // A GPU hang on real hardware can reset the GPU under the desktop.
        say(&format!(
            "REFUSED not a software device: {}",
            gpu.device_name()
        ));
        std::process::exit(3);
    }
    gpu
}

struct Scene {
    pipeline: Handle,
    target: Handle,
}

fn scene(gpu: &mut Gpu) -> Scene {
    let mut shader = |stage, entry| {
        let spirv = proto_gfx_lua::shaders::tool("shaders/hang.slang", entry);
        gpu.create_shader_spirv(stage, entry, spirv)
            .expect("shader")
    };
    let vertex = shader(ShaderStage::Vertex, "vs");
    let fragment = shader(ShaderStage::Fragment, "fs");
    let pipeline = gpu
        .create_render_pipeline(&RenderPipelineDesc {
            vertex,
            fragment,
            vertex_buffers: vec![],
            targets: vec![ColorTarget {
                format: Format::Rgba8Unorm,
                blend: None,
            }],
            topology: Topology::TriangleList,
            cull: CullMode::None,
        })
        .expect("pipeline");
    let target = gpu
        .create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: 16,
            height: 16,
            usage: ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
        })
        .expect("target");
    Scene { pipeline, target }
}

fn frame(gpu: &mut Gpu, scene: &Scene, stop: u32) -> Result<()> {
    gpu.begin_frame()?;
    gpu.begin_pass(&[ColorAttachment {
        image: scene.target,
        load: LoadOp::Clear([0.0; 4]),
        store: true,
    }])?;
    gpu.set_pipeline(scene.pipeline)?;
    gpu.push(0, &stop.to_ne_bytes())?;
    gpu.draw(3, 1, 0, 0)?;
    gpu.end_pass()?;
    gpu.end_frame()
}

/// How long the GPU may take for work that is expected to finish.
const PATIENT: Duration = Duration::from_secs(120);

/// A fragment shader that never returns.
fn shader_hang() {
    // Frames that are meant to finish get all the time a slow machine may
    // need. Only the wait for the frame that hangs is cut short.
    let mut gpu = software_gpu(PATIENT);
    let scene_a = scene(&mut gpu);
    frame(&mut gpu, &scene_a, 1).expect("normal frame");
    let pixels = gpu.read_image(scene_a.target).expect("read");
    assert_eq!(&pixels[..4], &[0, 255, 0, 255]);
    say("NORMAL_FRAME_OK");
    gpu.set_gpu_timeout(Duration::from_millis(1500));

    // The call is valid: nothing the API checks can tell that the shader
    // will not terminate.
    frame(&mut gpu, &scene_a, 0).expect("the hanging frame is accepted");
    say("HANG_SUBMITTED");
    let start = Instant::now();
    match gpu.wait_idle() {
        Err(e) if e.code == ErrorCode::Timeout => {
            say(&format!("TIMEOUT_REPORTED after {:?}", start.elapsed()));
        }
        other => say(&format!("UNEXPECTED {other:?}")),
    }
    let start = Instant::now();
    let after = frame(&mut gpu, &scene_a, 1);
    say(&format!(
        "AFTER_TIMEOUT {:?} in {:?}",
        after.map_err(|e| e.code),
        start.elapsed()
    ));

    // Can the same process get a working device while the first one is stuck?
    let start = Instant::now();
    let mut second = software_gpu(PATIENT);
    let scene_b = scene(&mut second);
    let result = frame(&mut second, &scene_b, 1).and_then(|()| second.read_image(scene_b.target));
    match result {
        Ok(pixels) if pixels[..4] == [0, 255, 0, 255] => {
            say(&format!("SECOND_DEVICE_OK in {:?}", start.elapsed()));
        }
        other => say(&format!(
            "SECOND_DEVICE_FAILED {:?} in {:?}",
            other.map(|p| p[..4].to_vec()).map_err(|e| e.code),
            start.elapsed()
        )),
    }
    // Dropping the hung device must not block (it leaks instead).
    let start = Instant::now();
    drop(second);
    drop(gpu);
    say(&format!("DROPPED in {:?}", start.elapsed()));
    say("EXITING");
}

/// Script sources that nest very deeply.
fn deep_script(kind: &str, depth: usize) {
    let source = match kind {
        "parens" => format!("return {}1{}", "(".repeat(depth), ")".repeat(depth)),
        "tables" => format!("return {}{}", "{".repeat(depth), "}".repeat(depth)),
        "calls" => format!(
            "local function f(...) return ... end return {}1{}",
            "f(".repeat(depth),
            ")".repeat(depth)
        ),
        other => panic!("unknown kind {other}"),
    };
    say(&format!("LOADING {kind} depth {depth}"));
    let lua = mlua::Lua::new();
    match lua.load(source).exec() {
        Ok(()) => say("LOADED"),
        Err(e) => say(&format!(
            "REJECTED: {}",
            e.to_string()
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(120)
                .collect::<String>()
        )),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let number = |i: usize| args.get(i).and_then(|a| a.parse().ok()).unwrap_or(1000);
    match args.get(1).map(String::as_str) {
        Some("shader-hang") => shader_hang(),
        Some("deep-script") => deep_script(&args[2], number(3)),
        _ => {
            eprintln!("usage: gfx-probe shader-hang | deep-script <kind> <depth>");
            std::process::exit(2);
        }
    }
    say("DONE");
}
