//! Reading committed buffers on the CPU: undoing buffer transforms and
//! mapping dmabufs.

use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufMappingMode, DmabufSyncFlags};

use crate::{
    BufferContents, BufferTransform, DmabufContents, FOURCC_ABGR8888, FOURCC_ARGB8888,
    FOURCC_XBGR8888, FOURCC_XRGB8888, MODIFIER_LINEAR, PixelError, ShmFormat,
};

const BYTES_PER_PIXEL: usize = 4;

/// The memory layout of a DRM format that has a `wl_shm` equivalent.
pub(crate) fn layout_of_fourcc(fourcc: u32) -> Option<ShmFormat> {
    Some(match fourcc {
        FOURCC_ARGB8888 => ShmFormat::Argb8888,
        FOURCC_XRGB8888 => ShmFormat::Xrgb8888,
        FOURCC_ABGR8888 => ShmFormat::Abgr8888,
        FOURCC_XBGR8888 => ShmFormat::Xbgr8888,
        _ => return None,
    })
}

/// Rearranges pixels so that the image is upright: undoes `transform`.
/// The result is tightly packed.
pub(crate) fn upright(contents: &BufferContents, transform: BufferTransform) -> BufferContents {
    let size = transform.upright_size((contents.width, contents.height));
    let stride = size.0 as usize * BYTES_PER_PIXEL;
    let mut data = vec![0u8; stride * size.1 as usize];
    for y in 0..size.1 {
        for x in 0..size.0 {
            let (bx, by) = transform.buffer_position(size, x, y);
            let from = by as usize * contents.stride as usize + bx as usize * BYTES_PER_PIXEL;
            let to = y as usize * stride + x as usize * BYTES_PER_PIXEL;
            data[to..to + BYTES_PER_PIXEL]
                .copy_from_slice(&contents.data[from..from + BYTES_PER_PIXEL]);
        }
    }
    BufferContents {
        width: size.0,
        height: size.1,
        stride: stride as u32,
        format: contents.format,
        data,
    }
}

/// Copies the pixels out of a dmabuf by mapping it.
pub(crate) fn read_dmabuf(
    dmabuf: &Dmabuf,
    description: &DmabufContents,
) -> Result<BufferContents, PixelError> {
    let unsupported = || PixelError::Unsupported {
        fourcc: description.fourcc,
        modifier: description.modifier,
    };
    let format = layout_of_fourcc(description.fourcc).ok_or_else(unsupported)?;
    // Only memory that is known to be linear. A buffer without a modifier
    // (`DRM_FORMAT_MOD_INVALID`) has whatever layout its driver uses by
    // default, which may well be tiled; reading that row by row would show
    // garbage without any error.
    if description.planes.len() != 1 || description.modifier != MODIFIER_LINEAR {
        return Err(unsupported());
    }
    let (width, height) = (description.width as usize, description.height as usize);
    let stride = description.planes[0].stride as usize;
    let row = width * BYTES_PER_PIXEL;
    if width == 0 || height == 0 || stride < row {
        return Err(PixelError::TooSmall);
    }

    // The mapping starts at the plane's offset.
    let mapping = dmabuf
        .map_plane(0, DmabufMappingMode::READ)
        .map_err(|e| PixelError::Map(e.to_string()))?;
    if mapping.length() < stride * (height - 1) + row {
        return Err(PixelError::TooSmall);
    }
    // Tell the exporter that the memory is about to be read by the CPU. Not
    // every buffer supports it (a memfd standing in for a dmabuf does not);
    // the read is correct without it wherever there is nothing to sync.
    let synced = dmabuf
        .sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::READ)
        .is_ok();
    let mut data = vec![0u8; row * height];
    for y in 0..height {
        let source_row = if description.y_inverted {
            height - 1 - y
        } else {
            y
        };
        // SAFETY: the mapping is `mapping.length()` bytes long, and
        // `source_row * stride + row` is within it (checked above). The
        // client may write to the memory at the same time, so the bytes are
        // copied without forming a reference to the shared memory.
        unsafe {
            std::ptr::copy_nonoverlapping(
                (mapping.ptr() as *const u8).add(source_row * stride),
                data.as_mut_ptr().add(y * row),
                row,
            );
        }
    }
    if synced {
        let _ = dmabuf.sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::READ);
    }
    Ok(BufferContents {
        width: description.width,
        height: description.height,
        stride: row as u32,
        format,
        data,
    })
}

#[cfg(test)]
mod tests {
    use smithay::backend::allocator::dmabuf::DmabufFlags;
    use smithay::backend::allocator::{Buffer as _, Fourcc, Modifier};

    use super::*;

    // One byte per channel is enough to tell pixels apart.
    const R: [u8; 4] = [1, 0, 0, 0];
    const G: [u8; 4] = [2, 0, 0, 0];
    const B: [u8; 4] = [3, 0, 0, 0];
    const W: [u8; 4] = [4, 0, 0, 0];
    const O: [u8; 4] = [9, 0, 0, 0];

    /// A buffer whose corners are, in memory: top left `R`, top right `G`,
    /// bottom left `B`, bottom right `W`. The stride is wider than the row.
    fn corners(width: u32, height: u32) -> BufferContents {
        let stride = (width as usize + 3) * 4;
        let mut data = vec![0xee; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let pixel = match (x, y) {
                    (0, 0) => R,
                    (x, 0) if x == width as usize - 1 => G,
                    (0, y) if y == height as usize - 1 => B,
                    (x, y) if x == width as usize - 1 && y == height as usize - 1 => W,
                    _ => O,
                };
                data[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&pixel);
            }
        }
        BufferContents {
            width,
            height,
            stride: stride as u32,
            format: ShmFormat::Argb8888,
            data,
        }
    }

    fn pixel(image: &BufferContents, x: u32, y: u32) -> [u8; 4] {
        let at = y as usize * image.stride as usize + x as usize * 4;
        image.data[at..at + 4].try_into().unwrap()
    }

    /// The corners of the upright image for each transform, worked out from
    /// the wording of the protocol and not from the code: the buffer holds
    /// the image rotated counter-clockwise by the named angle (after a
    /// mirror around the vertical axis for the flipped ones), so the
    /// upright image is the buffer turned back clockwise.
    #[test]
    fn every_transform_is_undone() {
        // (transform, [top left, top right, bottom left, bottom right])
        let cases = [
            (BufferTransform::Normal, [R, G, B, W]),
            // Turned back 90 degrees clockwise: the buffer's bottom left
            // comes to the top left.
            (BufferTransform::Rotate90, [B, R, W, G]),
            (BufferTransform::Rotate180, [W, B, G, R]),
            // Turned back 90 degrees counter-clockwise.
            (BufferTransform::Rotate270, [G, W, R, B]),
            (BufferTransform::Flipped, [G, R, W, B]),
            // Mirror, then rotate: a transpose.
            (BufferTransform::Flipped90, [R, B, G, W]),
            // Mirror and half turn: upside down.
            (BufferTransform::Flipped180, [B, W, R, G]),
            // The other diagonal.
            (BufferTransform::Flipped270, [W, G, B, R]),
        ];
        for (transform, expected) in cases {
            let buffer = corners(5, 3);
            let image = upright(&buffer, transform);
            let size = if transform.swaps_axes() {
                (3, 5)
            } else {
                (5, 3)
            };
            assert_eq!((image.width, image.height), size, "{transform:?}");
            assert_eq!(image.stride, size.0 * 4, "{transform:?}");
            assert_eq!(image.data.len(), (size.0 * size.1 * 4) as usize);
            let (w, h) = (size.0 - 1, size.1 - 1);
            let actual = [
                pixel(&image, 0, 0),
                pixel(&image, w, 0),
                pixel(&image, 0, h),
                pixel(&image, w, h),
            ];
            assert_eq!(actual, expected, "{transform:?}");
            // Nothing but the four corners moved into a corner, and every
            // other pixel is still there.
            let others = image.data.chunks(4).filter(|p| *p == O).count();
            assert_eq!(others, 5 * 3 - 4, "{transform:?}");
        }
    }

    #[test]
    fn undoing_twice_gives_the_inverse_pairs() {
        // 90 and 270 undo each other; the rest are their own inverse.
        let buffer = corners(4, 2);
        let pairs = [
            (BufferTransform::Rotate90, BufferTransform::Rotate270),
            (BufferTransform::Rotate180, BufferTransform::Rotate180),
            (BufferTransform::Flipped, BufferTransform::Flipped),
            (BufferTransform::Flipped90, BufferTransform::Flipped90),
            (BufferTransform::Flipped180, BufferTransform::Flipped180),
            (BufferTransform::Flipped270, BufferTransform::Flipped270),
        ];
        let plain = upright(&buffer, BufferTransform::Normal);
        for (first, second) in pairs {
            let back = upright(&upright(&buffer, first), second);
            assert_eq!(back, plain, "{first:?} then {second:?}");
        }
    }

    fn memory_dmabuf(modifier: Modifier, pixels: &[u8], width: i32, height: i32) -> Dmabuf {
        use std::io::Write;
        let fd = smithay::reexports::rustix::fs::memfd_create(
            "hizukuri-wl-test",
            smithay::reexports::rustix::fs::MemfdFlags::CLOEXEC,
        )
        .unwrap();
        let mut file = std::fs::File::from(fd);
        file.write_all(pixels).unwrap();
        let mut builder = Dmabuf::builder(
            (width, height),
            Fourcc::Argb8888,
            modifier,
            DmabufFlags::empty(),
        );
        assert!(builder.add_plane(std::os::fd::OwnedFd::from(file), 0, width as u32 * 4));
        builder.build().unwrap()
    }

    fn description(dmabuf: &Dmabuf) -> DmabufContents {
        DmabufContents {
            width: dmabuf.size().w as u32,
            height: dmabuf.size().h as u32,
            fourcc: dmabuf.format().code as u32,
            modifier: u64::from(dmabuf.format().modifier),
            planes: vec![crate::DmabufPlane {
                fd: dmabuf
                    .handles()
                    .next()
                    .unwrap()
                    .try_clone_to_owned()
                    .unwrap(),
                offset: 0,
                stride: dmabuf.strides().next().unwrap(),
            }],
            y_inverted: false,
        }
    }

    /// Linear memory is read; memory whose layout is not stated is not
    /// taken for linear.
    #[test]
    fn a_buffer_without_a_modifier_is_not_read_as_linear() {
        let pixels: Vec<u8> = (0..4 * 3 * 4).map(|i| i as u8).collect();
        let linear = memory_dmabuf(Modifier::Linear, &pixels, 4, 3);
        let read = read_dmabuf(&linear, &description(&linear)).unwrap();
        assert_eq!((read.width, read.height, read.stride), (4, 3, 16));
        assert_eq!(read.data, pixels);

        let unstated = memory_dmabuf(Modifier::Invalid, &pixels, 4, 3);
        let contents = description(&unstated);
        assert_eq!(contents.modifier, crate::MODIFIER_INVALID);
        assert!(matches!(
            read_dmabuf(&unstated, &contents),
            Err(PixelError::Unsupported { modifier, .. }) if modifier == crate::MODIFIER_INVALID
        ));
    }

    #[test]
    fn only_formats_with_an_shm_layout_are_readable() {
        assert_eq!(layout_of_fourcc(FOURCC_ARGB8888), Some(ShmFormat::Argb8888));
        assert_eq!(layout_of_fourcc(FOURCC_XBGR8888), Some(ShmFormat::Xbgr8888));
        // NV12.
        assert_eq!(layout_of_fourcc(u32::from_le_bytes(*b"NV12")), None);
    }
}
