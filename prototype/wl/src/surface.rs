//! Per-surface state kept next to Smithay's surface data, and the code that
//! reads committed buffers.

use std::sync::Mutex;

use proto_color::{AlphaMode, ImageDescription};
use smithay::backend::allocator::Buffer as _;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Transform;
use smithay::wayland::compositor::{
    BufferAssignment, Damage, SubsurfaceCachedState, SurfaceAttributes, SurfaceData,
    TraversalAction, with_states, with_surface_tree_upward,
};
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::wayland::shm::with_buffer_contents;
use smithay::wayland::viewporter::ViewportCachedState;

use crate::{
    BufferContents, BufferKind, BufferSource, BufferTransform, BufferUpdate,
    DEFAULT_SURFACE_IMAGE_DESCRIPTION, DamageRect, DmabufContents, DmabufPlane, ShmFormat,
    SurfaceId, SurfaceNode, SurfaceRole,
};

/// Role name Smithay gives to subsurfaces.
const SUBSURFACE_ROLE: &str = "subsurface";

/// A reference to a client buffer. The buffer is released (the client is
/// told it may reuse it) when this is dropped.
///
/// The compositor keeps one for as long as it may still read the buffer:
/// from the commit until the renderer is done with the contents. For
/// `wl_shm` that is the upload; for dmabuf, later, it will be for as long as
/// the buffer is on screen.
pub(crate) struct HeldBuffer(WlBuffer);

impl Drop for HeldBuffer {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// hizukuri's own state of a surface, stored in the surface's data map.
pub(crate) struct SurfaceState {
    pub id: SurfaceId,
    /// Size of the committed buffer. `None` when the surface has no
    /// contents.
    pub buffer: Option<(u32, u32)>,
    pub buffer_kind: Option<BufferKind>,
    /// The committed buffer, held until the renderer takes its contents.
    pub pending: Option<HeldBuffer>,
    /// Incremented every time the contents change.
    pub generation: u64,
    /// What the client marked as changed since the contents were last taken.
    pub damage: Vec<DamageRect>,
    pub image_description: ImageDescription,
    pub alpha_mode: AlphaMode,
    /// The transform the contents that were last handed out had.
    taken_transform: BufferTransform,
    warned_transform: bool,
}

impl SurfaceState {
    fn new(id: SurfaceId) -> Self {
        Self {
            id,
            buffer: None,
            buffer_kind: None,
            pending: None,
            generation: 0,
            damage: Vec::new(),
            image_description: DEFAULT_SURFACE_IMAGE_DESCRIPTION,
            alpha_mode: AlphaMode::PremultipliedElectrical,
            taken_transform: BufferTransform::Normal,
            warned_transform: false,
        }
    }
}

/// Attaches hizukuri's state to a new surface.
pub(crate) fn init_surface(surface: &WlSurface, id: SurfaceId) {
    with_states(surface, |states| {
        states
            .data_map
            .insert_if_missing_threadsafe(|| Mutex::new(SurfaceState::new(id)));
    });
}

/// Runs `f` with hizukuri's state of a surface. Returns `None` for surfaces
/// that were not initialized (which should not happen).
pub(crate) fn with_surface_state<T>(
    states: &SurfaceData,
    f: impl FnOnce(&mut SurfaceState) -> T,
) -> Option<T> {
    let state = states.data_map.get::<Mutex<SurfaceState>>()?;
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    Some(f(&mut guard))
}

pub(crate) fn surface_id(surface: &WlSurface) -> Option<SurfaceId> {
    with_states(surface, |states| with_surface_state(states, |s| s.id))
}

/// Records the newly committed buffers of a surface and of the subsurfaces
/// whose state was applied with it.
///
/// The pixels are not read here. The buffer is held, unread, until the
/// renderer asks for the contents ([`take_buffer`]); a buffer that is
/// replaced before that is released without ever being read.
pub(crate) fn process_commit(surface: &WlSurface) {
    with_surface_tree_upward(
        surface,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |_, states, _| {
            let (assignment, damage) = {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                let current = attributes.current();
                (current.buffer.take(), std::mem::take(&mut current.damage))
            };
            let Some(assignment) = assignment else {
                return;
            };
            with_surface_state(states, |state| {
                state.damage.extend(damage.iter().map(|d| match d {
                    Damage::Surface(r) => DamageRect::Surface {
                        x: r.loc.x,
                        y: r.loc.y,
                        width: r.size.w,
                        height: r.size.h,
                    },
                    Damage::Buffer(r) => DamageRect::Buffer {
                        x: r.loc.x,
                        y: r.loc.y,
                        width: r.size.w,
                        height: r.size.h,
                    },
                }));
                match assignment {
                    BufferAssignment::Removed => {
                        state.buffer = None;
                        state.buffer_kind = None;
                        state.pending = None;
                    }
                    BufferAssignment::NewBuffer(buffer) => {
                        // Dropping the previous `pending` releases a buffer
                        // that was superseded before it was shown.
                        let held = HeldBuffer(buffer);
                        // A dmabuf was validated when the client created
                        // it; a wl_shm buffer is checked here.
                        let (size, kind) = match get_dmabuf(&held.0) {
                            Ok(dmabuf) => {
                                let size = dmabuf.size();
                                (Some((size.w as u32, size.h as u32)), BufferKind::Dmabuf)
                            }
                            Err(_) => (shm_buffer_size(&held.0), BufferKind::Shm),
                        };
                        state.buffer_kind = size.map(|_| kind);
                        match size {
                            Some(size) => {
                                state.buffer = Some(size);
                                state.pending = Some(held);
                            }
                            None => {
                                // Not a buffer in a format we take. Show
                                // nothing rather than stale pixels.
                                state.buffer = None;
                                state.pending = None;
                            }
                        }
                    }
                }
                state.generation += 1;
            });
        },
        |_, _, _| true,
    );
}

/// Takes the buffer a surface committed since the last call. `wl_shm`
/// pixels are read now; a dmabuf is described, with duplicates of its file
/// descriptors. The buffer is released when the returned value is dropped.
pub(crate) fn take_buffer(surface: &WlSurface) -> Option<BufferUpdate> {
    with_states(surface, |states| {
        let transform = buffer_transform(states);
        with_surface_state(states, |state| {
            let held = state.pending.take()?;
            let damage = std::mem::take(&mut state.damage);
            let (source, dmabuf) = match get_dmabuf(&held.0) {
                Ok(dmabuf) => (
                    describe_dmabuf(dmabuf).map(BufferSource::Dmabuf),
                    Some(dmabuf.clone()),
                ),
                Err(_) => (read_shm_buffer(&held.0).map(BufferSource::Shm), None),
            };
            match source {
                Some(source) => {
                    state.taken_transform = transform;
                    Some(BufferUpdate {
                        source,
                        transform,
                        damage,
                        dmabuf,
                        _buffer: held,
                    })
                }
                None => {
                    // The pool shrank or vanished since the commit, or the
                    // file descriptors could not be duplicated.
                    state.buffer = None;
                    state.generation += 1;
                    None
                }
            }
        })
    })
    .flatten()
}

fn describe_dmabuf(dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf) -> Option<DmabufContents> {
    let size = dmabuf.size();
    let format = dmabuf.format();
    let mut planes = Vec::new();
    for ((fd, offset), stride) in dmabuf.handles().zip(dmabuf.offsets()).zip(dmabuf.strides()) {
        match fd.try_clone_to_owned() {
            Ok(fd) => planes.push(DmabufPlane { fd, offset, stride }),
            Err(error) => {
                tracing::warn!(%error, "failed to duplicate a dmabuf file descriptor");
                return None;
            }
        }
    }
    Some(DmabufContents {
        width: size.w as u32,
        height: size.h as u32,
        fourcc: format.code as u32,
        // As the client gave it. (`Dmabuf::has_modifier` is also false for
        // an explicit `DRM_FORMAT_MOD_LINEAR`, which an importer must be
        // able to tell from "no modifier".)
        modifier: u64::from(format.modifier),
        planes,
        y_inverted: dmabuf.y_inverted(),
    })
}

/// The committed buffer transform of a surface.
fn buffer_transform(states: &SurfaceData) -> BufferTransform {
    let transform = states
        .cached_state
        .get::<SurfaceAttributes>()
        .current()
        .buffer_transform;
    match Transform::from(transform) {
        Transform::Normal => BufferTransform::Normal,
        Transform::_90 => BufferTransform::Rotate90,
        Transform::_180 => BufferTransform::Rotate180,
        Transform::_270 => BufferTransform::Rotate270,
        Transform::Flipped => BufferTransform::Flipped,
        Transform::Flipped90 => BufferTransform::Flipped90,
        Transform::Flipped180 => BufferTransform::Flipped180,
        Transform::Flipped270 => BufferTransform::Flipped270,
    }
}

fn shm_format(format: wl_shm::Format) -> Option<ShmFormat> {
    Some(match format {
        wl_shm::Format::Argb8888 => ShmFormat::Argb8888,
        wl_shm::Format::Xrgb8888 => ShmFormat::Xrgb8888,
        wl_shm::Format::Abgr8888 => ShmFormat::Abgr8888,
        wl_shm::Format::Xbgr8888 => ShmFormat::Xbgr8888,
        _ => return None,
    })
}

/// Validates a buffer as a usable `wl_shm` buffer and returns its size.
fn shm_buffer_size(buffer: &WlBuffer) -> Option<(u32, u32)> {
    let result = with_buffer_contents(buffer, |_, len, data| {
        if shm_format(data.format).is_none() {
            tracing::warn!(format = ?data.format, "unsupported wl_shm format");
            return None;
        }
        if data.width <= 0 || data.height <= 0 || data.stride < data.width * 4 || data.offset < 0 {
            tracing::warn!(?data, "invalid wl_shm buffer geometry");
            return None;
        }
        let size = data.stride as usize * data.height as usize;
        if (data.offset as usize)
            .checked_add(size)
            .is_none_or(|end| end > len)
        {
            tracing::warn!(?data, len, "wl_shm buffer exceeds its pool");
            return None;
        }
        Some((data.width as u32, data.height as u32))
    });
    match result {
        Ok(size) => size,
        Err(error) => {
            tracing::warn!(?error, "buffer is not a readable wl_shm buffer");
            None
        }
    }
}

fn read_shm_buffer(buffer: &WlBuffer) -> Option<BufferContents> {
    // The geometry was validated at commit time, but the client can resize
    // or destroy the pool afterwards, so everything is checked again.
    shm_buffer_size(buffer)?;
    with_buffer_contents(buffer, |ptr, len, data| {
        let format = shm_format(data.format)?;
        let (offset, stride, height) = (
            data.offset as usize,
            data.stride as usize,
            data.height as usize,
        );
        let size = stride * height;
        if offset.checked_add(size).is_none_or(|end| end > len) {
            return None;
        }
        let mut bytes = vec![0u8; size];
        // SAFETY: `ptr` is valid for `len` bytes while this closure runs and
        // `offset + size <= len` was checked. The client may write to the
        // memory concurrently, so the bytes are copied with a raw copy
        // without ever forming a reference to the shared memory.
        unsafe { std::ptr::copy_nonoverlapping(ptr.add(offset), bytes.as_mut_ptr(), size) };
        Some(BufferContents {
            width: data.width as u32,
            height: data.height as u32,
            stride: data.stride as u32,
            format,
            data: bytes,
        })
    })
    .ok()
    .flatten()
}

/// Geometry of a surface derived from its buffer, scale and viewport.
struct SurfaceGeometry {
    size: (i32, i32),
    source: Option<(f64, f64, f64, f64)>,
    buffer_scale: i32,
    transform: BufferTransform,
}

fn surface_geometry(states: &SurfaceData, state: &mut SurfaceState) -> SurfaceGeometry {
    let scale = states
        .cached_state
        .get::<SurfaceAttributes>()
        .current()
        .buffer_scale
        .max(1);
    let transform = buffer_transform(states);
    let Some(buffer) = state.buffer else {
        return SurfaceGeometry {
            size: (0, 0),
            source: None,
            buffer_scale: scale,
            transform,
        };
    };
    // Everything below is about the upright image.
    let (buffer_w, buffer_h) = transform.upright_size(buffer);
    if state.pending.is_none() && transform != state.taken_transform && !state.warned_transform {
        // The contents were handed out upright for the old transform and
        // the buffer is not ours to read again. Clients send a new buffer
        // along with a new transform, so this does not come up in practice.
        tracing::warn!(
            surface = ?state.id,
            "the buffer transform changed without new contents; the old contents stay as they are"
        );
        state.warned_transform = true;
    }

    let viewport = {
        let mut guard = states.cached_state.get::<ViewportCachedState>();
        let current = guard.current();
        (current.src, current.dst)
    };
    let buffer_logical = (
        (buffer_w as f64 / scale as f64),
        (buffer_h as f64 / scale as f64),
    );
    // wp_viewport source rectangles are in surface coordinates before
    // scaling to the destination; buffer pixels are `scale` times that.
    let source = match viewport.0 {
        Some(src) => (
            src.loc.x * scale as f64,
            src.loc.y * scale as f64,
            src.size.w * scale as f64,
            src.size.h * scale as f64,
        ),
        None => (0.0, 0.0, buffer_w as f64, buffer_h as f64),
    };
    let size = match (viewport.1, viewport.0) {
        (Some(dst), _) => (dst.w, dst.h),
        (None, Some(src)) => (src.size.w.round() as i32, src.size.h.round() as i32),
        (None, None) => (
            buffer_logical.0.ceil() as i32,
            buffer_logical.1.ceil() as i32,
        ),
    };
    SurfaceGeometry {
        size,
        source: Some(source),
        buffer_scale: scale,
        transform,
    }
}

/// Appends the surfaces of the tree rooted at `root` to `out`, back to
/// front. `origin` is the position of the root surface.
pub(crate) fn collect_tree(
    root: &WlSurface,
    origin: (i32, i32),
    root_role: SurfaceRole,
    out: &mut Vec<SurfaceNode>,
) {
    let position = |states: &SurfaceData, parent: &(i32, i32)| -> (i32, i32) {
        if states.role == Some(SUBSURFACE_ROLE) {
            let location = states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location;
            (parent.0 + location.x, parent.1 + location.y)
        } else {
            *parent
        }
    };
    with_surface_tree_upward(
        root,
        origin,
        |_, states, parent| TraversalAction::DoChildren(position(states, parent)),
        |_, states, parent| {
            let offset = position(states, parent);
            let role = if states.role == Some(SUBSURFACE_ROLE) {
                SurfaceRole::Subsurface
            } else {
                root_role
            };
            let node = with_surface_state(states, |state| {
                let geometry = surface_geometry(states, state);
                SurfaceNode {
                    surface: state.id,
                    role,
                    offset,
                    size: geometry.size,
                    buffer_size: state.buffer,
                    buffer_kind: state.buffer.and(state.buffer_kind),
                    transform: geometry.transform,
                    source: geometry.source,
                    buffer_scale: geometry.buffer_scale,
                    image_description: state.image_description,
                    alpha_mode: state.alpha_mode,
                    generation: state.generation,
                }
            });
            if let Some(node) = node {
                out.push(node);
            }
        },
        |_, _, _| true,
    );
}

/// The logical size of a single surface.
pub(crate) fn surface_size(surface: &WlSurface) -> (i32, i32) {
    with_states(surface, |states| {
        with_surface_state(states, |state| surface_geometry(states, state).size)
    })
    .unwrap_or((0, 0))
}
