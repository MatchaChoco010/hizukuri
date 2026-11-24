//! Building a render pipeline on another thread
//! (`Gpu::prepare_render_pipeline`): the description is checked at once, the
//! driver's work happens elsewhere, and the device keeps recording frames
//! until the pipeline is there. The shaders are Slang; needs `slangc`.

use std::time::{Duration, Instant};

use proto_gfx_core::*;

mod slang;
use slang::*;

const SHADERS: &str = r#"
struct Push { float4 color; };
[[vk::push_constant]] ConstantBuffer<Push> pc;

[shader("vertex")]
float4 vs(uint i : SV_VulkanVertexID) : SV_Position {
    float2 p[3] = { float2(-1, -1), float2(3, -1), float2(-1, 3) };
    return float4(p[i], 0, 1);
}

[shader("fragment")]
float4 fs() : SV_Target0 { return pc.color; }
"#;

fn desc(vs: Handle, fs: Handle, format: Format) -> RenderPipelineDesc {
    RenderPipelineDesc {
        vertex: vs,
        fragment: fs,
        vertex_buffers: vec![],
        targets: vec![ColorTarget {
            format,
            blend: None,
        }],
        topology: Topology::TriangleList,
        cull: CullMode::None,
    }
}

struct Kit {
    gpu: Gpu,
    vs: Handle,
    fs: Handle,
    target: Handle,
}

fn kit_with(config: GpuConfig) -> Kit {
    let mut gpu = Gpu::new(config).unwrap();
    let vs = shader(&mut gpu, ShaderStage::Vertex, "vs", SHADERS);
    let fs = shader(&mut gpu, ShaderStage::Fragment, "fs", SHADERS);
    let target = image(
        &mut gpu,
        Format::Rgba8Unorm,
        2,
        2,
        ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
    );
    Kit {
        gpu,
        vs,
        fs,
        target,
    }
}

fn kit() -> Kit {
    kit_with(GpuConfig::for_tests())
}

/// Draws with `pipeline` (or only clears) and returns the first pixel.
fn frame(kit: &mut Kit, pipeline: Option<Handle>) -> [u8; 4] {
    let gpu = &mut kit.gpu;
    gpu.begin_frame().unwrap();
    gpu.begin_pass_desc(&PassDesc {
        color: vec![color(kit.target, [0.0, 0.0, 1.0, 1.0])],
        depth: None,
    })
    .unwrap();
    if let Some(pipeline) = pipeline {
        gpu.set_pipeline(pipeline).unwrap();
        gpu.push(0, &f32_bytes(&[1.0, 0.5, 0.0, 1.0])).unwrap();
        gpu.draw(3, 1, 0, 0).unwrap();
    }
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    gpu.read_image(kit.target).unwrap()[..4].try_into().unwrap()
}

/// The pixel of a frame drawn with the pipeline: the push constant's color,
/// whose 0.5 may come out as 127 or 128.
fn assert_orange(pixel: [u8; 4]) {
    assert_close8(pixel, [255, 128, 0, 255], UNORM8_CONVERSION, "drawn");
}

/// Polls until the pending pipeline is there.
fn wait(gpu: &mut Gpu, pending: &PendingPipeline) -> Result<Handle> {
    let start = Instant::now();
    loop {
        if let Some(handle) = gpu.finish_render_pipeline(pending)? {
            return Ok(handle);
        }
        assert!(start.elapsed() < Duration::from_secs(120), "never finished");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_pipeline_is_built_while_frames_go_on() {
    let mut kit = kit();
    let extra = PipelineExtra::default();
    let description = desc(kit.vs, kit.fs, Format::Rgba8Unorm);
    // For comparison: what the frame thread spends when it builds the
    // pipeline itself.
    let start = Instant::now();
    let direct = kit.gpu.create_render_pipeline(&description).unwrap();
    let building = start.elapsed();
    kit.gpu.destroy(direct).unwrap();
    let start = Instant::now();
    let (pending, job) = kit
        .gpu
        .prepare_render_pipeline(&description, &extra)
        .unwrap();
    let preparing = start.elapsed();
    eprintln!("building on the frame thread took {building:?}; preparing took {preparing:?}");
    assert_eq!(kit.gpu.pending_pipelines(), 1);
    // Nothing has run yet: the answer is "not yet", as often as it is asked.
    assert!(kit.gpu.finish_render_pipeline(&pending).unwrap().is_none());
    assert!(kit.gpu.finish_render_pipeline(&pending).unwrap().is_none());
    // The job has its own copy of the shaders.
    kit.gpu.destroy(kit.vs).unwrap();
    kit.gpu.destroy(kit.fs).unwrap();

    let worker = std::thread::spawn(move || job.run());
    // Frames go on meanwhile, without the pipeline.
    let mut frames = 0;
    let pipeline = loop {
        assert_eq!(frame(&mut kit, None), [0, 0, 255, 255]);
        frames += 1;
        if let Some(pipeline) = kit.gpu.finish_render_pipeline(&pending).unwrap() {
            break pipeline;
        }
        assert!(frames < 100_000, "never finished");
    };
    worker.join().unwrap();
    eprintln!("{frames} frame(s) were drawn while the pipeline was built");
    assert_eq!(kit.gpu.pending_pipelines(), 0);
    assert_orange(frame(&mut kit, Some(pipeline)));
    // The pending pipeline is used up.
    expect!(kit.gpu.finish_render_pipeline(&pending), InvalidHandle);
    kit.gpu.destroy(pipeline).unwrap();
    no_validation_errors();
}

#[test]
fn a_thread_is_started_for_the_job() {
    let mut kit = kit();
    let description = desc(kit.vs, kit.fs, Format::Rgba8Unorm);
    let pending = kit
        .gpu
        .create_render_pipeline_in_background(&description, &PipelineExtra::default())
        .unwrap();
    let pipeline = wait(&mut kit.gpu, &pending).unwrap();
    assert_orange(frame(&mut kit, Some(pipeline)));
    // Several at once, finished in another order than they were started.
    let pending: Vec<_> = (0..6)
        .map(|_| {
            kit.gpu
                .create_render_pipeline_in_background(&description, &PipelineExtra::default())
                .unwrap()
        })
        .collect();
    for pending in pending.iter().rev() {
        let pipeline = wait(&mut kit.gpu, pending).unwrap();
        assert_orange(frame(&mut kit, Some(pipeline)));
    }
    no_validation_errors();
}

#[test]
fn what_can_go_wrong_with_a_pending_pipeline() {
    let mut limits = GpuConfig::for_tests();
    limits.limits.max_pipelines = 2;
    let mut kit = kit_with(limits);
    let extra = PipelineExtra::default();
    let description = desc(kit.vs, kit.fs, Format::Rgba8Unorm);

    // A wrong description is refused when the pipeline is prepared, not
    // later; nothing is pending afterwards.
    expect!(
        kit.gpu
            .prepare_render_pipeline(&desc(kit.fs, kit.vs, Format::Rgba8Unorm), &extra),
        Mismatch
    );
    expect!(
        kit.gpu
            .prepare_render_pipeline(&desc(kit.vs, kit.fs, Format::R32Uint), &extra),
        ShaderInterface
    );
    expect!(
        kit.gpu
            .prepare_render_pipeline(&desc(kit.vs, kit.target, Format::Rgba8Unorm), &extra),
        WrongKind
    );
    assert_eq!(kit.gpu.pending_pipelines(), 0);

    // A job that is dropped without running: the pipeline finishes with an
    // error instead of staying pending for ever.
    let (dropped, job) = kit
        .gpu
        .prepare_render_pipeline(&description, &extra)
        .unwrap();
    drop(job);
    expect!(kit.gpu.finish_render_pipeline(&dropped), InvalidState);
    expect!(kit.gpu.finish_render_pipeline(&dropped), InvalidHandle);

    // Pending pipelines count towards the limit on pipelines (2 here).
    let (first, first_job) = kit
        .gpu
        .prepare_render_pipeline(&description, &extra)
        .unwrap();
    let (second, second_job) = kit
        .gpu
        .prepare_render_pipeline(&description, &extra)
        .unwrap();
    expect!(
        kit.gpu.prepare_render_pipeline(&description, &extra),
        LimitExceeded
    );
    expect!(kit.gpu.create_render_pipeline(&description), LimitExceeded);

    // Cancelled before its job ran, and after: what the job builds is thrown
    // away either way, and the place is free again.
    kit.gpu.cancel_render_pipeline(first);
    first_job.run();
    second_job.run();
    kit.gpu.cancel_render_pipeline(second);
    assert_eq!(kit.gpu.pending_pipelines(), 0);
    let pipeline = kit.gpu.create_render_pipeline(&description).unwrap();
    assert_orange(frame(&mut kit, Some(pipeline)));

    // A pending pipeline of another device is not this one's.
    let mut other = kit_with(GpuConfig::for_tests());
    let other_description = desc(other.vs, other.fs, Format::Rgba8Unorm);
    let (foreign, foreign_job) = other
        .gpu
        .prepare_render_pipeline(&other_description, &extra)
        .unwrap();
    expect!(kit.gpu.finish_render_pipeline(&foreign), InvalidHandle);
    foreign_job.run();
    expect!(kit.gpu.finish_render_pipeline(&foreign), InvalidHandle);
    assert!(
        other
            .gpu
            .finish_render_pipeline(&foreign)
            .unwrap()
            .is_some()
    );

    // On a lost device a built pipeline is not handed out; it stays pending
    // until it is cancelled or the device is dropped.
    let (lost, lost_job) = kit
        .gpu
        .prepare_render_pipeline(&description, &extra)
        .unwrap();
    lost_job.run();
    kit.gpu.simulate_device_loss();
    expect!(kit.gpu.finish_render_pipeline(&lost), DeviceLost);
    expect!(
        kit.gpu.prepare_render_pipeline(&description, &extra),
        DeviceLost
    );
    assert_eq!(kit.gpu.pending_pipelines(), 1);
    kit.gpu.cancel_render_pipeline(lost);
    assert_eq!(kit.gpu.pending_pipelines(), 0);
    no_validation_errors();
}

/// The device goes away with jobs outstanding: one that is running is waited
/// for, one that runs afterwards does nothing, and what was built and never
/// taken is destroyed with the device.
#[test]
fn the_device_is_dropped_with_jobs_outstanding() {
    let mut kit = kit();
    let extra = PipelineExtra::default();
    let description = desc(kit.vs, kit.fs, Format::Rgba8Unorm);
    let (_built, built_job) = kit
        .gpu
        .prepare_render_pipeline(&description, &extra)
        .unwrap();
    built_job.run();
    let (_late, late_job) = kit
        .gpu
        .prepare_render_pipeline(&description, &extra)
        .unwrap();
    let running: Vec<_> = (0..4)
        .map(|_| {
            let (pending, job) = kit
                .gpu
                .prepare_render_pipeline(&description, &extra)
                .unwrap();
            (pending, std::thread::spawn(move || job.run()))
        })
        .collect();
    drop(kit);
    for (_, thread) in running {
        thread.join().unwrap();
    }
    late_job.run();
    no_validation_errors();
}
