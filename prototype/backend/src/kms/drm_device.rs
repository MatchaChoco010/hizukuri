//! [`KmsDevice`] over a real DRM device, with the `drm` crate.
//!
//! This is the thin layer: it translates between the plain data of
//! [`super::model`] and ioctls, and decides nothing.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use drm::Device as _;
use drm::buffer::{Buffer as _, DrmFourcc, DrmModifier, PlanarBuffer};
use drm::control::{
    AtomicCommitFlags, Device as ControlDevice, FbCmd2Flags, ResourceHandle, atomic, connector,
    dumbbuffer, framebuffer, property,
};

use super::blob::{anonymize_edid, parse_in_formats, to_hex};
use super::device::{DeviceEvent, DmabufDescriptor, DumbBuffer, KmsDevice};
use super::model::{
    AtomicRequest, CommitFlags, Connector, Crtc, DeviceCaps, DeviceSnapshot, DriverInfo, Encoder,
    EnumValue, Fourcc, MODIFIER_INVALID, MODIFIER_LINEAR, Mode, Plane, Property, PropertyKind,
};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// A DRM device node.
pub struct DrmDevice {
    fd: OwnedFd,
    /// Dumb buffers by handle; the `drm` crate's object is needed to map
    /// and destroy them.
    dumbs: HashMap<u32, dumbbuffer::DumbBuffer>,
    /// Remove what identifies the displays from EDIDs in snapshots.
    anonymize: bool,
    /// Have the kernel probe connectors when taking a snapshot.
    probe: bool,
}

impl AsFd for DrmDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl drm::Device for DrmDevice {}
impl ControlDevice for DrmDevice {}

impl DrmDevice {
    /// Wraps an open DRM device and asks for universal planes and the
    /// atomic API. Both are settings of this file descriptor only.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        let device = Self {
            fd,
            dumbs: HashMap::new(),
            anonymize: false,
            probe: false,
        };
        device.set_client_capability(drm::ClientCapability::UniversalPlanes, true)?;
        device.set_client_capability(drm::ClientCapability::Atomic, true)?;
        // Events are read when the descriptor is readable; never block.
        let flags = rustix::fs::fcntl_getfl(&device.fd)?;
        rustix::fs::fcntl_setfl(&device.fd, flags | rustix::fs::OFlags::NONBLOCK)?;
        Ok(device)
    }

    /// Opens a device node read-only, to look at it. Such a device cannot
    /// commit anything, and opening it does not take DRM master away from
    /// whoever has it.
    pub fn open_for_inspection(path: &Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        Self::new(OwnedFd::from(file))
    }

    /// Makes [`KmsDevice::snapshot`] strip what identifies the displays from
    /// EDIDs.
    pub fn set_anonymize(&mut self, anonymize: bool) {
        self.anonymize = anonymize;
    }

    /// Makes [`KmsDevice::snapshot`] ask the kernel to probe the connectors
    /// instead of reporting what it last saw. The session's device does
    /// this: some drivers (vkms) report neither a state nor modes before
    /// the first probe. Off for inspection, because a probe can disturb
    /// whoever is driving the displays.
    pub fn set_probe_connectors(&mut self, probe: bool) {
        self.probe = probe;
    }

    fn properties<T: ResourceHandle>(&self, handle: T) -> io::Result<Vec<Property>> {
        let set = self.get_properties(handle)?;
        let mut out = Vec::new();
        for (id, value) in set.iter() {
            let info = self.get_property(*id)?;
            let name = info.name().to_string_lossy().into_owned();
            let enum_values = |values: &property::EnumValues| -> Vec<EnumValue> {
                values
                    .values()
                    .1
                    .iter()
                    .map(|v| EnumValue {
                        name: v.name().to_string_lossy().into_owned(),
                        value: v.value(),
                    })
                    .collect()
            };
            let kind = match info.value_type() {
                property::ValueType::Boolean => PropertyKind::Range { min: 0, max: 1 },
                property::ValueType::UnsignedRange(min, max) => PropertyKind::Range {
                    min: *min,
                    max: *max,
                },
                property::ValueType::SignedRange(min, max) => PropertyKind::SignedRange {
                    min: *min,
                    max: *max,
                },
                property::ValueType::Enum(values) => PropertyKind::Enum {
                    values: enum_values(values),
                },
                property::ValueType::Bitmask => PropertyKind::Bitmask { values: Vec::new() },
                property::ValueType::Blob => {
                    // Only the EDID is worth keeping in a snapshot.
                    let data = (name == "EDID" && *value != 0)
                        .then(|| self.get_property_blob(*value).ok())
                        .flatten()
                        .map(|mut bytes| {
                            if self.anonymize {
                                anonymize_edid(&mut bytes);
                            }
                            to_hex(&bytes)
                        });
                    PropertyKind::Blob { data }
                }
                property::ValueType::Object
                | property::ValueType::CRTC
                | property::ValueType::Connector
                | property::ValueType::Encoder
                | property::ValueType::Framebuffer
                | property::ValueType::Plane
                | property::ValueType::Property => PropertyKind::Object,
                property::ValueType::Unknown => PropertyKind::Unknown,
            };
            out.push(Property {
                id: u32::from(*id),
                name,
                value: *value,
                immutable: !info.mutable(),
                kind,
            });
        }
        Ok(out)
    }
}

fn mode_from_drm(mode: &drm::control::Mode) -> Mode {
    let (hdisplay, vdisplay) = mode.size();
    let (hsync_start, hsync_end, htotal) = mode.hsync();
    let (vsync_start, vsync_end, vtotal) = mode.vsync();
    Mode {
        name: mode.name().to_string_lossy().into_owned(),
        clock: mode.clock(),
        hdisplay,
        hsync_start,
        hsync_end,
        htotal,
        hskew: mode.hskew(),
        vdisplay,
        vsync_start,
        vsync_end,
        vtotal,
        vscan: mode.vscan(),
        vrefresh: mode.vrefresh(),
        flags: mode.flags().bits(),
        kind: mode.mode_type().bits(),
    }
}

fn mode_to_drm(mode: &Mode) -> drm::control::Mode {
    let mut name = [0 as std::ffi::c_char; 32];
    for (slot, byte) in name.iter_mut().zip(mode.name.bytes().take(31)) {
        *slot = byte as std::ffi::c_char;
    }
    drm::control::Mode::from(drm_ffi::drm_mode_modeinfo {
        clock: mode.clock,
        hdisplay: mode.hdisplay,
        hsync_start: mode.hsync_start,
        hsync_end: mode.hsync_end,
        htotal: mode.htotal,
        hskew: mode.hskew,
        vdisplay: mode.vdisplay,
        vsync_start: mode.vsync_start,
        vsync_end: mode.vsync_end,
        vtotal: mode.vtotal,
        vscan: mode.vscan,
        vrefresh: mode.vrefresh,
        flags: mode.flags,
        type_: mode.kind,
        name,
    })
}

/// A single-plane buffer for `drmModeAddFB2`.
struct SinglePlane {
    size: (u32, u32),
    format: DrmFourcc,
    modifier: Option<DrmModifier>,
    handle: drm::buffer::Handle,
    pitch: u32,
    offset: u32,
}

impl PlanarBuffer for SinglePlane {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn format(&self) -> DrmFourcc {
        self.format
    }
    fn modifier(&self) -> Option<DrmModifier> {
        self.modifier
    }
    fn pitches(&self) -> [u32; 4] {
        [self.pitch, 0, 0, 0]
    }
    fn handles(&self) -> [Option<drm::buffer::Handle>; 4] {
        [Some(self.handle), None, None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        [self.offset, 0, 0, 0]
    }
}

impl KmsDevice for DrmDevice {
    fn snapshot(&mut self) -> io::Result<DeviceSnapshot> {
        let driver = self.get_driver()?;
        let (major, minor, patch) = driver.version;
        let cap = |capability| self.get_driver_capability(capability).unwrap_or(0);
        let caps = DeviceCaps {
            dumb_buffer: cap(drm::DriverCapability::DumbBuffer) != 0,
            addfb2_modifiers: cap(drm::DriverCapability::AddFB2Modifiers) != 0,
            async_page_flip: cap(drm::DriverCapability::ASyncPageFlip) != 0,
            atomic_async_page_flip: cap(drm::DriverCapability::AtomicASyncPageFlip) != 0,
            cursor_width: cap(drm::DriverCapability::CursorWidth) as u32,
            cursor_height: cap(drm::DriverCapability::CursorHeight) as u32,
            monotonic_timestamp: cap(drm::DriverCapability::MonotonicTimestamp) != 0,
        };

        let resources = self.resource_handles()?;
        let crtc_mask = |crtcs: Vec<drm::control::crtc::Handle>| -> u32 {
            resources
                .crtcs()
                .iter()
                .enumerate()
                .filter(|(_, handle)| crtcs.contains(handle))
                .fold(0, |mask, (index, _)| mask | (1 << index))
        };

        let mut connectors = Vec::new();
        for handle in resources.connectors() {
            let info = self.get_connector(*handle, self.probe)?;
            connectors.push(Connector {
                id: u32::from(*handle),
                name: format!("{}-{}", info.interface().as_str(), info.interface_id()),
                connected: info.state() == connector::State::Connected,
                physical_size_mm: info.size().unwrap_or((0, 0)),
                modes: info.modes().iter().map(mode_from_drm).collect(),
                encoders: info.encoders().iter().map(|e| u32::from(*e)).collect(),
                properties: self.properties(*handle)?,
            });
        }

        let mut encoders = Vec::new();
        for handle in resources.encoders() {
            let info = self.get_encoder(*handle)?;
            encoders.push(Encoder {
                id: u32::from(*handle),
                possible_crtcs: crtc_mask(resources.filter_crtcs(info.possible_crtcs())),
            });
        }

        let mut crtcs = Vec::new();
        for handle in resources.crtcs() {
            crtcs.push(Crtc {
                id: u32::from(*handle),
                properties: self.properties(*handle)?,
            });
        }

        let mut planes = Vec::new();
        for handle in self.plane_handles()? {
            let info = self.get_plane(handle)?;
            let properties = self.properties(handle)?;
            let modifiers = properties
                .iter()
                .find(|p| p.name == "IN_FORMATS" && p.value != 0)
                .and_then(|p| self.get_property_blob(p.value).ok())
                .and_then(|blob| parse_in_formats(&blob));
            planes.push(Plane {
                id: u32::from(handle),
                possible_crtcs: crtc_mask(resources.filter_crtcs(info.possible_crtcs())),
                formats: info.formats().iter().map(|f| Fourcc(*f)).collect(),
                modifiers,
                properties,
            });
        }

        Ok(DeviceSnapshot {
            driver: DriverInfo {
                name: driver.name().to_string_lossy().into_owned(),
                description: driver.description().to_string_lossy().into_owned(),
                version: format!("{major}.{minor}.{patch}"),
            },
            caps,
            connectors,
            encoders,
            crtcs,
            planes,
        })
    }

    fn device_number(&self) -> Option<(u32, u32)> {
        let stat = rustix::fs::fstat(&self.fd).ok()?;
        Some((
            rustix::fs::major(stat.st_rdev),
            rustix::fs::minor(stat.st_rdev),
        ))
    }

    fn create_mode_blob(&mut self, mode: &Mode) -> io::Result<u32> {
        match self.create_property_blob(&mode_to_drm(mode))? {
            property::Value::Blob(id) => Ok(id as u32),
            other => Err(invalid(format!("unexpected blob value {other:?}"))),
        }
    }

    fn destroy_blob(&mut self, blob: u32) -> io::Result<()> {
        self.destroy_property_blob(blob as u64)
    }

    fn create_dumb_buffer(&mut self, width: u32, height: u32) -> io::Result<DumbBuffer> {
        let dumb =
            ControlDevice::create_dumb_buffer(self, (width, height), DrmFourcc::Xrgb8888, 32)?;
        let handle = u32::from(dumb.handle());
        let stride = dumb.pitch();
        self.dumbs.insert(handle, dumb);
        Ok(DumbBuffer {
            handle,
            width,
            height,
            stride,
            size: stride as usize * height as usize,
        })
    }

    fn write_dumb_buffer(
        &mut self,
        buffer: &DumbBuffer,
        write: &mut dyn FnMut(&mut [u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        // The map borrows the device, so take the buffer out for the call.
        let mut dumb = self
            .dumbs
            .remove(&buffer.handle)
            .ok_or_else(|| invalid("unknown dumb buffer"))?;
        let result = match self.map_dumb_buffer(&mut dumb) {
            Ok(mut mapping) => write(&mut mapping),
            Err(error) => Err(error),
        };
        self.dumbs.insert(buffer.handle, dumb);
        result
    }

    fn destroy_dumb_buffer(&mut self, buffer: &DumbBuffer) -> io::Result<()> {
        let dumb = self
            .dumbs
            .remove(&buffer.handle)
            .ok_or_else(|| invalid("unknown dumb buffer"))?;
        ControlDevice::destroy_dumb_buffer(self, dumb)
    }

    fn add_dumb_framebuffer(&mut self, buffer: &DumbBuffer) -> io::Result<u32> {
        let dumb = self
            .dumbs
            .get(&buffer.handle)
            .ok_or_else(|| invalid("unknown dumb buffer"))?;
        // Depth 24, 32 bits per pixel: XRGB8888.
        Ok(u32::from(self.add_framebuffer(dumb, 24, 32)?))
    }

    fn add_dmabuf_framebuffer(&mut self, dmabuf: &DmabufDescriptor<'_>) -> io::Result<u32> {
        let format = DrmFourcc::try_from(dmabuf.format.0)
            .map_err(|_| invalid(format!("unknown format {}", dmabuf.format)))?;
        let with_modifiers = self
            .get_driver_capability(drm::DriverCapability::AddFB2Modifiers)
            .is_ok_and(|value| value != 0);
        if !with_modifiers
            && dmabuf.modifier != MODIFIER_LINEAR
            && dmabuf.modifier != MODIFIER_INVALID
        {
            return Err(invalid(format!(
                "the driver takes no format modifiers, so only linear buffers can be scanned out \
                 (got modifier {:#x})",
                dmabuf.modifier
            )));
        }
        // A buffer whose modifier is not stated is added the old way, and
        // the driver takes it for its default layout. Naming "invalid" as
        // an explicit modifier would be refused.
        let with_modifiers = with_modifiers && dmabuf.modifier != MODIFIER_INVALID;
        let handle = self.prime_fd_to_buffer(dmabuf.fd)?;
        let plane = SinglePlane {
            size: (dmabuf.width, dmabuf.height),
            format,
            modifier: with_modifiers.then_some(DrmModifier::from(dmabuf.modifier)),
            handle,
            pitch: dmabuf.stride,
            offset: dmabuf.offset,
        };
        let flags = if with_modifiers {
            FbCmd2Flags::MODIFIERS
        } else {
            FbCmd2Flags::empty()
        };
        let result = self.add_planar_framebuffer(&plane, flags);
        // The framebuffer keeps the memory alive; the GEM handle is not
        // needed any longer, whether or not creating it worked.
        let _ = self.close_buffer(handle);
        Ok(u32::from(result?))
    }

    fn remove_framebuffer(&mut self, framebuffer: u32) -> io::Result<()> {
        let handle = drm::control::from_u32::<framebuffer::Handle>(framebuffer)
            .ok_or_else(|| invalid("framebuffer id 0"))?;
        self.destroy_framebuffer(handle)
    }

    fn commit(&mut self, request: &AtomicRequest, flags: CommitFlags) -> io::Result<()> {
        let mut atomic = atomic::AtomicModeReq::new();
        for change in &request.changes {
            let object = drm::control::RawResourceHandle::new(change.object)
                .ok_or_else(|| invalid("object id 0"))?;
            let property = drm::control::from_u32::<property::Handle>(change.property)
                .ok_or_else(|| invalid("property id 0"))?;
            atomic.add_raw_property(object, property, change.value);
        }
        let mut bits = AtomicCommitFlags::empty();
        bits.set(AtomicCommitFlags::TEST_ONLY, flags.test_only);
        bits.set(AtomicCommitFlags::ALLOW_MODESET, flags.allow_modeset);
        bits.set(AtomicCommitFlags::NONBLOCK, flags.nonblock);
        bits.set(AtomicCommitFlags::PAGE_FLIP_EVENT, flags.page_flip_event);
        bits.set(AtomicCommitFlags::PAGE_FLIP_ASYNC, flags.page_flip_async);
        self.atomic_commit(bits, atomic)
    }

    fn read_events(&mut self) -> io::Result<Vec<DeviceEvent>> {
        let events = match self.receive_events() {
            Ok(events) => events,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Ok(events
            .filter_map(|event| match event {
                drm::control::Event::PageFlip(flip) => Some(DeviceEvent::PageFlip {
                    crtc: u32::from(flip.crtc),
                    sequence: flip.frame,
                    // The field is named `duration` but holds the
                    // timestamp of the flip.
                    time: flip.duration,
                }),
                _ => None,
            })
            .collect())
    }

    fn poll_fd(&self) -> Option<BorrowedFd<'_>> {
        Some(self.fd.as_fd())
    }
}
