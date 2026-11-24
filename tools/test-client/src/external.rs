//! Buffers whose memory was made outside the client: real dma-bufs.
//!
//! [`BufferKind::Dmabuf`](crate::BufferKind::Dmabuf) sends a memfd through
//! `zwp_linux_dmabuf_v1`, which exercises the protocol but is not a dma-buf:
//! a compositor that imports buffers into its GPU has to refuse it. The
//! functions here send the real thing.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1;

use crate::{ClientError, ClientEvent, TestClient, Transform};

/// A dma-buf with one plane.
#[derive(Debug, Clone, Copy)]
pub struct ExternalDmabuf<'a> {
    pub fd: BorrowedFd<'a>,
    pub width: u32,
    pub height: u32,
    /// DRM format code.
    pub fourcc: u32,
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
    /// `wl_surface.set_buffer_transform` for the commit.
    pub transform: Transform,
    /// The first row in memory is the bottom row of the image
    /// (`zwp_linux_buffer_params_v1` flag `y_invert`).
    pub y_invert: bool,
}

/// `struct udmabuf_create` of `linux/udmabuf.h`.
#[repr(C)]
struct UdmabufCreate {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
}

/// `UDMABUF_CREATE`: turns a range of a sealed memfd into a dma-buf.
struct Create(UdmabufCreate);

// SAFETY: the opcode is `_IOW('u', 0x42, struct udmabuf_create)`; the kernel
// reads the struct and returns a new file descriptor.
unsafe impl rustix::ioctl::Ioctl for Create {
    type Output = OwnedFd;
    const IS_MUTATING: bool = false;

    fn opcode(&self) -> rustix::ioctl::Opcode {
        rustix::ioctl::opcode::write::<UdmabufCreate>(b'u', 0x42)
    }

    fn as_ptr(&mut self) -> *mut std::ffi::c_void {
        (&raw mut self.0).cast()
    }

    unsafe fn output_from_ptr(
        out: rustix::ioctl::IoctlOutput,
        _: *mut std::ffi::c_void,
    ) -> rustix::io::Result<OwnedFd> {
        // SAFETY: on success the return value is a descriptor nothing else
        // owns.
        Ok(unsafe { OwnedFd::from_raw_fd(out) })
    }
}

/// Creates a dma-buf in system memory with the given contents, through
/// `/dev/udmabuf`. Returns `Ok(None)` when the device is not there or may
/// not be opened (it usually belongs to the `kvm` group).
///
/// The memory is linear and can be mapped, so this stands in for the
/// buffers of a software renderer, and for buffers of another device.
pub fn create_udmabuf(contents: &[u8]) -> io::Result<Option<OwnedFd>> {
    let device = match File::options().read(true).write(true).open("/dev/udmabuf") {
        Ok(device) => device,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let page = rustix::param::page_size();
    let size = contents.len().max(1).div_ceil(page) * page;
    let memfd = rustix::fs::memfd_create(
        "hizukuri-test-udmabuf",
        rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
    )?;
    let mut file = File::from(memfd);
    file.write_all(contents)?;
    file.set_len(size as u64)?;
    // The kernel wants a memfd that cannot shrink under the dma-buf.
    rustix::fs::fcntl_add_seals(&file, rustix::fs::SealFlags::SHRINK)?;
    let create = Create(UdmabufCreate {
        memfd: file.as_raw_fd() as u32,
        flags: 1, // UDMABUF_FLAGS_CLOEXEC
        offset: 0,
        size: size as u64,
    });
    // SAFETY: `Create` describes the ioctl faithfully (see its `Ioctl`
    // implementation).
    let dmabuf = unsafe { rustix::ioctl::ioctl(&device, create) }?;
    Ok(Some(dmabuf))
}

impl TestClient {
    /// Shows a dma-buf made elsewhere as the contents of the window, once
    /// the window exists and was configured. The compositor may refuse the
    /// buffer, which is a protocol error on this connection.
    pub fn attach_dmabuf(&mut self, buffer: &ExternalDmabuf<'_>) -> Result<(), ClientError> {
        let state = &mut self.state;
        let (Some(dmabuf), Some(window)) = (&state.globals.dmabuf, &state.window) else {
            return Err(ClientError::Protocol(
                "zwp_linux_dmabuf_v1 or the window is missing".to_string(),
            ));
        };
        if !window.configured {
            return Err(ClientError::Protocol(
                "the window is not configured yet".to_string(),
            ));
        }
        let params = dmabuf.create_params(&self.handle, ());
        params.add(
            buffer.fd.as_fd(),
            0,
            buffer.offset,
            buffer.stride,
            (buffer.modifier >> 32) as u32,
            buffer.modifier as u32,
        );
        let wl_buffer = params.create_immed(
            buffer.width as i32,
            buffer.height as i32,
            buffer.fourcc,
            if buffer.y_invert {
                zwp_linux_buffer_params_v1::Flags::YInvert
            } else {
                zwp_linux_buffer_params_v1::Flags::empty()
            },
            &self.handle,
            (),
        );
        params.destroy();
        window.surface.frame(&self.handle, ());
        window.surface.set_buffer_scale(1);
        window.surface.set_buffer_transform(buffer.transform.wl());
        window.surface.attach(Some(&wl_buffer), 0, 0);
        window
            .surface
            .damage_buffer(0, 0, buffer.width as i32, buffer.height as i32);
        window.surface.commit();
        state.needs_draw = false;
        state.events.push(ClientEvent::Drawn {
            width: buffer.width,
            height: buffer.height,
        });
        Ok(())
    }
}
