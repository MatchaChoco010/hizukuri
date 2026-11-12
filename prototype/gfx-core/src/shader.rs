//! Shader compilation and reflection.
//!
//! The GPU API never looks at shader source. It asks a [`ShaderCompiler`] for
//! SPIR-V plus a [`Reflection`], and checks pipelines and draws against the
//! reflection. Swapping the shader language or the compiler means implementing
//! this trait again; nothing else in the crate depends on naga.

use crate::format::{Format, NumClass};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ShaderStage {
    Vertex,
    Fragment,
    Compute,
}

impl ShaderStage {
    pub fn name(self) -> &'static str {
        match self {
            Self::Vertex => "vertex",
            Self::Fragment => "fragment",
            Self::Compute => "compute",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "vertex" => Some(Self::Vertex),
            "fragment" => Some(Self::Fragment),
            "compute" => Some(Self::Compute),
            _ => None,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ShaderLanguage {
    Wgsl,
    Glsl,
}

impl ShaderLanguage {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "wgsl" => Some(Self::Wgsl),
            "glsl" => Some(Self::Glsl),
            _ => None,
        }
    }
}

pub struct ShaderSource<'a> {
    pub stage: ShaderStage,
    pub language: ShaderLanguage,
    pub source: &'a str,
    /// Entry point name. GLSL always uses `main`.
    pub entry: &'a str,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StorageAccess {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

/// What a shader expects behind one `binding`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BindingKind {
    /// A 2D texture that is sampled or loaded.
    SampledTexture {
        class: NumClass,
    },
    /// A fixed-size array of 2D textures, indexed in the shader. Backed by
    /// `count` consecutive heap slots. The compiler clamps the index.
    TextureArray {
        class: NumClass,
        count: u32,
    },
    Sampler,
    /// A 2D storage texture.
    StorageTexture {
        format: Format,
        access: StorageAccess,
    },
    UniformBuffer {
        min_size: u64,
    },
    StorageBuffer {
        min_size: u64,
        writable: bool,
    },
}

impl BindingKind {
    pub fn describe(&self) -> String {
        match self {
            Self::SampledTexture { class } => format!("sampled texture ({class:?})"),
            Self::TextureArray { class, count } => {
                format!("array of {count} sampled textures ({class:?})")
            }
            Self::Sampler => "sampler".into(),
            Self::StorageTexture { format, access } => {
                format!("storage texture ({}, {access:?})", format.name())
            }
            Self::UniformBuffer { min_size } => format!("uniform buffer (>= {min_size} bytes)"),
            Self::StorageBuffer { min_size, writable } => format!(
                "{} storage buffer (>= {min_size} bytes)",
                if *writable { "read-write" } else { "read-only" }
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingInfo {
    pub binding: u32,
    pub name: String,
    pub kind: BindingKind,
    /// Whether the entry point statically uses the variable.
    pub used: bool,
}

/// One `location` of the stage inputs or outputs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IoVar {
    pub location: u32,
    pub class: NumClass,
    pub components: u32,
}

#[derive(Clone, Debug)]
pub struct Reflection {
    pub stage: ShaderStage,
    pub bindings: Vec<BindingInfo>,
    /// Size in bytes of the push-constant block, 0 if there is none.
    pub push_size: u32,
    pub inputs: Vec<IoVar>,
    pub outputs: Vec<IoVar>,
    /// `(texture binding, sampler binding)` pairs that are used together.
    pub sampling_pairs: Vec<(u32, u32)>,
    pub workgroup_size: [u32; 3],
}

pub struct CompiledShader {
    pub spirv: Vec<u32>,
    /// Entry point name in the SPIR-V module.
    pub entry: String,
    pub reflection: Reflection,
}

/// Turns shader source into SPIR-V and a description of its interface.
///
/// Requirements on the SPIR-V, which the GPU API relies on for safety:
///
/// - Resources are declared with `DescriptorSet 0` and a `Binding` below
///   [`MAX_BINDINGS`]; the module never reads the descriptor heap directly.
/// - The reflection lists every such resource.
pub trait ShaderCompiler {
    fn compile(&self, source: &ShaderSource<'_>) -> Result<CompiledShader, String>;
}

/// Bindings are numbered `0..MAX_BINDINGS`.
pub const MAX_BINDINGS: u32 = 16;

/// Largest texture array a shader may declare. Every element is validated
/// when the bindings of a draw change, so this bounds that work.
pub const MAX_TEXTURE_ARRAY: u32 = 1024;

/// [`ShaderCompiler`] backed by naga (WGSL and Vulkan-flavoured GLSL).
#[derive(Default)]
pub struct NagaCompiler;

impl ShaderCompiler for NagaCompiler {
    fn compile(&self, source: &ShaderSource<'_>) -> Result<CompiledShader, String> {
        let naga_stage = match source.stage {
            ShaderStage::Vertex => naga::ShaderStage::Vertex,
            ShaderStage::Fragment => naga::ShaderStage::Fragment,
            ShaderStage::Compute => naga::ShaderStage::Compute,
        };
        let (module, entry) = match source.language {
            ShaderLanguage::Wgsl => (
                naga::front::wgsl::parse_str(source.source)
                    .map_err(|e| e.emit_to_string(source.source))?,
                source.entry,
            ),
            ShaderLanguage::Glsl => (
                naga::front::glsl::Frontend::default()
                    .parse(&naga::front::glsl::Options::from(naga_stage), source.source)
                    .map_err(|e| e.emit_to_string(source.source))?,
                "main",
            ),
        };
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::IMMEDIATES
                | naga::valid::Capabilities::TEXTURE_AND_SAMPLER_BINDING_ARRAY
                | naga::valid::Capabilities::TEXTURE_AND_SAMPLER_BINDING_ARRAY_NON_UNIFORM_INDEXING,
        )
        .validate(&module)
        .map_err(|e| e.emit_to_string(source.source))?;

        let Some(entry_index) = module
            .entry_points
            .iter()
            .position(|e| e.name == entry && e.stage == naga_stage)
        else {
            return Err(format!(
                "no {} entry point named `{entry}`",
                source.stage.name()
            ));
        };
        let reflection = reflect(&module, &info, entry_index, source.stage)?;

        use naga::back::spv;
        let policy = naga::proc::BoundsCheckPolicy::Restrict;
        let options = spv::Options {
            lang_version: (1, 3),
            // Keep Vulkan's clip space; do not flip Y behind the script's back.
            flags: spv::WriterFlags::LABEL_VARYINGS | spv::WriterFlags::CLAMP_FRAG_DEPTH,
            bounds_check_policies: naga::proc::BoundsCheckPolicies {
                index: policy,
                buffer: policy,
                image_load: policy,
                binding_array: policy,
            },
            ..Default::default()
        };
        let pipeline_options = spv::PipelineOptions {
            shader_stage: naga_stage,
            entry_point: entry.to_owned(),
        };
        let spirv = spv::write_vec(&module, &info, &options, Some(&pipeline_options))
            .map_err(|e| format!("SPIR-V generation failed: {e}"))?;
        Ok(CompiledShader {
            spirv,
            entry: entry.to_owned(),
            reflection,
        })
    }
}

fn reflect(
    module: &naga::Module,
    info: &naga::valid::ModuleInfo,
    entry_index: usize,
    stage: ShaderStage,
) -> Result<Reflection, String> {
    let entry = &module.entry_points[entry_index];
    let entry_info = info.get_entry_point(entry_index);
    let mut bindings: Vec<BindingInfo> = Vec::new();
    let mut push_size = 0;

    for (handle, var) in module.global_variables.iter() {
        let name = var.name.clone().unwrap_or_default();
        let inner = &module.types[var.ty].inner;
        if var.space == naga::AddressSpace::Immediate {
            push_size = push_size.max(inner.size(module.to_ctx()));
            continue;
        }
        let Some(binding) = &var.binding else {
            continue;
        };
        if binding.group != 0 {
            return Err(format!(
                "`{name}`: only group/set 0 is supported, got {}",
                binding.group
            ));
        }
        if binding.binding >= MAX_BINDINGS {
            return Err(format!(
                "`{name}`: binding {} is out of range (must be below {MAX_BINDINGS})",
                binding.binding
            ));
        }
        let kind = match (*inner).clone() {
            naga::TypeInner::Image {
                dim,
                arrayed,
                class,
            } => {
                if dim != naga::ImageDimension::D2 || arrayed {
                    return Err(format!(
                        "`{name}`: only non-arrayed 2D textures are supported"
                    ));
                }
                match class {
                    naga::ImageClass::Sampled { kind, multi: false } => {
                        BindingKind::SampledTexture {
                            class: scalar_class(kind)
                                .ok_or_else(|| format!("`{name}`: unsupported sample type"))?,
                        }
                    }
                    naga::ImageClass::Storage { format, access } => BindingKind::StorageTexture {
                        format: storage_format(format).ok_or_else(|| {
                            format!("`{name}`: storage format {format:?} is not supported")
                        })?,
                        access: match (
                            access.contains(naga::StorageAccess::LOAD),
                            access.contains(naga::StorageAccess::STORE),
                        ) {
                            (true, true) => StorageAccess::ReadWrite,
                            (true, false) => StorageAccess::ReadOnly,
                            _ => StorageAccess::WriteOnly,
                        },
                    },
                    _ => {
                        return Err(format!(
                            "`{name}`: depth, multisampled and external textures are not supported"
                        ));
                    }
                }
            }
            naga::TypeInner::Sampler { comparison: false } => BindingKind::Sampler,
            naga::TypeInner::Sampler { comparison: true } => {
                return Err(format!("`{name}`: comparison samplers are not supported"));
            }
            naga::TypeInner::BindingArray { base, size } => {
                // The index into the array comes from GPU data, which the API
                // cannot see. It is safe because the array has a fixed size,
                // naga clamps the index to it (`BoundsCheckPolicy::Restrict`),
                // and every element is validated before a draw.
                let naga::ArraySize::Constant(count) = size else {
                    return Err(format!(
                        "`{name}`: binding arrays must have a constant size"
                    ));
                };
                let count = count.get();
                if count > MAX_TEXTURE_ARRAY {
                    return Err(format!(
                        "`{name}`: binding array of {count} exceeds the limit {MAX_TEXTURE_ARRAY}"
                    ));
                }
                match module.types[base].inner {
                    naga::TypeInner::Image {
                        dim: naga::ImageDimension::D2,
                        arrayed: false,
                        class: naga::ImageClass::Sampled { kind, multi: false },
                    } => BindingKind::TextureArray {
                        class: scalar_class(kind)
                            .ok_or_else(|| format!("`{name}`: unsupported sample type"))?,
                        count,
                    },
                    _ => {
                        return Err(format!(
                            "`{name}`: only binding arrays of 2D sampled textures are supported"
                        ));
                    }
                }
            }
            other => {
                let min_size = u64::from(other.size(module.to_ctx()));
                match var.space {
                    naga::AddressSpace::Uniform => BindingKind::UniformBuffer { min_size },
                    naga::AddressSpace::Storage { access } => BindingKind::StorageBuffer {
                        min_size,
                        writable: access.contains(naga::StorageAccess::STORE),
                    },
                    _ => return Err(format!("`{name}`: unsupported resource type")),
                }
            }
        };
        if bindings.iter().any(|b| b.binding == binding.binding) {
            return Err(format!(
                "`{name}`: binding {} is declared twice",
                binding.binding
            ));
        }
        bindings.push(BindingInfo {
            binding: binding.binding,
            name,
            kind,
            used: !entry_info[handle].is_empty(),
        });
    }
    bindings.sort_by_key(|b| b.binding);

    let mut inputs = Vec::new();
    for argument in &entry.function.arguments {
        collect_io(module, argument.ty, argument.binding.as_ref(), &mut inputs)?;
    }
    let mut outputs = Vec::new();
    if let Some(result) = &entry.function.result {
        collect_io(module, result.ty, result.binding.as_ref(), &mut outputs)?;
    }
    inputs.sort_by_key(|v| v.location);
    outputs.sort_by_key(|v| v.location);

    let mut sampling_pairs = Vec::new();
    for key in &entry_info.sampling_set {
        let texture = module.global_variables[key.image].binding.as_ref();
        let sampler = module.global_variables[key.sampler].binding.as_ref();
        if let (Some(texture), Some(sampler)) = (texture, sampler) {
            sampling_pairs.push((texture.binding, sampler.binding));
        }
    }
    sampling_pairs.sort_unstable();

    Ok(Reflection {
        stage,
        bindings,
        push_size,
        inputs,
        outputs,
        sampling_pairs,
        workgroup_size: entry.workgroup_size,
    })
}

fn collect_io(
    module: &naga::Module,
    ty: naga::Handle<naga::Type>,
    binding: Option<&naga::Binding>,
    out: &mut Vec<IoVar>,
) -> Result<(), String> {
    match binding {
        Some(naga::Binding::Location { location, .. }) => {
            let (kind, components) = match module.types[ty].inner {
                naga::TypeInner::Scalar(scalar) => (scalar.kind, 1),
                naga::TypeInner::Vector { size, scalar } => (scalar.kind, size as u32),
                _ => return Err(format!("location {location}: unsupported type")),
            };
            out.push(IoVar {
                location: *location,
                class: scalar_class(kind)
                    .ok_or_else(|| format!("location {location}: unsupported scalar type"))?,
                components,
            });
        }
        Some(naga::Binding::BuiltIn(_)) => {}
        None => {
            if let naga::TypeInner::Struct { members, .. } = &module.types[ty].inner {
                for member in members {
                    collect_io(module, member.ty, member.binding.as_ref(), out)?;
                }
            }
        }
    }
    Ok(())
}

fn scalar_class(kind: naga::ScalarKind) -> Option<NumClass> {
    match kind {
        naga::ScalarKind::Float => Some(NumClass::Float),
        naga::ScalarKind::Sint => Some(NumClass::Sint),
        naga::ScalarKind::Uint => Some(NumClass::Uint),
        _ => None,
    }
}

fn storage_format(format: naga::StorageFormat) -> Option<Format> {
    use naga::StorageFormat as S;
    Some(match format {
        S::R8Unorm => Format::R8Unorm,
        S::Rg8Unorm => Format::Rg8Unorm,
        S::Rgba8Unorm => Format::Rgba8Unorm,
        S::Bgra8Unorm => Format::Bgra8Unorm,
        S::Rgb10a2Unorm => Format::Rgb10a2Unorm,
        S::R16Float => Format::R16Float,
        S::Rg16Float => Format::Rg16Float,
        S::Rgba16Float => Format::Rgba16Float,
        S::R32Float => Format::R32Float,
        S::Rgba32Float => Format::Rgba32Float,
        S::R32Uint => Format::R32Uint,
        S::Rgba8Uint => Format::Rgba8Uint,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(stage: ShaderStage, source: &str) -> Result<CompiledShader, String> {
        NagaCompiler.compile(&ShaderSource {
            stage,
            language: ShaderLanguage::Wgsl,
            source,
            entry: "main",
        })
    }

    #[test]
    fn reflects_bindings_io_and_push_data() {
        let shader = compile(
            ShaderStage::Fragment,
            r#"
            struct Push { tint: vec4<f32>, scale: f32 }
            var<immediate> push: Push;
            struct Globals { gain: vec4<f32> }
            @group(0) @binding(0) var tex: texture_2d<f32>;
            @group(0) @binding(1) var smp: sampler;
            @group(0) @binding(3) var<uniform> globals: Globals;
            @group(0) @binding(4) var unused: texture_2d<u32>;
            @fragment
            fn main(@location(0) uv: vec2<f32>, @location(2) @interpolate(flat) id: u32) -> @location(0) vec4<f32> {
                return textureSample(tex, smp, uv) * push.tint * globals.gain * push.scale;
            }
            "#,
        )
        .unwrap();
        let r = &shader.reflection;
        assert_eq!(r.push_size, 32);
        assert_eq!(r.bindings.len(), 4);
        assert_eq!(
            r.bindings[0].kind,
            BindingKind::SampledTexture {
                class: NumClass::Float
            }
        );
        assert_eq!(r.bindings[1].kind, BindingKind::Sampler);
        assert_eq!(
            r.bindings[2].kind,
            BindingKind::UniformBuffer { min_size: 16 }
        );
        assert!(r.bindings[2].used);
        assert!(!r.bindings[3].used);
        assert_eq!(r.sampling_pairs, vec![(0, 1)]);
        assert_eq!(
            r.inputs,
            vec![
                IoVar {
                    location: 0,
                    class: NumClass::Float,
                    components: 2
                },
                IoVar {
                    location: 2,
                    class: NumClass::Uint,
                    components: 1
                },
            ]
        );
        assert_eq!(r.outputs.len(), 1);
        assert_eq!(shader.spirv[0], 0x0723_0203);
    }

    #[test]
    fn rejects_bad_source_and_unsupported_resources() {
        assert!(compile(ShaderStage::Fragment, "fn main( {").is_err());
        // Type error caught by validation.
        assert!(
            compile(
                ShaderStage::Fragment,
                "@fragment fn main() -> @location(0) vec4<f32> { return 1.0; }"
            )
            .is_err()
        );
        // Wrong stage for the entry point.
        assert!(
            compile(
                ShaderStage::Vertex,
                "@fragment fn main() -> @location(0) vec4<f32> { return vec4(1.0); }"
            )
            .is_err()
        );
        let err = compile(
            ShaderStage::Fragment,
            r#"
            @group(1) @binding(0) var tex: texture_2d<f32>;
            @fragment fn main() -> @location(0) vec4<f32> { return textureLoad(tex, vec2(0), 0); }
            "#,
        )
        .err()
        .unwrap();
        assert!(err.contains("group/set 0"), "{err}");
    }
}
