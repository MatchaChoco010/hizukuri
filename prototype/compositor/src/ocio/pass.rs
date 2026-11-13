//! One OCIO conversion as a draw: the shader OCIO generates, compiled to
//! SPIR-V with `slangc`, with its lookup tables as textures and its dynamic
//! properties in a uniform buffer.
//!
//! Two steps, so that the slow one can run away from the frames:
//!
//! - [`prepare`] needs no GPU: it takes the shader out of the processor,
//!   wraps it in a fragment shader and compiles it (or finds the SPIR-V in
//!   the cache on disk). [`Prepared::check`] then runs the SPIR-V checks,
//!   which need no device either.
//! - [`Pass::new`] makes the GPU objects from that, and [`Pass::draw`] draws
//!   one full-target triangle that reads an input image texel by texel.
//!   [`Pass::ready`] has the pipeline for a target format built on another
//!   thread, so that the driver's compilation does not hold up a frame.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use proto_gfx_core::{
    AddressMode, BindingKind, BufferDesc, BufferUsage, CheckedShader, ColorAttachment, ColorTarget,
    CullMode, Filter, Format, Gpu, Handle, ImageDesc, ImageExtra, ImageKind, ImageUsage, LoadOp,
    PendingPipeline, PipelineExtra, RenderPipelineDesc, SamplerDesc, ShaderChecker, ShaderStage,
    Topology,
};
use proto_ocio::{Dynamic, Processor, ShaderDesc, Texture, Uniform, UniformType};
use proto_slangc::{Compiler, Input, Stage};

/// The name of the function OCIO is asked to generate.
const FUNCTION: &str = "OCIOMain";
/// The prefix of the names of OCIO's textures and uniforms.
const PREFIX: &str = "ocio";
/// The binding of the image that is converted. OCIO's own resources get the
/// bindings below it, in the order they are declared.
const INPUT_BINDING: u32 = 15;
const INPUT_NAME: &str = "hz_ocio_input";
const ENTRY: &str = "hz_ocio_fs";

/// The vertex stage (`shaders/ocio.slang`), compiled by build.rs.
const VERTEX_SPIRV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ocio.vs.spv"));
const VERTEX_ENTRY: &str = "vs";

/// What goes around OCIO's shader text. The rows `m0`..`m2` are a matrix
/// applied to the input's RGB, and `params.x` multiplies it, before the
/// conversion; a non-zero `params.y` says the input is stored premultiplied
/// by alpha and the result is to be stored so too.
const WRAPPER: &str = r#"
struct HzOcioPush {
    float4 params;
    float4 m0;
    float4 m1;
    float4 m2;
};
[[vk::push_constant]] ConstantBuffer<HzOcioPush> hz_ocio_push;

[[vk::binding(15, 0)]] Texture2D<float4> hz_ocio_input;

[shader("fragment")]
float4 hz_ocio_fs(float4 position : SV_Position) : SV_Target0 {
    float4 c = hz_ocio_input.Load(int3(int2(position.xy), 0));
    float alpha = c.a;
    bool premultiplied = hz_ocio_push.params.y != 0.0;
    if (premultiplied && alpha > 0.0) {
        c.rgb /= alpha;
    }
    float3 rgb = float3(
        dot(hz_ocio_push.m0.xyz, c.rgb),
        dot(hz_ocio_push.m1.xyz, c.rgb),
        dot(hz_ocio_push.m2.xyz, c.rgb));
    c = OCIOMain(float4(rgb * hz_ocio_push.params.x, alpha));
    if (premultiplied) {
        c.rgb *= alpha;
    }
    return float4(c.rgb, alpha);
}
"#;

/// The Slang source of the fragment stage for one OCIO shader.
pub fn fragment_source(ocio: &str) -> String {
    format!("{ocio}\n{WRAPPER}")
}

/// How long the steps of [`prepare`] took.
#[derive(Clone, Copy, Debug, Default)]
pub struct PrepareTiming {
    /// Taking the shader text and the lookup tables out of OCIO.
    pub extract: Duration,
    /// Running `slangc`. Zero when the SPIR-V came from the cache.
    pub compile: Duration,
    /// The SPIR-V checks ([`Prepared::check`]). Zero when they did not run.
    pub check: Duration,
}

/// Everything a [`Pass`] is made from. No GPU object yet.
pub struct Prepared {
    /// OCIO's hash of the shader and its resources.
    pub cache_id: String,
    pub spirv: Vec<u8>,
    pub textures: Vec<Texture>,
    pub uniforms: Vec<Uniform>,
    /// Kept for its dynamic properties.
    pub desc: ShaderDesc,
    /// The SPIR-V was read from the cache on disk.
    pub cached: bool,
    pub timing: PrepareTiming,
    /// The module after the SPIR-V checks, once [`Prepared::check`] ran.
    pub checked: Option<CheckedShader>,
}

impl Prepared {
    /// Runs the SPIR-V checks of the device `checker` is for, which take
    /// tens of milliseconds on the shaders of the tone-mapping views: meant
    /// to run on the thread that ran [`prepare`]. [`Pass::new`] then makes
    /// the shader without checking it again.
    pub fn check(&mut self, checker: &ShaderChecker) -> Result<(), String> {
        let started = Instant::now();
        let checked = checker
            .check(ShaderStage::Fragment, ENTRY, &self.spirv)
            .map_err(|e| format!("the OCIO shader was refused: {e}"))?;
        self.timing.check = started.elapsed();
        self.checked = Some(checked);
        Ok(())
    }
}

/// FNV-1a, 64 bits: names the cache's files.
fn fnv64(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in *part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // Keeps ("ab", "c") apart from ("a", "bc").
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// What tells one `slangc` from another without running it: its path, size
/// and modification time.
fn compiler_identity(compiler: &Compiler) -> String {
    let meta = std::fs::metadata(&compiler.slangc).ok();
    let modified = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    format!(
        "{}:{}:{}",
        compiler.slangc.display(),
        meta.map_or(0, |m| m.len()),
        modified
    )
}

/// The file of the cache for one source and compiler.
fn cache_file(dir: &Path, cache_id: &str, source: &str, compiler: &Compiler) -> PathBuf {
    let hash = fnv64(&[
        source.as_bytes(),
        compiler_identity(compiler).as_bytes(),
        compiler
            .args(Input::Source(""), Stage::Fragment, ENTRY)
            .join(" ")
            .as_bytes(),
    ]);
    dir.join(format!("{}-{hash:016x}.spv", cache_name(cache_id)))
}

/// The part of a cache file's name that comes from OCIO's identifier of the
/// shader. The identifier is a few words (the language, the names the shader
/// was asked to use) and then a hash in hexadecimal: the hash is kept.
pub fn cache_name(cache_id: &str) -> String {
    let last = cache_id.split_whitespace().next_back().unwrap_or("");
    let name: String = last
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(64)
        .collect();
    if name.is_empty() {
        "shader".into()
    } else {
        name
    }
}

fn is_spirv(bytes: &[u8]) -> bool {
    bytes.len() >= 20 && bytes.len().is_multiple_of(4) && bytes[..4] == 0x0723_0203u32.to_le_bytes()
}

/// Writes a cache file so that a reader never sees half of it.
fn store(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = file.with_extension(format!("spv.{}.partial", std::process::id()));
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, file)
}

/// Takes the shader of `processor` out and compiles it. Slow (`slangc` takes
/// a few hundred milliseconds): meant to run on a thread of its own.
///
/// With `cache_dir`, the SPIR-V is kept there under a name made from OCIO's
/// identifier of the shader, the source that is compiled and the compiler,
/// and read from there when it is found.
pub fn prepare(
    processor: &Processor,
    compiler: Option<&Compiler>,
    cache_dir: Option<&Path>,
) -> Result<Prepared, String> {
    let started = Instant::now();
    let mut desc = processor
        .shader(FUNCTION, PREFIX)
        .map_err(|e| format!("OCIO made no shader: {e}"))?;
    let textures = desc
        .textures()
        .map_err(|e| format!("reading the lookup tables of the OCIO shader: {e}"))?;
    let uniforms = desc
        .uniforms()
        .map_err(|e| format!("reading the uniforms of the OCIO shader: {e}"))?;
    for uniform in &uniforms {
        // Arrays and vectors would need the layout the compiler gave the
        // block, which the reflection does not tell.
        if !matches!(uniform.ty, UniformType::Float | UniformType::Bool) {
            return Err(format!(
                "the OCIO shader has a uniform `{}` of a type that is not supported ({:?})",
                uniform.name, uniform.ty
            ));
        }
    }
    let cache_id = desc.cache_id();
    let source = fragment_source(&desc.text());
    let extract = started.elapsed();

    let compiler = compiler.ok_or_else(|| {
        format!(
            "slangc, the Slang compiler, was not found (looked at {}, next to the executable, \
             and on PATH)",
            proto_slangc::SLANGC_VARIABLE
        )
    })?;
    let file = cache_dir.map(|dir| cache_file(dir, &cache_id, &source, compiler));
    if let Some(bytes) = file.as_ref().and_then(|f| std::fs::read(f).ok())
        && is_spirv(&bytes)
    {
        return Ok(Prepared {
            cache_id,
            spirv: bytes,
            textures,
            uniforms,
            desc,
            cached: true,
            timing: PrepareTiming {
                extract,
                ..PrepareTiming::default()
            },
            checked: None,
        });
    }
    let started = Instant::now();
    let compiled = compiler
        .compile(Input::Source(&source), Stage::Fragment, ENTRY)
        .map_err(|e| format!("compiling the OCIO shader: {e}"))?;
    let compile = started.elapsed();
    if let Some(file) = &file
        && let Err(e) = store(file, &compiled.spirv)
    {
        tracing::warn!(
            "cannot keep the compiled OCIO shader in {}: {e}",
            file.display()
        );
    }
    Ok(Prepared {
        cache_id,
        spirv: compiled.spirv,
        textures,
        uniforms,
        desc,
        cached: false,
        timing: PrepareTiming {
            extract,
            compile,
            ..PrepareTiming::default()
        },
        checked: None,
    })
}

/// How one draw of a [`Pass`] treats the input.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DrawParams {
    /// Multiplies the input's RGB before the conversion.
    pub scale: f32,
    /// The input is stored premultiplied by alpha, and so is the result.
    pub premultiplied: bool,
    /// Applied to the input's RGB first (row-major): from the colour space
    /// the input is in to the one the conversion starts from.
    pub matrix: [[f32; 3]; 3],
}

/// The matrix that changes nothing.
pub const IDENTITY: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

impl Default for DrawParams {
    fn default() -> Self {
        Self {
            scale: 1.0,
            premultiplied: false,
            matrix: IDENTITY,
        }
    }
}

struct Lut {
    image: Handle,
    slot: Handle,
    sampler: Handle,
    texture_binding: u32,
    sampler_binding: u32,
}

struct UniformBlock {
    binding: u32,
    buffer: Handle,
    slot: Handle,
    size: u64,
}

/// The vertex stage, which every pass shares. Created once per device.
pub fn create_vertex_shader(gpu: &mut Gpu) -> Result<Handle, String> {
    let vertex = gpu
        .create_shader_spirv(ShaderStage::Vertex, VERTEX_ENTRY, VERTEX_SPIRV)
        .map_err(|e| format!("the vertex shader of the OCIO pass: {e}"))?;
    gpu.protect(vertex);
    Ok(vertex)
}

/// A pipeline that is being built on another thread.
struct Building {
    pending: PendingPipeline,
    started: Instant,
    /// The time the frame's thread spent starting the build.
    prepare: Duration,
}

/// One conversion, ready to draw.
pub struct Pass {
    pub cache_id: String,
    vertex: Handle,
    fragment: Handle,
    /// One pipeline per format of the target.
    pipelines: HashMap<Format, Handle>,
    /// The pipelines that are being built, by format of the target.
    building: HashMap<Format, Building>,
    luts: Vec<Lut>,
    uniform: Option<UniformBlock>,
    uniforms: Vec<Uniform>,
    desc: ShaderDesc,
}

fn f32_bytes(values: impl Iterator<Item = f32>) -> Vec<u8> {
    values.flat_map(f32::to_le_bytes).collect()
}

impl Pass {
    /// Makes the GPU objects of a conversion. Called while a frame is being
    /// recorded and no pass is open (the lookup tables are uploaded). The
    /// objects are the core's: replacing the script keeps them.
    ///
    /// A module that went through [`Prepared::check`] becomes a shader
    /// without being checked again; one that did not is checked here.
    pub fn new(gpu: &mut Gpu, vertex: Handle, prepared: Prepared) -> Result<Self, String> {
        let fragment = match prepared.checked {
            Some(checked) => gpu.create_shader_checked(checked),
            None => gpu.create_shader_spirv(ShaderStage::Fragment, ENTRY, &prepared.spirv),
        }
        .map_err(|e| format!("the OCIO shader was refused: {e}"))?;
        gpu.protect(fragment);
        let mut pass = Self {
            cache_id: prepared.cache_id,
            vertex,
            fragment,
            pipelines: HashMap::new(),
            building: HashMap::new(),
            luts: Vec::new(),
            uniform: None,
            uniforms: prepared.uniforms,
            desc: prepared.desc,
        };
        match pass.create_resources(gpu, &prepared.textures) {
            Ok(()) => Ok(pass),
            Err(e) => {
                pass.destroy(gpu);
                Err(e)
            }
        }
    }

    fn create_resources(&mut self, gpu: &mut Gpu, textures: &[Texture]) -> Result<(), String> {
        let reflection = gpu
            .shader_reflection(self.fragment)
            .map_err(|e| e.to_string())?;
        let bindings: Vec<(u32, String, BindingKind)> = reflection
            .bindings
            .iter()
            .map(|b| (b.binding, b.name.clone(), b.kind))
            .collect();
        let find = |name: &str| bindings.iter().find(|b| b.1 == name).map(|b| b.0);
        if find(INPUT_NAME) != Some(INPUT_BINDING) {
            return Err("the OCIO shader lost its input texture".into());
        }
        for texture in textures {
            // A lookup table the compiler found unused has no binding.
            let (Some(texture_binding), Some(sampler_binding)) =
                (find(&texture.name), find(&texture.sampler))
            else {
                continue;
            };
            let lut = create_lut(gpu, texture, texture_binding, sampler_binding)
                .map_err(|e| format!("the lookup table `{}`: {e}", texture.name))?;
            self.luts.push(lut);
        }
        for (binding, name, kind) in &bindings {
            if let BindingKind::UniformBuffer { min_size } = kind {
                if self.uniform.is_some() {
                    return Err(format!("a second uniform buffer `{name}`"));
                }
                let needed = self
                    .uniforms
                    .iter()
                    .map(|u| u.buffer_offset as u64 + 4)
                    .max()
                    .unwrap_or(0);
                if needed > *min_size {
                    return Err(format!(
                        "the uniforms of the OCIO shader need {needed} bytes, its block has \
                         {min_size}"
                    ));
                }
                let size = (*min_size).max(16).next_multiple_of(16);
                let buffer = gpu
                    .create_buffer(&BufferDesc {
                        size,
                        usage: BufferUsage::UNIFORM,
                    })
                    .map_err(|e| e.to_string())?;
                gpu.protect(buffer);
                let slot = match gpu.create_uniform_slot(buffer, 0, size) {
                    Ok(slot) => slot,
                    Err(e) => {
                        let _ = gpu.destroy(buffer);
                        return Err(e.to_string());
                    }
                };
                gpu.protect(slot);
                self.uniform = Some(UniformBlock {
                    binding: *binding,
                    buffer,
                    slot,
                    size,
                });
            }
        }
        if self.uniform.is_none() && !self.uniforms.is_empty() {
            // Every uniform was optimized away: nothing to write.
            self.uniforms.clear();
        }
        self.write_uniforms(gpu)
    }

    fn write_uniforms(&mut self, gpu: &mut Gpu) -> Result<(), String> {
        let Some(block) = &self.uniform else {
            return Ok(());
        };
        let mut bytes = vec![0u8; block.size as usize];
        for uniform in &self.uniforms {
            let value = uniform.value.first().copied().unwrap_or(0.0);
            let encoded = match uniform.ty {
                UniformType::Bool => u32::from(value != 0.0).to_le_bytes(),
                _ => value.to_le_bytes(),
            };
            bytes[uniform.buffer_offset..uniform.buffer_offset + 4].copy_from_slice(&encoded);
        }
        gpu.write_buffer(block.buffer, 0, &bytes)
            .map_err(|e| e.to_string())
    }

    /// Whether the conversion has this control.
    pub fn has_dynamic(&self, property: Dynamic) -> bool {
        self.desc.has_dynamic(property)
    }

    /// Changes a dynamic property. The shader stays; the next draws read
    /// the new value.
    pub fn set_dynamic(
        &mut self,
        gpu: &mut Gpu,
        property: Dynamic,
        value: f64,
    ) -> Result<(), String> {
        self.desc
            .set_dynamic(property, value)
            .map_err(|e| e.to_string())?;
        let current = self.desc.uniforms().map_err(|e| e.to_string())?;
        for uniform in &mut self.uniforms {
            if let Some(new) = current.iter().find(|u| u.name == uniform.name) {
                uniform.value.clone_from(&new.value);
            }
        }
        self.write_uniforms(gpu)
    }

    fn pipeline_desc(&self, format: Format) -> RenderPipelineDesc {
        RenderPipelineDesc {
            vertex: self.vertex,
            fragment: self.fragment,
            vertex_buffers: vec![],
            targets: vec![ColorTarget {
                format,
                blend: None,
            }],
            topology: Topology::TriangleList,
            cull: CullMode::None,
        }
    }

    /// The pipeline for `format`, made here and now when there is none (the
    /// driver compiles the shaders: tens of milliseconds).
    fn pipeline(&mut self, gpu: &mut Gpu, format: Format) -> Result<Handle, String> {
        if let Some(pipeline) = self.pipelines.get(&format) {
            return Ok(*pipeline);
        }
        if let Some(building) = self.building.remove(&format) {
            gpu.cancel_render_pipeline(building.pending);
        }
        let pipeline = gpu
            .create_render_pipeline(&self.pipeline_desc(format))
            .map_err(|e| format!("the pipeline of the OCIO pass: {e}"))?;
        gpu.protect(pipeline);
        self.pipelines.insert(format, pipeline);
        Ok(pipeline)
    }

    /// Whether the pipeline for targets of `format` is there. The first
    /// call starts building it on a thread of its own and answers `false`;
    /// later calls take it when it is done. An error is a build that failed
    /// (the pass cannot draw into this format).
    ///
    /// `what` names the conversion in the log.
    pub fn ready(&mut self, gpu: &mut Gpu, format: Format, what: &str) -> Result<bool, String> {
        if self.pipelines.contains_key(&format) {
            return Ok(true);
        }
        let Some(building) = self.building.get(&format) else {
            let started = Instant::now();
            let pending = gpu
                .create_render_pipeline_in_background(
                    &self.pipeline_desc(format),
                    &PipelineExtra::default(),
                )
                .map_err(|e| format!("the pipeline of the OCIO pass: {e}"))?;
            self.building.insert(
                format,
                Building {
                    pending,
                    started,
                    prepare: started.elapsed(),
                },
            );
            return Ok(false);
        };
        let pipeline = match gpu.finish_render_pipeline(&building.pending) {
            Ok(None) => return Ok(false),
            Ok(Some(pipeline)) => pipeline,
            Err(e) => {
                if let Some(building) = self.building.remove(&format) {
                    gpu.cancel_render_pipeline(building.pending);
                }
                return Err(format!("the pipeline of the OCIO pass: {e}"));
            }
        };
        gpu.protect(pipeline);
        tracing::info!(
            "OCIO conversion {what}: the pipeline for {format:?} is ready after {:.1} ms \
             (on the frame's thread: {:.3} ms to start it)",
            building.started.elapsed().as_secs_f64() * 1e3,
            building.prepare.as_secs_f64() * 1e3,
        );
        self.building.remove(&format);
        self.pipelines.insert(format, pipeline);
        Ok(true)
    }

    /// Whether a pipeline is being built.
    pub fn is_building(&self) -> bool {
        !self.building.is_empty()
    }

    /// Converts `input` (a texture slot of an image as large as `target`)
    /// into `target`, in a pass of its own. Called while a frame is being
    /// recorded and no pass is open.
    pub fn draw(
        &mut self,
        gpu: &mut Gpu,
        input: Handle,
        target: Handle,
        params: DrawParams,
    ) -> Result<(), String> {
        let format = gpu.image_info(target).map_err(|e| e.to_string())?.format;
        let pipeline = self.pipeline(gpu, format)?;
        gpu.begin_pass(&[ColorAttachment {
            image: target,
            load: LoadOp::DontCare,
            store: true,
        }])
        .map_err(|e| e.to_string())?;
        let drawn = self.record(gpu, pipeline, input, params);
        let ended = gpu.end_pass();
        drawn.and(ended).map_err(|e| e.to_string())
    }

    fn record(
        &self,
        gpu: &mut Gpu,
        pipeline: Handle,
        input: Handle,
        params: DrawParams,
    ) -> proto_gfx_core::Result<()> {
        gpu.set_pipeline(pipeline)?;
        gpu.bind(INPUT_BINDING, input)?;
        for lut in &self.luts {
            gpu.bind(lut.texture_binding, lut.slot)?;
            gpu.bind(lut.sampler_binding, lut.sampler)?;
        }
        if let Some(block) = &self.uniform {
            gpu.bind(block.binding, block.slot)?;
        }
        let mut push = vec![
            params.scale,
            f32::from(u8::from(params.premultiplied)),
            0.0,
            0.0,
        ];
        for row in params.matrix {
            push.extend([row[0], row[1], row[2], 0.0]);
        }
        gpu.push(0, &f32_bytes(push.into_iter()))?;
        gpu.draw(3, 1, 0, 0)
    }

    /// Destroys the GPU objects.
    pub fn destroy(&mut self, gpu: &mut Gpu) {
        for (_, building) in self.building.drain() {
            gpu.cancel_render_pipeline(building.pending);
        }
        let mut handles: Vec<Handle> = self.pipelines.drain().map(|(_, p)| p).collect();
        for lut in self.luts.drain(..) {
            handles.extend([lut.slot, lut.sampler, lut.image]);
        }
        if let Some(block) = self.uniform.take() {
            handles.extend([block.slot, block.buffer]);
        }
        handles.push(self.fragment);
        for handle in handles {
            let _ = gpu.destroy(handle);
        }
    }
}

/// The image, slot and sampler of one lookup table. One-channel tables are
/// R32Float; RGB ones are padded to RGBA32Float (there is no three-channel
/// format).
fn create_lut(
    gpu: &mut Gpu,
    texture: &Texture,
    texture_binding: u32,
    sampler_binding: u32,
) -> Result<Lut, String> {
    let (format, data) = match texture.channels {
        1 => (Format::R32Float, f32_bytes(texture.values.iter().copied())),
        3 => (
            Format::Rgba32Float,
            f32_bytes(
                texture
                    .values
                    .chunks_exact(3)
                    .flat_map(|c| [c[0], c[1], c[2], 1.0]),
            ),
        ),
        other => return Err(format!("{other} channels")),
    };
    let image = gpu
        .create_image_with(
            &ImageDesc {
                format,
                width: texture.width,
                height: texture.height,
                usage: ImageUsage::SAMPLED | ImageUsage::COPY_DST,
            },
            &ImageExtra {
                kind: if texture.is_3d() {
                    ImageKind::D3
                } else {
                    ImageKind::D2
                },
                depth: texture.depth,
                ..ImageExtra::default()
            },
        )
        .map_err(|e| e.to_string())?;
    let made = (|| -> proto_gfx_core::Result<(Handle, Handle)> {
        gpu.upload_image(image, &data)?;
        let slot = gpu.create_texture_slot(image)?;
        let filter = if texture.linear {
            Filter::Linear
        } else {
            Filter::Nearest
        };
        let sampler = match gpu.create_sampler(&SamplerDesc {
            mag: filter,
            min: filter,
            address_u: AddressMode::ClampToEdge,
            address_v: AddressMode::ClampToEdge,
        }) {
            Ok(sampler) => sampler,
            Err(e) => {
                let _ = gpu.destroy(slot);
                return Err(e);
            }
        };
        Ok((slot, sampler))
    })();
    match made {
        Ok((slot, sampler)) => {
            for handle in [image, slot, sampler] {
                gpu.protect(handle);
            }
            Ok(Lut {
                image,
                slot,
                sampler,
                texture_binding,
                sampler_binding,
            })
        }
        Err(e) => {
            let _ = gpu.destroy(image);
            Err(e.to_string())
        }
    }
}
