//! The `hz` table: what the compositor core shows to a script.
//!
//! `gfx` (from `proto-gfx-lua`) is the GPU. `hz` is everything else: the windows
//! and their surfaces, the outputs, the pointer, color transforms, and the
//! requests a script can make of the core.
//!
//! Every function reads the shared state the core last filled in, or queues
//! a request the core carries out after the script returns. None of them
//! reaches into the Wayland server directly.

use std::cell::RefCell;
use std::rc::Rc;

use crate::wm::WindowId;
use mlua::{Lua, Table, Value};
use proto_backend::OutputInfo;
use proto_color::{
    AlphaMode, CompositeSpace, DecodeTransform, EncodeTransform, ImageDescription, Primaries,
};
use proto_gfx_core::{ErrorCode, GfxError, Gpu, Handle};
use proto_wl::SurfaceRole;

use super::Shared;
use crate::scene::{CursorImage, SceneRequest};

type SharedRef = Rc<RefCell<Shared>>;

/// Registry key of the table that caches loaded modules.
const MODULES: &str = "hz.modules";

fn arg_error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::external(GfxError {
        code: ErrorCode::InvalidArgument,
        message: message.into(),
    })
}

/// The primaries a script names: one of the named sets, or a table of eight
/// chromaticity coordinates (red x, y, green x, y, blue x, y, white x, y).
fn primaries(value: &Value) -> mlua::Result<Primaries> {
    match value {
        Value::String(name) => {
            let name = name.to_str()?;
            Primaries::from_name(&name).ok_or_else(|| {
                arg_error(format!(
                    "unknown primaries {:?} (known: {})",
                    &*name,
                    Primaries::NAMED
                        .iter()
                        .map(|(n, _)| *n)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
        }
        Value::Table(table) => {
            let c: Vec<f64> = table.sequence_values().collect::<mlua::Result<_>>()?;
            if c.len() != 8 || c.iter().any(|v| !v.is_finite()) {
                return Err(arg_error(
                    "primaries are a name or eight chromaticity coordinates",
                ));
            }
            let at = |i: usize| proto_color::Chromaticity::new(c[i], c[i + 1]);
            let primaries = Primaries {
                red: at(0),
                green: at(2),
                blue: at(4),
                white: at(6),
            };
            // Collinear primaries have no conversion to anything.
            if primaries.rgb_to_xyz().is_none() {
                return Err(arg_error("degenerate primaries"));
            }
            Ok(primaries)
        }
        _ => Err(arg_error(
            "primaries are a name or eight chromaticity coordinates",
        )),
    }
}

fn matrix_table(lua: &Lua, matrix: &[[f32; 3]; 3]) -> mlua::Result<Table> {
    lua.create_sequence_from(matrix.iter().flatten().copied())
}

fn color_table(lua: &Lua, description: &ImageDescription) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("name", description.name())?;
    t.set(
        "primaries",
        match description.primaries.name() {
            Some(name) => Value::String(lua.create_string(name)?),
            None => Value::Nil,
        },
    )?;
    let p = &description.primaries;
    t.set(
        "chromaticities",
        lua.create_sequence_from([
            p.red.x, p.red.y, p.green.x, p.green.y, p.blue.x, p.blue.y, p.white.x, p.white.y,
        ])?,
    )?;
    t.set("transfer", description.transfer.name())?;
    t.set("transfer_id", description.transfer.shader_id())?;
    t.set(
        "transfer_param",
        description.transfer.param(&description.luminance),
    )?;
    t.set("luminance_min", description.luminance.min)?;
    t.set("luminance_max", description.luminance.max)?;
    t.set("luminance_reference", description.luminance.reference)?;
    Ok(t)
}

/// An output as a script sees it.
pub fn output_table(lua: &Lua, info: &OutputInfo) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("id", info.id.0)?;
    t.set("name", info.name.as_str())?;
    // Physical pixels.
    t.set("width", info.size.0)?;
    t.set("height", info.size.1)?;
    t.set("scale", info.scale)?;
    // Position and size in the global logical space.
    t.set("x", info.position.0)?;
    t.set("y", info.position.1)?;
    t.set("logical_width", info.logical_size().0)?;
    t.set("logical_height", info.logical_size().1)?;
    t.set("refresh_mhz", info.refresh_mhz)?;
    t.set(
        "format",
        match info.format {
            proto_backend::OutputFormat::Rgba8Unorm => "rgba8unorm",
            proto_backend::OutputFormat::Rgba16Unorm => "rgba16unorm",
            proto_backend::OutputFormat::Bgra8Unorm => "bgra8unorm",
        },
    )?;
    t.set("color", color_table(lua, &info.image_description)?)?;
    Ok(t)
}

fn role_name(role: SurfaceRole) -> &'static str {
    match role {
        SurfaceRole::Toplevel => "toplevel",
        SurfaceRole::Subsurface => "subsurface",
        SurfaceRole::Popup => "popup",
        SurfaceRole::Cursor => "cursor",
        SurfaceRole::DragIcon => "drag-icon",
        SurfaceRole::InputPopup => "input-popup",
    }
}

/// The fields of a surface that come from its texture.
fn texture_fields(shared: &Shared, t: &Table, surface: proto_wl::SurfaceId) -> mlua::Result<()> {
    set_texture_fields(t, shared.textures.get(&surface))
}

fn set_texture_fields(t: &Table, texture: Option<&super::SurfaceTexture>) -> mlua::Result<()> {
    if let Some(texture) = texture {
        t.set("texture", texture.slot.to_bits())?;
        t.set("image", texture.image.to_bits())?;
        t.set("buffer_width", texture.width)?;
        t.set("buffer_height", texture.height)?;
        t.set("opaque", texture.opaque)?;
        t.set("generation", texture.generation)?;
        t.set("serial", texture.serial)?;
        t.set("color", texture.color)?;
        t.set("kind", if texture.imported { "dmabuf" } else { "shm" })?;
    }
    Ok(())
}

/// How the unit square of a surface maps to its texture: `{a, b, c, d, tx,
/// ty}` with `u = a*s + b*t + tx` and `v = c*s + d*t + ty` (the layout of
/// the script's affine transforms).
///
/// `source` is the shown part of the upright image in pixels. A texture
/// that is an imported client buffer may hold the image turned
/// (`wl_surface.set_buffer_transform`) or bottom-up; this is where that is
/// undone, so a script only ever multiplies.
pub(crate) fn uv_transform(
    texture: &super::SurfaceTexture,
    source: (f64, f64, f64, f64),
) -> [f64; 6] {
    let (upright_w, upright_h) = texture
        .transform
        .upright_size((texture.width, texture.height));
    let (upright_w, upright_h) = (upright_w.max(1) as f64, upright_h.max(1) as f64);
    // Upright image, normalized: p = p0 + scale * (s, t).
    let (px, py) = (source.0 / upright_w, source.1 / upright_h);
    let (pw, ph) = (source.2 / upright_w, source.3 / upright_h);
    // Upright to buffer, normalized. The mapping of pixel positions is the
    // protocol layer's; in normalized coordinates it does not depend on the
    // size, so it is taken from a 2x2 image: uv = m * p + o.
    let at = |x, y| {
        let (bx, by) = texture.transform.buffer_position((2, 2), x, y);
        (bx as f64, by as f64)
    };
    let (origin, right, down) = (at(0, 0), at(1, 0), at(0, 1));
    let m = [
        right.0 - origin.0,
        down.0 - origin.0,
        right.1 - origin.1,
        down.1 - origin.1,
    ];
    let o = [
        (origin.0 + 0.5) / 2.0 - (m[0] + m[1]) * 0.25,
        (origin.1 + 0.5) / 2.0 - (m[2] + m[3]) * 0.25,
    ];
    let mut uv = [
        m[0] * pw,
        m[1] * ph,
        m[2] * pw,
        m[3] * ph,
        m[0] * px + m[1] * py + o[0],
        m[2] * px + m[3] * py + o[1],
    ];
    if texture.y_inverted {
        uv[2] = -uv[2];
        uv[3] = -uv[3];
        uv[5] = 1.0 - uv[5];
    }
    uv
}

/// A surface that follows the pointer (a cursor image, a drag icon).
fn pointer_surface(
    lua: &Lua,
    shared: &Shared,
    surface: &crate::scene::SurfaceView,
) -> mlua::Result<Table> {
    let s = lua.create_table()?;
    s.set("id", surface.surface.0)?;
    s.set("x", surface.offset.0)?;
    s.set("y", surface.offset.1)?;
    s.set("width", surface.size.0)?;
    s.set("height", surface.size.1)?;
    texture_fields(shared, &s, surface.surface)?;
    if let (Some(source), Some(texture)) = (surface.source, shared.textures.get(&surface.surface)) {
        s.set(
            "uv",
            lua.create_sequence_from(uv_transform(texture, source))?,
        )?;
    }
    Ok(s)
}

/// Removes the cached modules, so that a reload reads them again.
pub fn clear_modules(lua: &Lua) -> mlua::Result<()> {
    lua.set_named_registry_value(MODULES, lua.create_table()?)
}

/// Installs the global `hz` table.
pub fn register(
    lua: &Lua,
    shared: &SharedRef,
    windows: &Rc<RefCell<crate::fallback::Windows>>,
    gpu: &Rc<RefCell<Gpu>>,
) -> mlua::Result<()> {
    let hz = lua.create_table()?;
    clear_modules(lua)?;

    macro_rules! def {
        ($name:literal, |$lua:ident, $shared:ident, $args:tt : $ty:ty| $body:expr) => {{
            let state = Rc::clone(shared);
            hz.set(
                $name,
                lua.create_function(move |$lua, $args: $ty| {
                    let _ = $lua;
                    let $shared = &state;
                    $body
                })?,
            )?;
        }};
    }

    // ----- the world ----------------------------------------------------------

    def!("outputs", |lua, shared, (): ()| {
        let shared = shared.borrow();
        let list = lua.create_table()?;
        for output in &shared.world.outputs {
            list.push(output_table(lua, output)?)?;
        }
        Ok(list)
    });

    // Windows in stacking order, bottom to top.
    def!("windows", |lua, shared, (): ()| {
        let shared = shared.borrow();
        let list = lua.create_table()?;
        for window in &shared.world.windows {
            let t = lua.create_table()?;
            t.set("id", window.window.0)?;
            t.set("title", window.title.as_deref())?;
            t.set("app_id", window.app_id.as_deref())?;
            t.set("mapped", window.mapped)?;
            t.set("x", window.position.0)?;
            t.set("y", window.position.1)?;
            if let Some((width, height)) = window.size {
                t.set("width", width)?;
                t.set("height", height)?;
            }
            t.set("activated", window.activated)?;
            t.set("focused", shared.world.focus == Some(window.window))?;
            t.set("parent", window.parent.map(|p| p.0))?;
            t.set("min_width", window.min_size.0)?;
            t.set("min_height", window.min_size.1)?;
            t.set("max_width", window.max_size.0)?;
            t.set("max_height", window.max_size.1)?;
            t.set("x11", window.x11)?;
            t.set(
                "opacity",
                shared.opacity.get(&window.window).copied().unwrap_or(1.0),
            )?;
            list.push(t)?;
        }
        Ok(list)
    });

    // The surfaces of a window, back to front. Offsets are relative to the
    // window geometry, in logical coordinates. Also for a closed window that
    // is kept (`hz.keep_closed`): what it last showed.
    def!("surfaces", |lua, shared, window: u64| {
        let shared = shared.borrow();
        let list = lua.create_table()?;
        let id = WindowId(window);
        let (nodes, textures) = match shared.world.windows.iter().find(|w| w.window == id) {
            Some(window) => (&window.surfaces, &shared.textures),
            None => match shared.closed.iter().find(|c| c.window.window == id) {
                Some(closed) => (&closed.window.surfaces, &closed.textures),
                None => return Ok(list),
            },
        };
        for node in nodes {
            let t = lua.create_table()?;
            t.set("id", node.surface.0)?;
            t.set("role", role_name(node.role))?;
            t.set("x", node.offset.0)?;
            t.set("y", node.offset.1)?;
            t.set("width", node.size.0)?;
            t.set("height", node.size.1)?;
            t.set("scale", node.buffer_scale)?;
            t.set("alpha_mode", node.alpha_mode.name())?;
            set_texture_fields(&t, textures.get(&node.surface))?;
            // Where the surface's rectangle lies in the texture.
            if let (Some(source), Some(texture)) = (node.source, textures.get(&node.surface)) {
                t.set(
                    "uv",
                    lua.create_sequence_from(uv_transform(texture, source))?,
                )?;
            }
            list.push(t)?;
        }
        Ok(list)
    });

    // What changed with the last update of a surface: a list of
    // {x, y, width, height} in buffer pixels. Empty means "unknown, treat
    // as everything".
    def!("damage", |lua, shared, surface: u64| {
        let shared = shared.borrow();
        let list = lua.create_table()?;
        if let Some(texture) = shared.textures.get(&proto_wl::SurfaceId(surface)) {
            for rect in &texture.damage {
                list.push(lua.create_sequence_from(rect.iter().copied())?)?;
            }
        }
        Ok(list)
    });

    def!("pointer", |lua, shared, (): ()| {
        Ok(match shared.borrow().world.pointer {
            Some((x, y)) => (Some(x), Some(y)),
            None => (None, None),
        })
    });

    // The cursor to draw, or nil. `texture` is the built-in arrow, or the
    // surfaces a client supplied (offsets relative to the pointer).
    {
        let state = Rc::clone(shared);
        let gpu = Rc::clone(gpu);
        hz.set(
            "cursor",
            lua.create_function(move |lua, (): ()| {
                let Some(cursor) = state.borrow().world.cursor.clone() else {
                    return Ok(Value::Nil);
                };
                let t = lua.create_table()?;
                t.set("x", cursor.position.0)?;
                t.set("y", cursor.position.1)?;
                match &cursor.image {
                    CursorImage::Default => {
                        let (_, slot) = super::arrow_texture(&state, &mut gpu.borrow_mut())
                            .map_err(mlua::Error::external)?;
                        t.set("texture", slot.to_bits())?;
                        t.set("width", crate::scene::ARROW_WIDTH)?;
                        t.set("height", crate::scene::ARROW_HEIGHT)?;
                    }
                    CursorImage::Surfaces(surfaces) => {
                        let shared = state.borrow();
                        let list = lua.create_table()?;
                        for surface in surfaces {
                            list.push(pointer_surface(lua, &shared, surface)?)?;
                        }
                        t.set("surfaces", list)?;
                    }
                }
                Ok(Value::Table(t))
            })?,
        )?;
    }

    // The surfaces of the icon a client attached to a drag in progress,
    // back to front, with offsets relative to the pointer. Empty otherwise.
    def!("drag_icon", |lua, shared, (): ()| {
        let shared = shared.borrow();
        let list = lua.create_table()?;
        for surface in &shared.world.drag_icon {
            list.push(pointer_surface(lua, &shared, surface)?)?;
        }
        Ok(list)
    });

    // What a surface's values mean: the image description behind the
    // `color` field of a surface.
    def!("color_info", |lua, shared, color: u32| {
        let (description, alpha) = *shared
            .borrow()
            .colors
            .get(color as usize)
            .ok_or_else(|| arg_error(format!("unknown color {color}")))?;
        let t = color_table(lua, &description)?;
        t.set("alpha_mode", alpha.name())?;
        Ok(t)
    });

    // The background color: straight RGB, to be read like content without
    // color information.
    def!("background", |lua, shared, (): ()| {
        let [r, g, b] = shared.borrow().world.background;
        Ok((r, g, b))
    });

    // Seconds since the script runtime started, on the clock frames are
    // timed with (the clock `frame` is given its time on).
    def!("time", |lua, shared, (): ()| {
        let shared = shared.borrow();
        Ok(shared.seconds(shared.clock.now()))
    });

    // ----- closed windows -----------------------------------------------------
    //
    // A window that closes takes its surfaces with it. A script that wants
    // to show it a while longer (a close animation) asks beforehand: then
    // what the window last showed stays, until the script lets go.

    // Keeps what a window shows once it closes (`keep` defaults to true).
    def!(
        "keep_closed",
        |lua, shared, (window, keep): (u64, Option<bool>)| {
            let mut shared = shared.borrow_mut();
            let id = WindowId(window);
            if !shared.world.windows.iter().any(|w| w.window == id) {
                return Err(arg_error(format!("unknown window {window}")));
            }
            if keep.unwrap_or(true) {
                shared.keep_closed.insert(id);
            } else {
                shared.keep_closed.remove(&id);
            }
            Ok(())
        }
    );

    // The closed windows that are kept, oldest first: id, title, app_id,
    // x, y, width, height as they last were, and `closed_at` (`hz.time()`).
    def!("closed_windows", |lua, shared, (): ()| {
        let shared = shared.borrow();
        let list = lua.create_table()?;
        for closed in &shared.closed {
            let window = &closed.window;
            let t = lua.create_table()?;
            t.set("id", window.window.0)?;
            t.set("title", window.title.as_deref())?;
            t.set("app_id", window.app_id.as_deref())?;
            t.set("x", window.position.0)?;
            t.set("y", window.position.1)?;
            if let Some((width, height)) = window.size {
                t.set("width", width)?;
                t.set("height", height)?;
            }
            t.set("closed_at", closed.closed_at)?;
            list.push(t)?;
        }
        Ok(list)
    });

    // Lets go of a closed window that is kept. Returns whether there was one.
    {
        let state = Rc::clone(shared);
        let gpu = Rc::clone(gpu);
        hz.set(
            "forget_closed",
            lua.create_function(move |_, window: u64| {
                let mut shared = state.borrow_mut();
                let index = shared
                    .closed
                    .iter()
                    .position(|c| c.window.window == WindowId(window));
                let Some(index) = index else {
                    return Ok(false);
                };
                let closed = shared.closed.remove(index);
                closed.release(&mut shared, &mut gpu.borrow_mut());
                Ok(true)
            })?,
        )?;
    }

    // ----- color --------------------------------------------------------------
    //
    // The core supplies the numbers; the script's shaders apply them. A
    // script chooses its working space by naming its primaries.

    // How to bring a surface's values into linear light in the working
    // primaries: a row-major 3x3 matrix, the transfer function and how alpha
    // is stored. `color` is the `color` field of a surface.
    def!(
        "decode_transform",
        |lua, shared, (color, working): (u32, Value)| {
            let (description, alpha) = *shared
                .borrow()
                .colors
                .get(color as usize)
                .ok_or_else(|| arg_error(format!("unknown color {color}")))?;
            let space = CompositeSpace {
                primaries: primaries(&working)?,
            };
            let transform = DecodeTransform::new(&description, alpha, &space)
                .ok_or_else(|| arg_error("degenerate primaries or luminances"))?;
            let t = lua.create_table()?;
            t.set("matrix", matrix_table(lua, &transform.matrix)?)?;
            t.set("transfer_id", transform.transfer.shader_id())?;
            // For HLG, the system gamma of the OOTF.
            t.set("transfer_param", transform.transfer_param)?;
            // For HLG: the luminance of the decoded scene light, which the
            // OOTF scales by. Zero otherwise.
            let coefficients = transform.ootf.map_or([0.0; 3], |o| o.coefficients);
            t.set("ootf_coefficients", lua.create_sequence_from(coefficients)?)?;
            t.set("alpha_mode", transform.alpha_mode.shader_id())?;
            Ok(t)
        }
    );

    // How to bring linear light in the working primaries to the encoded
    // values of an output.
    def!(
        "encode_transform",
        |lua, shared, (working, output): (Value, u64)| {
            let shared = shared.borrow();
            let output = shared
                .world
                .outputs
                .iter()
                .find(|o| o.id.0 == output)
                .ok_or_else(|| arg_error(format!("unknown output {output}")))?;
            let space = CompositeSpace {
                primaries: primaries(&working)?,
            };
            let transform = EncodeTransform::new(&space, &output.image_description)
                .ok_or_else(|| arg_error("degenerate primaries or luminances"))?;
            let t = lua.create_table()?;
            t.set("matrix", matrix_table(lua, &transform.matrix)?)?;
            t.set("transfer_id", transform.transfer.shader_id())?;
            t.set("transfer_param", transform.transfer_param)?;
            t.set("clip", !transform.transfer.is_extended_range())?;
            Ok(t)
        }
    );

    // One color, converted on the CPU: values as a client without color
    // information would store them, to premultiplied linear light in the
    // working primaries. For clear colors.
    def!("decode_color", |lua,
                          shared,
                          (r, g, b, a, working): (
        f32,
        f32,
        f32,
        f32,
        Value
    )| {
        let _ = shared;
        let space = CompositeSpace {
            primaries: primaries(&working)?,
        };
        let transform = DecodeTransform::new(
            &proto_wl::DEFAULT_SURFACE_IMAGE_DESCRIPTION,
            AlphaMode::Straight,
            &space,
        )
        .ok_or_else(|| arg_error("degenerate primaries"))?;
        let [r, g, b, a] = transform.apply([r, g, b, a]);
        Ok((r, g, b, a))
    });

    // The `color` of content without color information that is stored
    // premultiplied, like the built-in cursor.
    def!("default_color", |lua, shared, (): ()| {
        Ok(shared.borrow_mut().color_id(
            proto_wl::DEFAULT_SURFACE_IMAGE_DESCRIPTION,
            AlphaMode::PremultipliedElectrical,
        ))
    });

    // ----- requests -----------------------------------------------------------

    // Draw another frame even if nothing else changes (animations). Asked
    // for from `frame`, it is the next frame of the output.
    def!("request_frame", |lua, shared, (): ()| {
        shared.borrow_mut().requests.push(SceneRequest::Redraw);
        Ok(())
    });

    // Offers the linear compositing buffer for verification captures.
    def!(
        "debug_composite",
        |lua, shared, (image, working): (i64, Value)| {
            shared.borrow_mut().debug_composite =
                Some((Handle::from_bits(image), primaries(&working)?));
            Ok(())
        }
    );

    hz.set(
        "log",
        lua.create_function(|_, message: String| {
            tracing::info!(target: "proto_compositor::script", "{message}");
            Ok(())
        })?,
    )?;

    // Loads another module of the project once and returns what it returns.
    {
        let state = Rc::clone(shared);
        hz.set(
            "include",
            lua.create_function(move |lua, name: String| {
                let modules: Table = lua.named_registry_value(MODULES)?;
                let cached: Value = modules.get(name.as_str())?;
                if !cached.is_nil() {
                    return Ok(cached);
                }
                // A project may use the modules of the built-in project it
                // does not replace.
                let project = state.borrow().project.clone();
                let source = project
                    .read(&name)
                    .or_else(|error| super::Project::Builtin.read(&name).map_err(|_| error))
                    .map_err(arg_error)?;
                let value: Value = lua
                    .load(source.as_str())
                    .set_name(format!("{name}.luau"))
                    .call(())?;
                // A module that returns nothing is still remembered as loaded.
                let stored = if value.is_nil() {
                    Value::Boolean(true)
                } else {
                    value.clone()
                };
                modules.set(name.as_str(), stored.clone())?;
                Ok(stored)
            })?,
        )?;
    }

    super::wm::register(lua, &hz, shared, windows)?;

    lua.globals().set("hz", hz)?;
    Ok(())
}
