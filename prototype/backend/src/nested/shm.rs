//! Shared memory for `wl_shm` buffers: an anonymous file, mapped for
//! writing.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

/// A mapped anonymous file. The parent maps the same file through the
/// `wl_shm_pool` made from [`ShmMap::fd`].
pub(super) struct ShmMap {
    fd: OwnedFd,
    pointer: *mut u8,
    length: usize,
}

impl ShmMap {
    pub fn new(length: usize) -> io::Result<Self> {
        if length == 0 {
            return Err(io::Error::other("an empty shared memory buffer"));
        }
        let fd = rustix::fs::memfd_create("hizukuri-nested", rustix::fs::MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&fd, length as u64)?;
        // SAFETY: a fresh shared mapping of a file this process owns, at an
        // address the kernel picks. It is unmapped in `Drop`.
        let pointer = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                length,
                rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                rustix::mm::MapFlags::SHARED,
                &fd,
                0,
            )?
        };
        Ok(Self {
            fd,
            pointer: pointer.cast(),
            length,
        })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    pub fn len(&self) -> usize {
        self.length
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: the mapping is `length` bytes, readable and writable, and
        // lives as long as `self`. The parent only reads it.
        unsafe { std::slice::from_raw_parts_mut(self.pointer, self.length) }
    }
}

impl Drop for ShmMap {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `new`, unmapped once.
        unsafe {
            let _ = rustix::mm::munmap(self.pointer.cast(), self.length);
        }
    }
}
