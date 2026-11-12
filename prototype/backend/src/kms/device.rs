//! The thin interface to a DRM/KMS device.
//!
//! Everything the KMS backend does to hardware goes through [`KmsDevice`].
//! There are two implementations: the real one over the `drm` crate
//! (`drm_device`, behind the `kms` feature), and [`super::mock::MockDevice`],
//! which follows the kernel's rules closely enough that the backend can be
//! tested without hardware, a VT or a VM.

use std::io;
use std::os::fd::BorrowedFd;
use std::time::Duration;

use super::model::{AtomicRequest, CommitFlags, DeviceSnapshot, Fourcc, Mode};

/// A single-plane dma-buf to be wrapped into a framebuffer.
#[derive(Debug, Clone, Copy)]
pub struct DmabufDescriptor<'a> {
    pub fd: BorrowedFd<'a>,
    pub width: u32,
    pub height: u32,
    pub format: Fourcc,
    pub modifier: u64,
    /// Bytes per row.
    pub stride: u32,
    pub offset: u32,
}

/// A dumb buffer: CPU-writable memory that can be scanned out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DumbBuffer {
    /// The buffer's handle on the device.
    pub handle: u32,
    pub width: u32,
    pub height: u32,
    /// Bytes per row; at least `width * 4`, often more.
    pub stride: u32,
    /// Size of the mapping in bytes.
    pub size: usize,
}

/// Something the device reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceEvent {
    /// A commit with `page_flip_event` is on screen.
    PageFlip {
        crtc: u32,
        /// The CRTC's vblank counter.
        sequence: u32,
        /// When it happened, on `CLOCK_MONOTONIC`.
        time: Duration,
    },
}

/// What the KMS backend needs from a DRM device. Errors carry the errno the
/// kernel returned (`io::Error::raw_os_error`).
pub trait KmsDevice {
    /// Reads the current resources and property values.
    fn snapshot(&mut self) -> io::Result<DeviceSnapshot>;

    /// Device number `(major, minor)` of the device node, to match it with a
    /// Vulkan device.
    fn device_number(&self) -> Option<(u32, u32)>;

    /// Creates the property blob a CRTC's `MODE_ID` refers to.
    fn create_mode_blob(&mut self, mode: &Mode) -> io::Result<u32>;

    fn destroy_blob(&mut self, blob: u32) -> io::Result<()>;

    /// Creates a 32 bit per pixel dumb buffer.
    fn create_dumb_buffer(&mut self, width: u32, height: u32) -> io::Result<DumbBuffer>;

    /// Calls `write` with the memory of a dumb buffer.
    fn write_dumb_buffer(
        &mut self,
        buffer: &DumbBuffer,
        write: &mut dyn FnMut(&mut [u8]) -> io::Result<()>,
    ) -> io::Result<()>;

    fn destroy_dumb_buffer(&mut self, buffer: &DumbBuffer) -> io::Result<()>;

    /// Wraps a dumb buffer into an XRGB8888 framebuffer. Returns its id.
    fn add_dumb_framebuffer(&mut self, buffer: &DumbBuffer) -> io::Result<u32>;

    /// Imports a dma-buf and wraps it into a framebuffer. Returns its id.
    fn add_dmabuf_framebuffer(&mut self, dmabuf: &DmabufDescriptor<'_>) -> io::Result<u32>;

    fn remove_framebuffer(&mut self, framebuffer: u32) -> io::Result<()>;

    /// An atomic commit.
    fn commit(&mut self, request: &AtomicRequest, flags: CommitFlags) -> io::Result<()>;

    /// Reads the events that are waiting, without blocking.
    fn read_events(&mut self) -> io::Result<Vec<DeviceEvent>>;

    /// Readable when events are waiting. `None` for devices that are polled
    /// by hand (the mock).
    fn poll_fd(&self) -> Option<BorrowedFd<'_>>;
}
