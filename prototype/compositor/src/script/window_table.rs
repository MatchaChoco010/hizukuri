//! `hz.window_table()`: the numbers to read every window surface with, in
//! one buffer the core fills in.
//!
//! The Slang module `color` reads a window through `WindowParams`. A script
//! can fill them in itself (the built-in `color` module does, into push data
//! or a buffer of its own). Here the core does it instead: one storage buffer
//! holds the `WindowParams` of every surface there is, a shader declares it
//! as `StructuredBuffer<WindowParams>`, and a script passes on nothing but
//! an index (`params` of a surface).
//!
//! The table is made when a script asks for it, once per frame: the indices
//! `hz.surfaces` hands out are those of the table asked for last in the
//! same frame.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Lua, Table};
use proto_color::{CompositeSpace, DecodeTransform};
use proto_gfx_core::{BufferDesc, BufferUsage, GfxError, Gpu, Handle};
use proto_wl::SurfaceId;

use super::api::{SharedRef, arg_error, uv_transform};
use super::{Shared, SurfaceTexture};
use crate::scene::CursorImage;

/// The size of one `WindowParams` of the Slang module `color`.
pub(crate) const PARAMS_SIZE: usize = 96;

/// Entries the buffer is made for at first; it grows by doubling.
const FIRST_CAPACITY: usize = 64;

#[derive(Default)]
pub(crate) struct WindowTable {
    /// The buffer, its storage buffer slot, and how many entries it holds.
    buffer: Option<(Handle, Handle, usize)>,
    /// Where each surface is in the table made last.
    index: HashMap<SurfaceId, u32>,
    /// Whether the table was made for the frame being drawn.
    fresh: bool,
}

impl WindowTable {
    /// Forgets the indices: a new frame starts, or nothing is being drawn.
    pub(crate) fn expire(&mut self) {
        self.fresh = false;
        self.index.clear();
    }

    /// The index of a surface in the table of this frame.
    pub(crate) fn index_of(&self, surface: SurfaceId) -> Option<u32> {
        self.index.get(&surface).copied().filter(|_| self.fresh)
    }
}

/// One `WindowParams`: where the surface lies in its texture, and how its
/// values become premultiplied linear light in the working space.
pub(crate) fn params(uv: &[f64; 6], transform: &DecodeTransform) -> [u8; PARAMS_SIZE] {
    let mut out = [0u8; PARAMS_SIZE];
    let mut at = 0;
    let mut put = |bytes: [u8; 4]| {
        out[at..at + 4].copy_from_slice(&bytes);
        at += 4;
    };
    let float = |v: f64| (v as f32).to_ne_bytes();
    for value in [uv[0], uv[1], uv[4], 0.0, uv[2], uv[3], uv[5], 0.0] {
        put(float(value));
    }
    let ootf = transform.ootf.map_or([0.0; 3], |o| o.coefficients);
    for (row, coefficient) in transform.matrix.iter().zip(ootf) {
        for value in row {
            put(value.to_ne_bytes());
        }
        put(coefficient.to_ne_bytes());
    }
    put(transform.transfer_param.to_ne_bytes());
    put(0f32.to_ne_bytes());
    put(transform.transfer.shader_id().to_ne_bytes());
    put(transform.alpha_mode.shader_id().to_ne_bytes());
    out
}

/// A surface, the part of its buffer that is shown (x, y, width, height in
/// buffer pixels), and its texture.
type Shown<'a> = (SurfaceId, (f64, f64, f64, f64), &'a SurfaceTexture);

/// Every surface a script can draw: those of the windows, of what follows
/// the pointer, and of the closed windows that are kept.
fn surfaces(shared: &Shared) -> Vec<Shown<'_>> {
    let mut out = Vec::new();
    for window in &shared.world.windows {
        for node in &window.surfaces {
            if let (Some(source), Some(texture)) = (node.source, shared.textures.get(&node.surface))
            {
                out.push((node.surface, source, texture));
            }
        }
    }
    let pointer =
        shared
            .world
            .drag_icon
            .iter()
            .chain(match shared.world.cursor.as_ref().map(|c| &c.image) {
                Some(CursorImage::Surfaces(surfaces)) => surfaces.as_slice(),
                _ => &[],
            });
    for view in pointer {
        if let (Some(source), Some(texture)) = (view.source, shared.textures.get(&view.surface)) {
            out.push((view.surface, source, texture));
        }
    }
    for closed in &shared.closed {
        for node in &closed.window.surfaces {
            if let (Some(source), Some(texture)) = (node.source, closed.textures.get(&node.surface))
            {
                out.push((node.surface, source, texture));
            }
        }
    }
    out
}

/// Makes the table for the frame being drawn and returns its slot.
fn rebuild(shared: &mut Shared, gpu: &mut Gpu) -> Result<Handle, GfxError> {
    let space = CompositeSpace {
        primaries: shared.working,
    };
    let mut bytes = Vec::new();
    let mut index = HashMap::new();
    for (surface, source, texture) in surfaces(shared) {
        let Some((description, alpha)) = shared.colors.get(texture.color as usize) else {
            continue;
        };
        let Some(transform) = DecodeTransform::new(description, *alpha, &space) else {
            continue;
        };
        index.insert(surface, (bytes.len() / PARAMS_SIZE) as u32);
        bytes.extend_from_slice(&params(&uv_transform(texture, source), &transform));
    }
    let entries = bytes.len() / PARAMS_SIZE;
    let (buffer, slot) = match shared.window_table.buffer {
        Some((buffer, slot, capacity)) if capacity >= entries => (buffer, slot),
        previous => {
            let capacity = entries.next_power_of_two().max(FIRST_CAPACITY);
            let size = (capacity * PARAMS_SIZE) as u64;
            let buffer = gpu.create_buffer(&BufferDesc {
                size,
                usage: BufferUsage::STORAGE,
            })?;
            let slot = match gpu.create_storage_buffer_slot(buffer, 0, size) {
                Ok(slot) => slot,
                Err(error) => {
                    let _ = gpu.destroy(buffer);
                    return Err(error);
                }
            };
            // The core's: a script cannot destroy or overwrite them, and
            // they outlive a reload.
            gpu.protect(buffer);
            gpu.protect(slot);
            if let Some((old_buffer, old_slot, _)) = previous {
                let _ = gpu.destroy(old_slot);
                let _ = gpu.destroy(old_buffer);
            }
            shared.window_table.buffer = Some((buffer, slot, capacity));
            (buffer, slot)
        }
    };
    if !bytes.is_empty() {
        gpu.write_buffer(buffer, 0, &bytes)?;
    }
    shared.window_table.index = index;
    shared.window_table.fresh = true;
    Ok(slot)
}

pub(super) fn register(
    lua: &Lua,
    hz: &Table,
    shared: &SharedRef,
    gpu: &Rc<RefCell<Gpu>>,
) -> mlua::Result<()> {
    // The slot of a storage buffer with the `WindowParams` of every surface,
    // for a shader's `StructuredBuffer<WindowParams>`. Asked for in `frame`,
    // before `hz.surfaces`: the surfaces listed afterwards carry their index
    // into it as `params`. The brightness of a window is not in the table;
    // a shader that wants one scales what it reads.
    let state = Rc::clone(shared);
    let gpu = Rc::clone(gpu);
    hz.set(
        "window_table",
        lua.create_function(move |_, (): ()| {
            let mut shared = state.borrow_mut();
            if !shared.in_frame {
                return Err(arg_error(
                    "hz.window_table is for `frame`: the table is made for the frame being drawn",
                ));
            }
            let slot =
                rebuild(&mut shared, &mut gpu.borrow_mut()).map_err(mlua::Error::external)?;
            Ok(slot.to_bits())
        })?,
    )?;
    Ok(())
}
