//! Deliberate misuse of the GPU API.
//!
//! Every case must come back as an error value with the expected code, must
//! not reach Vulkan (no validation message when `GFX_SPIKE_VALIDATION=1`), and
//! must leave the device able to render a correct frame afterwards.

use proto_gfx_core::*;

mod common;

use common::{ShaderLanguage, ShaderSource};

fn no_validation_errors() {
    assert_eq!(
        validation_error_count(),
        0,
        "validation layer messages: {:#?}",
        validation_messages()
    );
}

macro_rules! expect {
    ($result:expr, $code:ident) => {{
        match $result {
            Ok(_) => panic!("expected {:?}, but the call succeeded", ErrorCode::$code),
            Err(e) => {
                assert_eq!(e.code, ErrorCode::$code, "unexpected error: {e}");
                assert!(!e.message.is_empty());
                e
            }
        }
    }};
}

const QUAD: &str = r#"
struct Push { color: vec4<f32> }
var<immediate> pc: Push;
struct Globals { gain: vec4<f32> }
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var<uniform> globals: Globals;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs(@location(0) position: vec2<f32>) -> VsOut {
    var out: VsOut;
    out.position = vec4(position, 0.0, 1.0);
    out.uv = position * 0.5 + 0.5;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, smp, in.uv) * pc.color * globals.gain;
}
"#;

fn wgsl(gpu: &mut Gpu, stage: ShaderStage, source: &str, entry: &str) -> Result<Handle> {
    common::create_shader(
        gpu,
        &ShaderSource {
            stage,
            language: ShaderLanguage::Wgsl,
            source,
            entry,
        },
    )
}

fn f32_bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_ne_bytes()).collect()
}

/// A complete, valid setup: a pipeline that needs a vertex buffer, a texture,
/// a sampler and a uniform buffer.
struct Fixture {
    vs: Handle,
    fs: Handle,
    pipeline: Handle,
    vertices: Handle,
    indices: Handle,
    texture: Handle,
    texture_slot: Handle,
    sampler: Handle,
    globals: Handle,
    globals_slot: Handle,
    target: Handle,
}

fn quad_desc(vs: Handle, fs: Handle, format: Format) -> RenderPipelineDesc {
    RenderPipelineDesc {
        vertex: vs,
        fragment: fs,
        vertex_buffers: vec![VertexBufferLayout {
            stride: 8,
            per_instance: false,
            attributes: vec![VertexAttribute {
                location: 0,
                format: VertexFormat::F32x2,
                offset: 0,
            }],
        }],
        targets: vec![ColorTarget {
            format,
            blend: None,
        }],
        topology: Topology::TriangleList,
        cull: CullMode::None,
    }
}

fn image(gpu: &mut Gpu, format: Format, width: u32, height: u32, usage: ImageUsage) -> Handle {
    gpu.create_image(&ImageDesc {
        format,
        width,
        height,
        usage,
    })
    .unwrap()
}

fn solid_texture(gpu: &mut Gpu, color: [u8; 4]) -> Handle {
    let texture = image(
        gpu,
        Format::Rgba8Unorm,
        1,
        1,
        ImageUsage::SAMPLED | ImageUsage::COPY_DST,
    );
    let was_recording = gpu.is_recording();
    if !was_recording {
        gpu.begin_frame().unwrap();
    }
    gpu.upload_image(texture, &color).unwrap();
    if !was_recording {
        gpu.end_frame().unwrap();
    }
    texture
}

impl Fixture {
    fn new(gpu: &mut Gpu) -> Self {
        let vs = wgsl(gpu, ShaderStage::Vertex, QUAD, "vs").unwrap();
        let fs = wgsl(gpu, ShaderStage::Fragment, QUAD, "fs").unwrap();
        let pipeline = gpu
            .create_render_pipeline(&quad_desc(vs, fs, Format::Rgba8Unorm))
            .unwrap();
        // A full-screen triangle, then three more vertices to make a quad.
        let vertices = gpu
            .create_buffer(&BufferDesc {
                size: 6 * 8,
                usage: BufferUsage::VERTEX,
            })
            .unwrap();
        gpu.write_buffer(
            vertices,
            0,
            &f32_bytes(&[
                -1.0, -1.0, 3.0, -1.0, -1.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
            ]),
        )
        .unwrap();
        let indices = gpu
            .create_buffer(&BufferDesc {
                size: 6 * 2,
                usage: BufferUsage::INDEX,
            })
            .unwrap();
        gpu.write_buffer(indices, 0, &[0, 0, 1, 0, 2, 0, 0, 0, 0, 0, 0, 0])
            .unwrap();
        let texture = solid_texture(gpu, [255, 255, 255, 255]);
        let texture_slot = gpu.create_texture_slot(texture).unwrap();
        let sampler = gpu
            .create_sampler(&SamplerDesc {
                mag: Filter::Nearest,
                min: Filter::Nearest,
                address_u: AddressMode::ClampToEdge,
                address_v: AddressMode::ClampToEdge,
            })
            .unwrap();
        let globals = gpu
            .create_buffer(&BufferDesc {
                size: 16,
                usage: BufferUsage::UNIFORM,
            })
            .unwrap();
        gpu.write_buffer(globals, 0, &f32_bytes(&[1.0; 4])).unwrap();
        let globals_slot = gpu.create_uniform_slot(globals, 0, 16).unwrap();
        let target = image(
            gpu,
            Format::Rgba8Unorm,
            8,
            8,
            ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC | ImageUsage::SAMPLED,
        );
        Self {
            vs,
            fs,
            pipeline,
            vertices,
            indices,
            texture,
            texture_slot,
            sampler,
            globals,
            globals_slot,
            target,
        }
    }

    fn pass(&self) -> [ColorAttachment; 1] {
        [ColorAttachment {
            image: self.target,
            load: LoadOp::Clear([0.0, 0.0, 0.0, 1.0]),
            store: true,
        }]
    }

    /// Opens a frame and a pass with everything bound correctly.
    fn begin(&self, gpu: &mut Gpu) {
        gpu.begin_frame().unwrap();
        gpu.begin_pass(&self.pass()).unwrap();
        self.bind_all(gpu);
    }

    fn bind_all(&self, gpu: &mut Gpu) {
        gpu.set_pipeline(self.pipeline).unwrap();
        gpu.set_vertex_buffer(0, self.vertices, 0).unwrap();
        gpu.bind(0, self.texture_slot).unwrap();
        gpu.bind(1, self.sampler).unwrap();
        gpu.bind(2, self.globals_slot).unwrap();
        gpu.push(0, &f32_bytes(&[0.0, 1.0, 0.0, 1.0])).unwrap();
    }

    fn end(&self, gpu: &mut Gpu) {
        gpu.end_pass().unwrap();
        gpu.end_frame().unwrap();
    }

    /// Renders a correct frame and checks its pixels: proof that the device
    /// still works after whatever misuse came before.
    fn render_ok(&self, gpu: &mut Gpu) {
        self.begin(gpu);
        gpu.draw(3, 1, 0, 0).unwrap();
        self.end(gpu);
        let pixels = gpu.read_image(self.target).unwrap();
        for pixel in pixels.as_chunks::<4>().0 {
            assert_eq!(*pixel, [0, 255, 0, 255]);
        }
    }
}

fn gpu() -> Gpu {
    Gpu::new(GpuConfig::for_tests()).unwrap()
}

#[test]
fn destroyed_and_foreign_handles() {
    let mut gpu = gpu();
    let f = Fixture::new(&mut gpu);

    // Destroyed buffer, image, pipeline.
    let buffer = gpu
        .create_buffer(&BufferDesc {
            size: 16,
            usage: BufferUsage::VERTEX,
        })
        .unwrap();
    gpu.destroy(buffer).unwrap();
    expect!(gpu.write_buffer(buffer, 0, &[0; 4]), Destroyed);
    expect!(gpu.destroy(buffer), Destroyed);
    let dead_image = image(&mut gpu, Format::Rgba8Unorm, 8, 8, ImageUsage::COLOR_TARGET);
    gpu.destroy(dead_image).unwrap();
    let dead_pipeline = gpu
        .create_render_pipeline(&quad_desc(f.vs, f.fs, Format::Rgba8Unorm))
        .unwrap();
    gpu.destroy(dead_pipeline).unwrap();

    gpu.begin_frame().unwrap();
    expect!(
        gpu.begin_pass(&[ColorAttachment {
            image: dead_image,
            load: LoadOp::Load,
            store: true
        }]),
        Destroyed
    );
    gpu.begin_pass(&f.pass()).unwrap();
    expect!(gpu.set_pipeline(dead_pipeline), Destroyed);
    expect!(gpu.set_vertex_buffer(0, buffer, 0), Destroyed);

    // A handle of another kind.
    expect!(gpu.set_pipeline(f.texture), WrongKind);
    expect!(gpu.set_vertex_buffer(0, f.texture, 0), WrongKind);
    expect!(gpu.bind(0, f.vertices), WrongKind);
    expect!(gpu.write_buffer(f.texture, 0, &[0; 4]), WrongKind);
    expect!(gpu.create_texture_slot(f.vertices), WrongKind);
    expect!(gpu.create_uniform_slot(f.texture, 0, 16), WrongKind);
    expect!(
        gpu.create_render_pipeline(&quad_desc(f.texture, f.fs, Format::Rgba8Unorm)),
        WrongKind
    );

    // Values that never were handles.
    for bits in [
        0,
        -1,
        1,
        12345,
        i64::MAX,
        i64::MIN,
        1 << 52,
        (6 << 48) | 0xfffff,
    ] {
        let bogus = Handle::from_bits(bits);
        assert!(gpu.set_pipeline(bogus).is_err(), "{bits:#x}");
        assert!(gpu.bind(0, bogus).is_err(), "{bits:#x}");
        assert!(gpu.set_vertex_buffer(0, bogus, 0).is_err(), "{bits:#x}");
        assert!(gpu.write_buffer(bogus, 0, &[0]).is_err(), "{bits:#x}");
        assert!(gpu.destroy(bogus).is_err(), "{bits:#x}");
    }
    expect!(gpu.set_pipeline(Handle::from_bits(0)), InvalidHandle);
    expect!(gpu.bind(0, Handle::from_bits(12345)), InvalidHandle);

    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    f.render_ok(&mut gpu);
    no_validation_errors();
}

#[test]
fn objects_destroyed_while_in_use() {
    let mut gpu = gpu();
    let f = Fixture::new(&mut gpu);

    // A slot whose image was destroyed.
    let texture = solid_texture(&mut gpu, [255, 0, 0, 255]);
    let slot = gpu.create_texture_slot(texture).unwrap();
    f.begin(&mut gpu);
    gpu.bind(0, slot).unwrap();
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.destroy(texture).unwrap();
    // The cached validation must not hide the destruction.
    expect!(gpu.draw(3, 1, 0, 0), Destroyed);
    // A destroyed slot.
    gpu.destroy(slot).unwrap();
    expect!(gpu.draw(3, 1, 0, 0), Destroyed);
    expect!(gpu.bind(0, slot), Destroyed);
    // The pipeline destroyed in the middle of the pass.
    let pipeline = gpu
        .create_render_pipeline(&quad_desc(f.vs, f.fs, Format::Rgba8Unorm))
        .unwrap();
    gpu.set_pipeline(pipeline).unwrap();
    gpu.bind(0, f.texture_slot).unwrap();
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.destroy(pipeline).unwrap();
    expect!(gpu.draw(3, 1, 0, 0), Destroyed);
    // The vertex buffer destroyed after it was bound.
    let vertices = gpu
        .create_buffer(&BufferDesc {
            size: 24,
            usage: BufferUsage::VERTEX,
        })
        .unwrap();
    gpu.set_pipeline(f.pipeline).unwrap();
    gpu.set_vertex_buffer(0, vertices, 0).unwrap();
    gpu.destroy(vertices).unwrap();
    expect!(gpu.draw(3, 1, 0, 0), Destroyed);
    // The attachment of the open pass destroyed: the pass still closes.
    f.end(&mut gpu);

    let doomed = image(
        &mut gpu,
        Format::Rgba8Unorm,
        8,
        8,
        ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
    );
    gpu.begin_frame().unwrap();
    gpu.begin_pass(&[ColorAttachment {
        image: doomed,
        load: LoadOp::Clear([1.0; 4]),
        store: true,
    }])
    .unwrap();
    gpu.destroy(doomed).unwrap();
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();
    expect!(gpu.read_image(doomed), Destroyed);

    f.render_ok(&mut gpu);
    no_validation_errors();
}

/// What a recorded draw refers to must survive until the GPU has executed it,
/// and its heap slot must not be handed out again before that.
#[test]
fn recorded_draws_keep_their_resources_and_slots() {
    let mut gpu = gpu();
    let f = Fixture::new(&mut gpu);
    let red = solid_texture(&mut gpu, [255, 0, 0, 255]);
    let red_slot = gpu.create_texture_slot(red).unwrap();
    gpu.wait_idle().unwrap();

    f.begin(&mut gpu);
    gpu.push(0, &f32_bytes(&[1.0; 4])).unwrap();
    gpu.bind(0, red_slot).unwrap();
    gpu.draw(3, 1, 0, 0).unwrap();
    // Destroy everything the draw used, then create replacements.
    gpu.destroy(red_slot).unwrap();
    gpu.destroy(red).unwrap();
    gpu.end_pass().unwrap();
    let green = solid_texture(&mut gpu, [0, 255, 0, 255]);
    let green_slot = gpu.create_texture_slot(green).unwrap();
    assert_ne!(
        green_slot.index(),
        red_slot.index(),
        "the slot of a recorded draw was reused before the frame ran"
    );
    let pending = gpu.stats().pending_garbage;
    assert!(pending >= 2, "destruction must be deferred, got {pending}");
    gpu.end_frame().unwrap();

    let pixels = gpu.read_image(f.target).unwrap();
    assert_eq!(
        &pixels[..4],
        &[255, 0, 0, 255],
        "the draw must still see red"
    );
    // After the GPU is done the slot index becomes available again.
    assert_eq!(gpu.stats().pending_garbage, 0);
    let again = gpu.create_texture_slot(green).unwrap();
    assert_eq!(again.index(), red_slot.index());
    f.render_ok(&mut gpu);
    no_validation_errors();
}

#[test]
fn out_of_range_and_mismatched_sizes() {
    let mut gpu = gpu();
    let f = Fixture::new(&mut gpu);

    // Writes outside a buffer.
    expect!(gpu.write_buffer(f.vertices, 40, &[0; 16]), OutOfRange);
    expect!(gpu.write_buffer(f.vertices, 48, &[0; 1]), OutOfRange);
    expect!(gpu.write_buffer(f.vertices, u64::MAX, &[0; 2]), OutOfRange);
    gpu.write_buffer(f.vertices, 48, &[]).unwrap();
    // Slot ranges outside a buffer.
    expect!(gpu.create_uniform_slot(f.globals, 0, 17), OutOfRange);
    expect!(gpu.create_uniform_slot(f.globals, 256, 16), OutOfRange);
    expect!(gpu.create_uniform_slot(f.globals, 0, 0), OutOfRange);
    expect!(gpu.create_uniform_slot(f.globals, u64::MAX, 16), OutOfRange);
    expect!(
        gpu.create_storage_buffer_slot(f.globals, 0, 16),
        InvalidArgument
    );

    // Copies whose sizes do not agree.
    let small = image(
        &mut gpu,
        Format::Rgba8Unorm,
        4,
        4,
        ImageUsage::COPY_SRC | ImageUsage::COPY_DST | ImageUsage::SAMPLED,
    );
    let large = image(
        &mut gpu,
        Format::Rgba8Unorm,
        8,
        8,
        ImageUsage::COPY_SRC | ImageUsage::COPY_DST | ImageUsage::SAMPLED,
    );
    let wide = image(
        &mut gpu,
        Format::Rgba16Float,
        4,
        4,
        ImageUsage::COPY_SRC | ImageUsage::COPY_DST | ImageUsage::SAMPLED,
    );
    let no_copy = image(&mut gpu, Format::Rgba8Unorm, 4, 4, ImageUsage::SAMPLED);
    gpu.begin_frame().unwrap();
    expect!(gpu.upload_image(small, &[0; 63]), Mismatch);
    expect!(gpu.upload_image(small, &[0; 65]), Mismatch);
    expect!(gpu.upload_image(small, &[]), Mismatch);
    expect!(gpu.upload_image(no_copy, &[0; 64]), InvalidArgument);
    expect!(gpu.copy_image(small, large), Mismatch);
    expect!(gpu.copy_image(small, wide), Mismatch);
    expect!(gpu.copy_image(small, small), InvalidArgument);
    expect!(gpu.copy_image(small, no_copy), InvalidArgument);
    gpu.upload_image(small, &[7; 64]).unwrap();
    let copy = image(
        &mut gpu,
        Format::Rgba8Unorm,
        4,
        4,
        ImageUsage::COPY_SRC | ImageUsage::COPY_DST,
    );
    gpu.copy_image(small, copy).unwrap();

    // Draw ranges outside the bound buffers.
    gpu.begin_pass(&f.pass()).unwrap();
    f.bind_all(&mut gpu);
    expect!(gpu.draw(7, 1, 0, 0), OutOfRange);
    expect!(gpu.draw(3, 1, 4, 0), OutOfRange);
    expect!(gpu.draw(u32::MAX, 1, u32::MAX, 0), LimitExceeded);
    // A draw Vulkan would accept (no per-instance buffer limits the count),
    // refused by policy: 3 * (2^32 - 1) vertices.
    expect!(gpu.draw(3, u32::MAX, 0, 0), LimitExceeded);
    gpu.set_vertex_buffer(0, f.vertices, 24).unwrap();
    expect!(gpu.draw(4, 1, 0, 0), OutOfRange);
    expect!(gpu.set_vertex_buffer(0, f.vertices, 52), OutOfRange);
    expect!(gpu.set_vertex_buffer(8, f.vertices, 0), OutOfRange);
    expect!(gpu.set_vertex_buffer(0, f.indices, 0), InvalidArgument);
    gpu.set_vertex_buffer(0, f.vertices, 0).unwrap();
    expect!(gpu.draw_indexed(3, 1, 0, 0, 0), MissingBinding);
    expect!(
        gpu.set_index_buffer(f.vertices, 0, IndexType::U16),
        InvalidArgument
    );
    expect!(
        gpu.set_index_buffer(f.indices, 1, IndexType::U16),
        OutOfRange
    );
    gpu.set_index_buffer(f.indices, 0, IndexType::U16).unwrap();
    expect!(gpu.draw_indexed(7, 1, 0, 0, 0), OutOfRange);
    expect!(gpu.draw_indexed(3, 1, 4, 0, 0), OutOfRange);
    gpu.set_index_buffer(f.indices, 0, IndexType::U32).unwrap();
    expect!(gpu.draw_indexed(4, 1, 0, 0, 0), OutOfRange);
    // Push data outside the shader's block, bindings outside the table.
    expect!(gpu.push(8, &[0; 12]), OutOfRange);
    expect!(gpu.push(u32::MAX, &[0; 4]), OutOfRange);
    expect!(gpu.bind(32, f.texture_slot), OutOfRange);
    // Scissor and viewport outside what the target allows.
    expect!(gpu.set_scissor(4, 4, 8, 8), OutOfRange);
    expect!(gpu.set_scissor(u32::MAX, 0, 2, 2), OutOfRange);
    expect!(gpu.set_viewport(0.0, 0.0, 0.0, 8.0), InvalidArgument);
    expect!(gpu.set_viewport(f32::NAN, 0.0, 8.0, 8.0), InvalidArgument);
    expect!(gpu.set_viewport(0.0, 0.0, 1e9, 8.0), InvalidArgument);
    // An index buffer whose values point far outside the vertex buffer is not
    // rejected (the API does not read index data); robust buffer access makes
    // the fetch harmless.
    let wild = gpu
        .create_buffer(&BufferDesc {
            size: 12,
            usage: BufferUsage::INDEX,
        })
        .unwrap();
    gpu.write_buffer(wild, 0, &[0xff; 12]).unwrap();
    gpu.set_index_buffer(wild, 0, IndexType::U32).unwrap();
    gpu.draw_indexed(3, 1, 0, 0, 0).unwrap();
    f.end(&mut gpu);
    gpu.wait_idle().unwrap();

    let pixels = gpu.read_image(copy).unwrap();
    assert!(pixels.iter().all(|b| *b == 7));
    f.render_ok(&mut gpu);
    no_validation_errors();
}

#[test]
fn targets_that_do_not_fit() {
    let mut gpu = gpu();
    let f = Fixture::new(&mut gpu);
    let float_target = image(
        &mut gpu,
        Format::Rgba16Float,
        8,
        8,
        ImageUsage::COLOR_TARGET,
    );
    let other_size = image(&mut gpu, Format::Rgba8Unorm, 4, 4, ImageUsage::COLOR_TARGET);
    let attach = |image| ColorAttachment {
        image,
        load: LoadOp::Clear([0.0; 4]),
        store: true,
    };

    gpu.begin_frame().unwrap();
    // The pipeline was built for rgba8unorm.
    gpu.begin_pass(&[attach(float_target)]).unwrap();
    let e = expect!(gpu.set_pipeline(f.pipeline), Mismatch);
    assert!(e.message.contains("rgba16float"), "{e}");
    expect!(gpu.draw(3, 1, 0, 0), InvalidState);
    gpu.end_pass().unwrap();
    // An image that is not a color target; two sizes; the same image twice.
    expect!(gpu.begin_pass(&[attach(f.texture)]), InvalidArgument);
    expect!(
        gpu.begin_pass(&[attach(f.target), attach(other_size)]),
        Mismatch
    );
    expect!(
        gpu.begin_pass(&[attach(f.target), attach(f.target)]),
        InvalidArgument
    );
    expect!(gpu.begin_pass(&[]), InvalidArgument);
    expect!(
        gpu.begin_pass(&[ColorAttachment {
            image: f.target,
            load: LoadOp::Clear([f32::NAN, 0.0, 0.0, 1.0]),
            store: true
        }]),
        InvalidArgument
    );
    // A pass with two targets and a pipeline with one.
    let second = image(&mut gpu, Format::Rgba8Unorm, 8, 8, ImageUsage::COLOR_TARGET);
    gpu.begin_pass(&[attach(f.target), attach(second)]).unwrap();
    expect!(gpu.set_pipeline(f.pipeline), Mismatch);
    gpu.end_pass().unwrap();
    gpu.end_frame().unwrap();

    // Formats the device cannot use the requested way.
    expect!(
        gpu.create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: 0,
            height: 4,
            usage: ImageUsage::SAMPLED
        }),
        InvalidArgument
    );
    expect!(
        gpu.create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: 1 << 20,
            height: 4,
            usage: ImageUsage::SAMPLED
        }),
        InvalidArgument
    );
    expect!(
        gpu.create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: 4,
            height: 4,
            usage: ImageUsage(0)
        }),
        InvalidArgument
    );
    f.render_ok(&mut gpu);
    no_validation_errors();
}

#[test]
fn draws_without_what_the_pipeline_needs() {
    let mut gpu = gpu();
    let f = Fixture::new(&mut gpu);

    gpu.begin_frame().unwrap();
    expect!(gpu.draw(3, 1, 0, 0), InvalidState);
    gpu.begin_pass(&f.pass()).unwrap();
    expect!(gpu.draw(3, 1, 0, 0), InvalidState);
    expect!(gpu.push(0, &[0; 4]), InvalidState);
    gpu.set_pipeline(f.pipeline).unwrap();
    // Nothing bound at all.
    let e = expect!(gpu.draw(3, 1, 0, 0), MissingBinding);
    assert!(e.message.contains("binding 0"), "{e}");
    gpu.bind(0, f.texture_slot).unwrap();
    gpu.bind(1, f.sampler).unwrap();
    let e = expect!(gpu.draw(3, 1, 0, 0), MissingBinding);
    assert!(e.message.contains("globals"), "{e}");
    gpu.bind(2, f.globals_slot).unwrap();
    let e = expect!(gpu.draw(3, 1, 0, 0), MissingBinding);
    assert!(e.message.contains("vertex buffer 0"), "{e}");
    gpu.set_vertex_buffer(0, f.vertices, 0).unwrap();
    gpu.draw(3, 1, 0, 0).unwrap();

    // Slots of the wrong kind in each binding.
    gpu.bind(0, f.globals_slot).unwrap();
    expect!(gpu.draw(3, 1, 0, 0), Mismatch);
    gpu.bind(0, f.sampler).unwrap();
    expect!(gpu.draw(3, 1, 0, 0), WrongKind);
    gpu.bind(0, f.texture_slot).unwrap();
    gpu.bind(1, f.texture_slot).unwrap();
    expect!(gpu.draw(3, 1, 0, 0), WrongKind);
    gpu.bind(1, f.sampler).unwrap();
    gpu.bind(2, f.texture_slot).unwrap();
    expect!(gpu.draw(3, 1, 0, 0), Mismatch);
    // A uniform range smaller than the shader's block.
    let small = gpu
        .create_buffer(&BufferDesc {
            size: 8,
            usage: BufferUsage::UNIFORM,
        })
        .unwrap();
    let small_slot = gpu.create_uniform_slot(small, 0, 8).unwrap();
    gpu.bind(2, small_slot).unwrap();
    expect!(gpu.draw(3, 1, 0, 0), Mismatch);
    gpu.bind(2, f.globals_slot).unwrap();
    // An integer texture where the shader samples floats.
    let uint = image(&mut gpu, Format::R32Uint, 2, 2, ImageUsage::SAMPLED);
    let uint_slot = gpu.create_texture_slot(uint).unwrap();
    gpu.bind(0, uint_slot).unwrap();
    expect!(gpu.draw(3, 1, 0, 0), Mismatch);
    // The attachment of the open pass as a texture.
    let target_slot = gpu.create_texture_slot(f.target).unwrap();
    gpu.bind(0, target_slot).unwrap();
    expect!(gpu.draw(3, 1, 0, 0), InvalidState);
    // An image created inside the pass has no contents and no layout yet.
    let fresh = image(&mut gpu, Format::Rgba8Unorm, 2, 2, ImageUsage::SAMPLED);
    let fresh_slot = gpu.create_texture_slot(fresh).unwrap();
    gpu.bind(0, fresh_slot).unwrap();
    expect!(gpu.draw(3, 1, 0, 0), InvalidState);

    // Heap slots that are free or outside the heap.
    let freed = gpu.create_texture_slot(f.texture).unwrap();
    gpu.destroy(freed).unwrap();
    expect!(gpu.bind(0, freed), Destroyed);
    // Same kind and device, an index nobody allocated.
    let never = Handle::from_bits(f.texture_slot.to_bits() | 0xf_ffff);
    expect!(gpu.bind(0, never), InvalidHandle);
    // Images that cannot back the requested slot kind.
    expect!(gpu.create_storage_texture_slot(f.texture), InvalidArgument);
    let no_sample = image(&mut gpu, Format::Rgba8Unorm, 2, 2, ImageUsage::COLOR_TARGET);
    expect!(gpu.create_texture_slot(no_sample), InvalidArgument);

    gpu.bind(0, f.texture_slot).unwrap();
    gpu.draw(3, 1, 0, 0).unwrap();
    f.end(&mut gpu);
    f.render_ok(&mut gpu);
    no_validation_errors();
}

#[test]
fn bad_shaders_and_pipelines() {
    let mut gpu = gpu();
    let f = Fixture::new(&mut gpu);

    // Source errors are the compiler's, before the API sees anything (the
    // build tool reports them). What the API sees is SPIR-V whose entry point
    // is not the one asked for: none of that name, or one of another stage.
    expect!(
        wgsl(&mut gpu, ShaderStage::Fragment, QUAD, "nope"),
        ShaderCompile
    );
    expect!(
        wgsl(&mut gpu, ShaderStage::Fragment, QUAD, "vs"),
        ShaderCompile
    );
    // Resources the API does not map.
    expect!(
        wgsl(
            &mut gpu,
            ShaderStage::Fragment,
            "@group(2) @binding(0) var t: texture_2d<f32>;
             @fragment fn main() -> @location(0) vec4<f32> { return textureLoad(t, vec2(0), 0); }",
            "main"
        ),
        ShaderCompile
    );
    expect!(
        wgsl(
            &mut gpu,
            ShaderStage::Fragment,
            "@group(0) @binding(99) var t: texture_2d<f32>;
             @fragment fn main() -> @location(0) vec4<f32> { return textureLoad(t, vec2(0), 0); }",
            "main"
        ),
        ShaderCompile
    );
    // Modules beyond the size limit are refused before the checks see them.
    let e = expect!(
        gpu.create_shader_spirv(ShaderStage::Fragment, "main", &vec![0; MAX_SPIRV_BYTES + 4]),
        ShaderCompile
    );
    assert!(e.message.contains("limit"), "{e}");

    // Stages swapped.
    expect!(
        gpu.create_render_pipeline(&quad_desc(f.fs, f.vs, Format::Rgba8Unorm)),
        Mismatch
    );
    expect!(gpu.create_compute_pipeline(f.fs), Mismatch);

    // The fragment shader reads a location the vertex shader does not write.
    let fs_extra = wgsl(
        &mut gpu,
        ShaderStage::Fragment,
        "@fragment fn main(@location(0) uv: vec2<f32>, @location(1) extra: vec4<f32>) -> @location(0) vec4<f32> { return extra + uv.xyxy; }",
        "main",
    )
    .unwrap();
    let e = expect!(
        gpu.create_render_pipeline(&quad_desc(f.vs, fs_extra, Format::Rgba8Unorm)),
        ShaderInterface
    );
    assert!(e.message.contains("location 1"), "{e}");
    // Same location, different type.
    let fs_int = wgsl(
        &mut gpu,
        ShaderStage::Fragment,
        "@fragment fn main(@location(0) @interpolate(flat) id: vec2<u32>) -> @location(0) vec4<f32> { return vec4(f32(id.x)); }",
        "main",
    )
    .unwrap();
    expect!(
        gpu.create_render_pipeline(&quad_desc(f.vs, fs_int, Format::Rgba8Unorm)),
        ShaderInterface
    );
    // The fragment output does not fit the target's numeric class.
    let fs_uint_out = wgsl(
        &mut gpu,
        ShaderStage::Fragment,
        "@fragment fn main() -> @location(0) vec4<u32> { return vec4(1u); }",
        "main",
    )
    .unwrap();
    expect!(
        gpu.create_render_pipeline(&quad_desc(f.vs, fs_uint_out, Format::Rgba8Unorm)),
        ShaderInterface
    );
    // Two targets, one output.
    let mut two_targets = quad_desc(f.vs, f.fs, Format::Rgba8Unorm);
    two_targets.targets.push(ColorTarget {
        format: Format::Rgba8Unorm,
        blend: None,
    });
    expect!(gpu.create_render_pipeline(&two_targets), ShaderInterface);
    let mut no_targets = quad_desc(f.vs, f.fs, Format::Rgba8Unorm);
    no_targets.targets.clear();
    expect!(gpu.create_render_pipeline(&no_targets), InvalidArgument);

    // The vertex shader reads an attribute nobody provides, or of another type.
    let mut no_attributes = quad_desc(f.vs, f.fs, Format::Rgba8Unorm);
    no_attributes.vertex_buffers.clear();
    expect!(gpu.create_render_pipeline(&no_attributes), ShaderInterface);
    let mut wrong_type = quad_desc(f.vs, f.fs, Format::Rgba8Unorm);
    wrong_type.vertex_buffers[0].attributes[0].format = VertexFormat::U32;
    expect!(gpu.create_render_pipeline(&wrong_type), ShaderInterface);
    // Vertex layouts that do not make sense.
    let mut bad_offset = quad_desc(f.vs, f.fs, Format::Rgba8Unorm);
    bad_offset.vertex_buffers[0].attributes[0].offset = 4;
    expect!(gpu.create_render_pipeline(&bad_offset), InvalidArgument);
    let mut bad_stride = quad_desc(f.vs, f.fs, Format::Rgba8Unorm);
    bad_stride.vertex_buffers[0].stride = 0;
    expect!(gpu.create_render_pipeline(&bad_stride), InvalidArgument);
    let mut twice = quad_desc(f.vs, f.fs, Format::Rgba8Unorm);
    let duplicate = twice.vertex_buffers[0].attributes[0];
    twice.vertex_buffers[0].attributes.push(duplicate);
    expect!(gpu.create_render_pipeline(&twice), InvalidArgument);

    // One binding number, two meanings.
    let vs_conflict = wgsl(
        &mut gpu,
        ShaderStage::Vertex,
        "@group(0) @binding(0) var<uniform> offset: vec4<f32>;
         @vertex fn main(@location(0) p: vec2<f32>) -> @builtin(position) vec4<f32> { return vec4(p, 0.0, 1.0) + offset; }",
        "main",
    )
    .unwrap();
    let fs_plain = wgsl(
        &mut gpu,
        ShaderStage::Fragment,
        "@group(0) @binding(0) var t: texture_2d<f32>;
         @fragment fn main() -> @location(0) vec4<f32> { return textureLoad(t, vec2(0), 0); }",
        "main",
    )
    .unwrap();
    let e = expect!(
        gpu.create_render_pipeline(&quad_desc(vs_conflict, fs_plain, Format::Rgba8Unorm)),
        ShaderInterface
    );
    assert!(e.message.contains("binding 0"), "{e}");

    // A push block that fills the device's push data leaves no room for the
    // heap index of the texture binding.
    let fs_full_push = wgsl(
        &mut gpu,
        ShaderStage::Fragment,
        "struct Push { values: array<vec4<f32>, 16> }
         var<immediate> pc: Push;
         @group(0) @binding(0) var t: texture_2d<f32>;
         @fragment fn main() -> @location(0) vec4<f32> { return pc.values[15] + textureLoad(t, vec2(0), 0); }",
        "main",
    )
    .unwrap();
    let vs_plain = wgsl(
        &mut gpu,
        ShaderStage::Vertex,
        "@vertex fn main(@location(0) p: vec2<f32>) -> @builtin(position) vec4<f32> { return vec4(p, 0.0, 1.0); }",
        "main",
    )
    .unwrap();
    let e = expect!(
        gpu.create_render_pipeline(&quad_desc(vs_plain, fs_full_push, Format::Rgba8Unorm)),
        LimitExceeded
    );
    assert!(e.message.contains("push data"), "{e}");
    // Writable storage in a render pipeline is outside the spike.
    let fs_storage = wgsl(
        &mut gpu,
        ShaderStage::Fragment,
        "@group(0) @binding(0) var<storage, read_write> data: array<u32>;
         @fragment fn main() -> @location(0) vec4<f32> { data[0] = 1u; return vec4(1.0); }",
        "main",
    )
    .unwrap();
    expect!(
        gpu.create_render_pipeline(&quad_desc(vs_plain, fs_storage, Format::Rgba8Unorm)),
        Unsupported
    );

    f.render_ok(&mut gpu);
    no_validation_errors();
}

#[test]
fn calls_in_the_wrong_state() {
    let mut gpu = gpu();
    let f = Fixture::new(&mut gpu);

    // Recording calls without a frame.
    expect!(gpu.begin_pass(&f.pass()), InvalidState);
    expect!(gpu.end_pass(), InvalidState);
    expect!(gpu.end_frame(), InvalidState);
    expect!(gpu.set_pipeline(f.pipeline), InvalidState);
    expect!(gpu.bind(0, f.texture_slot), InvalidState);
    expect!(gpu.draw(3, 1, 0, 0), InvalidState);
    expect!(gpu.dispatch(1, 1, 1), InvalidState);
    expect!(gpu.upload_image(f.texture, &[0; 4]), InvalidState);

    gpu.begin_frame().unwrap();
    expect!(gpu.begin_frame(), InvalidState);
    expect!(gpu.read_image(f.target), InvalidState);
    expect!(gpu.end_pass(), InvalidState);
    expect!(gpu.set_pipeline(f.pipeline), InvalidState);
    expect!(gpu.set_viewport(0.0, 0.0, 8.0, 8.0), InvalidState);
    expect!(gpu.dispatch(1, 1, 1), InvalidState);
    gpu.begin_pass(&f.pass()).unwrap();
    expect!(gpu.begin_pass(&f.pass()), InvalidState);
    expect!(gpu.upload_image(f.texture, &[0; 4]), InvalidState);
    expect!(gpu.copy_image(f.texture, f.target), InvalidState);
    expect!(gpu.dispatch(1, 1, 1), InvalidState);
    // Ending the frame with the pass open: the pass is closed, the frame is
    // submitted, and the caller is told.
    f.bind_all(&mut gpu);
    gpu.draw(3, 1, 0, 0).unwrap();
    let e = expect!(gpu.end_frame(), InvalidState);
    assert!(e.message.contains("render pass was open"), "{e}");
    assert!(!gpu.is_recording());
    let pixels = gpu.read_image(f.target).unwrap();
    assert_eq!(
        &pixels[..4],
        &[0, 255, 0, 255],
        "the partial frame was executed"
    );

    f.render_ok(&mut gpu);
    no_validation_errors();
}

#[test]
fn compute_misuse() {
    let mut gpu = gpu();
    let f = Fixture::new(&mut gpu);
    let shader = wgsl(
        &mut gpu,
        ShaderStage::Compute,
        "@group(0) @binding(0) var src: texture_2d<f32>;
         @group(0) @binding(1) var dst: texture_storage_2d<rgba16float, write>;
         @group(0) @binding(2) var<storage, read_write> counts: array<u32, 4>;
         @compute @workgroup_size(1)
         fn main(@builtin(global_invocation_id) id: vec3<u32>) {
             let p = vec2<i32>(id.xy);
             textureStore(dst, p, textureLoad(src, p, 0) * 2.0);
             counts[0] = counts[0] + 1u;
         }",
        "main",
    )
    .unwrap();
    let pipeline = gpu.create_compute_pipeline(shader).unwrap();
    let usage = ImageUsage::STORAGE | ImageUsage::SAMPLED | ImageUsage::COPY_SRC;
    let dst = image(&mut gpu, Format::Rgba16Float, 2, 2, usage);
    let dst_slot = gpu.create_storage_texture_slot(dst).unwrap();
    let dst_as_texture = gpu.create_texture_slot(dst).unwrap();
    let wrong_format = image(&mut gpu, Format::Rgba8Unorm, 2, 2, usage);
    let wrong_slot = gpu.create_storage_texture_slot(wrong_format).unwrap();
    let counts = gpu
        .create_buffer(&BufferDesc {
            size: 16,
            usage: BufferUsage::STORAGE,
        })
        .unwrap();
    let counts_slot = gpu.create_storage_buffer_slot(counts, 0, 16).unwrap();
    let short_slot = gpu.create_storage_buffer_slot(counts, 0, 8).unwrap();

    gpu.begin_frame().unwrap();
    expect!(gpu.dispatch(1, 1, 1), InvalidState);
    gpu.set_pipeline(pipeline).unwrap();
    expect!(gpu.dispatch(2, 2, 1), MissingBinding);
    gpu.bind(0, f.texture_slot).unwrap();
    gpu.bind(1, wrong_slot).unwrap();
    gpu.bind(2, counts_slot).unwrap();
    expect!(gpu.dispatch(2, 2, 1), Mismatch);
    gpu.bind(1, f.texture_slot).unwrap();
    expect!(gpu.dispatch(2, 2, 1), Mismatch);
    gpu.bind(1, dst_slot).unwrap();
    gpu.bind(2, short_slot).unwrap();
    expect!(gpu.dispatch(2, 2, 1), Mismatch);
    gpu.bind(2, f.globals_slot).unwrap();
    expect!(gpu.dispatch(2, 2, 1), Mismatch);
    gpu.bind(2, counts_slot).unwrap();
    // Reading and writing the same image in one dispatch.
    gpu.bind(0, dst_as_texture).unwrap();
    expect!(gpu.dispatch(2, 2, 1), InvalidState);
    gpu.bind(0, f.texture_slot).unwrap();
    // Valid Vulkan on drivers that allow 2^32 - 1 groups, refused by policy.
    let e = gpu.dispatch(u32::MAX, 1, 1).unwrap_err();
    assert!(
        matches!(e.code, ErrorCode::OutOfRange | ErrorCode::LimitExceeded),
        "{e}"
    );
    expect!(gpu.dispatch(4096, 4096, 1), LimitExceeded);
    // A render pipeline's state is not compute state.
    gpu.begin_pass(&f.pass()).unwrap();
    expect!(gpu.set_pipeline(pipeline), InvalidState);
    gpu.end_pass().unwrap();

    gpu.dispatch(2, 2, 1).unwrap();
    gpu.dispatch(2, 2, 1).unwrap();
    gpu.end_frame().unwrap();

    let pixels = gpu.read_image(dst).unwrap();
    let first = half::f16::from_bits(u16::from_ne_bytes([pixels[0], pixels[1]])).to_f32();
    assert_eq!(first, 2.0, "white texel doubled");
    f.render_ok(&mut gpu);
    no_validation_errors();
}

/// Creating objects without end: every limit turns into an error, nothing is
/// leaked, and the device keeps working.
#[test]
fn resource_exhaustion() {
    let mut config = GpuConfig::for_tests();
    config.limits = Limits {
        max_buffers: 64,
        max_images: 64,
        max_shaders: 8,
        max_pipelines: 4,
        resource_slots: 16,
        sampler_slots: 4,
        max_memory_bytes: 32 << 20,
        ..Limits::default()
    };
    let mut gpu = Gpu::new(config).unwrap();
    let f = Fixture::new(&mut gpu);
    let baseline = gpu.stats();

    // The memory budget: 4 MiB images until the budget says no.
    let mut images = Vec::new();
    let error = loop {
        match gpu.create_image(&ImageDesc {
            format: Format::Rgba8Unorm,
            width: 1024,
            height: 1024,
            usage: ImageUsage::SAMPLED,
        }) {
            Ok(handle) => images.push(handle),
            Err(e) => break e,
        }
        assert!(images.len() < 1000, "the budget never triggered");
    };
    assert_eq!(error.code, ErrorCode::LimitExceeded, "{error}");
    assert!(error.message.contains("memory budget"), "{error}");
    assert_eq!(
        images.len(),
        7,
        "32 MiB budget minus the fixture's few bytes"
    );
    f.render_ok(&mut gpu);
    // Buffers draw from the same budget.
    expect!(
        gpu.create_buffer(&BufferDesc {
            size: 8 << 20,
            usage: BufferUsage::VERTEX
        }),
        LimitExceeded
    );
    for handle in images.drain(..) {
        gpu.destroy(handle).unwrap();
    }
    gpu.wait_idle().unwrap();
    assert_eq!(gpu.stats().memory_bytes, baseline.memory_bytes);

    // Sizes no device could satisfy.
    expect!(
        gpu.create_buffer(&BufferDesc {
            size: u64::MAX,
            usage: BufferUsage::VERTEX
        }),
        LimitExceeded
    );
    expect!(
        gpu.create_buffer(&BufferDesc {
            size: 1 << 50,
            usage: BufferUsage::VERTEX
        }),
        LimitExceeded
    );
    expect!(
        gpu.create_buffer(&BufferDesc {
            size: 0,
            usage: BufferUsage::VERTEX
        }),
        InvalidArgument
    );

    // Object counts.
    let mut buffers = Vec::new();
    let error = loop {
        match gpu.create_buffer(&BufferDesc {
            size: 16,
            usage: BufferUsage::VERTEX,
        }) {
            Ok(handle) => buffers.push(handle),
            Err(e) => break e,
        }
    };
    assert_eq!(error.code, ErrorCode::LimitExceeded, "{error}");
    assert_eq!(buffers.len() as u32 + baseline.live_buffers, 64);
    for handle in buffers.drain(..) {
        gpu.destroy(handle).unwrap();
    }

    // Heap slots: 16 in total, the fixture holds 2.
    let mut slots = Vec::new();
    let error = loop {
        match gpu.create_texture_slot(f.texture) {
            Ok(handle) => slots.push(handle),
            Err(e) => break e,
        }
    };
    assert_eq!(error.code, ErrorCode::LimitExceeded, "{error}");
    assert_eq!(slots.len(), 14);
    // Used in a frame, then freed: the indices come back only once the GPU is done.
    f.begin(&mut gpu);
    for slot in &slots {
        gpu.bind(0, *slot).unwrap();
        gpu.draw(3, 1, 0, 0).unwrap();
    }
    for slot in slots.drain(..) {
        gpu.destroy(slot).unwrap();
    }
    expect!(gpu.create_texture_slot(f.texture), LimitExceeded);
    f.end(&mut gpu);
    gpu.wait_idle().unwrap();
    let slot = gpu.create_texture_slot(f.texture).unwrap();
    gpu.destroy(slot).unwrap();

    let sampler = SamplerDesc {
        mag: Filter::Linear,
        min: Filter::Linear,
        address_u: AddressMode::Repeat,
        address_v: AddressMode::Repeat,
    };
    let mut samplers = Vec::new();
    let error = loop {
        match gpu.create_sampler(&sampler) {
            Ok(handle) => samplers.push(handle),
            Err(e) => break e,
        }
    };
    assert_eq!(error.code, ErrorCode::LimitExceeded, "{error}");
    assert_eq!(samplers.len(), 3);

    // Pipelines and shaders.
    let mut pipelines = Vec::new();
    let error = loop {
        match gpu.create_render_pipeline(&quad_desc(f.vs, f.fs, Format::Rgba8Unorm)) {
            Ok(handle) => pipelines.push(handle),
            Err(e) => break e,
        }
    };
    assert_eq!(error.code, ErrorCode::LimitExceeded, "{error}");
    assert_eq!(pipelines.len(), 3);
    let mut shaders = Vec::new();
    let error = loop {
        match wgsl(&mut gpu, ShaderStage::Vertex, QUAD, "vs") {
            Ok(handle) => shaders.push(handle),
            Err(e) => break e,
        }
    };
    assert_eq!(error.code, ErrorCode::LimitExceeded, "{error}");
    assert_eq!(shaders.len(), 6);

    // Create and destroy forever: nothing accumulates.
    for round in 0..200 {
        let handle = gpu
            .create_image(&ImageDesc {
                format: Format::Rgba8Unorm,
                width: 256,
                height: 256,
                usage: ImageUsage::SAMPLED,
            })
            .unwrap_or_else(|e| panic!("round {round}: {e}"));
        gpu.destroy(handle).unwrap();
        if round % 20 == 0 {
            f.render_ok(&mut gpu);
        }
    }
    gpu.wait_idle().unwrap();
    assert_eq!(gpu.stats().memory_bytes, baseline.memory_bytes);
    assert_eq!(gpu.stats().pending_garbage, 0);
    f.render_ok(&mut gpu);
    no_validation_errors();
}

/// A lost device: every call reports it, and a new device starts clean.
#[test]
fn device_loss_and_recreation() {
    let mut gpu = gpu();
    let f = Fixture::new(&mut gpu);
    f.render_ok(&mut gpu);

    f.begin(&mut gpu);
    gpu.draw(3, 1, 0, 0).unwrap();
    gpu.simulate_device_loss();
    assert!(gpu.is_lost());
    expect!(gpu.draw(3, 1, 0, 0), DeviceLost);
    expect!(gpu.end_pass(), DeviceLost);
    expect!(gpu.end_frame(), DeviceLost);
    expect!(gpu.begin_frame(), DeviceLost);
    expect!(gpu.read_image(f.target), DeviceLost);
    expect!(
        gpu.create_buffer(&BufferDesc {
            size: 16,
            usage: BufferUsage::VERTEX
        }),
        DeviceLost
    );
    expect!(gpu.wait_idle(), DeviceLost);
    // Cleaning up stays possible.
    gpu.destroy(f.texture_slot).unwrap();
    drop(gpu);

    // The host creates a new device and the script sets itself up again.
    let mut gpu = self::gpu();
    // Handles of the old device mean nothing here.
    expect!(gpu.write_buffer(f.vertices, 0, &[0; 4]), InvalidHandle);
    gpu.begin_frame().unwrap();
    expect!(gpu.begin_pass(&f.pass()), InvalidHandle);
    gpu.end_frame().unwrap();
    let f = Fixture::new(&mut gpu);
    f.render_ok(&mut gpu);
    no_validation_errors();
}
