//! A DRM/KMS device that exists only in memory.
//!
//! [`MockDevice`] implements [`KmsDevice`] and checks commits the way the
//! kernel does, so the KMS backend can be exercised without hardware, a VT
//! or a VM. It is built from a [`DeviceSnapshot`]: one made up with
//! [`SnapshotBuilder`], or one captured from a real machine
//! (`hizukuri kms-info`).
//!
//! Rules it enforces (with the errno the kernel uses):
//!
//! - Commits need DRM master (`EACCES` while the session is paused).
//! - A property must belong to the object, must not be immutable, and its
//!   value must be valid for its type (`ENOENT`, `EINVAL`).
//! - A CRTC is active only with a mode; a mode needs a connector and a
//!   connector on a CRTC needs a mode (`EINVAL`).
//! - A plane has a framebuffer exactly when it has a CRTC; the CRTC must be
//!   one the plane can be on; the framebuffer's format (and modifier) must
//!   be one the plane takes; the source rectangle must lie inside the
//!   framebuffer (`EINVAL`, `ENOSPC`).
//! - A primary plane covers its CRTC exactly, unscaled (`EINVAL`, `ERANGE`).
//! - Changing a mode, the active state or the routing needs
//!   `allow_modeset` (`EINVAL`).
//! - A non-blocking commit on a CRTC whose previous flip has not completed
//!   fails (`EBUSY`).
//! - An asynchronous (tearing) flip needs driver support and may only
//!   change the framebuffer of a primary plane, to one of the same format
//!   and size (`EINVAL`).
//! - Page flip events arrive at the next vblank ([`MockDevice::vblank`]),
//!   or at once for blocking commits and asynchronous flips.
//!
//! The memory behind framebuffers is kept (dumb buffers) or mapped
//! (dma-bufs, where the exporter allows mapping), so a test can look at
//! what is "on screen" ([`MockDevice::displayed`]).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::rc::Rc;
use std::time::Duration;

use super::clock::{Clock, ManualClock};
use super::device::{DeviceEvent, DmabufDescriptor, DumbBuffer, KmsDevice};
use super::model::{
    AtomicRequest, CommitFlags, Connector, Crtc, DeviceCaps, DeviceSnapshot, DriverInfo, Encoder,
    EnumValue, FOURCC_ARGB8888, FOURCC_XRGB8888, FormatModifier, Fourcc, MODIFIER_INVALID,
    MODIFIER_LINEAR, Mode, Plane, PlaneKind, Property, PropertyKind, property,
};

const EACCES: i32 = 13;
const EBUSY: i32 = 16;
const EINVAL: i32 = 22;
const ENOENT: i32 = 2;
const ENOSPC: i32 = 28;
const ERANGE: i32 = 34;
const EOPNOTSUPP: i32 = 95;

fn errno(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

/// Builds made-up device snapshots for tests. Property ids are shared by
/// name across objects, as the kernel does.
pub struct SnapshotBuilder {
    driver: String,
    crtcs: usize,
    connectors: Vec<(String, bool, Vec<Mode>)>,
    modifiers: Option<Vec<u64>>,
    cursor_planes: bool,
    async_flips: bool,
    next_id: u32,
}

impl SnapshotBuilder {
    pub fn new(driver: &str) -> Self {
        Self {
            driver: driver.to_string(),
            crtcs: 1,
            connectors: Vec::new(),
            modifiers: None,
            cursor_planes: true,
            async_flips: false,
            next_id: 30,
        }
    }

    pub fn crtcs(mut self, count: usize) -> Self {
        self.crtcs = count;
        self
    }

    /// Adds a connector. Modes are `(width, height, refresh in Hz,
    /// preferred)`.
    pub fn connector(
        mut self,
        name: &str,
        connected: bool,
        modes: &[(u16, u16, u32, bool)],
    ) -> Self {
        let modes = modes
            .iter()
            .map(|(w, h, hz, preferred)| Mode::synthetic(*w, *h, *hz, *preferred))
            .collect();
        self.connectors.push((name.to_string(), connected, modes));
        self
    }

    /// Makes the planes report these modifiers for every format
    /// (`IN_FORMATS`). Without this the device takes no modifiers.
    pub fn modifiers(mut self, modifiers: &[u64]) -> Self {
        self.modifiers = Some(modifiers.to_vec());
        self
    }

    pub fn without_cursor_planes(mut self) -> Self {
        self.cursor_planes = false;
        self
    }

    /// Makes the device support tearing flips through the atomic API.
    pub fn async_flips(mut self) -> Self {
        self.async_flips = true;
        self
    }

    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    pub fn build(mut self) -> DeviceSnapshot {
        // One id per property name, shared between objects.
        let mut ids: HashMap<&'static str, u32> = HashMap::new();
        let mut property_id = 0;
        let mut prop = |name: &'static str, value: u64, kind: PropertyKind, immutable: bool| {
            let id = *ids.entry(name).or_insert_with(|| {
                property_id += 1;
                property_id
            });
            Property {
                id,
                name: name.to_string(),
                value,
                immutable,
                kind,
            }
        };
        let range = |max| PropertyKind::Range { min: 0, max };
        let signed = PropertyKind::SignedRange {
            min: i32::MIN as i64,
            max: i32::MAX as i64,
        };

        let crtcs: Vec<Crtc> = (0..self.crtcs)
            .map(|_| Crtc {
                id: self.id(),
                properties: vec![
                    prop("ACTIVE", 0, range(1), false),
                    prop("MODE_ID", 0, PropertyKind::Blob { data: None }, false),
                    prop("VRR_ENABLED", 0, range(1), false),
                ],
            })
            .collect();
        let all_crtcs = (1u32 << self.crtcs) - 1;
        let encoder = Encoder {
            id: self.id(),
            possible_crtcs: all_crtcs,
        };

        let plane_kinds = || PropertyKind::Enum {
            values: ["Overlay", "Primary", "Cursor"]
                .iter()
                .enumerate()
                .map(|(v, name)| EnumValue {
                    name: name.to_string(),
                    value: v as u64,
                })
                .collect(),
        };
        let mut planes = Vec::new();
        for index in 0..self.crtcs {
            let mut kinds = vec![(PlaneKind::Primary, 1u64, vec![FOURCC_XRGB8888])];
            if self.cursor_planes {
                kinds.push((PlaneKind::Cursor, 2, vec![FOURCC_ARGB8888]));
            }
            for (_, kind_value, formats) in kinds {
                let id = self.id();
                let modifiers = self.modifiers.as_ref().map(|list| {
                    formats
                        .iter()
                        .flat_map(|format| {
                            list.iter().map(|modifier| FormatModifier {
                                format: *format,
                                modifier: *modifier,
                            })
                        })
                        .collect()
                });
                let mut properties = vec![
                    prop("type", kind_value, plane_kinds(), true),
                    prop("FB_ID", 0, PropertyKind::Object, false),
                    prop("CRTC_ID", 0, PropertyKind::Object, false),
                    prop("SRC_X", 0, range(u32::MAX as u64), false),
                    prop("SRC_Y", 0, range(u32::MAX as u64), false),
                    prop("SRC_W", 0, range(u32::MAX as u64), false),
                    prop("SRC_H", 0, range(u32::MAX as u64), false),
                    prop("CRTC_X", 0, signed.clone(), false),
                    prop("CRTC_Y", 0, signed.clone(), false),
                    prop("CRTC_W", 0, range(i32::MAX as u64), false),
                    prop("CRTC_H", 0, range(i32::MAX as u64), false),
                    prop(
                        "IN_FENCE_FD",
                        u64::MAX,
                        PropertyKind::SignedRange {
                            min: -1,
                            max: i32::MAX as i64,
                        },
                        false,
                    ),
                ];
                if self.modifiers.is_some() {
                    properties.push(prop(
                        "IN_FORMATS",
                        0,
                        PropertyKind::Blob { data: None },
                        true,
                    ));
                }
                planes.push(Plane {
                    id,
                    possible_crtcs: 1 << index,
                    formats,
                    modifiers,
                    properties,
                });
            }
        }

        let connectors = std::mem::take(&mut self.connectors)
            .into_iter()
            .map(|(name, connected, modes)| Connector {
                id: self.id(),
                name,
                connected,
                physical_size_mm: (600, 340),
                modes,
                encoders: vec![encoder.id],
                properties: vec![
                    prop("CRTC_ID", 0, PropertyKind::Object, false),
                    prop("EDID", 0, PropertyKind::Blob { data: None }, true),
                    prop(
                        "DPMS",
                        0,
                        PropertyKind::Enum {
                            values: ["On", "Standby", "Suspend", "Off"]
                                .iter()
                                .enumerate()
                                .map(|(v, name)| EnumValue {
                                    name: name.to_string(),
                                    value: v as u64,
                                })
                                .collect(),
                        },
                        false,
                    ),
                ],
            })
            .collect();

        DeviceSnapshot {
            driver: DriverInfo {
                name: self.driver,
                description: "made-up device for tests".to_string(),
                version: "0.0.0".to_string(),
            },
            caps: DeviceCaps {
                dumb_buffer: true,
                addfb2_modifiers: self.modifiers.is_some(),
                async_page_flip: self.async_flips,
                atomic_async_page_flip: self.async_flips,
                cursor_width: 64,
                cursor_height: 64,
                monotonic_timestamp: true,
            },
            connectors,
            encoders: vec![encoder],
            crtcs,
            planes,
        }
    }

    /// Marks a connector as currently routed to a CRTC, as if someone had
    /// been driving it before.
    pub fn route(snapshot: &mut DeviceSnapshot, connector: &str, crtc: u32) {
        let connector = snapshot
            .connectors
            .iter_mut()
            .find(|c| c.name == connector)
            .expect("connector exists");
        let property = connector
            .properties
            .iter_mut()
            .find(|p| p.name == "CRTC_ID")
            .expect("connectors have CRTC_ID");
        property.value = crtc as u64;
    }
}

/// A read-only mapping of a dma-buf.
struct Mapping {
    pointer: *mut std::ffi::c_void,
    length: usize,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly the region mapped in `add_dmabuf_framebuffer`.
        let _ = unsafe { rustix::mm::munmap(self.pointer, self.length) };
    }
}

enum FramebufferMemory {
    /// A framebuffer that was on a plane when the snapshot was taken: it
    /// belongs to whoever drove the display before.
    Foreign,
    Dumb(u32),
    Dmabuf {
        /// Keeps the buffer alive, as the kernel's import does.
        _fd: OwnedFd,
        /// `None` when the exporter does not allow mapping.
        mapping: Option<Mapping>,
        offset: usize,
    },
}

struct Framebuffer {
    width: u32,
    height: u32,
    stride: u32,
    format: Fourcc,
    modifier: u64,
    memory: FramebufferMemory,
}

struct Dumb {
    info: DumbBuffer,
    data: Vec<u8>,
}

/// One commit the mock accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub request: AtomicRequest,
    pub flags: CommitFlags,
}

/// What a CRTC is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Displayed {
    pub framebuffer: u32,
    pub width: u32,
    pub height: u32,
    pub format: Fourcc,
    pub modifier: u64,
    /// Tightly packed rows of 4 bytes per pixel (B, G, R, X for XRGB8888).
    /// `None` when the memory cannot be read (a dma-buf that cannot be
    /// mapped).
    pub pixels: Option<Vec<u8>>,
}

impl Displayed {
    /// The pixel at a position as `[r, g, b]`. Panics when the memory is
    /// not readable.
    pub fn rgb(&self, x: u32, y: u32) -> [u8; 3] {
        let pixels = self
            .pixels
            .as_ref()
            .expect("framebuffer memory is readable");
        let i = (y as usize * self.width as usize + x as usize) * 4;
        [pixels[i + 2], pixels[i + 1], pixels[i]]
    }
}

struct State {
    snapshot: DeviceSnapshot,
    clock: ManualClock,
    master: bool,
    next_id: u32,
    blobs: HashMap<u32, Mode>,
    dumbs: HashMap<u32, Dumb>,
    framebuffers: HashMap<u32, Framebuffer>,
    /// CRTCs with a flip that has not completed, and whether an event was
    /// asked for.
    pending: Vec<(u32, bool)>,
    events: VecDeque<DeviceEvent>,
    sequence: u32,
    commits: Vec<CommitRecord>,
    rejected: Vec<(CommitRecord, i32)>,
    fail_next_commit: Option<i32>,
    accept_dmabufs: bool,
    dumb_stride_alignment: u32,
}

/// A DRM/KMS device in memory. Clones share the device, so a test can keep
/// one to drive vblanks and look at the screen while the backend owns the
/// other.
#[derive(Clone)]
pub struct MockDevice {
    state: Rc<RefCell<State>>,
}

impl MockDevice {
    /// Creates a device with the resources and current state of a snapshot.
    pub fn new(snapshot: DeviceSnapshot, clock: ManualClock) -> Self {
        let next_id = 1000
            + snapshot
                .connectors
                .iter()
                .map(|c| c.id)
                .chain(snapshot.crtcs.iter().map(|c| c.id))
                .chain(snapshot.planes.iter().map(|p| p.id))
                .max()
                .unwrap_or(0);
        // Framebuffers the snapshot's planes refer to exist, as they do on
        // the real device; only their contents are unknown.
        let mut framebuffers = HashMap::new();
        let mut blobs = HashMap::new();
        for plane in &snapshot.planes {
            let value = |name| {
                property(&plane.properties, name)
                    .map(|p| p.value)
                    .unwrap_or(0)
            };
            let id = value("FB_ID") as u32;
            if id != 0 {
                framebuffers.insert(
                    id,
                    Framebuffer {
                        width: (value("SRC_W") >> 16) as u32,
                        height: (value("SRC_H") >> 16) as u32,
                        stride: (value("SRC_W") >> 16) as u32 * 4,
                        format: plane.formats.first().copied().unwrap_or(FOURCC_XRGB8888),
                        modifier: MODIFIER_INVALID,
                        memory: FramebufferMemory::Foreign,
                    },
                );
            }
        }
        // Likewise the modes that CRTCs are running. Which mode a blob
        // holds is not in the snapshot; the connector's first mode stands
        // in for it.
        for crtc in &snapshot.crtcs {
            let blob = property(&crtc.properties, "MODE_ID")
                .map(|p| p.value)
                .unwrap_or(0) as u32;
            if blob == 0 {
                continue;
            }
            let mode = snapshot
                .connectors
                .iter()
                .find(|c| {
                    property(&c.properties, "CRTC_ID").is_some_and(|p| p.value == crtc.id as u64)
                })
                .and_then(|c| c.modes.first().cloned());
            if let Some(mode) = mode {
                blobs.insert(blob, mode);
            }
        }
        Self {
            state: Rc::new(RefCell::new(State {
                snapshot,
                clock,
                master: true,
                next_id,
                blobs,
                dumbs: HashMap::new(),
                framebuffers,
                pending: Vec::new(),
                events: VecDeque::new(),
                sequence: 0,
                commits: Vec::new(),
                rejected: Vec::new(),
                fail_next_commit: None,
                accept_dmabufs: true,
                dumb_stride_alignment: 256,
            })),
        }
    }

    // -- driving the device from a test ---------------------------------------

    /// A vertical blank: time advances by one refresh period and pending
    /// flips complete.
    pub fn vblank(&self) {
        let mut state = self.state.borrow_mut();
        let interval = state.refresh_interval();
        state.clock.advance(interval);
        state.sequence += 1;
        let (time, sequence) = (state.clock.now(), state.sequence);
        for (crtc, wants_event) in std::mem::take(&mut state.pending) {
            if wants_event {
                state.events.push_back(DeviceEvent::PageFlip {
                    crtc,
                    sequence,
                    time,
                });
            }
        }
    }

    /// Takes DRM master away (the session was paused) or gives it back.
    pub fn set_master(&self, master: bool) {
        let mut state = self.state.borrow_mut();
        state.master = master;
        if !master {
            // Flips in flight when the VT goes away never report back.
            state.pending.clear();
        }
    }

    /// Plugs or unplugs a display.
    pub fn set_connected(&self, connector: &str, connected: bool) {
        let mut state = self.state.borrow_mut();
        let connector = state
            .snapshot
            .connectors
            .iter_mut()
            .find(|c| c.name == connector)
            .expect("connector exists");
        connector.connected = connected;
    }

    /// Makes the next commit fail with this errno, whatever it asks for.
    pub fn fail_next_commit(&self, code: i32) {
        self.state.borrow_mut().fail_next_commit = Some(code);
    }

    /// Whether dma-bufs can be imported. A real device refuses buffers it
    /// did not allocate (for example a software rasterizer's).
    pub fn set_accept_dmabufs(&self, accept: bool) {
        self.state.borrow_mut().accept_dmabufs = accept;
    }

    /// Row alignment of dumb buffers in bytes. The default (256) makes the
    /// stride larger than the width for most sizes, as real drivers do.
    pub fn set_dumb_stride_alignment(&self, alignment: u32) {
        self.state.borrow_mut().dumb_stride_alignment = alignment.max(4);
    }

    // -- looking at the device from a test ------------------------------------

    /// The commits the device accepted and applied, oldest first.
    pub fn commits(&self) -> Vec<CommitRecord> {
        self.state.borrow().commits.clone()
    }

    /// The commits the device refused, with the errno.
    pub fn rejected(&self) -> Vec<(CommitRecord, i32)> {
        self.state.borrow().rejected.clone()
    }

    /// The mode a CRTC is running, if it is active.
    pub fn active_mode(&self, crtc: u32) -> Option<Mode> {
        let state = self.state.borrow();
        let crtc = state.snapshot.crtc(crtc)?;
        if property(&crtc.properties, "ACTIVE")?.value == 0 {
            return None;
        }
        let blob = property(&crtc.properties, "MODE_ID")?.value as u32;
        state.blobs.get(&blob).cloned()
    }

    /// The CRTC a connector is routed to.
    pub fn connector_crtc(&self, connector: &str) -> Option<u32> {
        let state = self.state.borrow();
        let connector = state.snapshot.connector_by_name(connector)?;
        Some(property(&connector.properties, "CRTC_ID")?.value as u32).filter(|id| *id != 0)
    }

    /// What the primary plane of a CRTC is showing.
    pub fn displayed(&self, crtc: u32) -> Option<Displayed> {
        let state = self.state.borrow();
        let plane = state.snapshot.planes.iter().find(|plane| {
            plane.kind() == Some(PlaneKind::Primary)
                && property(&plane.properties, "CRTC_ID").is_some_and(|p| p.value == crtc as u64)
        })?;
        let id = property(&plane.properties, "FB_ID")?.value as u32;
        let fb = state.framebuffers.get(&id)?;
        let row = fb.width as usize * 4;
        let stride = fb.stride as usize;
        let pack = |bytes: &[u8]| -> Vec<u8> {
            (0..fb.height as usize)
                .flat_map(|y| bytes[y * stride..y * stride + row].iter().copied())
                .collect()
        };
        let pixels = match &fb.memory {
            FramebufferMemory::Dumb(handle) => state.dumbs.get(handle).map(|d| pack(&d.data)),
            FramebufferMemory::Dmabuf {
                mapping: Some(mapping),
                offset,
                ..
            } => {
                // SAFETY: the mapping covers `length` bytes and lives as
                // long as the framebuffer; the exporter is idle while a
                // test looks at the screen.
                let bytes = unsafe {
                    std::slice::from_raw_parts(mapping.pointer.cast::<u8>(), mapping.length)
                };
                Some(pack(&bytes[*offset..]))
            }
            FramebufferMemory::Dmabuf { mapping: None, .. } | FramebufferMemory::Foreign => None,
        };
        Some(Displayed {
            framebuffer: id,
            width: fb.width,
            height: fb.height,
            format: fb.format,
            modifier: fb.modifier,
            pixels,
        })
    }

    /// Number of framebuffers that exist (to check that none leak).
    pub fn framebuffer_count(&self) -> usize {
        self.state.borrow().framebuffers.len()
    }

    /// Number of dumb buffers that exist.
    pub fn dumb_buffer_count(&self) -> usize {
        self.state.borrow().dumbs.len()
    }

    /// Whether a flip is in flight on a CRTC.
    pub fn flip_pending(&self, crtc: u32) -> bool {
        self.state.borrow().pending.iter().any(|(c, _)| *c == crtc)
    }
}

impl State {
    fn allocate_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    /// Refresh period of the first active CRTC, or 60 Hz.
    fn refresh_interval(&self) -> Duration {
        self.snapshot
            .crtcs
            .iter()
            .filter(|crtc| property(&crtc.properties, "ACTIVE").is_some_and(|p| p.value != 0))
            .filter_map(|crtc| {
                let blob = property(&crtc.properties, "MODE_ID")?.value as u32;
                self.blobs.get(&blob)
            })
            .map(|mode| Duration::from_nanos(1_000_000_000_000 / mode.refresh_mhz().max(1) as u64))
            .next()
            .unwrap_or(Duration::from_nanos(16_666_667))
    }

    /// Checks one property assignment on its own.
    fn check_change(&self, object: u32, property_id: u32, value: u64) -> Result<(), i32> {
        let properties = self.snapshot.object_properties(object).ok_or(ENOENT)?;
        let property = properties
            .iter()
            .find(|p| p.id == property_id)
            .ok_or(ENOENT)?;
        if property.immutable {
            return Err(EINVAL);
        }
        match &property.kind {
            PropertyKind::Range { min, max } => {
                if value < *min || value > *max {
                    return Err(EINVAL);
                }
            }
            PropertyKind::SignedRange { min, max } => {
                let value = value as i64;
                if value < *min || value > *max {
                    return Err(EINVAL);
                }
            }
            PropertyKind::Enum { values } => {
                if !values.iter().any(|v| v.value == value) {
                    return Err(EINVAL);
                }
            }
            PropertyKind::Bitmask { values } => {
                let valid = values.iter().fold(0u64, |mask, v| mask | (1 << v.value));
                if value & !valid != 0 {
                    return Err(EINVAL);
                }
            }
            PropertyKind::Blob { .. } => {
                if value != 0 && !self.blobs.contains_key(&(value as u32)) {
                    return Err(EINVAL);
                }
            }
            PropertyKind::Object => {
                let id = value as u32;
                let exists = match property.name.as_str() {
                    "FB_ID" => self.framebuffers.contains_key(&id),
                    "CRTC_ID" => self.snapshot.crtc(id).is_some(),
                    _ => true,
                };
                if value != 0 && !exists {
                    return Err(EINVAL);
                }
            }
            PropertyKind::Unknown => {}
        }
        Ok(())
    }

    /// Checks a whole commit and returns the state it leads to together
    /// with the CRTCs it affects.
    fn check_commit(
        &self,
        request: &AtomicRequest,
        flags: CommitFlags,
    ) -> Result<(DeviceSnapshot, Vec<u32>), i32> {
        if !self.master {
            return Err(EACCES);
        }
        if flags.test_only && flags.page_flip_event {
            return Err(EINVAL);
        }

        let old = &self.snapshot;
        let mut new = old.clone();
        let mut changed: Vec<(u32, String)> = Vec::new();
        for change in &request.changes {
            self.check_change(change.object, change.property, change.value)?;
            let properties = object_properties_mut(&mut new, change.object).ok_or(ENOENT)?;
            let property = properties
                .iter_mut()
                .find(|p| p.id == change.property)
                .ok_or(ENOENT)?;
            if property.value != change.value {
                property.value = change.value;
                changed.push((change.object, property.name.clone()));
            }
        }

        let value = |properties: &[Property], name: &str| {
            property(properties, name).map(|p| p.value).unwrap_or(0)
        };

        // CRTCs touched directly, through a plane, or through a connector.
        let mut affected: Vec<u32> = Vec::new();
        let mut touch = |crtc: u64| {
            let crtc = crtc as u32;
            if crtc != 0 && !affected.contains(&crtc) {
                affected.push(crtc);
            }
        };
        for change in &request.changes {
            if old.crtc(change.object).is_some() {
                touch(change.object as u64);
            }
            for snapshot in [old, &new] {
                if let Some(plane) = snapshot.plane(change.object) {
                    touch(value(&plane.properties, "CRTC_ID"));
                }
                if let Some(connector) = snapshot.connector(change.object) {
                    touch(value(&connector.properties, "CRTC_ID"));
                }
            }
        }

        // Does this need a modeset?
        let needs_modeset = changed.iter().any(|(object, name)| {
            (old.crtc(*object).is_some() && (name == "ACTIVE" || name == "MODE_ID"))
                || (old.connector(*object).is_some() && name == "CRTC_ID")
        });
        if needs_modeset && !flags.allow_modeset {
            return Err(EINVAL);
        }

        // CRTC state.
        for crtc in &new.crtcs {
            let active = value(&crtc.properties, "ACTIVE") != 0;
            let has_mode = value(&crtc.properties, "MODE_ID") != 0;
            let has_connector = new
                .connectors
                .iter()
                .any(|c| value(&c.properties, "CRTC_ID") == crtc.id as u64);
            if active && !has_mode {
                return Err(EINVAL);
            }
            if has_mode != has_connector {
                return Err(EINVAL);
            }
        }
        // Connector routing.
        for connector in &new.connectors {
            let crtc = value(&connector.properties, "CRTC_ID") as u32;
            if crtc == 0 {
                continue;
            }
            let possible = connector
                .encoders
                .iter()
                .filter_map(|id| new.encoders.iter().find(|e| e.id == *id))
                .any(|encoder| new.crtcs_in_mask(encoder.possible_crtcs).contains(&crtc));
            if !possible {
                return Err(EINVAL);
            }
        }
        // Plane state. Planes this commit does not touch keep whatever
        // state they were found in.
        for plane in &new.planes {
            if !request.changes.iter().any(|c| c.object == plane.id) {
                continue;
            }
            let fb_id = value(&plane.properties, "FB_ID") as u32;
            let crtc_id = value(&plane.properties, "CRTC_ID") as u32;
            if (fb_id != 0) != (crtc_id != 0) {
                return Err(EINVAL);
            }
            if fb_id == 0 {
                continue;
            }
            if !new.crtcs_in_mask(plane.possible_crtcs).contains(&crtc_id) {
                return Err(EINVAL);
            }
            let fb = self.framebuffers.get(&fb_id).ok_or(EINVAL)?;
            if !plane.formats.contains(&fb.format) {
                return Err(EINVAL);
            }
            if let Some(modifiers) = plane.modifiers_for(fb.format) {
                let modifier = if fb.modifier == MODIFIER_INVALID {
                    MODIFIER_LINEAR
                } else {
                    fb.modifier
                };
                if !modifiers.contains(&modifier) {
                    return Err(EINVAL);
                }
            }
            let src = |name| value(&plane.properties, name);
            let (src_w, src_h) = (src("SRC_W") >> 16, src("SRC_H") >> 16);
            if (src("SRC_X") >> 16) + src_w > fb.width as u64
                || (src("SRC_Y") >> 16) + src_h > fb.height as u64
            {
                return Err(ENOSPC);
            }
            if plane.kind() == Some(PlaneKind::Primary) {
                let crtc = new.crtc(crtc_id).ok_or(EINVAL)?;
                let blob = value(&crtc.properties, "MODE_ID") as u32;
                // A plane on a CRTC without a mode shows nothing; the
                // kernel accepts it only while the CRTC is being disabled.
                let Some(mode) = self.blobs.get(&blob) else {
                    return Err(EINVAL);
                };
                let (crtc_w, crtc_h) = (src("CRTC_W"), src("CRTC_H"));
                if src_w != crtc_w || src_h != crtc_h {
                    // No scaling on a primary plane.
                    return Err(ERANGE);
                }
                let (crtc_x, crtc_y) = (src("CRTC_X") as i64, src("CRTC_Y") as i64);
                if crtc_x != 0 || crtc_y != 0 || (crtc_w as u32, crtc_h as u32) != mode.size() {
                    return Err(EINVAL);
                }
            }
        }

        // A flip is still in flight.
        if flags.nonblock
            && !flags.test_only
            && affected
                .iter()
                .any(|crtc| self.pending.iter().any(|(pending, _)| pending == crtc))
        {
            return Err(EBUSY);
        }

        let active = |snapshot: &DeviceSnapshot, crtc: u32| {
            snapshot
                .crtc(crtc)
                .is_some_and(|c| value(&c.properties, "ACTIVE") != 0)
        };
        if flags.page_flip_event
            && (affected.is_empty()
                || affected
                    .iter()
                    .any(|crtc| !active(old, *crtc) && !active(&new, *crtc)))
        {
            return Err(EINVAL);
        }

        if flags.page_flip_async {
            if !old.caps.atomic_async_page_flip || needs_modeset {
                return Err(EINVAL);
            }
            for (object, name) in &changed {
                let plane = old.plane(*object).ok_or(EINVAL)?;
                if plane.kind() != Some(PlaneKind::Primary) || name != "FB_ID" {
                    return Err(EINVAL);
                }
                let old_fb = self
                    .framebuffers
                    .get(&(value(&plane.properties, "FB_ID") as u32));
                let new_plane = new.plane(*object).expect("same planes");
                let new_fb = self
                    .framebuffers
                    .get(&(value(&new_plane.properties, "FB_ID") as u32));
                match (old_fb, new_fb) {
                    (Some(a), Some(b))
                        if (a.format, a.modifier, a.width, a.height, a.stride)
                            == (b.format, b.modifier, b.width, b.height, b.stride) => {}
                    _ => return Err(EINVAL),
                }
            }
        }

        Ok((new, affected))
    }
}

fn object_properties_mut(snapshot: &mut DeviceSnapshot, id: u32) -> Option<&mut Vec<Property>> {
    if let Some(connector) = snapshot.connectors.iter_mut().find(|c| c.id == id) {
        return Some(&mut connector.properties);
    }
    if let Some(crtc) = snapshot.crtcs.iter_mut().find(|c| c.id == id) {
        return Some(&mut crtc.properties);
    }
    snapshot
        .planes
        .iter_mut()
        .find(|p| p.id == id)
        .map(|plane| &mut plane.properties)
}

impl KmsDevice for MockDevice {
    fn snapshot(&mut self) -> io::Result<DeviceSnapshot> {
        Ok(self.state.borrow().snapshot.clone())
    }

    fn device_number(&self) -> Option<(u32, u32)> {
        // DRM primary nodes have major 226.
        Some((226, 0))
    }

    fn create_mode_blob(&mut self, mode: &Mode) -> io::Result<u32> {
        let mut state = self.state.borrow_mut();
        let id = state.allocate_id();
        state.blobs.insert(id, mode.clone());
        Ok(id)
    }

    fn destroy_blob(&mut self, blob: u32) -> io::Result<()> {
        self.state
            .borrow_mut()
            .blobs
            .remove(&blob)
            .map(|_| ())
            .ok_or_else(|| errno(ENOENT))
    }

    fn create_dumb_buffer(&mut self, width: u32, height: u32) -> io::Result<DumbBuffer> {
        let mut state = self.state.borrow_mut();
        if !state.snapshot.caps.dumb_buffer {
            return Err(errno(EOPNOTSUPP));
        }
        if width == 0 || height == 0 {
            return Err(errno(EINVAL));
        }
        let stride = (width * 4).next_multiple_of(state.dumb_stride_alignment);
        let size = stride as usize * height as usize;
        let info = DumbBuffer {
            handle: state.allocate_id(),
            width,
            height,
            stride,
            size,
        };
        state.dumbs.insert(
            info.handle,
            Dumb {
                info,
                // Not zero: reading before writing should stand out.
                data: vec![0x5a; size],
            },
        );
        Ok(info)
    }

    fn write_dumb_buffer(
        &mut self,
        buffer: &DumbBuffer,
        write: &mut dyn FnMut(&mut [u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        let dumb = state
            .dumbs
            .get_mut(&buffer.handle)
            .ok_or_else(|| errno(ENOENT))?;
        write(&mut dumb.data)
    }

    fn destroy_dumb_buffer(&mut self, buffer: &DumbBuffer) -> io::Result<()> {
        self.state
            .borrow_mut()
            .dumbs
            .remove(&buffer.handle)
            .map(|_| ())
            .ok_or_else(|| errno(ENOENT))
    }

    fn add_dumb_framebuffer(&mut self, buffer: &DumbBuffer) -> io::Result<u32> {
        let mut state = self.state.borrow_mut();
        let info = state
            .dumbs
            .get(&buffer.handle)
            .ok_or_else(|| errno(ENOENT))?
            .info;
        let id = state.allocate_id();
        state.framebuffers.insert(
            id,
            Framebuffer {
                width: info.width,
                height: info.height,
                stride: info.stride,
                format: FOURCC_XRGB8888,
                modifier: MODIFIER_INVALID,
                memory: FramebufferMemory::Dumb(info.handle),
            },
        );
        Ok(id)
    }

    fn add_dmabuf_framebuffer(&mut self, dmabuf: &DmabufDescriptor<'_>) -> io::Result<u32> {
        let mut state = self.state.borrow_mut();
        if !state.accept_dmabufs {
            // What a driver answers to a buffer it cannot import.
            return Err(errno(EINVAL));
        }
        let with_modifiers = state.snapshot.caps.addfb2_modifiers;
        if !with_modifiers
            && dmabuf.modifier != MODIFIER_LINEAR
            && dmabuf.modifier != MODIFIER_INVALID
        {
            return Err(errno(EINVAL));
        }
        // As the real device does: no stated modifier, no modifier flag.
        let with_modifiers = with_modifiers && dmabuf.modifier != MODIFIER_INVALID;
        if dmabuf.width == 0
            || dmabuf.height == 0
            || (dmabuf.stride as u64) < dmabuf.width as u64 * 4
        {
            return Err(errno(EINVAL));
        }
        let fd = dmabuf.fd.try_clone_to_owned()?;
        let length = dmabuf.offset as usize + dmabuf.stride as usize * dmabuf.height as usize;
        // SAFETY: a fresh read-only shared mapping of a file descriptor we
        // own; it is unmapped when the framebuffer is removed.
        let mapping = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                length,
                rustix::mm::ProtFlags::READ,
                rustix::mm::MapFlags::SHARED,
                &fd,
                0,
            )
        }
        .ok()
        .map(|pointer| Mapping { pointer, length });
        let id = state.allocate_id();
        state.framebuffers.insert(
            id,
            Framebuffer {
                width: dmabuf.width,
                height: dmabuf.height,
                stride: dmabuf.stride,
                format: dmabuf.format,
                modifier: if with_modifiers {
                    dmabuf.modifier
                } else {
                    MODIFIER_INVALID
                },
                memory: FramebufferMemory::Dmabuf {
                    _fd: fd,
                    mapping,
                    offset: dmabuf.offset as usize,
                },
            },
        );
        Ok(id)
    }

    fn remove_framebuffer(&mut self, framebuffer: u32) -> io::Result<()> {
        self.state
            .borrow_mut()
            .framebuffers
            .remove(&framebuffer)
            .map(|_| ())
            .ok_or_else(|| errno(ENOENT))
    }

    fn commit(&mut self, request: &AtomicRequest, flags: CommitFlags) -> io::Result<()> {
        let mut state = self.state.borrow_mut();
        let record = CommitRecord {
            request: request.clone(),
            flags,
        };
        if let Some(code) = state.fail_next_commit.take() {
            state.rejected.push((record, code));
            return Err(errno(code));
        }
        let (new, affected) = match state.check_commit(request, flags) {
            Ok(result) => result,
            Err(code) => {
                state.rejected.push((record, code));
                return Err(errno(code));
            }
        };
        if flags.test_only {
            return Ok(());
        }
        state.snapshot = new;
        state.commits.push(record);

        let immediate = !flags.nonblock || flags.page_flip_async;
        if immediate {
            // A blocking commit returns when the change is on screen: the
            // flips before it have completed too.
            let (time, sequence) = (state.clock.now(), state.sequence);
            for (crtc, wants_event) in std::mem::take(&mut state.pending) {
                if wants_event {
                    state.events.push_back(DeviceEvent::PageFlip {
                        crtc,
                        sequence,
                        time,
                    });
                }
            }
            if flags.page_flip_event {
                for crtc in affected {
                    state.events.push_back(DeviceEvent::PageFlip {
                        crtc,
                        sequence,
                        time,
                    });
                }
            }
        } else {
            for crtc in affected {
                state.pending.push((crtc, flags.page_flip_event));
            }
        }
        Ok(())
    }

    fn read_events(&mut self) -> io::Result<Vec<DeviceEvent>> {
        Ok(self.state.borrow_mut().events.drain(..).collect())
    }

    fn poll_fd(&self) -> Option<BorrowedFd<'_>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::plan::{
        OutputRequest, Pipeline, disable_request, flip_request, modeset_request, plan_output,
    };
    use super::*;

    struct Rig {
        device: MockDevice,
        clock: ManualClock,
        pipeline: Pipeline,
        blob: u32,
        framebuffers: Vec<u32>,
    }

    /// A device with one 640x480 output, a mode blob and two framebuffers,
    /// before any commit.
    fn rig_with(builder: SnapshotBuilder) -> Rig {
        let clock = ManualClock::new(Duration::from_secs(100));
        let mut device = MockDevice::new(
            builder
                .connector("Virtual-1", true, &[(640, 480, 60, true)])
                .build(),
            clock.clone(),
        );
        let pipeline = plan_output(&device.snapshot().unwrap(), &OutputRequest::default()).unwrap();
        let blob = device.create_mode_blob(&pipeline.mode).unwrap();
        let framebuffers = (0..2)
            .map(|_| {
                let dumb = device.create_dumb_buffer(640, 480).unwrap();
                device.add_dumb_framebuffer(&dumb).unwrap()
            })
            .collect();
        Rig {
            device,
            clock,
            pipeline,
            blob,
            framebuffers,
        }
    }

    fn rig() -> Rig {
        rig_with(SnapshotBuilder::new("mock"))
    }

    fn modeset(rig: &mut Rig) {
        rig.device
            .commit(
                &modeset_request(&rig.pipeline, rig.framebuffers[0], rig.blob),
                CommitFlags::MODESET,
            )
            .unwrap();
        rig.device.read_events().unwrap();
    }

    fn code(result: io::Result<()>) -> Option<i32> {
        result.err().and_then(|e| e.raw_os_error())
    }

    #[test]
    fn modeset_turns_the_output_on_and_reports_at_once() {
        let mut rig = rig();
        assert_eq!(rig.device.active_mode(rig.pipeline.crtc), None);
        rig.device
            .commit(
                &modeset_request(&rig.pipeline, rig.framebuffers[0], rig.blob),
                CommitFlags::MODESET,
            )
            .unwrap();
        assert_eq!(
            rig.device.active_mode(rig.pipeline.crtc).unwrap().size(),
            (640, 480)
        );
        assert_eq!(
            rig.device.connector_crtc("Virtual-1"),
            Some(rig.pipeline.crtc)
        );
        assert_eq!(
            rig.device.displayed(rig.pipeline.crtc).unwrap().framebuffer,
            rig.framebuffers[0]
        );
        // Blocking commit: the event is there without a vblank.
        let events = rig.device.read_events().unwrap();
        assert_eq!(
            events,
            [DeviceEvent::PageFlip {
                crtc: rig.pipeline.crtc,
                sequence: 0,
                time: Duration::from_secs(100)
            }]
        );
        assert!(rig.device.read_events().unwrap().is_empty());
    }

    #[test]
    fn modeset_needs_the_allow_modeset_flag() {
        let mut rig = rig();
        let request = modeset_request(&rig.pipeline, rig.framebuffers[0], rig.blob);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::FLIP)),
            Some(EINVAL)
        );
        assert_eq!(rig.device.commits().len(), 0);
        assert_eq!(rig.device.rejected().len(), 1);
    }

    #[test]
    fn flips_complete_at_vblank_with_a_timestamp() {
        let mut rig = rig();
        modeset(&mut rig);
        let flip = flip_request(&rig.pipeline, rig.framebuffers[1]);
        rig.device.commit(&flip, CommitFlags::FLIP).unwrap();
        assert!(rig.device.flip_pending(rig.pipeline.crtc));
        assert!(rig.device.read_events().unwrap().is_empty());
        // The new framebuffer is latched for the next scanout.
        assert_eq!(
            rig.device.displayed(rig.pipeline.crtc).unwrap().framebuffer,
            rig.framebuffers[1]
        );

        rig.device.vblank();
        let events = rig.device.read_events().unwrap();
        let [
            DeviceEvent::PageFlip {
                crtc,
                sequence,
                time,
            },
        ] = events[..]
        else {
            panic!("expected one flip event, got {events:?}");
        };
        assert_eq!((crtc, sequence), (rig.pipeline.crtc, 1));
        // One 60 Hz period after the start.
        let elapsed = time - Duration::from_secs(100);
        assert!(
            (elapsed.as_micros() as i64 - 16_667).abs() < 100,
            "{elapsed:?}"
        );
        assert_eq!(rig.clock.now(), time);
        assert!(!rig.device.flip_pending(rig.pipeline.crtc));
    }

    #[test]
    fn a_second_flip_before_vblank_is_busy() {
        let mut rig = rig();
        modeset(&mut rig);
        rig.device
            .commit(
                &flip_request(&rig.pipeline, rig.framebuffers[1]),
                CommitFlags::FLIP,
            )
            .unwrap();
        let again = flip_request(&rig.pipeline, rig.framebuffers[0]);
        assert_eq!(
            code(rig.device.commit(&again, CommitFlags::FLIP)),
            Some(EBUSY)
        );
        rig.device.vblank();
        rig.device.commit(&again, CommitFlags::FLIP).unwrap();
    }

    #[test]
    fn commits_without_master_are_refused() {
        let mut rig = rig();
        modeset(&mut rig);
        rig.device
            .commit(
                &flip_request(&rig.pipeline, rig.framebuffers[1]),
                CommitFlags::FLIP,
            )
            .unwrap();
        rig.device.set_master(false);
        // The flip that was in flight never reports.
        rig.device.vblank();
        assert!(rig.device.read_events().unwrap().is_empty());
        let flip = flip_request(&rig.pipeline, rig.framebuffers[0]);
        assert_eq!(
            code(rig.device.commit(&flip, CommitFlags::FLIP)),
            Some(EACCES)
        );
        rig.device.set_master(true);
        rig.device.commit(&flip, CommitFlags::FLIP).unwrap();
    }

    #[test]
    fn unknown_and_immutable_properties_are_rejected() {
        let mut rig = rig();
        modeset(&mut rig);
        let plane = rig.pipeline.primary_plane;
        let snapshot = rig.device.snapshot().unwrap();
        let type_property = property(&snapshot.plane(plane).unwrap().properties, "type")
            .unwrap()
            .id;
        let active = rig.pipeline.properties.crtc_active;

        let mut request = AtomicRequest::default();
        request.set(plane, 9999, 1);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::FLIP)),
            Some(ENOENT)
        );

        // ACTIVE is a CRTC property; the plane does not have it.
        let mut request = AtomicRequest::default();
        request.set(plane, active, 1);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::FLIP)),
            Some(ENOENT)
        );

        let mut request = AtomicRequest::default();
        request.set(plane, type_property, 0);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::FLIP)),
            Some(EINVAL)
        );

        let mut request = AtomicRequest::default();
        request.set(4242, active, 1);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::FLIP)),
            Some(ENOENT)
        );
    }

    #[test]
    fn values_out_of_range_and_dangling_objects_are_rejected() {
        let mut rig = rig();
        modeset(&mut rig);
        let p = rig.pipeline.properties.clone();
        let mut request = AtomicRequest::default();
        request.set(rig.pipeline.crtc, p.crtc_active, 2);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::MODESET)),
            Some(EINVAL)
        );
        // A framebuffer that does not exist.
        let request = flip_request(&rig.pipeline, 31337);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::FLIP)),
            Some(EINVAL)
        );
        // A mode blob that does not exist.
        let mut request = AtomicRequest::default();
        request.set(rig.pipeline.crtc, p.crtc_mode_id, 31337);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::MODESET)),
            Some(EINVAL)
        );
    }

    #[test]
    fn inconsistent_states_are_rejected() {
        let mut rig = rig();
        let p = rig.pipeline.properties.clone();
        let flags = CommitFlags {
            page_flip_event: false,
            ..CommitFlags::MODESET
        };

        // Active without a mode.
        let mut request = AtomicRequest::default();
        request.set(rig.pipeline.crtc, p.crtc_active, 1);
        assert_eq!(code(rig.device.commit(&request, flags)), Some(EINVAL));

        // A mode without a connector.
        let mut request = AtomicRequest::default();
        request.set(rig.pipeline.crtc, p.crtc_mode_id, rig.blob as u64);
        request.set(rig.pipeline.crtc, p.crtc_active, 1);
        assert_eq!(code(rig.device.commit(&request, flags)), Some(EINVAL));

        // A connector on a CRTC without a mode.
        let mut request = AtomicRequest::default();
        request.set(
            rig.pipeline.connector,
            p.connector_crtc_id,
            rig.pipeline.crtc as u64,
        );
        assert_eq!(code(rig.device.commit(&request, flags)), Some(EINVAL));

        // A framebuffer without a CRTC on the plane.
        modeset(&mut rig);
        let mut request = AtomicRequest::default();
        request.set(rig.pipeline.primary_plane, p.plane_crtc_id, 0);
        assert_eq!(code(rig.device.commit(&request, flags)), Some(EINVAL));
    }

    #[test]
    fn primary_plane_must_cover_the_crtc_unscaled_from_inside_the_framebuffer() {
        let mut rig = rig();
        modeset(&mut rig);
        let p = rig.pipeline.properties.clone();
        let plane = rig.pipeline.primary_plane;

        // Source larger than the framebuffer.
        let mut request = AtomicRequest::default();
        request.set(plane, p.plane_src_w, 641 << 16);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::FLIP)),
            Some(ENOSPC)
        );

        // Scaling: half the source onto the whole CRTC.
        let mut request = AtomicRequest::default();
        request.set(plane, p.plane_src_w, 320 << 16);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::FLIP)),
            Some(ERANGE)
        );

        // Not at the origin.
        let mut request = AtomicRequest::default();
        request.set(plane, p.plane_crtc_x, 10);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::FLIP)),
            Some(EINVAL)
        );

        // A framebuffer of the wrong size.
        let small = rig.device.create_dumb_buffer(320, 240).unwrap();
        let small_fb = rig.device.add_dumb_framebuffer(&small).unwrap();
        let request = flip_request(&rig.pipeline, small_fb);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::FLIP)),
            Some(ENOSPC)
        );
    }

    #[test]
    fn framebuffer_format_must_be_one_the_plane_takes() {
        let mut rig = rig();
        modeset(&mut rig);
        // The cursor plane takes ARGB8888 only; an XRGB8888 framebuffer on
        // it is refused.
        let snapshot = rig.device.snapshot().unwrap();
        let cursor = snapshot
            .planes
            .iter()
            .find(|plane| plane.kind() == Some(PlaneKind::Cursor))
            .unwrap();
        let p = &rig.pipeline.properties;
        let mut request = AtomicRequest::default();
        request.set(cursor.id, p.plane_fb_id, rig.framebuffers[1] as u64);
        request.set(cursor.id, p.plane_crtc_id, rig.pipeline.crtc as u64);
        request.set(cursor.id, p.plane_src_w, 64 << 16);
        request.set(cursor.id, p.plane_src_h, 64 << 16);
        request.set(cursor.id, p.plane_crtc_w, 64);
        request.set(cursor.id, p.plane_crtc_h, 64);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::FLIP)),
            Some(EINVAL)
        );
    }

    #[test]
    fn test_only_commits_check_without_applying() {
        let mut rig = rig();
        let request = modeset_request(&rig.pipeline, rig.framebuffers[0], rig.blob);
        let test = CommitFlags {
            test_only: true,
            page_flip_event: false,
            ..CommitFlags::MODESET
        };
        rig.device.commit(&request, test).unwrap();
        assert_eq!(rig.device.active_mode(rig.pipeline.crtc), None);
        assert!(rig.device.commits().is_empty());
        // TEST_ONLY cannot ask for an event.
        let bad = CommitFlags {
            test_only: true,
            ..CommitFlags::MODESET
        };
        assert_eq!(code(rig.device.commit(&request, bad)), Some(EINVAL));
    }

    #[test]
    fn async_flips_need_support_and_may_only_swap_the_primary_framebuffer() {
        let async_flip = CommitFlags {
            page_flip_async: true,
            ..CommitFlags::FLIP
        };

        // A device without support (virtio-gpu).
        let mut rig = rig();
        modeset(&mut rig);
        let flip = flip_request(&rig.pipeline, rig.framebuffers[1]);
        assert_eq!(code(rig.device.commit(&flip, async_flip)), Some(EINVAL));

        // A device with support.
        let mut rig = rig_with(SnapshotBuilder::new("mock").async_flips());
        modeset(&mut rig);
        let flip = flip_request(&rig.pipeline, rig.framebuffers[1]);
        rig.device.commit(&flip, async_flip).unwrap();
        // No waiting for vblank.
        assert_eq!(rig.device.read_events().unwrap().len(), 1);
        assert!(!rig.device.flip_pending(rig.pipeline.crtc));

        // Anything besides FB_ID of the primary plane is refused: here the
        // cursor plane moves in the same commit.
        let snapshot = rig.device.snapshot().unwrap();
        let cursor = snapshot
            .planes
            .iter()
            .find(|plane| plane.kind() == Some(PlaneKind::Cursor))
            .unwrap()
            .id;
        let mut request = flip_request(&rig.pipeline, rig.framebuffers[0]);
        request.set(cursor, rig.pipeline.properties.plane_crtc_x, 5);
        assert_eq!(code(rig.device.commit(&request, async_flip)), Some(EINVAL));

        // A framebuffer of a different size cannot be flipped to
        // asynchronously (it would need a real update).
        let other = rig.device.create_dumb_buffer(800, 600).unwrap();
        let other_fb = rig.device.add_dumb_framebuffer(&other).unwrap();
        let mut request = flip_request(&rig.pipeline, other_fb);
        request.set(
            rig.pipeline.primary_plane,
            rig.pipeline.properties.plane_src_w,
            640 << 16,
        );
        assert_eq!(code(rig.device.commit(&request, async_flip)), Some(EINVAL));
    }

    #[test]
    fn disable_turns_everything_off() {
        let mut rig = rig();
        modeset(&mut rig);
        rig.device
            .commit(&disable_request(&rig.pipeline), CommitFlags::MODESET)
            .unwrap();
        assert_eq!(rig.device.active_mode(rig.pipeline.crtc), None);
        assert_eq!(rig.device.connector_crtc("Virtual-1"), None);
        assert!(rig.device.displayed(rig.pipeline.crtc).is_none());
    }

    #[test]
    fn dumb_buffers_have_padded_rows_and_keep_what_is_written() {
        let mut rig = rig();
        modeset(&mut rig);
        let dumb = rig.device.create_dumb_buffer(640, 480).unwrap();
        // 640 * 4 = 2560 is a multiple of 256; use an odd width too.
        assert_eq!(dumb.stride, 2560);
        let odd = rig.device.create_dumb_buffer(100, 10).unwrap();
        assert_eq!(odd.stride, 512);
        assert_eq!(odd.size, 5120);

        rig.device
            .write_dumb_buffer(&dumb, &mut |memory| {
                assert_eq!(memory.len(), dumb.size);
                // B, G, R, X at pixel (3, 2).
                let i = 2 * dumb.stride as usize + 3 * 4;
                memory[i..i + 4].copy_from_slice(&[10, 20, 30, 0]);
                Ok(())
            })
            .unwrap();
        let fb = rig.device.add_dumb_framebuffer(&dumb).unwrap();
        rig.device
            .commit(&flip_request(&rig.pipeline, fb), CommitFlags::FLIP)
            .unwrap();
        let displayed = rig.device.displayed(rig.pipeline.crtc).unwrap();
        assert_eq!(displayed.rgb(3, 2), [30, 20, 10]);
        assert_eq!(displayed.format, FOURCC_XRGB8888);

        rig.device.destroy_dumb_buffer(&odd).unwrap();
        assert!(rig.device.destroy_dumb_buffer(&odd).is_err());
    }

    #[test]
    fn modifiers_are_checked_when_the_device_has_them() {
        // Without modifier support only linear dma-bufs are accepted. A
        // memfd stands in for a dma-buf here.
        let fd = rustix::fs::memfd_create("fb", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
        rustix::fs::ftruncate(&fd, 64 * 4 * 64).unwrap();
        use std::os::fd::AsFd;
        let descriptor = |modifier| DmabufDescriptor {
            fd: fd.as_fd(),
            width: 64,
            height: 64,
            format: FOURCC_XRGB8888,
            modifier,
            stride: 256,
            offset: 0,
        };

        let mut rig = rig();
        assert!(
            rig.device
                .add_dmabuf_framebuffer(&descriptor(MODIFIER_LINEAR))
                .is_ok()
        );
        assert_eq!(
            rig.device
                .add_dmabuf_framebuffer(&descriptor(0x1234))
                .err()
                .and_then(|e| e.raw_os_error()),
            Some(EINVAL)
        );
        rig.device.set_accept_dmabufs(false);
        assert!(
            rig.device
                .add_dmabuf_framebuffer(&descriptor(MODIFIER_LINEAR))
                .is_err()
        );

        // With modifier support the plane's list decides at commit time.
        let clock = ManualClock::default();
        let mut device = MockDevice::new(
            SnapshotBuilder::new("mock")
                .modifiers(&[MODIFIER_LINEAR, 0x77])
                .connector("DP-1", true, &[(64, 64, 60, true)])
                .build(),
            clock,
        );
        let pipeline = plan_output(&device.snapshot().unwrap(), &OutputRequest::default()).unwrap();
        assert_eq!(pipeline.plane_modifiers, Some(vec![MODIFIER_LINEAR, 0x77]));
        let blob = device.create_mode_blob(&pipeline.mode).unwrap();
        let good = device.add_dmabuf_framebuffer(&descriptor(0x77)).unwrap();
        let bad = device.add_dmabuf_framebuffer(&descriptor(0x99)).unwrap();
        assert_eq!(
            code(device.commit(&modeset_request(&pipeline, bad, blob), CommitFlags::MODESET)),
            Some(EINVAL)
        );
        device
            .commit(
                &modeset_request(&pipeline, good, blob),
                CommitFlags::MODESET,
            )
            .unwrap();
        // A memfd can be mapped, so the mock can show its contents.
        assert!(device.displayed(pipeline.crtc).unwrap().pixels.is_some());
        assert_eq!(device.displayed(pipeline.crtc).unwrap().modifier, 0x77);
    }

    #[test]
    fn injected_failures_hit_exactly_one_commit() {
        let mut rig = rig();
        rig.device.fail_next_commit(EBUSY);
        let request = modeset_request(&rig.pipeline, rig.framebuffers[0], rig.blob);
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::MODESET)),
            Some(EBUSY)
        );
        rig.device.commit(&request, CommitFlags::MODESET).unwrap();
    }

    #[test]
    fn page_flip_events_need_an_active_crtc() {
        let mut rig = rig();
        // Nothing is active yet and this commit does not enable anything.
        let mut request = AtomicRequest::default();
        request.set(
            rig.pipeline.primary_plane,
            rig.pipeline.properties.plane_src_x,
            0,
        );
        assert_eq!(
            code(rig.device.commit(&request, CommitFlags::FLIP)),
            Some(EINVAL)
        );
    }
}
