//! The `gfx` table: one script function per `gfx-core` call.
//!
//! Handles cross the boundary as plain numbers. Descriptions are tables.
//! Everything semantic is checked in `gfx-core`; this file only converts
//! values, so porting it to another script language is mechanical.

use std::cell::RefCell;
use std::rc::Rc;

use gfx_core::*;
use mlua::{Lua, Table, Value, Variadic};

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
    match value {
        Value::String(s) => f(&s.as_bytes()),
        #[cfg(feature = "luau")]
        Value::Buffer(b) => {
            // mlua 0.12 has no borrowed view of a buffer; small ones are read
            // into a stack array to keep the hot `push` path allocation-free.
            use std::io::Read;
            let len = b.len();
            let mut small = [0u8; 256];
            if len <= small.len() {
                b.clone()
                    .cursor()
                    .read_exact(&mut small[..len])
                    .map_err(mlua::Error::external)?;
                f(&small[..len])
            } else {
                f(&b.to_vec())
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

fn render_pipeline_desc(t: &Table) -> mlua::Result<RenderPipelineDesc> {
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
    for target in field::<Vec<Table>>(t, "targets")? {
        targets.push(ColorTarget {
            format: name_field(&target, "format", None, Format::from_name)?,
            blend: blend(field(&target, "blend")?)?,
        });
    }
    Ok(RenderPipelineDesc {
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
    })
}

fn color_attachments(t: &Table) -> mlua::Result<Vec<ColorAttachment>> {
    let mut color = Vec::new();
    for a in field::<Vec<Table>>(t, "color")? {
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
        color.push(ColorAttachment {
            image: handle(field(&a, "image")?),
            load,
            store: field::<Option<bool>>(&a, "store")?.unwrap_or(true),
        });
    }
    Ok(color)
}

/// Installs the global `gfx` table.
pub fn register(lua: &Lua, gpu: &Shared) -> mlua::Result<()> {
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
    def!("write_buffer", |lua,
                          gpu,
                          (buffer, offset, data): (
        i64,
        u64,
        Value
    )| {
        gpu.check_not_protected(handle(buffer), "write to")
            .map_err(gfx_err)?;
        with_bytes(&data, |bytes| {
            gpu.write_buffer(handle(buffer), offset, bytes)
                .map_err(gfx_err)
        })
    });
    def!("create_image", |lua, gpu, t: Table| {
        gpu.create_image(&ImageDesc {
            format: name_field(&t, "format", None, Format::from_name)?,
            width: field(&t, "width")?,
            height: field(&t, "height")?,
            usage: usage_field(&t, ImageUsage::from_name)?,
        })
        .map(Handle::to_bits)
        .map_err(gfx_err)
    });
    def!("image_info", |lua, gpu, image: i64| {
        let info = gpu.image_info(handle(image)).map_err(gfx_err)?;
        Ok((info.width, info.height, info.format.name()))
    });
    def!("upload_image", |lua, gpu, (image, data): (i64, Value)| {
        gpu.check_not_protected(handle(image), "upload to")
            .map_err(gfx_err)?;
        with_bytes(&data, |bytes| {
            gpu.upload_image(handle(image), bytes).map_err(gfx_err)
        })
    });
    def!("copy_image", |lua, gpu, (src, dst): (i64, i64)| {
        gpu.check_not_protected(handle(dst), "copy into")
            .map_err(gfx_err)?;
        gpu.copy_image(handle(src), handle(dst)).map_err(gfx_err)
    });
    def!("create_shader", |lua, gpu, t: Table| {
        let source: mlua::LuaString = field(&t, "source")?;
        let entry: Option<String> = field(&t, "entry")?;
        gpu.create_shader(&ShaderSource {
            stage: name_field(&t, "stage", None, ShaderStage::from_name)?,
            language: name_field(&t, "language", Some("wgsl"), ShaderLanguage::from_name)?,
            source: &source.to_str()?,
            entry: entry.as_deref().unwrap_or("main"),
        })
        .map(Handle::to_bits)
        .map_err(gfx_err)
    });
    def!("create_render_pipeline", |lua, gpu, t: Table| {
        gpu.create_render_pipeline(&render_pipeline_desc(&t)?)
            .map(Handle::to_bits)
            .map_err(gfx_err)
    });
    def!("create_compute_pipeline", |lua, gpu, shader: i64| {
        gpu.create_compute_pipeline(handle(shader))
            .map(Handle::to_bits)
            .map_err(gfx_err)
    });
    def!("create_sampler", |lua, gpu, t: Table| {
        gpu.create_sampler(&SamplerDesc {
            mag: name_field(&t, "mag", Some("linear"), filter)?,
            min: name_field(&t, "min", Some("linear"), filter)?,
            address_u: name_field(&t, "address_u", Some("clamp"), address_mode)?,
            address_v: name_field(&t, "address_v", Some("clamp"), address_mode)?,
        })
        .map(Handle::to_bits)
        .map_err(gfx_err)
    });
    def!("create_texture_slot", |lua, gpu, image: i64| {
        gpu.create_texture_slot(handle(image))
            .map(Handle::to_bits)
            .map_err(gfx_err)
    });
    def!("create_texture_slots", |lua, gpu, images: Vec<i64>| {
        let images: Vec<Handle> = images.into_iter().map(handle).collect();
        gpu.create_texture_slot_run(&images)
            .map(|slots| slots.into_iter().map(Handle::to_bits).collect::<Vec<_>>())
            .map_err(gfx_err)
    });
    def!("create_storage_texture_slot", |lua, gpu, image: i64| {
        gpu.create_storage_texture_slot(handle(image))
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
        gpu.begin_pass(&color_attachments(&t)?).map_err(gfx_err)
    });
    def!("end_pass", |lua, gpu, (): ()| gpu
        .end_pass()
        .map_err(gfx_err));
    def!("set_pipeline", |lua, gpu, pipeline: i64| {
        gpu.set_pipeline(handle(pipeline)).map_err(gfx_err)
    });
    def!(
        "set_viewport",
        |lua, gpu, (x, y, w, h): (f32, f32, f32, f32)| {
            gpu.set_viewport(x, y, w, h).map_err(gfx_err)
        }
    );
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
