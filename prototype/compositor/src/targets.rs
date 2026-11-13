//! The images of the outputs, made with the checked GPU API of
//! `proto-gfx-core`, as the backend sees them ([`TargetProvider`]).
//!
//! An offscreen target has two images: whatever draws draws into the one
//! that is not shown ([`OutputImage::back`]), and a frame that fails never
//! replaces the one that is. A scanout target has one image, exported as a
//! dma-buf; the backend rotates between several targets.
//!
//! The script's drawing ([`ScriptTargets`](crate::script::ScriptTargets))
//! and the KMS self test ([`PatternRenderer`](crate::selftest::PatternRenderer))
//! keep their targets in an [`OutputImages`].

use std::collections::HashMap;

use proto_backend::{
    DRM_FORMAT_XRGB8888, DmabufPlane, OutputFormat, TargetDesc, TargetError, TargetId,
};
use proto_gfx_core::{ErrorCode, Format, GfxError, Gpu, Handle, ImageDesc, ImageUsage};

/// The image format of an output format.
pub fn output_format(format: OutputFormat) -> Format {
    match format {
        OutputFormat::Rgba8Unorm => Format::Rgba8Unorm,
        OutputFormat::Rgba16Unorm => Format::Rgba16Unorm,
        OutputFormat::Bgra8Unorm => Format::Bgra8Unorm,
    }
}

fn provider_error(context: &str, error: GfxError) -> TargetError {
    TargetError::Provider(format!("{context}: {error}"))
}

/// The image(s) of one output target.
pub struct OutputImage {
    pub desc: TargetDesc,
    /// One image for a scanout target, two for an offscreen one.
    pub images: Vec<Handle>,
    /// The image that is shown.
    pub front: usize,
    /// Whether a frame was drawn into it yet.
    pub rendered: bool,
}

impl OutputImage {
    /// The image to draw the next frame into.
    pub fn back(&self) -> Handle {
        self.images[(self.front + 1) % self.images.len()]
    }

    /// The image is exported for scanout: the display reads it right after
    /// it was drawn.
    pub fn is_scanout(&self) -> bool {
        self.images.len() == 1
    }

    /// The frame in [`Self::back`] is complete: show it.
    pub fn flip(&mut self) {
        self.front = (self.front + 1) % self.images.len();
        self.rendered = true;
    }

    /// The image of the last frame drawn.
    pub fn shown(&self) -> Result<Handle, TargetError> {
        if !self.rendered {
            return Err(TargetError::TargetNotRendered);
        }
        Ok(self.images[self.front])
    }
}

/// The targets, by id.
pub struct OutputImages {
    targets: HashMap<u64, OutputImage>,
    next: u64,
}

impl Default for OutputImages {
    fn default() -> Self {
        Self {
            targets: HashMap::new(),
            next: 1,
        }
    }
}

impl OutputImages {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: TargetId) -> Option<&OutputImage> {
        self.targets.get(&id.raw())
    }

    pub fn get_mut(&mut self, id: TargetId) -> Option<&mut OutputImage> {
        self.targets.get_mut(&id.raw())
    }

    fn insert(&mut self, desc: &TargetDesc, images: Vec<Handle>) -> TargetId {
        let id = self.next;
        self.next += 1;
        self.targets.insert(
            id,
            OutputImage {
                desc: *desc,
                images,
                front: 0,
                rendered: false,
            },
        );
        TargetId::from_raw(id)
    }

    /// An offscreen target ([`TargetProvider::create_target`]).
    ///
    /// [`TargetProvider::create_target`]: proto_backend::TargetProvider::create_target
    pub fn create(&mut self, gpu: &mut Gpu, desc: &TargetDesc) -> Result<TargetId, TargetError> {
        let mut images = Vec::new();
        for _ in 0..2 {
            let image = gpu
                .create_image(&ImageDesc {
                    format: output_format(desc.format),
                    width: desc.width,
                    height: desc.height,
                    usage: ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
                })
                .map_err(|e| provider_error("creating an output image", e));
            match image {
                Ok(image) => {
                    gpu.protect(image);
                    images.push(image);
                }
                Err(error) => {
                    for image in images {
                        let _ = gpu.destroy(image);
                    }
                    return Err(error);
                }
            }
        }
        Ok(self.insert(desc, images))
    }

    /// A target exported for scanout
    /// ([`TargetProvider::create_scanout_target`]).
    ///
    /// [`TargetProvider::create_scanout_target`]: proto_backend::TargetProvider::create_scanout_target
    pub fn create_scanout(
        &mut self,
        gpu: &mut Gpu,
        desc: &TargetDesc,
        allowed_modifiers: &[u64],
    ) -> Result<(TargetId, DmabufPlane), TargetError> {
        if desc.format != OutputFormat::Bgra8Unorm {
            return Err(TargetError::UnsupportedFormat(
                "scanout targets must be bgra8 (DRM XRGB8888)",
            ));
        }
        let (image, plane) = gpu
            .create_scanout_image(desc.width, desc.height, allowed_modifiers)
            .map_err(|e| match e.code {
                ErrorCode::Unsupported => TargetError::ScanoutUnsupported,
                _ => provider_error("creating a scanout image", e),
            })?;
        gpu.protect(image);
        let id = self.insert(desc, vec![image]);
        Ok((
            id,
            DmabufPlane {
                fd: plane.fd,
                width: desc.width,
                height: desc.height,
                fourcc: DRM_FORMAT_XRGB8888,
                modifier: plane.modifier,
                stride: plane.stride,
                offset: plane.offset,
            },
        ))
    }

    pub fn destroy(&mut self, gpu: &mut Gpu, id: TargetId) {
        if let Some(target) = self.targets.remove(&id.raw()) {
            for image in target.images {
                let _ = gpu.destroy(image);
            }
        }
    }

    /// The image of the last frame drawn into a target.
    pub fn shown(&self, id: TargetId) -> Result<Handle, TargetError> {
        self.get(id).ok_or(TargetError::UnknownTarget)?.shown()
    }

    /// Copies the last frame drawn into a target to `destination`
    /// ([`TargetProvider::read_encoded_into`]). The GPU frame that drew it
    /// must have ended.
    ///
    /// [`TargetProvider::read_encoded_into`]: proto_backend::TargetProvider::read_encoded_into
    pub fn read_into(
        &self,
        gpu: &mut Gpu,
        id: TargetId,
        destination: &mut [u8],
        stride: usize,
    ) -> Result<(), TargetError> {
        let image = self.shown(id)?;
        gpu.read_image_into(image, destination, stride)
            .map_err(|e| provider_error("reading an output image", e))
    }
}
