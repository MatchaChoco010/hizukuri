//! The `gfx` table: one script function per `proto-gfx-core` call.
//!
//! Handles cross the boundary as plain numbers. Descriptions are tables.
//! Everything semantic is checked in `proto-gfx-core`; this file only converts
//! values, so porting it to another script language is mechanical.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Table, Value, Variadic};
use proto_gfx_core::*;

type Shared = Rc<RefCell<Gpu>>;

fn gfx_err(e: GfxError) -> mlua::Error {
    mlua::Error::external(e)
}

fn arg_err(message: impl Into<String>) -> mlua::Error {
    gfx_err(GfxError {
        code: ErrorCode::InvalidArgument,
        message: message.into(),
    })
}

fn handle(bits: i64) -> Handle {
    Handle::from_bits(bits)
}

fn field<T: mlua::FromLua>(table: &Table, key: &str) -> mlua::Result<T> {
    table
        .get::<T>(key)
        .map_err(|e| arg_err(format!("field `{key}`: {e}")))
}

fn name_field<T>(
    table: &Table,
    key: &str,
    default: Option<&str>,
    parse: impl Fn(&str) -> Option<T>,
) -> mlua::Result<T> {
    let name: Option<String> = field(table, key)?;
    let name = match (name, default) {
        (Some(name), _) => name,
        (None, Some(default)) => default.to_owned(),
        (None, None) => return Err(arg_err(format!("field `{key}` is required"))),
    };
    parse(&name).ok_or_else(|| arg_err(format!("field `{key}`: unknown value `{name}`")))
}

fn usage_field<T: std::ops::BitOr<Output = T> + Default>(
    table: &Table,
    parse: impl Fn(&str) -> Option<T>,
) -> mlua::Result<T> {
    let names: Vec<String> = field(table, "usage")?;
    let mut usage = T::default();
    for name in names {
        usage = usage | parse(&name).ok_or_else(|| arg_err(format!("unknown usage `{name}`")))?;
    }
    Ok(usage)
}

/// Runs `f` with the bytes of a script value: a string, or a Luau buffer.
fn with_bytes<R>(value: &Value, f: impl FnOnce(&[u8]) -> mlua::Result<R>) -> mlua::Result<R> {
    with_byte_range(value, None, None, f)
}

/// Runs `f` with `len` bytes of a script value from `start` on (all of it
/// by default). The range has to lie inside the value.
fn with_byte_range<R>(
    value: &Value,
    start: Option<usize>,
    len: Option<usize>,
    f: impl FnOnce(&[u8]) -> mlua::Result<R>,
) -> mlua::Result<R> {
    let range = |total: usize| -> mlua::Result<(usize, usize)> {
        let start = start.unwrap_or(0);
        let len = match len {
            Some(len) => len,
            None => total.saturating_sub(start),
        };
        if start.checked_add(len).is_none_or(|end| end > total) {
            return Err(gfx_err(GfxError {
                code: ErrorCode::OutOfRange,
                message: format!("bytes {start}+{len} are outside data of {total} bytes"),
            }));
        }
        Ok((start, len))
    };
    match value {
        Value::String(s) => {
            let bytes = s.as_bytes();
            let (start, len) = range(bytes.len())?;
            f(&bytes[start..start + len])
        }
        #[cfg(feature = "luau")]
        Value::Buffer(b) => {
            // mlua 0.12 has no borrowed view of a buffer; small ranges are
            // read into a stack array to keep the hot `push` path
            // allocation-free.
            use std::io::{Read, Seek, SeekFrom};
            let (start, len) = range(b.len())?;
            let mut cursor = b.clone().cursor();
            cursor
                .seek(SeekFrom::Start(start as u64))
                .map_err(mlua::Error::external)?;
            let mut small = [0u8; 256];
            if len <= small.len() {
                cursor
                    .read_exact(&mut small[..len])
                    .map_err(mlua::Error::external)?;
                f(&small[..len])
            } else {
                let mut bytes = vec![0u8; len];
                cursor
                    .read_exact(&mut bytes)
                    .map_err(mlua::Error::external)?;
                f(&bytes)
            }
        }
        other => Err(arg_err(format!(
            "expected bytes (a string{}), got {}",
            if cfg!(feature = "luau") {
                " or a buffer"
            } else {
                ""
            },
            other.type_name()
        ))),
    }
}

fn filter(name: &str) -> Option<Filter> {
    match name {
        "nearest" => Some(Filter::Nearest),
        "linear" => Some(Filter::Linear),
        _ => None,
    }
}

fn address_mode(name: &str) -> Option<AddressMode> {
    match name {
        "clamp" => Some(AddressMode::ClampToEdge),
        "repeat" => Some(AddressMode::Repeat),
        "mirror" => Some(AddressMode::MirrorRepeat),
        "border" => Some(AddressMode::ClampToBorder),
        _ => None,
    }
}

fn blend(value: Value) -> mlua::Result<Option<Blend>> {
    match value {
        Value::Nil => Ok(None),
        Value::String(s) => match &*s.to_str()? {
            "none" => Ok(None),
            "premultiplied" => Ok(Some(Blend::PREMULTIPLIED)),
            "additive" => Ok(Some(Blend::ADDITIVE)),
            other => Err(arg_err(format!("unknown blend preset `{other}`"))),
        },
        Value::Table(t) => Ok(Some(Blend {
            src_color: name_field(&t, "src_color", None, BlendFactor::from_name)?,
            dst_color: name_field(&t, "dst_color", None, BlendFactor::from_name)?,
            color_op: name_field(&t, "color_op", Some("add"), BlendOp::from_name)?,
            src_alpha: name_field(&t, "src_alpha", None, BlendFactor::from_name)?,
            dst_alpha: name_field(&t, "dst_alpha", None, BlendFactor::from_name)?,
            alpha_op: name_field(&t, "alpha_op", Some("add"), BlendOp::from_name)?,
        })),
        other => Err(arg_err(format!(
            "blend must be a preset name or a table, got {}",
            other.type_name()
        ))),
    }
}

fn render_pipeline_desc(t: &Table) -> mlua::Result<(RenderPipelineDesc, PipelineExtra)> {
    let mut vertex_buffers = Vec::new();
    for layout in field::<Option<Vec<Table>>>(t, "vertex_buffers")?.unwrap_or_default() {
        let mut attributes = Vec::new();
        for a in field::<Vec<Table>>(&layout, "attributes")? {
            attributes.push(VertexAttribute {
                location: field(&a, "location")?,
                format: name_field(&a, "format", None, VertexFormat::from_name)?,
                offset: field(&a, "offset")?,
            });
        }
        vertex_buffers.push(VertexBufferLayout {
            stride: field(&layout, "stride")?,
            per_instance: name_field(&layout, "step", Some("vertex"), |s| match s {
                "vertex" => Some(false),
                "instance" => Some(true),
                _ => None,
            })?,
            attributes,
        });
    }
    let mut targets = Vec::new();
    for target in field::<Option<Vec<Table>>>(t, "targets")?.unwrap_or_default() {
        targets.push(ColorTarget {
            format: name_field(&target, "format", None, Format::from_name)?,
            blend: blend(field(&target, "blend")?)?,
        });
    }
    let depth = match field::<Option<Table>>(t, "depth")? {
        None => None,
        Some(d) => Some(DepthState {
            format: name_field(&d, "format", None, Format::from_name)?,
            test: field::<Option<bool>>(&d, "test")?.unwrap_or(true),
            write: field::<Option<bool>>(&d, "write")?.unwrap_or(true),
            compare: name_field(&d, "compare", Some("less"), CompareOp::from_name)?,
            bias: match field::<Option<Table>>(&d, "bias")? {
                None => None,
                Some(b) => Some(DepthBias {
                    constant: field::<Option<f32>>(&b, "constant")?.unwrap_or(0.0),
                    slope: field::<Option<f32>>(&b, "slope")?.unwrap_or(0.0),
                    clamp: field::<Option<f32>>(&b, "clamp")?.unwrap_or(0.0),
                }),
            },
        }),
    };
    let extra = PipelineExtra {
        depth,
        front_face: name_field(t, "front_face", Some("ccw"), |s| match s {
            "ccw" => Some(FrontFace::CounterClockwise),
            "cw" => Some(FrontFace::Clockwise),
            _ => None,
        })?,
        samples: field::<Option<u32>>(t, "samples")?.unwrap_or(1),
    };
    let desc = RenderPipelineDesc {
        vertex: handle(field(t, "vertex")?),
        fragment: handle(field(t, "fragment")?),
        vertex_buffers,
        targets,
        topology: name_field(t, "topology", Some("triangle-list"), |s| match s {
            "triangle-list" => Some(Topology::TriangleList),
            "triangle-strip" => Some(Topology::TriangleStrip),
            _ => None,
        })?,
        cull: name_field(t, "cull", Some("none"), |s| match s {
            "none" => Some(CullMode::None),
            "front" => Some(CullMode::Front),
            "back" => Some(CullMode::Back),
            _ => None,
        })?,
    };
    Ok((desc, extra))
}

/// The `mip` and `layer` fields of a table, 0 when absent.
fn subresource(t: &Table) -> mlua::Result<Subresource> {
    Ok(Subresource {
        mip: field::<Option<u32>>(t, "mip")?.unwrap_or(0),
        layer: field::<Option<u32>>(t, "layer")?.unwrap_or(0),
    })
}

/// A texture view description: `nil`, or a table with `base_mip`,
/// `mip_count` and `layer`.
fn texture_view(value: Option<Table>) -> mlua::Result<TextureView> {
    let Some(t) = value else {
        return Ok(TextureView::default());
    };
    Ok(TextureView {
        base_mip: field::<Option<u32>>(&t, "base_mip")?.unwrap_or(0),
        mip_count: field(&t, "mip_count")?,
        layer: field(&t, "layer")?,
    })
}

fn pass_desc(t: &Table) -> mlua::Result<PassDesc> {
    let mut color = Vec::new();
    for a in field::<Option<Vec<Table>>>(t, "color")?.unwrap_or_default() {
        let load = match field::<Option<String>>(&a, "load")?.as_deref() {
            Some("clear") | None => {
                let c: Option<Vec<f32>> = field(&a, "clear")?;
                let c = c.unwrap_or_else(|| vec![0.0; 4]);
                if c.len() != 4 {
                    return Err(arg_err("field `clear` must have 4 numbers"));
                }
                LoadOp::Clear([c[0], c[1], c[2], c[3]])
            }
            Some("load") => LoadOp::Load,
            Some("dont-care") => LoadOp::DontCare,
            Some(other) => return Err(arg_err(format!("unknown load op `{other}`"))),
        };
        // `resolve` is an image, or a table with `image`, `mip` and `layer`.
        let resolve = match field::<Value>(&a, "resolve")? {
            Value::Nil => None,
            Value::Table(r) => Some((handle(field(&r, "image")?), subresource(&r)?)),
            Value::Integer(image) => Some((handle(image), Subresource::default())),
            Value::Number(image) => Some((handle(image as i64), Subresource::default())),
            other => {
                return Err(arg_err(format!(
                    "field `resolve` must be an image or a table, got {}",
                    other.type_name()
                )));
            }
        };
        color.push(PassColor {
            image: handle(field(&a, "image")?),
            sub: subresource(&a)?,
            load,
            store: field::<Option<bool>>(&a, "store")?.unwrap_or(true),
            resolve,
        });
    }
    let depth = match field::<Option<Table>>(t, "depth")? {
        None => None,
        Some(d) => Some(PassDepth {
            image: handle(field(&d, "image")?),
            sub: subresource(&d)?,
            load: match field::<Option<String>>(&d, "load")?.as_deref() {
                Some("clear") | None => {
                    DepthLoadOp::Clear(field::<Option<f32>>(&d, "clear")?.unwrap_or(1.0))
                }
                Some("load") => DepthLoadOp::Load,
                Some("dont-care") => DepthLoadOp::DontCare,
                Some(other) => return Err(arg_err(format!("unknown load op `{other}`"))),
            },
            store: field::<Option<bool>>(&d, "store")?.unwrap_or(true),
        }),
    };
    if color.is_empty() && depth.is_none() && !t.contains_key("color")? {
        return Err(arg_err("field `color` or field `depth` is required"));
    }
    Ok(PassDesc { color, depth })
}

/// Whether the device supports one named capability of a format.
fn format_capability(gpu: &Gpu, format: Format, name: &str) -> Option<bool> {
    let caps = gpu.format_caps(format);
    Some(match name {
        "sampled" => caps.sampled,
        "filter_linear" => caps.filter_linear,
        "color_target" => caps.color_attachment,
        "blend" => caps.blend,
        "depth_target" => caps.depth_attachment,
        "storage" => caps.storage,
        "copy_src" => caps.transfer_src,
        "copy_dst" => caps.transfer_dst,
        "mipmaps" => caps.blit_src && caps.blit_dst && caps.filter_linear,
        _ => return None,
    })
}
/// Installs the global `gfx` table.
pub fn register(lua: &Lua, gpu: &Shared, loader: Option<crate::ShaderLoader>) -> mlua::Result<()> {
    let gfx = lua.create_table()?;

    // Each entry: name, closure taking (&Lua, &mut Gpu, args).
    macro_rules! def {
        ($name:literal, |$lua:ident, $gpu:ident, $args:tt : $ty:ty| $body:expr) => {{
            let shared = Rc::clone(gpu);
            gfx.set(
                $name,
                lua.create_function(move |$lua, $args: $ty| {
                    let _ = $lua;
                    let mut guard = shared.borrow_mut();
                    let $gpu: &mut Gpu = &mut guard;
                    $body
                })?,
            )?;
        }};
    }

    // ----- objects ------------------------------------------------------------

    def!("create_buffer", |lua, gpu, t: Table| {
        gpu.create_buffer(&BufferDesc {
            size: field(&t, "size")?,
            usage: usage_field(&t, BufferUsage::from_name)?,
        })
        .map(Handle::to_bits)
        .map_err(gfx_err)
    });
    // The optional fourth and fifth arguments pick a part of `data`: its
    // first byte and its length.
    def!("write_buffer", |lua,
                          gpu,
                          (
        buffer,
        offset,
        data,
        data_offset,
        data_len,
    ): (
        i64,
        u64,
        Value,
        Option<usize>,
        Option<usize>
    )| {
        gpu.check_not_protected(handle(buffer), "write to")
            .map_err(gfx_err)?;
        with_byte_range(&data, data_offset, data_len, |bytes| {
            gpu.write_buffer(handle(buffer), offset, bytes)
                .map_err(gfx_err)
        })
    });
    // `kind` ("2d", "3d", "cube"), `depth`, `mip_levels` and `samples` are
    // optional: a 2D image with one mip level and one sample.
    def!("create_image", |lua, gpu, t: Table| {
        gpu.create_image_with(
            &ImageDesc {
                format: name_field(&t, "format", None, Format::from_name)?,
                width: field(&t, "width")?,
                height: field(&t, "height")?,
                usage: usage_field(&t, ImageUsage::from_name)?,
            },
            &ImageExtra {
                kind: name_field(&t, "kind", Some("2d"), ImageKind::from_name)?,
                depth: field::<Option<u32>>(&t, "depth")?.unwrap_or(1),
                mip_levels: field::<Option<u32>>(&t, "mip_levels")?.unwrap_or(1),
                samples: field::<Option<u32>>(&t, "samples")?.unwrap_or(1),
            },
        )
        .map(Handle::to_bits)
        .map_err(gfx_err)
    });
    def!("image_info", |lua, gpu, image: i64| {
        let info = gpu.image_info(handle(image)).map_err(gfx_err)?;
        Ok((
            info.width,
            info.height,
            info.format.name(),
            info.mip_levels,
            info.depth,
            info.kind.name(),
            info.samples,
        ))
    });
    // The optional third argument names the mip level and the layer:
    // `{ mip = 1, layer = 0 }`.
    def!("upload_image", |lua,
                          gpu,
                          (image, data, sub): (
        i64,
        Value,
        Option<Table>
    )| {
        gpu.check_not_protected(handle(image), "upload to")
            .map_err(gfx_err)?;
        let sub = match &sub {
            Some(t) => subresource(t)?,
            None => Subresource::default(),
        };
        with_bytes(&data, |bytes| {
            gpu.upload_image_level(handle(image), sub, bytes)
                .map_err(gfx_err)
        })
    });
    def!("generate_mipmaps", |lua, gpu, image: i64| {
        gpu.check_not_protected(handle(image), "generate mip levels of")
            .map_err(gfx_err)?;
        gpu.generate_mipmaps(handle(image)).map_err(gfx_err)
    });
    // Whether the device supports a format for one capability or for each
    // of a list: "sampled", "filter_linear", "color_target", "blend",
    // "depth_target", "storage", "copy_src", "copy_dst", "mipmaps".
    def!(
        "format_supports",
        |lua, gpu, (format, what): (String, Value)| {
            let format = Format::from_name(&format)
                .ok_or_else(|| arg_err(format!("unknown format `{format}`")))?;
            let names: Vec<String> = match what {
                Value::String(s) => vec![s.to_str()?.to_owned()],
                Value::Table(t) => t.sequence_values::<String>().collect::<mlua::Result<_>>()?,
                other => {
                    return Err(arg_err(format!(
                        "expected a capability name or a list of names, got {}",
                        other.type_name()
                    )));
                }
            };
            let mut supported = true;
            for name in names {
                supported &= format_capability(gpu, format, &name)
                    .ok_or_else(|| arg_err(format!("unknown capability `{name}`")))?;
            }
            Ok(supported)
        }
    );
    def!("copy_image", |lua, gpu, (src, dst): (i64, i64)| {
        gpu.check_not_protected(handle(dst), "copy into")
            .map_err(gfx_err)?;
        gpu.copy_image(handle(src), handle(dst)).map_err(gfx_err)
    });
    // A shader is named (`shader`, the path of its Slang source); the host's
    // loader finds its SPIR-V.
    def!("create_shader", |lua, gpu, t: Table| {
        if t.contains_key("source")? || t.contains_key("language")? {
            return Err(arg_err(
                "shaders are named by their Slang source (`shader = \"shaders/x.slang\"`); \
                 `source` and `language` are not accepted",
            ));
        }
        let stage = name_field(&t, "stage", None, ShaderStage::from_name)?;
        let entry: Option<String> = field(&t, "entry")?;
        let entry = entry.as_deref().unwrap_or("main");
        let shader: String = field(&t, "shader")?;
        let shader_error =
            |message: String| gfx_err(GfxError::new(ErrorCode::ShaderCompile, message));
        let Some(loader) = &loader else {
            return Err(shader_error(
                "this host does not load shaders by name".into(),
            ));
        };
        let spirv = (loader.0)(&shader, entry).map_err(shader_error)?;
        gpu.create_shader_spirv(stage, entry, &spirv)
            .map(Handle::to_bits)
            .map_err(gfx_err)
    });
    def!("create_render_pipeline", |lua, gpu, t: Table| {
        let (desc, extra) = render_pipeline_desc(&t)?;
        gpu.create_render_pipeline_with(&desc, &extra)
            .map(Handle::to_bits)
            .map_err(gfx_err)
    });
    def!("create_compute_pipeline", |lua, gpu, shader: i64| {
        gpu.create_compute_pipeline(handle(shader))
            .map(Handle::to_bits)
            .map_err(gfx_err)
    });
    def!("create_sampler", |lua, gpu, t: Table| {
        let defaults = SamplerExtra::default();
        gpu.create_sampler_with(
            &SamplerDesc {
                mag: name_field(&t, "mag", Some("linear"), filter)?,
                min: name_field(&t, "min", Some("linear"), filter)?,
                address_u: name_field(&t, "address_u", Some("clamp"), address_mode)?,
                address_v: name_field(&t, "address_v", Some("clamp"), address_mode)?,
            },
            &SamplerExtra {
                mip: name_field(&t, "mip", Some("nearest"), filter)?,
                address_w: name_field(&t, "address_w", Some("clamp"), address_mode)?,
                lod_min: field::<Option<f32>>(&t, "lod_min")?.unwrap_or(defaults.lod_min),
                lod_max: field::<Option<f32>>(&t, "lod_max")?.unwrap_or(defaults.lod_max),
                lod_bias: field::<Option<f32>>(&t, "lod_bias")?.unwrap_or(defaults.lod_bias),
                anisotropy: field::<Option<f32>>(&t, "anisotropy")?.unwrap_or(defaults.anisotropy),
                border: name_field(&t, "border", Some("transparent"), |s| match s {
                    "transparent" => Some(BorderColor::TransparentBlack),
                    "black" => Some(BorderColor::OpaqueBlack),
                    "white" => Some(BorderColor::OpaqueWhite),
                    _ => None,
                })?,
                compare: match field::<Option<String>>(&t, "compare")? {
                    None => None,
                    Some(name) => Some(CompareOp::from_name(&name).ok_or_else(|| {
                        arg_err(format!("field `compare`: unknown value `{name}`"))
                    })?),
                },
            },
        )
        .map(Handle::to_bits)
        .map_err(gfx_err)
    });
    // The optional second argument shows a part of the image:
    // `{ base_mip = 0, mip_count = 1, layer = 2 }`.
    def!("create_texture_slot", |lua,
                                 gpu,
                                 (image, view): (
        i64,
        Option<Table>
    )| {
        gpu.create_texture_slot_view(handle(image), &texture_view(view)?)
            .map(Handle::to_bits)
            .map_err(gfx_err)
    });
    def!("create_texture_slots", |lua, gpu, images: Vec<i64>| {
        let images: Vec<Handle> = images.into_iter().map(handle).collect();
        gpu.create_texture_slot_run(&images)
            .map(|slots| slots.into_iter().map(Handle::to_bits).collect::<Vec<_>>())
            .map_err(gfx_err)
    });
    def!("create_storage_texture_slot", |lua,
                                         gpu,
                                         (image, view): (
        i64,
        Option<Table>
    )| {
        gpu.create_storage_texture_slot_view(handle(image), &texture_view(view)?)
            .map(Handle::to_bits)
            .map_err(gfx_err)
    });
    def!("create_uniform_slot", |lua,
                                 gpu,
                                 (buffer, offset, size): (
        i64,
        u64,
        u64
    )| {
        gpu.create_uniform_slot(handle(buffer), offset, size)
            .map(Handle::to_bits)
            .map_err(gfx_err)
    });
    def!("create_storage_buffer_slot", |lua,
                                        gpu,
                                        (buffer, offset, size): (
        i64,
        u64,
        u64
    )| {
        gpu.create_storage_buffer_slot(handle(buffer), offset, size)
            .map(Handle::to_bits)
            .map_err(gfx_err)
    });
    def!("slot_index", |lua, gpu, slot: i64| {
        let _ = gpu;
        Ok(handle(slot).index())
    });
    def!("destroy", |lua, gpu, object: i64| {
        gpu.check_not_protected(handle(object), "destroy")
            .map_err(gfx_err)?;
        gpu.destroy(handle(object)).map_err(gfx_err)
    });

    // ----- recording ----------------------------------------------------------

    def!("begin_pass", |lua, gpu, t: Table| {
        gpu.begin_pass_desc(&pass_desc(&t)?).map_err(gfx_err)
    });
    def!("end_pass", |lua, gpu, (): ()| gpu
        .end_pass()
        .map_err(gfx_err));
    def!("set_pipeline", |lua, gpu, pipeline: i64| {
        gpu.set_pipeline(handle(pipeline)).map_err(gfx_err)
    });
    def!("set_viewport", |lua,
                          gpu,
                          (x, y, w, h, min_depth, max_depth): (
        f32,
        f32,
        f32,
        f32,
        Option<f32>,
        Option<f32>
    )| {
        gpu.set_viewport_depth(
            x,
            y,
            w,
            h,
            min_depth.unwrap_or(0.0),
            max_depth.unwrap_or(1.0),
        )
        .map_err(gfx_err)
    });
    def!(
        "set_scissor",
        |lua, gpu, (x, y, w, h): (u32, u32, u32, u32)| {
            gpu.set_scissor(x, y, w, h).map_err(gfx_err)
        }
    );
    def!("set_vertex_buffer", |lua,
                               gpu,
                               (index, buffer, offset): (
        u32,
        i64,
        Option<u64>
    )| {
        gpu.set_vertex_buffer(index, handle(buffer), offset.unwrap_or(0))
            .map_err(gfx_err)
    });
    def!("set_index_buffer", |lua,
                              gpu,
                              (buffer, index_type, offset): (
        i64,
        Option<String>,
        Option<u64>
    )| {
        let index_type = match index_type.as_deref() {
            Some("u16") => IndexType::U16,
            Some("u32") | None => IndexType::U32,
            Some(other) => return Err(arg_err(format!("unknown index type `{other}`"))),
        };
        gpu.set_index_buffer(handle(buffer), offset.unwrap_or(0), index_type)
            .map_err(gfx_err)
    });
    def!("bind", |lua, gpu, (binding, slot): (u32, i64)| {
        gpu.bind(binding, handle(slot)).map_err(gfx_err)
    });
    def!("push", |lua, gpu, (offset, data): (u32, Value)| {
        with_bytes(&data, |bytes| gpu.push(offset, bytes).map_err(gfx_err))
    });
    def!(
        "push_f32",
        |lua, gpu, (offset, values): (u32, Variadic<Value>)| {
            // Numbers, and on Luau `vector` values, packed as consecutive f32.
            let mut bytes = [0u8; 256];
            let mut len = 0;
            let mut put = |v: f32| -> mlua::Result<()> {
                if len + 4 > bytes.len() {
                    return Err(arg_err("too many values for push data"));
                }
                bytes[len..len + 4].copy_from_slice(&v.to_ne_bytes());
                len += 4;
                Ok(())
            };
            for value in values.iter() {
                match value {
                    Value::Number(n) => put(*n as f32)?,
                    Value::Integer(n) => put(*n as f32)?,
                    #[cfg(feature = "luau")]
                    Value::Vector(v) => {
                        put(v.x())?;
                        put(v.y())?;
                        put(v.z())?;
                    }
                    other => {
                        return Err(arg_err(format!(
                            "push_f32 takes numbers, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
            gpu.push(offset, &bytes[..len]).map_err(gfx_err)
        }
    );
    def!("draw", |lua,
                  gpu,
                  (
        vertices,
        instances,
        first_vertex,
        first_instance,
    ): (
        u32,
        Option<u32>,
        Option<u32>,
        Option<u32>
    )| {
        gpu.draw(
            vertices,
            instances.unwrap_or(1),
            first_vertex.unwrap_or(0),
            first_instance.unwrap_or(0),
        )
        .map_err(gfx_err)
    });
    def!("draw_indexed", |lua,
                          gpu,
                          (
        indices,
        instances,
        first_index,
        vertex_offset,
        first_instance,
    ): (
        u32,
        Option<u32>,
        Option<u32>,
        Option<i32>,
        Option<u32>
    )| {
        gpu.draw_indexed(
            indices,
            instances.unwrap_or(1),
            first_index.unwrap_or(0),
            vertex_offset.unwrap_or(0),
            first_instance.unwrap_or(0),
        )
        .map_err(gfx_err)
    });
    def!(
        "dispatch",
        |lua, gpu, (x, y, z): (u32, Option<u32>, Option<u32>)| {
            gpu.dispatch(x, y.unwrap_or(1), z.unwrap_or(1))
                .map_err(gfx_err)
        }
    );

    // A call that does nothing, to measure the cost of crossing the boundary.
    gfx.set(
        "noop",
        lua.create_function(|_, (_a, _b): (u32, i64)| Ok(()))?,
    )?;

    lua.globals().set("gfx", gfx)?;
    Ok(())
}
