//! Client dma-bufs as textures.
//!
//! A client that renders with the GPU commits a dma-buf per frame. It is
//! imported into the script's device (`gfx_core::Gpu::import_dmabuf`) and
//! shown to the script as the texture of the surface, like an uploaded
//! `wl_shm` buffer, only without a copy.
//!
//! Three things differ from uploaded pixels, and all three are handled here
//! so that a script does not have to know:
//!
//! - **The buffer stays the client's.** The [`BufferUpdate`] is kept for as
//!   long as frames may sample the import; dropping it tells the client it
//!   may draw into the buffer again. A replaced update is dropped once the
//!   GPU has finished every frame that could have used it.
//! - **Clients cycle through a few buffers.** Imports are kept per surface
//!   and found again by the identity of the dma-buf (its inode), so a
//!   steady client costs no image creation per frame.
//! - **The client's GPU may still be drawing** when the commit arrives. The
//!   dma-buf is polled for readability, which waits for the writers'
//!   fences. That wait is bounded and blocks the compositor.
//!
//! What cannot be imported (a format or modifier the device does not take,
//! a descriptor that is not a dma-buf) falls back to reading the pixels
//! through a mapping, when that is possible.

use std::collections::{HashMap, HashSet};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use gfx_core::{DmabufDesc, DmabufPlaneDesc, Handle};
use hizukuri_color::{AlphaMode, ImageDescription};
use hizukuri_wl::{BufferUpdate, DamageRect, DmabufContents, SurfaceId};
use serde_json::{Value, json};

use super::{ScriptRuntime, SurfaceTexture};

/// Imports kept per surface. Clients use two to four buffers.
const MAX_IMPORTS: usize = 4;
/// How long a commit may wait for the client's GPU to finish the buffer.
const READY_TIMEOUT: Duration = Duration::from_millis(100);

/// An imported dma-buf of a surface.
pub(crate) struct Import {
    /// Device and inode of the dma-buf.
    key: (u64, u64),
    shape: (u32, u32, u32, u64, u32, u32),
    image: Handle,
    slot: Handle,
    has_alpha: bool,
    last_used: u64,
}

#[derive(Default)]
pub(crate) struct DmabufState {
    imports: HashMap<SurfaceId, Vec<Import>>,
    /// The update whose buffer each surface currently shows.
    held: HashMap<SurfaceId, BufferUpdate>,
    /// Replaced updates, with the serial of the last GPU frame that may
    /// have sampled their buffer.
    retired: Vec<(BufferUpdate, u64)>,
    /// Surfaces whose buffers could not be imported (said once each).
    warned: HashSet<SurfaceId>,
    clock: u64,
    imported: u64,
    reused: u64,
    fallbacks: u64,
    waits: u64,
    wait_timeouts: u64,
    waited: Duration,
}

impl DmabufState {
    pub(crate) fn describe(&self) -> Value {
        json!({
            "imported": self.imported,
            "reused": self.reused,
            "fallbacks": self.fallbacks,
            "live_imports": self.imports.values().map(Vec::len).sum::<usize>(),
            "held_buffers": self.held.len(),
            "retired_buffers": self.retired.len(),
            "waits": self.waits,
            "wait_timeouts": self.wait_timeouts,
            "waited_ms": self.waited.as_secs_f64() * 1000.0,
        })
    }
}

impl ScriptRuntime {
    /// Shows a dma-buf as the texture of a surface. On success the caller
    /// hands the update to [`Self::hold_buffer`].
    pub(crate) fn import_surface(
        &self,
        surface: SurfaceId,
        contents: &DmabufContents,
        update: &BufferUpdate,
        image_description: ImageDescription,
        alpha_mode: AlphaMode,
    ) -> Result<(), String> {
        let [plane] = contents.planes.as_slice() else {
            return Err(format!("{} planes", contents.planes.len()));
        };
        if !self
            .gpu()
            .dmabuf_import_formats()
            .iter()
            .any(|f| f.fourcc == contents.fourcc && f.modifier == contents.modifier)
        {
            return Err(format!(
                "format {:#x} with modifier {:#x} is not importable",
                contents.fourcc, contents.modifier
            ));
        }
        let stat = rustix::fs::fstat(&plane.fd).map_err(|e| format!("fstat: {e}"))?;
        let key = (stat.st_dev, stat.st_ino);
        let shape = (
            contents.width,
            contents.height,
            contents.fourcc,
            contents.modifier,
            plane.offset,
            plane.stride,
        );

        // Wait for whoever is still writing. A dma-buf is readable when no
        // write fence is pending.
        {
            let started = Instant::now();
            let mut fds = [rustix::event::PollFd::new(
                &plane.fd,
                rustix::event::PollFlags::IN,
            )];
            let timeout = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: READY_TIMEOUT.subsec_nanos() as _,
            };
            let ready = rustix::event::poll(&mut fds, Some(&timeout));
            let waited = started.elapsed();
            let mut shared = self.shared.borrow_mut();
            if waited > Duration::from_micros(200) {
                shared.dmabuf.waits += 1;
                shared.dmabuf.waited += waited;
            }
            if matches!(ready, Ok(0)) {
                shared.dmabuf.wait_timeouts += 1;
            }
        }

        self.ensure_frame().map_err(|e| e.to_string())?;
        let known = {
            let mut shared = self.shared.borrow_mut();
            let state = &mut shared.dmabuf;
            state.clock += 1;
            let clock = state.clock;
            state
                .imports
                .get_mut(&surface)
                .and_then(|imports| {
                    imports
                        .iter_mut()
                        .find(|i| i.key == key && i.shape == shape)
                })
                .map(|import| {
                    import.last_used = clock;
                    (import.image, import.slot, import.has_alpha)
                })
        };
        let (image, slot, has_alpha) = match known {
            Some((image, slot, has_alpha)) => {
                self.gpu()
                    .dmabuf_contents_changed(image)
                    .map_err(|e| e.to_string())?;
                self.shared.borrow_mut().dmabuf.reused += 1;
                (image, slot, has_alpha)
            }
            None => {
                let mut gpu = self.gpu();
                let imported = gpu
                    .import_dmabuf(&DmabufDesc {
                        width: contents.width,
                        height: contents.height,
                        fourcc: contents.fourcc,
                        modifier: contents.modifier,
                        planes: &[DmabufPlaneDesc {
                            fd: plane.fd.as_fd(),
                            offset: plane.offset,
                            stride: plane.stride,
                        }],
                    })
                    .map_err(|e| e.to_string())?;
                let slot = if imported.has_alpha {
                    gpu.create_texture_slot(imported.image)
                } else {
                    gpu.create_opaque_texture_slot(imported.image)
                };
                let slot = match slot {
                    Ok(slot) => slot,
                    Err(e) => {
                        let _ = gpu.destroy(imported.image);
                        return Err(e.to_string());
                    }
                };
                gpu.protect(imported.image);
                gpu.protect(slot);
                let mut shared = self.shared.borrow_mut();
                let state = &mut shared.dmabuf;
                state.imported += 1;
                let clock = state.clock;
                let imports = state.imports.entry(surface).or_default();
                imports.push(Import {
                    key,
                    shape,
                    image: imported.image,
                    slot,
                    has_alpha: imported.has_alpha,
                    last_used: clock,
                });
                // Forget the buffer that was not used for the longest time.
                // Frames in flight keep their objects alive.
                while imports.len() > MAX_IMPORTS {
                    let oldest = imports
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, i)| i.last_used)
                        .map(|(index, _)| index)
                        .expect("not empty");
                    let old = imports.swap_remove(oldest);
                    let _ = gpu.destroy(old.slot);
                    let _ = gpu.destroy(old.image);
                }
                (imported.image, slot, imported.has_alpha)
            }
        };

        let format = self
            .gpu()
            .image_info(image)
            .map_err(|e| e.to_string())?
            .format;
        // What was there before, if it was an upload, is gone.
        let previous = self.shared.borrow_mut().textures.remove(&surface);
        let (generation, serial) = match previous {
            Some(old) => {
                if !old.imported {
                    let mut gpu = self.gpu();
                    let _ = gpu.destroy(old.slot);
                    let _ = gpu.destroy(old.image);
                }
                // A different image is a new texture for the script.
                (
                    old.generation + u64::from(old.image != image),
                    old.serial + 1,
                )
            }
            None => (1, 1),
        };
        let mut shared = self.shared.borrow_mut();
        let color = shared.color_id(image_description, alpha_mode);
        shared.textures.insert(
            surface,
            SurfaceTexture {
                image,
                slot,
                width: contents.width,
                height: contents.height,
                format,
                opaque: !has_alpha,
                generation,
                serial,
                // The texture is the client's buffer as it is, so damage in
                // buffer coordinates applies unchanged.
                damage: update
                    .damage
                    .iter()
                    .map(|d| match *d {
                        DamageRect::Buffer {
                            x,
                            y,
                            width,
                            height,
                        } => Some([x, y, width, height]),
                        DamageRect::Surface { .. } => None,
                    })
                    .collect::<Option<Vec<_>>>()
                    .unwrap_or_default(),
                color,
                imported: true,
                transform: update.transform,
                y_inverted: contents.y_inverted,
            },
        );
        Ok(())
    }

    /// Keeps the update whose buffer a surface now shows; the one it showed
    /// before is released once no frame can still sample it.
    pub(crate) fn hold_buffer(&self, surface: SurfaceId, update: BufferUpdate) {
        let serial = self.gpu().current_serial();
        let mut shared = self.shared.borrow_mut();
        if let Some(old) = shared.dmabuf.held.insert(surface, update) {
            shared.dmabuf.retired.push((old, serial));
        }
    }

    /// The surface shows no dma-buf any more: its imports go, and its
    /// buffer is released once no frame can still sample it.
    pub(crate) fn release_surface_buffers(&self, surface: SurfaceId) {
        let serial = self.gpu().current_serial();
        let imports = {
            let mut shared = self.shared.borrow_mut();
            let state = &mut shared.dmabuf;
            if let Some(old) = state.held.remove(&surface) {
                state.retired.push((old, serial));
            }
            state.warned.remove(&surface);
            state.imports.remove(&surface).unwrap_or_default()
        };
        let mut gpu = self.gpu();
        for import in imports {
            let _ = gpu.destroy(import.slot);
            let _ = gpu.destroy(import.image);
        }
    }

    /// Releases the buffers no frame in flight can still sample.
    pub(crate) fn collect_retired_buffers(&self) {
        if self.shared.borrow().dmabuf.retired.is_empty() {
            return;
        }
        let completed = self.gpu().completed_serial();
        self.shared
            .borrow_mut()
            .dmabuf
            .retired
            .retain(|(_, serial)| *serial > completed);
    }

    /// Notes that a dma-buf was read through a mapping instead. Returns
    /// whether this is the first time for the surface.
    pub(crate) fn note_dmabuf_fallback(&self, surface: SurfaceId) -> bool {
        let mut shared = self.shared.borrow_mut();
        shared.dmabuf.fallbacks += 1;
        shared.dmabuf.warned.insert(surface)
    }
}
