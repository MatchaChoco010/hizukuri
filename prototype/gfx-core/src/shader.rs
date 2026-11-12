//! What the GPU API knows about a shader: its stage, and the interface it
//! declares (the reflection), which pipelines and draws are checked against.
//! The reflection comes from the SPIR-V itself (`spirv_check.rs`).

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
    /// `count` consecutive heap slots. The index is clamped to the array
    /// before the device sees the shader.
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

pub(crate) struct CompiledShader {
    pub spirv: Vec<u32>,
    /// Entry point name in the SPIR-V module.
    pub entry: String,
    pub reflection: Reflection,
}

/// Bindings are numbered `0..MAX_BINDINGS`.
pub const MAX_BINDINGS: u32 = 16;

/// Largest texture array a shader may declare. Every element is validated
/// when the bindings of a draw change, so this bounds that work.
pub const MAX_TEXTURE_ARRAY: u32 = 1024;
