//! Checks and reflection of SPIR-V that the GPU API did not write itself.
//!
//! [`crate::spirv_input`] runs these last, on a module that SPIRV-Tools has
//! already validated and passed through `graphics-robust-access`. They do not
//! trust either step: a module is accepted only if
//!
//! - every capability, extension and extended instruction set is on a list
//!   of things the device has enabled and the API can reason about
//!   (no device addresses, no variable pointers, no untyped pointers, no
//!   direct descriptor heap access, no unbounded descriptor arrays);
//! - the addressing model is `Logical` and the version is at least 1.4 (so
//!   the entry point lists every global it uses);
//! - every resource is declared with `DescriptorSet 0` and a `Binding`
//!   below [`MAX_BINDINGS`], in a form [`BindingKind`] describes;
//! - every index into a fixed-size array, a vector or a matrix in an access
//!   chain is a constant in range or the result of a clamp to the range
//!   (`GLSL.std.450` `SClamp`, `UClamp` or `UMin` with constant bounds), as
//!   written by SPIRV-Tools' `graphics-robust-access` pass. Runtime arrays
//!   are allowed only at the end of a storage buffer, where the device's
//!   `robustBufferAccess` applies.

use std::collections::{HashMap, HashSet};

use rspirv::dr::{Instruction, Module, Operand};
use rspirv::spirv::{self, Capability, Decoration, Op, StorageClass};

use crate::device::OptionalFeatures;
use crate::format::{Format, NumClass};
use crate::shader::{
    BindingInfo, BindingKind, IoVar, MAX_BINDINGS, MAX_TEXTURE_ARRAY, Reflection, ShaderStage,
    StorageAccess,
};

/// Capabilities that Vulkan 1.4 allows without a feature, or whose feature
/// the device is created with (`device.rs`). Each one is also something the
/// checks below understand. Adding one means enabling its feature too.
const ALLOWED_CAPABILITIES: &[Capability] = &[
    // Vulkan 1.0 core. (Some compilers declare Sampled1D in every module,
    // naga for one; 1D images themselves are refused below.)
    Capability::Shader,
    Capability::Sampled1D,
    Capability::Matrix,
    Capability::ImageQuery,
    Capability::DerivativeControl,
    Capability::StorageImageExtendedFormats,
    // Vulkan 1.2 core.
    Capability::ShaderNonUniform,
    // shaderSampledImageArrayDynamicIndexing.
    Capability::SampledImageArrayDynamicIndexing,
    // shaderSampledImageArrayNonUniformIndexing.
    Capability::SampledImageArrayNonUniformIndexing,
    // shaderDemoteToHelperInvocation.
    Capability::DemoteToHelperInvocation,
];

/// Capabilities whose device feature the API enables only when the device
/// has it (`OptionalFeatures`). A module may use them on such a device.
///
/// - `DrawParameters`: `SV_VertexID`, `SV_InstanceID` and the base vertex
///   and instance.
/// - `ClipDistance`: user clip planes. The array is at most 8 long, which
///   every device with the feature supports.
/// - `SampleRateShading`, `InterpolationFunction`: shading per sample.
/// - `Float16`, `Int8`, `Int16`, `Int64`: arithmetic in those types. Storing
///   them in buffers needs further capabilities, which are not allowed.
/// - `ImageGatherExtended`: gathers with offsets.
pub(crate) fn optional_capabilities(optional: &OptionalFeatures) -> Vec<Capability> {
    let mut caps = Vec::new();
    let mut add = |enabled: bool, list: &[Capability]| {
        if enabled {
            caps.extend_from_slice(list);
        }
    };
    add(
        optional.shader_draw_parameters,
        &[Capability::DrawParameters],
    );
    add(optional.shader_clip_distance, &[Capability::ClipDistance]);
    add(
        optional.sample_rate_shading,
        &[
            Capability::SampleRateShading,
            Capability::InterpolationFunction,
        ],
    );
    add(optional.shader_float16, &[Capability::Float16]);
    add(optional.shader_int8, &[Capability::Int8]);
    add(optional.shader_int16, &[Capability::Int16]);
    add(optional.shader_int64, &[Capability::Int64]);
    add(
        optional.shader_image_gather_extended,
        &[Capability::ImageGatherExtended],
    );
    caps
}

/// Longest `ClipDistance` array accepted: the least `maxClipDistances` of a
/// device that has the feature.
const MAX_CLIP_DISTANCES: u64 = 8;

/// SPIR-V extensions that are core in Vulkan 1.4. Reflection decorations
/// (`SPV_GOOGLE_*`) are stripped before the checks.
const ALLOWED_EXTENSIONS: &[&str] = &[
    "SPV_KHR_non_semantic_info",
    "SPV_KHR_storage_buffer_storage_class",
    "SPV_EXT_descriptor_indexing",
    "SPV_EXT_demote_to_helper_invocation",
];

/// `GLSL.std.450` instruction numbers.
const GLSL_UMIN: u32 = 38;
const GLSL_UCLAMP: u32 = 44;
const GLSL_SCLAMP: u32 = 45;

struct Ctx<'a> {
    defs: HashMap<u32, &'a Instruction>,
    decorations: HashMap<u32, Vec<&'a Instruction>>,
    member_decorations: HashMap<(u32, u32), Vec<&'a Instruction>>,
    names: HashMap<u32, String>,
    glsl_std: Option<u32>,
}

fn id(op: &Operand) -> Result<u32, String> {
    match op {
        Operand::IdRef(id) => Ok(*id),
        other => Err(format!("expected an id, found {other:?}")),
    }
}

fn lit(op: &Operand) -> Result<u32, String> {
    match op {
        Operand::LiteralBit32(v) => Ok(*v),
        other => Err(format!("expected a literal, found {other:?}")),
    }
}

impl<'a> Ctx<'a> {
    fn new(module: &'a Module) -> Result<Self, String> {
        let mut defs = HashMap::new();
        let mut glsl_std = None;
        for inst in module.ext_inst_imports.iter() {
            let name = match inst.operands.first() {
                Some(Operand::LiteralString(s)) => s.as_str(),
                _ => "",
            };
            if name == "GLSL.std.450" {
                glsl_std = inst.result_id;
            } else if !name.starts_with("NonSemantic.") {
                return Err(format!("extended instruction set `{name}` is not allowed"));
            }
        }
        for inst in module.types_global_values.iter() {
            if let Some(r) = inst.result_id {
                defs.insert(r, inst);
            }
        }
        for f in &module.functions {
            for p in &f.parameters {
                if let Some(r) = p.result_id {
                    defs.insert(r, p);
                }
            }
            for b in &f.blocks {
                for inst in &b.instructions {
                    if let Some(r) = inst.result_id {
                        defs.insert(r, inst);
                    }
                }
            }
        }
        // `NoContraction` belongs on the result of an arithmetic instruction.
        // The validator lets it pass on a type, a variable or a struct
        // member, which a driver's SPIR-V reader then complains about.
        let no_contraction = Some(&Operand::Decoration(Decoration::NoContraction));
        // (Slang decorates every arithmetic result when it compiles for
        // precise floating point, so this is looked up, not searched.)
        let computed: HashSet<u32> = module
            .functions
            .iter()
            .flat_map(|f| &f.blocks)
            .flat_map(|b| &b.instructions)
            .filter(|i| i.class.opcode != Op::Variable)
            .filter_map(|i| i.result_id)
            .collect();
        let is_computed = |target: u32| computed.contains(&target);
        let mut decorations: HashMap<u32, Vec<&Instruction>> = HashMap::new();
        let mut member_decorations: HashMap<(u32, u32), Vec<&Instruction>> = HashMap::new();
        for inst in &module.annotations {
            match inst.class.opcode {
                Op::Decorate | Op::DecorateId => {
                    let target = id(&inst.operands[0])?;
                    if inst.operands.get(1) == no_contraction && !is_computed(target) {
                        return Err(
                            "NoContraction decorates something that is not the result of an instruction"
                                .into(),
                        );
                    }
                    decorations.entry(target).or_default().push(inst);
                }
                Op::MemberDecorate => {
                    if inst.operands.get(2) == no_contraction {
                        return Err("NoContraction decorates a struct member".into());
                    }
                    let key = (id(&inst.operands[0])?, lit(&inst.operands[1])?);
                    member_decorations.entry(key).or_default().push(inst);
                }
                Op::DecorateString | Op::MemberDecorateString => {}
                other => return Err(format!("annotation {other:?} is not allowed")),
            }
        }
        let mut names = HashMap::new();
        for inst in &module.debug_names {
            if inst.class.opcode == Op::Name
                && let (Some(Operand::IdRef(target)), Some(Operand::LiteralString(s))) =
                    (inst.operands.first(), inst.operands.get(1))
            {
                names.insert(*target, s.clone());
            }
        }
        Ok(Self {
            defs,
            decorations,
            member_decorations,
            names,
            glsl_std,
        })
    }

    fn def(&self, id: u32) -> Result<&'a Instruction, String> {
        self.defs
            .get(&id)
            .copied()
            .ok_or_else(|| format!("id {id} is not defined"))
    }

    fn decoration(&self, target: u32, d: Decoration) -> Option<&'a [Operand]> {
        self.decorations.get(&target)?.iter().find_map(|inst| {
            (inst.operands.get(1) == Some(&Operand::Decoration(d))).then(|| &inst.operands[2..])
        })
    }

    fn member_decoration(&self, st: u32, member: u32, d: Decoration) -> Option<&'a [Operand]> {
        self.member_decorations
            .get(&(st, member))?
            .iter()
            .find_map(|inst| {
                (inst.operands.get(2) == Some(&Operand::Decoration(d))).then(|| &inst.operands[3..])
            })
    }

    fn name(&self, id: u32) -> String {
        self.names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("%{id}"))
    }

    fn constant(&self, id: u32) -> Option<u64> {
        let inst = self.defs.get(&id)?;
        if inst.class.opcode != Op::Constant {
            return None;
        }
        match inst.operands.first()? {
            Operand::LiteralBit32(v) => Some(u64::from(*v)),
            Operand::LiteralBit64(v) => Some(*v),
            _ => None,
        }
    }

    /// A signed view of an integer constant, for clamp bounds.
    fn constant_signed(&self, id: u32) -> Option<i64> {
        let value = self.constant(id)?;
        let ty = self.def(self.def(id).ok()?.result_type?).ok()?;
        let width = lit(ty.operands.first()?).ok()?;
        let signed = lit(ty.operands.get(1)?).ok()? == 1;
        Some(match (signed, width) {
            (true, 32) => i64::from(value as u32 as i32),
            (true, 16) => i64::from(value as u16 as i16),
            (true, 8) => i64::from(value as u8 as i8),
            _ => value as i64,
        })
    }

    fn pointee(&self, pointer_type: u32) -> Result<(StorageClass, u32), String> {
        let ty = self.def(pointer_type)?;
        if ty.class.opcode != Op::TypePointer {
            return Err(format!("{:?} is not a typed pointer", ty.class.opcode));
        }
        let sc = match ty.operands[0] {
            Operand::StorageClass(sc) => sc,
            _ => return Err("pointer without a storage class".into()),
        };
        Ok((sc, id(&ty.operands[1])?))
    }

    /// Size in bytes with the layout decorations of the module.
    fn size_of(&self, ty: u32) -> Result<u64, String> {
        let inst = self.def(ty)?;
        Ok(match inst.class.opcode {
            Op::TypeInt | Op::TypeFloat => u64::from(lit(&inst.operands[0])? / 8),
            Op::TypeVector => {
                u64::from(lit(&inst.operands[1])?) * self.size_of(id(&inst.operands[0])?)?
            }
            Op::TypeMatrix => {
                u64::from(lit(&inst.operands[1])?) * self.size_of(id(&inst.operands[0])?)?
            }
            Op::TypeArray => {
                let len = self
                    .constant(id(&inst.operands[1])?)
                    .ok_or("array length is not a constant")?;
                let stride = match self.decoration(ty, Decoration::ArrayStride) {
                    Some([Operand::LiteralBit32(s), ..]) => u64::from(*s),
                    _ => self.size_of(id(&inst.operands[0])?)?,
                };
                len * stride
            }
            Op::TypeRuntimeArray => match self.decoration(ty, Decoration::ArrayStride) {
                Some([Operand::LiteralBit32(s), ..]) => u64::from(*s),
                _ => self.size_of(id(&inst.operands[0])?)?,
            },
            Op::TypeStruct => {
                let mut size = 0;
                for (i, member) in inst.operands.iter().enumerate() {
                    let member_ty = id(member)?;
                    let offset = match self.member_decoration(ty, i as u32, Decoration::Offset) {
                        Some([Operand::LiteralBit32(o), ..]) => u64::from(*o),
                        _ => return Err("struct member without an Offset".into()),
                    };
                    let mut member_size = self.size_of(member_ty)?;
                    let member_inst = self.def(member_ty)?;
                    if member_inst.class.opcode == Op::TypeMatrix
                        && let Some([Operand::LiteralBit32(stride), ..]) =
                            self.member_decoration(ty, i as u32, Decoration::MatrixStride)
                    {
                        let column = self.def(id(&member_inst.operands[0])?)?;
                        let cols = u64::from(lit(&member_inst.operands[1])?);
                        let rows = u64::from(lit(&column.operands[1])?);
                        let row_major = self
                            .member_decoration(ty, i as u32, Decoration::RowMajor)
                            .is_some();
                        member_size = u64::from(*stride) * if row_major { rows } else { cols };
                    }
                    size = size.max(offset + member_size);
                }
                // A struct's size is rounded up to its alignment, as WGSL
                // counts it.
                size.next_multiple_of(self.align_of(ty)?)
            }
            other => return Err(format!("{other:?} has no size in a buffer")),
        })
    }

    /// Base alignment (std430 rules).
    fn align_of(&self, ty: u32) -> Result<u64, String> {
        let inst = self.def(ty)?;
        Ok(match inst.class.opcode {
            Op::TypeInt | Op::TypeFloat => u64::from(lit(&inst.operands[0])? / 8),
            Op::TypeVector => {
                let n = lit(&inst.operands[1])?;
                self.align_of(id(&inst.operands[0])?)? * if n == 2 { 2 } else { 4 }
            }
            Op::TypeMatrix | Op::TypeArray | Op::TypeRuntimeArray => {
                self.align_of(id(&inst.operands[0])?)?
            }
            Op::TypeStruct => {
                let mut align = 1;
                for member in &inst.operands {
                    align = align.max(self.align_of(id(member)?)?);
                }
                align
            }
            other => return Err(format!("{other:?} has no alignment in a buffer")),
        })
    }

    fn num_class(&self, scalar: u32) -> Result<NumClass, String> {
        let inst = self.def(scalar)?;
        match inst.class.opcode {
            Op::TypeFloat => Ok(NumClass::Float),
            Op::TypeInt if lit(&inst.operands[1])? == 1 => Ok(NumClass::Sint),
            Op::TypeInt => Ok(NumClass::Uint),
            other => Err(format!("{other:?} is not a numeric scalar")),
        }
    }

    /// Bits of a numeric scalar or vector type; nothing for other types.
    fn value_bits(&self, ty: u32) -> Option<u64> {
        let inst = self.def(ty).ok()?;
        match inst.class.opcode {
            Op::TypeInt | Op::TypeFloat => Some(u64::from(lit(inst.operands.first()?).ok()?)),
            Op::TypeVector => Some(
                self.value_bits(id(inst.operands.first()?).ok()?)?
                    * u64::from(lit(inst.operands.get(1)?).ok()?),
            ),
            _ => None,
        }
    }

    /// Whether `index` is known to be below `len`.
    fn index_in_range(&self, index: u32, len: u64) -> bool {
        if let Some(v) = self.constant(index) {
            return v < len;
        }
        let Ok(inst) = self.def(index) else {
            return false;
        };
        if inst.class.opcode != Op::ExtInst || self.glsl_std.is_none() {
            return false;
        }
        if inst.operands.first() != Some(&Operand::IdRef(self.glsl_std.unwrap())) {
            return false;
        }
        let Some(Operand::LiteralExtInstInteger(op)) = inst.operands.get(1) else {
            return false;
        };
        let arg = |i: usize| inst.operands.get(2 + i).and_then(|o| id(o).ok());
        let max_ok = |c: Option<u32>| {
            c.and_then(|c| self.constant_signed(c))
                .is_some_and(|v| v >= 0 && (v as u64) < len)
        };
        let min_ok = |c: Option<u32>| {
            c.and_then(|c| self.constant_signed(c))
                .is_some_and(|v| v >= 0)
        };
        match *op {
            GLSL_SCLAMP => min_ok(arg(1)) && max_ok(arg(2)),
            GLSL_UCLAMP => max_ok(arg(2)),
            GLSL_UMIN => max_ok(arg(0)) || max_ok(arg(1)),
            _ => false,
        }
    }

    /// Follows a loaded image or sampler back to its global variable.
    fn source_variable(&self, value: u32) -> Result<u32, String> {
        let inst = self.def(value)?;
        match inst.class.opcode {
            Op::Load => self.root_variable(id(&inst.operands[0])?),
            Op::CopyObject => self.source_variable(id(&inst.operands[0])?),
            other => Err(format!(
                "a texture or sampler comes from {other:?}; it must be loaded from a global"
            )),
        }
    }

    fn root_variable(&self, pointer: u32) -> Result<u32, String> {
        let inst = self.def(pointer)?;
        match inst.class.opcode {
            Op::Variable => Ok(pointer),
            Op::AccessChain | Op::InBoundsAccessChain | Op::CopyObject => {
                self.root_variable(id(&inst.operands[0])?)
            }
            other => Err(format!("cannot follow a resource through {other:?}")),
        }
    }
}

/// [`reflect_spirv_with`] for a device without any optional feature.
#[cfg(test)]
pub(crate) fn reflect_spirv(
    words: &[u32],
    stage: ShaderStage,
    entry: &str,
) -> Result<Reflection, String> {
    reflect_spirv_with(words, stage, entry, &[])
}

/// Checks `words` and describes its interface for the GPU API. `optional`
/// lists the capabilities the device has beyond [`ALLOWED_CAPABILITIES`].
pub(crate) fn reflect_spirv_with(
    words: &[u32],
    stage: ShaderStage,
    entry: &str,
    optional: &[Capability],
) -> Result<Reflection, String> {
    let allowed = |c: &Capability| ALLOWED_CAPABILITIES.contains(c) || optional.contains(c);
    // Capabilities first, on the raw words, so that the refusal does not
    // depend on what the parser below understands.
    let mut at = 5;
    while at < words.len() {
        let count = (words[at] >> 16) as usize;
        let op = words[at] & 0xffff;
        if count == 0 || at + count > words.len() {
            return Err("invalid SPIR-V: bad instruction length".into());
        }
        if op == Op::Capability as u32 {
            let cap = Capability::from_u32(words[at + 1]);
            if !cap.is_some_and(|c| allowed(&c)) {
                return Err(format!(
                    "the shader needs capability {}, which is not available",
                    cap.map_or_else(|| words[at + 1].to_string(), |c| format!("{c:?}"))
                ));
            }
        } else if op != Op::Extension as u32 && op != Op::ExtInstImport as u32 {
            // Capabilities come first in a valid module.
            break;
        }
        at += count;
    }
    let module = rspirv::dr::load_words(words).map_err(|e| format!("invalid SPIR-V: {e:?}"))?;
    let header = module.header.as_ref().ok_or("SPIR-V without a header")?;
    if header.version() < (1, 4) {
        return Err("SPIR-V older than 1.4 is not accepted".into());
    }
    for inst in &module.capabilities {
        let Operand::Capability(cap) = inst.operands[0] else {
            return Err("malformed OpCapability".into());
        };
        if !allowed(&cap) {
            return Err(format!(
                "the shader needs capability {cap:?}, which is not available"
            ));
        }
    }
    for inst in &module.extensions {
        let Some(Operand::LiteralString(name)) = inst.operands.first() else {
            return Err("malformed OpExtension".into());
        };
        if !ALLOWED_EXTENSIONS.contains(&name.as_str()) {
            return Err(format!(
                "the shader needs extension {name}, which is not available"
            ));
        }
    }
    let memory_model = module.memory_model.as_ref().ok_or("no OpMemoryModel")?;
    if memory_model.operands[0] != Operand::AddressingModel(spirv::AddressingModel::Logical) {
        return Err("only the Logical addressing model is accepted".into());
    }
    let ctx = Ctx::new(&module)?;
    check_layout_decorations(&ctx, &module)?;

    let model = match stage {
        ShaderStage::Vertex => spirv::ExecutionModel::Vertex,
        ShaderStage::Fragment => spirv::ExecutionModel::Fragment,
        ShaderStage::Compute => spirv::ExecutionModel::GLCompute,
    };
    if module.entry_points.len() != 1 {
        return Err(format!(
            "expected one entry point, found {}",
            module.entry_points.len()
        ));
    }
    let ep = &module.entry_points[0];
    let ep_name = ep.operands[2].unwrap_literal_string();
    if ep.operands[0] != Operand::ExecutionModel(model) || ep_name != entry {
        return Err(format!("no {} entry point named `{entry}`", stage.name()));
    }
    let ep_id = id(&ep.operands[1])?;
    let interface: HashSet<u32> = ep.operands[3..].iter().map(id).collect::<Result<_, _>>()?;

    let mut bindings: Vec<BindingInfo> = Vec::new();
    let mut push_size = 0u32;
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut var_binding: HashMap<u32, u32> = HashMap::new();
    for inst in &module.types_global_values {
        match inst.class.opcode {
            Op::Variable => {}
            Op::UntypedVariableKHR => return Err("untyped variables are not accepted".into()),
            Op::TypePointer => {
                let Operand::StorageClass(sc) = inst.operands[0] else {
                    return Err("malformed OpTypePointer".into());
                };
                if matches!(
                    sc,
                    StorageClass::PhysicalStorageBuffer | StorageClass::CrossWorkgroup
                ) {
                    return Err(format!("pointers to {sc:?} are not accepted"));
                }
                continue;
            }
            Op::TypeUntypedPointerKHR => return Err("untyped pointers are not accepted".into()),
            _ => continue,
        }
        let var = inst.result_id.ok_or("variable without an id")?;
        let name = ctx.name(var);
        let (sc, pointee) = ctx.pointee(inst.result_type.ok_or("untyped variable")?)?;
        let used = interface.contains(&var);
        match sc {
            StorageClass::Input | StorageClass::Output => {
                check_clip_distances(&ctx, var, pointee)?;
                if !used || ctx.decoration(var, Decoration::BuiltIn).is_some() {
                    continue;
                }
                let location = match ctx.decoration(var, Decoration::Location) {
                    Some([Operand::LiteralBit32(l), ..]) => *l,
                    _ => return Err(format!("`{name}`: stage variable without a location")),
                };
                let ty = ctx.def(pointee)?;
                let (class, components) = match ty.class.opcode {
                    Op::TypeVector => (ctx.num_class(id(&ty.operands[0])?)?, lit(&ty.operands[1])?),
                    Op::TypeInt | Op::TypeFloat => (ctx.num_class(pointee)?, 1),
                    other => {
                        return Err(format!("location {location}: {other:?} is not supported"));
                    }
                };
                // Vertex attributes and attachments are at most 32 bits a
                // component.
                let scalar = match ty.class.opcode {
                    Op::TypeVector => ctx.def(id(&ty.operands[0])?)?,
                    _ => ty,
                };
                if lit(&scalar.operands[0])? > 32 {
                    return Err(format!(
                        "location {location}: 64-bit stage variables are not supported"
                    ));
                }
                let io = IoVar {
                    location,
                    class,
                    components,
                };
                if sc == StorageClass::Input {
                    inputs.push(io);
                } else {
                    outputs.push(io);
                }
            }
            StorageClass::PushConstant => {
                if used {
                    push_size = push_size.max(ctx.size_of(pointee)? as u32);
                }
            }
            StorageClass::Private | StorageClass::Workgroup | StorageClass::Function => {}
            StorageClass::UniformConstant | StorageClass::Uniform | StorageClass::StorageBuffer => {
                if ctx.decoration(var, Decoration::BuiltIn).is_some() {
                    return Err(format!("`{name}`: built-in resources are not accepted"));
                }
                let set = match ctx.decoration(var, Decoration::DescriptorSet) {
                    Some([Operand::LiteralBit32(s), ..]) => *s,
                    _ => return Err(format!("`{name}`: resource without a descriptor set")),
                };
                let binding = match ctx.decoration(var, Decoration::Binding) {
                    Some([Operand::LiteralBit32(b), ..]) => *b,
                    _ => return Err(format!("`{name}`: resource without a binding")),
                };
                if set != 0 {
                    return Err(format!(
                        "`{name}`: only group/set 0 is supported, got {set}"
                    ));
                }
                if binding >= MAX_BINDINGS {
                    return Err(format!(
                        "`{name}`: binding {binding} is out of range (must be below {MAX_BINDINGS})"
                    ));
                }
                let kind = resource_kind(&ctx, var, sc, pointee, &name)?;
                if bindings.iter().any(|b| b.binding == binding) {
                    return Err(format!("`{name}`: binding {binding} is declared twice"));
                }
                var_binding.insert(var, binding);
                bindings.push(BindingInfo {
                    binding,
                    name,
                    kind,
                    used,
                });
            }
            other => return Err(format!("`{name}`: storage class {other:?} is not accepted")),
        }
    }
    bindings.sort_by_key(|b| b.binding);
    inputs.sort_by_key(|v: &IoVar| v.location);
    outputs.sort_by_key(|v: &IoVar| v.location);

    let mut sampling_pairs = Vec::new();
    // The pairs behind the results of OpSampledImage, and how each is used.
    // (A result of OpSampledImage is only ever an operand of the lookups in
    // its own block; the validator sees to that.)
    let mut combined: HashMap<u32, (u32, u32)> = HashMap::new();
    let mut comparison_pairs: Vec<(u32, u32)> = Vec::new();
    let mut plain_pairs: Vec<(u32, u32)> = Vec::new();
    for f in &module.functions {
        for b in &f.blocks {
            for inst in &b.instructions {
                check_instruction(&ctx, inst)?;
                match inst.class.opcode {
                    Op::SampledImage => {
                        let image = ctx.source_variable(id(&inst.operands[0])?)?;
                        let sampler = ctx.source_variable(id(&inst.operands[1])?)?;
                        if let (Some(t), Some(s)) =
                            (var_binding.get(&image), var_binding.get(&sampler))
                        {
                            if !sampling_pairs.contains(&(*t, *s)) {
                                sampling_pairs.push((*t, *s));
                            }
                            if let Some(result) = inst.result_id {
                                combined.insert(result, (*t, *s));
                            }
                        }
                    }
                    Op::AccessChain | Op::InBoundsAccessChain => {
                        check_access_chain(&ctx, inst)?;
                    }
                    // A comparison needs a sampler made for comparisons and a
                    // depth texture; a plain read needs a plain sampler.
                    // Which one a pair is used for is checked when it is
                    // bound.
                    Op::ImageSampleDrefImplicitLod
                    | Op::ImageSampleDrefExplicitLod
                    | Op::ImageSampleProjDrefImplicitLod
                    | Op::ImageSampleProjDrefExplicitLod
                    | Op::ImageDrefGather => {
                        let pair = combined
                            .get(&id(&inst.operands[0])?)
                            .ok_or("a comparison reads a texture the checks cannot follow")?;
                        if !comparison_pairs.contains(pair) {
                            comparison_pairs.push(*pair);
                        }
                    }
                    Op::ImageSampleImplicitLod
                    | Op::ImageSampleExplicitLod
                    | Op::ImageSampleProjImplicitLod
                    | Op::ImageSampleProjExplicitLod
                    | Op::ImageGather
                    | Op::ImageQueryLod => {
                        if let Some(pair) = combined.get(&id(&inst.operands[0])?)
                            && !plain_pairs.contains(pair)
                        {
                            plain_pairs.push(*pair);
                        }
                    }
                    Op::PtrAccessChain
                    | Op::InBoundsPtrAccessChain
                    | Op::UntypedAccessChainKHR
                    | Op::UntypedInBoundsAccessChainKHR
                    | Op::UntypedPtrAccessChainKHR
                    | Op::UntypedInBoundsPtrAccessChainKHR
                    | Op::ConvertUToPtr
                    | Op::ConvertPtrToU => {
                        return Err(format!("{:?} is not accepted", inst.class.opcode));
                    }
                    _ => {}
                }
            }
        }
    }
    sampling_pairs.sort_unstable();
    comparison_pairs.sort_unstable();
    if let Some((texture, sampler)) = comparison_pairs.iter().find(|p| plain_pairs.contains(p)) {
        return Err(format!(
            "the texture at binding {texture} and the sampler at binding {sampler} are used both for comparisons and for plain reads; use a sampler for each"
        ));
    }

    let mut workgroup_size = [0; 3];
    for inst in &module.execution_modes {
        if inst.operands.first() != Some(&Operand::IdRef(ep_id)) {
            continue;
        }
        match inst.operands.get(1) {
            Some(Operand::ExecutionMode(spirv::ExecutionMode::LocalSize)) => {
                for (i, w) in workgroup_size.iter_mut().enumerate() {
                    *w = lit(&inst.operands[2 + i])?;
                }
            }
            Some(Operand::ExecutionMode(spirv::ExecutionMode::LocalSizeId)) => {
                for (i, w) in workgroup_size.iter_mut().enumerate() {
                    *w = ctx
                        .constant(id(&inst.operands[2 + i])?)
                        .ok_or("LocalSizeId is not a constant")? as u32;
                }
            }
            _ => {}
        }
    }
    Ok(Reflection {
        stage,
        bindings,
        push_size,
        inputs,
        outputs,
        sampling_pairs,
        comparison_pairs,
        workgroup_size,
    })
}

/// Image operands the API has a reason to accept. The others either need a
/// capability that is not allowed (the Vulkan memory model) or are extension
/// modes that the bundled validator accepts in forms a driver does not
/// (`SignExtend` and `ZeroExtend` on a floating-point texel).
const ALLOWED_IMAGE_OPERANDS: spirv::ImageOperands = spirv::ImageOperands::BIAS
    .union(spirv::ImageOperands::LOD)
    .union(spirv::ImageOperands::GRAD)
    .union(spirv::ImageOperands::CONST_OFFSET)
    .union(spirv::ImageOperands::OFFSET)
    .union(spirv::ImageOperands::CONST_OFFSETS)
    .union(spirv::ImageOperands::SAMPLE)
    .union(spirv::ImageOperands::MIN_LOD);

/// What the bundled validator lets through and a newer one, or a driver,
/// does not.
fn check_instruction(ctx: &Ctx<'_>, inst: &Instruction) -> Result<(), String> {
    for operand in &inst.operands {
        if let Operand::ImageOperands(mask) = operand
            && !ALLOWED_IMAGE_OPERANDS.contains(*mask)
        {
            return Err(format!(
                "image operands {:?} are not accepted",
                mask.difference(ALLOWED_IMAGE_OPERANDS)
            ));
        }
    }
    if inst.class.opcode == Op::Bitcast {
        // Between numeric values of the same total width only; never a
        // pointer.
        let from = ctx
            .def(id(&inst.operands[0])?)?
            .result_type
            .and_then(|ty| ctx.value_bits(ty));
        let to = inst.result_type.and_then(|ty| ctx.value_bits(ty));
        if from.is_none() || from != to {
            return Err("OpBitcast between types of different sizes is not accepted".into());
        }
    }
    Ok(())
}

/// A `ClipDistance` built-in, as a variable or as a member of a block, is an
/// array of at most [`MAX_CLIP_DISTANCES`] floats.
fn check_clip_distances(ctx: &Ctx<'_>, var: u32, pointee: u32) -> Result<(), String> {
    let is_clip = |operands: Option<&[Operand]>| {
        matches!(
            operands,
            Some([Operand::BuiltIn(spirv::BuiltIn::ClipDistance), ..])
        )
    };
    let mut arrays = Vec::new();
    if is_clip(ctx.decoration(var, Decoration::BuiltIn)) {
        arrays.push(pointee);
    }
    let ty = ctx.def(pointee)?;
    if ty.class.opcode == Op::TypeStruct {
        for (member, member_ty) in ty.operands.iter().enumerate() {
            if is_clip(ctx.member_decoration(pointee, member as u32, Decoration::BuiltIn)) {
                arrays.push(id(member_ty)?);
            }
        }
    }
    for array in arrays {
        let ty = ctx.def(array)?;
        let len = match ty.class.opcode {
            Op::TypeArray => ctx.constant(id(&ty.operands[1])?),
            _ => None,
        };
        if len.is_none_or(|len| len > MAX_CLIP_DISTANCES) {
            return Err(format!(
                "a ClipDistance array has at most {MAX_CLIP_DISTANCES} elements"
            ));
        }
    }
    Ok(())
}

fn resource_kind(
    ctx: &Ctx<'_>,
    var: u32,
    sc: StorageClass,
    pointee: u32,
    name: &str,
) -> Result<BindingKind, String> {
    let ty = ctx.def(pointee)?;
    match (sc, ty.class.opcode) {
        (StorageClass::UniformConstant, Op::TypeImage) => image_kind(ctx, var, pointee, name),
        (StorageClass::UniformConstant, Op::TypeSampler) => Ok(BindingKind::Sampler),
        (StorageClass::UniformConstant, Op::TypeArray) => {
            let element = id(&ty.operands[0])?;
            let count = ctx
                .constant(id(&ty.operands[1])?)
                .ok_or_else(|| format!("`{name}`: binding arrays must have a constant size"))?;
            if count > u64::from(MAX_TEXTURE_ARRAY) {
                return Err(format!(
                    "`{name}`: binding array of {count} exceeds the limit {MAX_TEXTURE_ARRAY}"
                ));
            }
            match image_kind(ctx, var, element, name)? {
                BindingKind::SampledTexture { class } => Ok(BindingKind::TextureArray {
                    class,
                    count: count as u32,
                }),
                _ => Err(format!(
                    "`{name}`: only binding arrays of 2D sampled textures are supported"
                )),
            }
        }
        (StorageClass::UniformConstant, Op::TypeSampledImage) => Err(format!(
            "`{name}`: combined texture-samplers are not supported; declare a texture and a sampler"
        )),
        (StorageClass::UniformConstant, Op::TypeRuntimeArray) => Err(format!(
            "`{name}`: binding arrays must have a constant size"
        )),
        (StorageClass::Uniform, Op::TypeStruct)
            if ctx.decoration(pointee, Decoration::Block).is_some() =>
        {
            Ok(BindingKind::UniformBuffer {
                min_size: ctx.size_of(pointee)?,
            })
        }
        (StorageClass::StorageBuffer, Op::TypeStruct)
            if ctx.decoration(pointee, Decoration::Block).is_some() =>
        {
            let members = ty.operands.len() as u32;
            let read_only = ctx.decoration(var, Decoration::NonWritable).is_some()
                || (0..members).all(|m| {
                    ctx.member_decoration(pointee, m, Decoration::NonWritable)
                        .is_some()
                });
            Ok(BindingKind::StorageBuffer {
                min_size: ctx.size_of(pointee)?,
                writable: !read_only,
            })
        }
        (_, other) => Err(format!("`{name}`: unsupported resource type {other:?}")),
    }
}

fn image_kind(ctx: &Ctx<'_>, var: u32, image: u32, name: &str) -> Result<BindingKind, String> {
    let ty = ctx.def(image)?;
    if ty.class.opcode != Op::TypeImage {
        return Err(format!("`{name}`: expected an image"));
    }
    let dim = ty.operands[1].unwrap_dim();
    let depth = lit(&ty.operands[2])?;
    let arrayed = lit(&ty.operands[3])?;
    let ms = lit(&ty.operands[4])?;
    let sampled = lit(&ty.operands[5])?;
    // 3D and cube textures are sampled only, never storage. The bind-time
    // check keeps an image of another kind out of the binding: the view type
    // of the descriptor has to be the one declared here.
    let sampled_only = matches!(dim, spirv::Dim::Dim3D | spirv::Dim::DimCube) && sampled == 1;
    if (dim != spirv::Dim::Dim2D && !sampled_only) || arrayed != 0 || ms != 0 || depth == 1 {
        return Err(format!(
            "`{name}`: only non-arrayed, single-sampled 2D textures, and sampled 3D and cube textures, are supported"
        ));
    }
    let class = ctx
        .num_class(id(&ty.operands[0])?)
        .map_err(|e| format!("`{name}`: {e}"))?;
    if sampled_only {
        let declared = ty.operands[6].unwrap_image_format();
        if declared != spirv::ImageFormat::Unknown {
            return Err(format!(
                "`{name}`: a sampled texture must not declare a format, found {declared:?}"
            ));
        }
        return Ok(if dim == spirv::Dim::Dim3D {
            BindingKind::SampledTexture3D { class }
        } else {
            BindingKind::SampledTextureCube { class }
        });
    }
    // The validator that the spirv-tools crate carries does not check that an
    // image's format agrees with its sampled type (newer ones do:
    // VUID-StandaloneSpirv-Image-04965). Sampled images declare no format;
    // the format of a storage image must be of its numeric class.
    let declared = ty.operands[6].unwrap_image_format();
    match sampled {
        1 if declared != spirv::ImageFormat::Unknown => Err(format!(
            "`{name}`: a sampled texture must not declare a format, found {declared:?}"
        )),
        1 => Ok(BindingKind::SampledTexture { class }),
        2 => {
            use spirv::ImageFormat as F;
            let format = match declared {
                F::Rgba8 => Format::Rgba8Unorm,
                F::Rg8 => Format::Rg8Unorm,
                F::R8 => Format::R8Unorm,
                F::Rgb10A2 => Format::Rgb10a2Unorm,
                F::R16f => Format::R16Float,
                F::Rg16f => Format::Rg16Float,
                F::Rgba16f => Format::Rgba16Float,
                F::R32f => Format::R32Float,
                F::Rgba32f => Format::Rgba32Float,
                F::R32ui => Format::R32Uint,
                F::Rgba8ui => Format::Rgba8Uint,
                other => {
                    return Err(format!(
                        "`{name}`: storage format {other:?} is not supported (declare it with [format(...)])"
                    ));
                }
            };
            if format.class() != class {
                return Err(format!(
                    "`{name}`: storage format {} does not hold {class:?} values",
                    format.name()
                ));
            }
            let access = match (
                ctx.decoration(var, Decoration::NonReadable).is_some(),
                ctx.decoration(var, Decoration::NonWritable).is_some(),
            ) {
                (false, false) => StorageAccess::ReadWrite,
                (true, false) => StorageAccess::WriteOnly,
                (false, true) => StorageAccess::ReadOnly,
                (true, true) => return Err(format!("`{name}`: neither readable nor writable")),
            };
            Ok(BindingKind::StorageTexture { format, access })
        }
        _ => Err(format!("`{name}`: textures must be sampled or storage")),
    }
}

/// The validator does not check every layout decoration: on a type that no
/// interface block uses (a local array, say), `ArrayStride 0` passes it, and
/// it accepts an array stride below the size of the elements, which a newer
/// validator does not. A driver may then fail to build the pipeline. Here a
/// stride is at least the size of what it steps over.
fn check_layout_decorations(ctx: &Ctx<'_>, module: &Module) -> Result<(), String> {
    for inst in &module.annotations {
        let (decoration, value) = match inst.class.opcode {
            Op::Decorate => (inst.operands.get(1), inst.operands.get(2)),
            Op::MemberDecorate => (inst.operands.get(2), inst.operands.get(3)),
            _ => continue,
        };
        let Some(Operand::Decoration(decoration)) = decoration else {
            continue;
        };
        if !matches!(
            decoration,
            Decoration::ArrayStride | Decoration::MatrixStride
        ) {
            continue;
        }
        let Some(Operand::LiteralBit32(stride)) = value else {
            return Err("a stride without a size".into());
        };
        if *stride == 0 {
            return Err("a stride of 0 bytes is not accepted".into());
        }
        let needed = match (inst.class.opcode, decoration) {
            (Op::Decorate, Decoration::ArrayStride) => {
                let array = ctx.def(id(&inst.operands[0])?)?;
                // Compilers also put the decoration on other types (Slang
                // does, on the block of a structured buffer), where it
                // means nothing.
                if !matches!(array.class.opcode, Op::TypeArray | Op::TypeRuntimeArray) {
                    continue;
                }
                ctx.size_of(id(&array.operands[0])?)?
            }
            (Op::MemberDecorate, Decoration::MatrixStride) => {
                // The member is a matrix or arrays of one; the stride steps
                // over its column vectors (its rows, when row-major).
                let st = id(&inst.operands[0])?;
                let member = lit(&inst.operands[1])?;
                let st_def = ctx.def(st)?;
                let mut ty = id(st_def
                    .operands
                    .get(member as usize)
                    .ok_or("MatrixStride on a member the struct does not have")?)?;
                let matrix = loop {
                    let def = ctx.def(ty)?;
                    match def.class.opcode {
                        Op::TypeMatrix => break def,
                        Op::TypeArray | Op::TypeRuntimeArray => ty = id(&def.operands[0])?,
                        _ => {
                            return Err(
                                "MatrixStride decorates something that is not a matrix".into()
                            );
                        }
                    }
                };
                let column = ctx.def(id(&matrix.operands[0])?)?;
                let component = ctx.size_of(id(&column.operands[0])?)?;
                let rows = u64::from(lit(&column.operands[1])?);
                let columns = u64::from(lit(&matrix.operands[1])?);
                let row_major = ctx
                    .member_decoration(st, member, Decoration::RowMajor)
                    .is_some();
                component * if row_major { columns } else { rows }
            }
            // On anything else the decoration means nothing; whether it may
            // be there is the validator's business.
            _ => continue,
        };
        if u64::from(*stride) < needed {
            return Err(format!(
                "a stride of {stride} bytes is smaller than the {needed} bytes it steps over"
            ));
        }
    }
    Ok(())
}

fn check_access_chain(ctx: &Ctx<'_>, inst: &Instruction) -> Result<(), String> {
    let base = id(&inst.operands[0])?;
    let base_def = ctx.def(base)?;
    let (sc, mut ty) = ctx.pointee(base_def.result_type.ok_or("untyped base")?)?;
    for index in &inst.operands[1..] {
        let index = id(index)?;
        let t = ctx.def(ty)?;
        let (next, len) = match t.class.opcode {
            Op::TypeStruct => {
                let m = ctx
                    .constant(index)
                    .ok_or("struct member index is not a constant")?;
                (id(t.operands.get(m as usize).ok_or("bad member")?)?, None)
            }
            Op::TypeArray => (
                id(&t.operands[0])?,
                Some(
                    ctx.constant(id(&t.operands[1])?)
                        .ok_or("array length is not a constant")?,
                ),
            ),
            Op::TypeVector | Op::TypeMatrix => {
                (id(&t.operands[0])?, Some(u64::from(lit(&t.operands[1])?)))
            }
            Op::TypeRuntimeArray => {
                // The validator allows a runtime array only as the last
                // member of a block (unbounded descriptor arrays are refused
                // earlier). In a storage buffer, the device's
                // robustBufferAccess keeps accesses in bounds.
                if sc != StorageClass::StorageBuffer {
                    return Err("runtime arrays are only accepted in storage buffers".into());
                }
                (id(&t.operands[0])?, None)
            }
            other => return Err(format!("access chain into {other:?}")),
        };
        if let Some(len) = len
            && !ctx.index_in_range(index, len)
        {
            return Err(format!(
                "an index into an array of {len} is not clamped (the SPIR-V was not processed by graphics-robust-access)"
            ));
        }
        ty = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WGSL to SPIR-V with naga, without bounds checks.
    fn naga_words(stage: ShaderStage, source: &str) -> Vec<u32> {
        let module = naga::front::wgsl::parse_str(source).unwrap();
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap();
        let policy = naga::proc::BoundsCheckPolicy::Unchecked;
        let options = naga::back::spv::Options {
            lang_version: (1, 4),
            flags: naga::back::spv::WriterFlags::DEBUG,
            bounds_check_policies: naga::proc::BoundsCheckPolicies {
                index: policy,
                buffer: policy,
                image_load: policy,
                binding_array: policy,
            },
            ..Default::default()
        };
        let shader_stage = match stage {
            ShaderStage::Vertex => naga::ShaderStage::Vertex,
            ShaderStage::Fragment => naga::ShaderStage::Fragment,
            ShaderStage::Compute => naga::ShaderStage::Compute,
        };
        naga::back::spv::write_vec(
            &module,
            &info,
            &options,
            Some(&naga::back::spv::PipelineOptions {
                shader_stage,
                entry_point: "main".into(),
            }),
        )
        .unwrap()
    }

    /// Bindings, push data, stage inputs and outputs, and which texture is
    /// sampled with which sampler.
    #[test]
    fn reflects_bindings_io_and_push_data() {
        let source = r#"
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
        "#;
        let words = naga_words(ShaderStage::Fragment, source);
        let r = crate::spirv_input::accept_spirv(&words, ShaderStage::Fragment, "main")
            .unwrap()
            .reflection;
        // The struct of a vector and a scalar, rounded up to its alignment.
        assert_eq!(r.push_size, 32);
        let float = NumClass::Float;
        assert_eq!(
            r.inputs,
            [
                IoVar {
                    location: 0,
                    class: float,
                    components: 2
                },
                IoVar {
                    location: 2,
                    class: NumClass::Uint,
                    components: 1
                },
            ]
        );
        assert_eq!(
            r.outputs,
            [IoVar {
                location: 0,
                class: float,
                components: 4
            }]
        );
        assert_eq!(r.sampling_pairs, [(0, 1)]);
        // The unused texture does not reach the SPIR-V side at all. The API
        // checks only the bindings that are used.
        let bindings: Vec<_> = r
            .bindings
            .iter()
            .map(|b| (b.binding, b.kind, b.used))
            .collect();
        assert_eq!(
            bindings,
            [
                (0, BindingKind::SampledTexture { class: float }, true),
                (1, BindingKind::Sampler, true),
                (3, BindingKind::UniformBuffer { min_size: 16 }, true),
            ]
        );
    }

    #[test]
    fn compute_interface() {
        let source = r#"
            @group(0) @binding(0) var image: texture_storage_2d<rgba16float, write>;
            @group(0) @binding(1) var<storage, read> colors: array<vec4<f32>>;
            @group(0) @binding(2) var<storage, read_write> counts: array<u32>;
            @compute @workgroup_size(8, 4, 1)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                textureStore(image, id.xy, colors[id.x % 4u]);
                counts[0] = counts[0] + 1u;
            }
        "#;
        let words = naga_words(ShaderStage::Compute, source);
        let r = crate::spirv_input::accept_spirv(&words, ShaderStage::Compute, "main")
            .unwrap()
            .reflection;
        assert_eq!(r.workgroup_size, [8, 4, 1]);
        let kinds: Vec<BindingKind> = r.bindings.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            [
                BindingKind::StorageTexture {
                    format: Format::Rgba16Float,
                    access: StorageAccess::WriteOnly
                },
                BindingKind::StorageBuffer {
                    min_size: 16,
                    writable: false
                },
                BindingKind::StorageBuffer {
                    min_size: 4,
                    writable: true
                },
            ]
        );
    }

    /// Without `graphics-robust-access`, an index computed by the shader is
    /// not known to be in range, and the checks refuse the module.
    #[test]
    fn unclamped_indices_are_refused() {
        let words = naga_words(
            ShaderStage::Fragment,
            r#"
            struct P { i: u32 }
            var<immediate> pc: P;
            @fragment fn main() -> @location(0) vec4<f32> {
                var c = array<vec4<f32>, 3>(vec4(1.0), vec4(2.0), vec4(3.0));
                return c[pc.i];
            }"#,
        );
        let e = reflect_spirv(&words, ShaderStage::Fragment, "main").unwrap_err();
        assert!(e.contains("not clamped"), "{e}");
        crate::spirv_input::accept_spirv(&words, ShaderStage::Fragment, "main").unwrap();
    }

    /// Capabilities are read from the raw words first: a module with untyped
    /// pointers is refused even though the SPIR-V reader cannot parse the
    /// instructions that use them.
    #[test]
    fn capabilities_are_checked_on_the_raw_words() {
        let capability = |c: u32| [(2 << 16) | Op::Capability as u32, c];
        let mut words = vec![0x0723_0203, 0x0001_0600, 0, 8, 0];
        words.extend(capability(Capability::Shader as u32));
        words.extend(capability(Capability::UntypedPointersKHR as u32));
        let e = reflect_spirv(&words, ShaderStage::Fragment, "main").unwrap_err();
        assert!(e.contains("UntypedPointersKHR"), "{e}");
        // A capability this crate has no name for.
        let mut words = vec![0x0723_0203, 0x0001_0600, 0, 8, 0];
        words.extend(capability(0x7fff_0000));
        let e = reflect_spirv(&words, ShaderStage::Fragment, "main").unwrap_err();
        assert!(e.contains("2147418112"), "{e}");
    }

    /// The push block's size is rounded up to its alignment, as naga counts
    /// it for WGSL: 3 vectors and 3 scalars are 64 bytes, not 60.
    #[test]
    fn push_size_is_rounded_to_the_alignment() {
        let words = naga_words(
            ShaderStage::Fragment,
            r#"
            struct P { m0: vec4<f32>, m1: vec4<f32>, m2: vec4<f32>, tf: u32, p: f32, clip: u32 }
            var<immediate> pc: P;
            @fragment fn main() -> @location(0) vec4<f32> {
                return pc.m0 + pc.m1 + pc.m2 + vec4(f32(pc.tf) + pc.p + f32(pc.clip));
            }"#,
        );
        let r = reflect_spirv(&words, ShaderStage::Fragment, "main").unwrap();
        assert_eq!(r.push_size, 64);
    }
}
