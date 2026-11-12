//! Importing dma-bufs (host API). The dma-buf comes from the same device: a
//! scanout image is exported, imported again and sampled. No other process
//! or device is involved, so this runs wherever the device supports both
//! directions.

use std::os::fd::AsFd;

use gfx_core::*;

const WIDTH: u32 = 16;
const HEIGHT: u32 = 8;

const VS: &str = r#"
@vertex fn main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(p[i], 0.0, 1.0);
}"#;

/// A gradient that tells rows, columns and channels apart.
const GRADIENT: &str = r#"
struct Push { gain: vec4<f32> }
var<immediate> push: Push;
@fragment fn main(@builtin(position) at: vec4<f32>) -> @location(0) vec4<f32> {
    return vec4(at.x / 16.0, at.y / 8.0, 0.25, 1.0) * push.gain;
}"#;

/// Copies a texture texel for texel.
const COPY: &str = r#"
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;
@fragment fn main(@builtin(position) at: vec4<f32>) -> @location(0) vec4<f32> {
    return textureSampleLevel(source, source_sampler, at.xy / vec2(16.0, 8.0), 0.0);
}"#;

fn shader(gpu: &mut Gpu, stage: ShaderStage, source: &str) -> Handle {
    gpu.create_shader(&ShaderSource {
        stage,
        language: ShaderLanguage::Wgsl,
        entry: "main",
        source,
    })
    .unwrap()
}

fn pipeline(gpu: &mut Gpu, vertex: Handle, fragment: Handle, format: Format) -> Handle {
    gpu.create_render_pipeline(&RenderPipelineDesc {
        vertex,
        fragment,
        vertex_buffers: vec![],
        targets: vec![ColorTarget {
            format,
            blend: None,
        }],
        topology: Topology::TriangleList,
        cull: CullMode::None,
    })
    .unwrap()
}

fn fill(gpu: &mut Gpu, pipeline: Handle, target: Handle, gain: [f32; 4]) {
    gpu.begin_frame().unwrap();
    gpu.begin_pass(&[ColorAttachment {
        image: target,
        load: LoadOp::Clear([0.0; 4]),
        store: true,
    }])
    .unwrap();
    gpu.set_pipeline(pipeline).unwrap();
    let bytes: Vec<u8> = gain.iter().flat_map(|c| c.to_ne_bytes()).collect();
    gpu.push(0, &bytes).unwrap();
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    // The producer is done before the consumer looks.
    gpu.wait_idle().unwrap();
}

/// Whether a test that needs dma-bufs runs on a device that does not offer
/// them (`what` says which direction is missing).
///
/// Lavapipe handles dma-bufs through the kernel's `/dev/udmabuf`. When that
/// cannot be opened, the tests fail, unless `HIZUKURI_TEST_SKIP_UDMABUF=1`
/// leaves them out. Any other device without dma-bufs is skipped with a
/// note: there it is a property of the device.
fn without_dmabufs(gpu: &Gpu, what: &str) {
    if gpu.device_name().contains("llvmpipe")
        && std::env::var_os("HIZUKURI_TEST_SKIP_UDMABUF").is_none()
    {
        panic!(
            "lavapipe {what}: /dev/udmabuf cannot be opened. This test needs it; set \
             HIZUKURI_TEST_SKIP_UDMABUF=1 to run without the tests that do"
        );
    }
    eprintln!("skipped: {} {what}", gpu.device_name());
}

#[test]
fn an_exported_image_is_imported_and_sampled() {
    let mut gpu = Gpu::new(GpuConfig::for_tests()).expect("gpu");
    let xrgb = u32::from_le_bytes(*b"XR24");
    let importable: Vec<u64> = gpu
        .dmabuf_import_formats()
        .iter()
        .filter(|f| f.fourcc == xrgb)
        .map(|f| f.modifier)
        .collect();
    eprintln!(
        "{}: {} importable (format, modifier) pairs, {} for XR24, export modifiers {:x?}",
        gpu.device_name(),
        gpu.dmabuf_import_formats().len(),
        importable.len(),
        gpu.scanout_modifiers(),
    );
    assert_eq!(
        gpu.device_description().dmabuf_import,
        !gpu.dmabuf_import_formats().is_empty()
    );
    if importable.is_empty() || gpu.scanout_modifiers().is_empty() {
        without_dmabufs(&gpu, "cannot both export and import dma-bufs");
        return;
    }

    let (scanout, plane) = gpu
        .create_scanout_image(WIDTH, HEIGHT, &importable)
        .expect("an image both directions accept");
    let vs = shader(&mut gpu, ShaderStage::Vertex, VS);
    let gradient = shader(&mut gpu, ShaderStage::Fragment, GRADIENT);
    let copy = shader(&mut gpu, ShaderStage::Fragment, COPY);
    let produce = pipeline(&mut gpu, vs, gradient, Format::Bgra8Unorm);
    let consume = pipeline(&mut gpu, vs, copy, Format::Bgra8Unorm);
    let sampler = gpu
        .create_sampler(&SamplerDesc {
            mag: Filter::Nearest,
            min: Filter::Nearest,
            address_u: AddressMode::ClampToEdge,
            address_v: AddressMode::ClampToEdge,
        })
        .unwrap();
    let target = gpu
        .create_image(&ImageDesc {
            format: Format::Bgra8Unorm,
            width: WIDTH,
            height: HEIGHT,
            usage: ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
        })
        .unwrap();

    fill(&mut gpu, produce, scanout, [1.0; 4]);
    let imported = gpu
        .import_dmabuf(&DmabufDesc {
            width: WIDTH,
            height: HEIGHT,
            fourcc: xrgb,
            modifier: plane.modifier,
            planes: &[DmabufPlaneDesc {
                fd: plane.fd.as_fd(),
                offset: plane.offset,
                stride: plane.stride,
            }],
        })
        .expect("importing what the device exported");
    assert_eq!(imported.format, Format::Bgra8Unorm);
    assert!(!imported.has_alpha);
    let slot = gpu.create_opaque_texture_slot(imported.image).unwrap();

    let sample = |gpu: &mut Gpu| {
        gpu.begin_frame().unwrap();
        gpu.begin_pass(&[ColorAttachment {
            image: target,
            load: LoadOp::Clear([0.0; 4]),
            store: true,
        }])
        .unwrap();
        gpu.set_pipeline(consume).unwrap();
        gpu.bind(0, slot).unwrap();
        gpu.bind(1, sampler).unwrap();
        gpu.draw(3, 1, 0, 0).unwrap();
        gpu.end_pass().unwrap();
        gpu.end_frame().unwrap();
        gpu.read_image(target).unwrap()
    };

    let first = sample(&mut gpu);
    assert_eq!(first, gpu.read_image(scanout).unwrap());
    // Not a flat color: rows and columns differ.
    assert_ne!(first[..4], first[4..8]);
    assert_ne!(
        first[..4],
        first[(WIDTH * 4) as usize..(WIDTH * 4 + 4) as usize]
    );

    // The producer writes again; the consumer is told and sees the new
    // contents through the same image.
    fill(&mut gpu, produce, scanout, [0.5, 1.0, 1.0, 1.0]);
    gpu.dmabuf_contents_changed(imported.image).unwrap();
    let second = sample(&mut gpu);
    assert_eq!(second, gpu.read_image(scanout).unwrap());
    assert_ne!(first, second);

    // What is not a dma-buf is refused.
    assert_eq!(
        gpu.dmabuf_contents_changed(target).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    gpu.destroy(slot).unwrap();
    gpu.destroy(imported.image).unwrap();
    gpu.wait_idle().unwrap();
    assert_eq!(validation_error_count(), 0, "{:?}", validation_messages());
}

#[test]
fn bad_descriptions_are_refused() {
    let mut gpu = Gpu::new(GpuConfig::for_tests()).expect("gpu");
    let Some(format) = gpu.dmabuf_import_formats().first().copied() else {
        without_dmabufs(&gpu, "cannot import dma-bufs");
        return;
    };
    // An ordinary file is not a dma-buf, whatever the description says.
    let file = std::fs::File::open("/proc/self/exe").unwrap();
    let plane = |stride| DmabufPlaneDesc {
        fd: file.as_fd(),
        offset: 0,
        stride,
    };
    let desc = |fourcc, modifier, width, planes| DmabufDesc {
        width,
        height: 4,
        fourcc,
        modifier,
        planes,
    };
    let code = |gpu: &mut Gpu, desc: &DmabufDesc<'_>| gpu.import_dmabuf(desc).unwrap_err().code;

    let one = [plane(16)];
    let two = [plane(16), plane(16)];
    assert_eq!(
        code(&mut gpu, &desc(0x1234_5678, format.modifier, 4, &one)),
        ErrorCode::Unsupported
    );
    assert_eq!(
        code(
            &mut gpu,
            &desc(format.fourcc, 0x00ff_ffff_ffff_fffe, 4, &one)
        ),
        ErrorCode::Unsupported
    );
    assert_eq!(
        code(&mut gpu, &desc(format.fourcc, format.modifier, 0, &one)),
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        code(&mut gpu, &desc(format.fourcc, format.modifier, 4, &two)),
        ErrorCode::Unsupported
    );
    // A stride shorter than a row.
    assert_eq!(
        code(&mut gpu, &desc(format.fourcc, format.modifier, 64, &one)),
        ErrorCode::InvalidArgument
    );
    // The right shape, but the descriptor is not a dma-buf: refused before
    // the device sees it, and nothing is left behind.
    let images = gpu.stats().live_images;
    let wide = [plane(1 << 14)];
    assert_eq!(
        code(&mut gpu, &desc(format.fourcc, format.modifier, 4, &wide)),
        ErrorCode::InvalidArgument
    );
    // Shared memory is not one either, although it can be mapped and sized.
    let memfd = rustix::fs::memfd_create("not-a-dmabuf", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    rustix::fs::ftruncate(&memfd, 1 << 20).unwrap();
    let shared = [DmabufPlaneDesc {
        fd: memfd.as_fd(),
        offset: 0,
        stride: 16,
    }];
    assert_eq!(
        code(&mut gpu, &desc(format.fourcc, format.modifier, 4, &shared)),
        ErrorCode::InvalidArgument
    );
    assert_eq!(gpu.stats().live_images, images);
    assert_eq!(validation_error_count(), 0, "{:?}", validation_messages());
}
