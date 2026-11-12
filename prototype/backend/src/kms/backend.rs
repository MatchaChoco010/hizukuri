//! The KMS backend proper: drives one output of a [`KmsDevice`] and
//! implements [`Backend`] for the compositor core.
//!
//! It is generic over the device and the clock, so the same code runs on
//! real hardware (with the session glue in `session`) and on the mock
//! device in tests.
//!
//! What it does:
//!
//! - plans a pipeline (connector, CRTC, primary plane, mode) with the pure
//!   functions in [`super::plan`], and picks the scanout path,
//! - keeps a small set of scanout buffers and rotates through them,
//! - turns the output on with a blocking modeset for the first frame (and
//!   the first frame after the session resumes), then page flips,
//! - paces frames with [`FramePacer`] and reports `Presented` with the
//!   timestamp of the flip,
//! - survives the session being paused and resumed, displays being
//!   unplugged and plugged back, and commits that fail.
//!
//! One output is driven. Which one is chosen once, at startup or when a
//! display first appears; other connectors are never touched.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use crate::target::{
    DRM_FORMAT_XRGB8888, OutputFormat, TargetDesc, TargetId, TargetProvider, Targets,
};
use proto_color::{CompositeSpace, ImageDescription};

use super::blob::{edid_identity, from_hex};
use super::clock::Clock;
use super::device::{DeviceEvent, DmabufDescriptor, DumbBuffer, KmsDevice};
use super::frame::{FlipStatistics, FramePacer, flip_statistics};
use super::input::{Hotkey, InputTranslator, OutputArea, RawInput, Translated};
use super::model::{CommitFlags, Fourcc, PropertyKind, property};
use super::plan::{
    OutputRequest, Pipeline, ScanoutFacts, ScanoutPath, ScanoutPreference, choose_scanout_path,
    disable_request, flip_request, modeset_request, plan_output, stale_planes,
};
use crate::{Backend, BackendError, BackendEvent, OutputId, OutputInfo, Presentation};

const EACCES: i32 = 13;
const EBUSY: i32 = 16;

/// Configuration of the KMS backend.
#[derive(Debug, Clone, PartialEq)]
pub struct KmsConfig {
    /// Which connector and mode to drive.
    pub output: OutputRequest,
    pub scanout: ScanoutPreference,
    /// Factor between logical coordinates and pixels.
    pub scale: f64,
    /// What the frames sent to the display mean.
    pub image_description: ImageDescription,
    pub composite_space: CompositeSpace,
    /// A flip that takes longer than this is given up on.
    pub flip_timeout: Duration,
}

impl Default for KmsConfig {
    fn default() -> Self {
        Self {
            output: OutputRequest::default(),
            scanout: ScanoutPreference::Auto,
            scale: 1.0,
            image_description: ImageDescription::SRGB_GAMMA22,
            composite_space: CompositeSpace::default(),
            flip_timeout: Duration::from_secs(1),
        }
    }
}

/// One scanout buffer.
struct Buffer {
    framebuffer: u32,
    /// dma-buf path: the render target whose image this framebuffer is.
    target: Option<TargetId>,
    /// CPU copy path: the dumb buffer behind the framebuffer.
    dumb: Option<DumbBuffer>,
}

/// How frames reach the plane, with the resources that needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveScanout {
    /// Render targets exported as dma-bufs are scanned out directly.
    Dmabuf,
    /// Frames are read back and copied into dumb buffers.
    CpuCopy,
}

impl ActiveScanout {
    pub fn name(self) -> &'static str {
        match self {
            ActiveScanout::Dmabuf => "dmabuf",
            ActiveScanout::CpuCopy => "cpu-copy",
        }
    }
}

struct Output {
    info: OutputInfo,
    pipeline: Pipeline,
    mode_blob: u32,
    scanout: ActiveScanout,
    buffers: Vec<Buffer>,
    /// CPU copy path: the offscreen target frames are rendered into.
    offscreen: Option<TargetId>,
    /// Index of the buffer the next frame goes into.
    back: usize,
    /// The next commit must be a full modeset.
    needs_modeset: bool,
    pacer: FramePacer,
    /// Timestamps of the last flips, newest last.
    flips: VecDeque<Duration>,
    presented: u64,
    last_sequence: u32,
}

/// What the backend reports about itself for state dumps and diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct KmsStatus {
    pub driver: String,
    pub session_active: bool,
    pub output: Option<KmsOutputStatus>,
    /// Commits the device refused since startup.
    pub failed_commits: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct KmsOutputStatus {
    pub connector: String,
    pub crtc: u32,
    pub plane: u32,
    pub mode: (u32, u32),
    pub refresh_mhz: u32,
    pub scanout: ActiveScanout,
    pub frames_presented: u64,
    pub flip_statistics: FlipStatistics,
}

const FLIP_HISTORY: usize = 600;

struct Inner<D: KmsDevice, C: Clock> {
    device: D,
    clock: C,
    config: KmsConfig,
    driver: String,
    events: VecDeque<BackendEvent>,
    output: Option<Output>,
    session_active: bool,
    translator: InputTranslator,
    next_output_id: u64,
    failed_commits: u64,
    /// The displays may have changed; `reconfigure` needs to look.
    rescan: bool,
    /// Whether what draws runs on the GPU that owns this device.
    draws_on_kms_device: bool,
    hotkeys: VecDeque<Hotkey>,
}

/// The KMS backend. Clones share the backend, so the event sources of the
/// session can feed it while the compositor core owns it.
pub struct KmsBackend<D: KmsDevice, C: Clock> {
    inner: Rc<RefCell<Inner<D, C>>>,
}

impl<D: KmsDevice, C: Clock> Clone for KmsBackend<D, C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<D: KmsDevice, C: Clock> KmsBackend<D, C> {
    /// Creates the backend and sets up the output if a display is
    /// connected. With a target provider the output gets render targets; without
    /// one it shows black frames, which is still enough to exercise modes,
    /// flips and timing.
    ///
    /// `draws_on_kms_device` tells whether what draws runs on the GPU
    /// that owns this KMS device; only then can its images be scanned out
    /// directly.
    pub fn new(
        mut device: D,
        clock: C,
        config: KmsConfig,
        targets: Targets<'_>,
        draws_on_kms_device: bool,
    ) -> Result<Self, BackendError> {
        let driver = device
            .snapshot()
            .map_err(|e| BackendError::Device(format!("reading the device resources: {e}")))?
            .driver
            .name;
        let mut inner = Inner {
            device,
            clock,
            config,
            driver,
            events: VecDeque::new(),
            output: None,
            session_active: true,
            translator: InputTranslator::new(),
            next_output_id: 1,
            failed_commits: 0,
            rescan: false,
            draws_on_kms_device,
            hotkeys: VecDeque::new(),
        };
        inner.setup_output(targets, true)?;
        Ok(Self {
            inner: Rc::new(RefCell::new(inner)),
        })
    }

    /// Reads the device's events (page flips). Call when its file
    /// descriptor is readable, or after driving the mock.
    pub fn handle_device_events(&self) -> Result<(), BackendError> {
        self.inner.borrow_mut().handle_device_events()
    }

    /// The session was paused (VT switched away): the device may no longer
    /// be used until [`Self::session_resumed`].
    pub fn session_paused(&self) {
        let mut inner = self.inner.borrow_mut();
        if !inner.session_active {
            return;
        }
        inner.session_active = false;
        if let Some(output) = &mut inner.output {
            output.pacer.session_paused();
        }
        // Whatever is held is released now: the real releases happen while
        // the session is away.
        let now_us = inner.clock.now().as_micros() as u64;
        for event in inner.translator.release_all(now_us) {
            inner.events.push_back(BackendEvent::Input(event));
        }
        inner.events.push_back(BackendEvent::SessionPaused);
    }

    /// The session is active again. The output needs a full modeset, and
    /// the displays may have changed while away.
    pub fn session_resumed(&self) {
        let mut inner = self.inner.borrow_mut();
        if inner.session_active {
            return;
        }
        inner.session_active = true;
        inner.translator.reset();
        let mut grant = None;
        if let Some(output) = &mut inner.output {
            output.needs_modeset = true;
            if output.pacer.session_resumed() {
                grant = Some(output.info.id);
            }
        }
        inner.rescan = true;
        inner.events.push_back(BackendEvent::Reconfigure);
        inner.events.push_back(BackendEvent::SessionResumed);
        if let Some(output) = grant {
            inner.events.push_back(BackendEvent::Frame { output });
        }
    }

    /// A display may have been plugged or unplugged (a udev change event).
    pub fn displays_changed(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.rescan = true;
        inner.events.push_back(BackendEvent::Reconfigure);
    }

    /// Gives up on flips that have been pending for too long. Call
    /// periodically.
    pub fn check_timeouts(&self) {
        let mut inner = self.inner.borrow_mut();
        let now = inner.clock.now();
        let Some(output) = &mut inner.output else {
            return;
        };
        if output.pacer.flip_overdue(now) {
            tracing::warn!(
                connector = %output.pipeline.connector_name,
                "a page flip did not complete in time; starting over with a modeset"
            );
            output.needs_modeset = true;
            let id = output.info.id;
            if output.pacer.flip_abandoned() {
                inner.events.push_back(BackendEvent::Frame { output: id });
            }
        }
    }

    /// Feeds an input event from a device. Emergency key combinations are
    /// taken out here, before anything else sees them: Ctrl+Alt+Backspace
    /// becomes [`BackendEvent::QuitRequested`], Ctrl+Alt+F<n> is handed to
    /// [`Self::take_hotkeys`] for the session to act on.
    pub fn push_input(&self, raw: RawInput) {
        let mut inner = self.inner.borrow_mut();
        let area = inner.output.as_ref().map(|o| OutputArea {
            position: o.info.position,
            logical_size: o.info.logical_size(),
        });
        match inner.translator.translate(raw, area) {
            Translated::Event(event) => inner.events.push_back(BackendEvent::Input(event)),
            Translated::Hotkey(Hotkey::Quit) => {
                tracing::info!("Ctrl+Alt+Backspace: quitting");
                inner.events.push_back(BackendEvent::QuitRequested);
            }
            Translated::Hotkey(hotkey) => inner.hotkeys.push_back(hotkey),
            Translated::Nothing => {}
        }
    }

    /// Takes the hotkeys the session has to act on (VT switches).
    pub fn take_hotkeys(&self) -> Vec<Hotkey> {
        self.inner.borrow_mut().hotkeys.drain(..).collect()
    }

    /// Describes the backend for state dumps and diagnostics.
    pub fn status(&self) -> KmsStatus {
        let inner = self.inner.borrow();
        KmsStatus {
            driver: inner.driver.clone(),
            session_active: inner.session_active,
            failed_commits: inner.failed_commits,
            output: inner.output.as_ref().map(|output| {
                let flips: Vec<Duration> = output.flips.iter().copied().collect();
                KmsOutputStatus {
                    connector: output.pipeline.connector_name.clone(),
                    crtc: output.pipeline.crtc,
                    plane: output.pipeline.primary_plane,
                    mode: output.pipeline.mode.size(),
                    refresh_mhz: output.pipeline.mode.refresh_mhz(),
                    scanout: output.scanout,
                    frames_presented: output.presented,
                    flip_statistics: flip_statistics(&flips, output.info.frame_interval()),
                }
            }),
        }
    }

    /// Turns the output off and frees its buffers. For a clean exit.
    pub fn shutdown(&self, targets: Targets<'_>) {
        self.inner.borrow_mut().teardown_output(targets, true);
    }

    /// Runs `f` with the device. For tests and for the session glue.
    pub fn with_device<T>(&self, f: impl FnOnce(&mut D) -> T) -> T {
        f(&mut self.inner.borrow_mut().device)
    }
}

impl<D: KmsDevice, C: Clock> Inner<D, C> {
    /// Plans and creates the output, if a display is connected. Having no
    /// display is not an error: one may still appear. `initial` is the
    /// setup at startup, where a configuration that can never work is.
    fn setup_output(
        &mut self,
        mut targets: Targets<'_>,
        initial: bool,
    ) -> Result<(), BackendError> {
        let snapshot = self
            .device
            .snapshot()
            .map_err(|e| BackendError::Device(format!("reading the device resources: {e}")))?;
        let pipeline = match plan_output(&snapshot, &self.config.output) {
            Ok(pipeline) => pipeline,
            Err(error) => {
                // A named connector that does not exist is a configuration
                // mistake; a display that is not there may still appear.
                let fatal =
                    initial && matches!(error, super::plan::PlanError::UnknownConnector { .. });
                if fatal {
                    return Err(BackendError::InvalidConfig(error.to_string()));
                }
                tracing::warn!("no output to drive: {error}");
                return Ok(());
            }
        };
        let (width, height) = pipeline.mode.size();

        let export_modifiers = targets
            .as_deref()
            .map(|r| r.scanout_modifiers())
            .unwrap_or_default();
        let facts = ScanoutFacts {
            preference: self.config.scanout,
            export_modifiers,
            draws_on_kms_device: self.draws_on_kms_device,
            plane_modifiers: pipeline.plane_modifiers.clone(),
            dumb_buffers: snapshot.caps.dumb_buffer,
        };
        let path =
            choose_scanout_path(&facts).map_err(|e| BackendError::InvalidConfig(e.to_string()))?;

        let desc = TargetDesc {
            width,
            height,
            format: OutputFormat::Bgra8Unorm,
            image_description: self.config.image_description,
            composite_space: self.config.composite_space,
        };

        // dma-buf path first; on failure fall back to copying unless the
        // user insisted.
        let mut created: Option<(ActiveScanout, Vec<Buffer>, Option<TargetId>)> = None;
        if let ScanoutPath::Dmabuf { modifiers } = &path {
            let targets = targets
                .as_deref_mut()
                .expect("the dma-buf path is only chosen with a target provider");
            match self.create_dmabuf_buffers(targets, &desc, modifiers) {
                Ok(buffers) => created = Some((ActiveScanout::Dmabuf, buffers, None)),
                Err(error) if self.config.scanout == ScanoutPreference::Auto => {
                    tracing::warn!("the dma-buf scanout path failed ({error}); copying on the CPU");
                }
                Err(error) => return Err(error),
            }
        }
        let (scanout, buffers, offscreen) = match created {
            Some(created) => created,
            None => {
                if !snapshot.caps.dumb_buffer {
                    return Err(BackendError::Device(
                        "the driver has no dumb buffers for the CPU copy path".to_string(),
                    ));
                }
                let buffers = self.create_dumb_buffers(width, height)?;
                let offscreen = match targets {
                    Some(targets) => Some(targets.create_target(&desc)?),
                    None => None,
                };
                (ActiveScanout::CpuCopy, buffers, offscreen)
            }
        };

        let mode_blob = self
            .device
            .create_mode_blob(&pipeline.mode)
            .map_err(|e| BackendError::Device(format!("creating the mode blob: {e}")))?;

        // Who made the display, from its EDID.
        let identity = snapshot
            .connector(pipeline.connector)
            .and_then(|c| property(&c.properties, "EDID"))
            .and_then(|p| match &p.kind {
                PropertyKind::Blob { data: Some(hex) } => from_hex(hex),
                _ => None,
            })
            .and_then(|edid| edid_identity(&edid));
        let (make, model) = match identity {
            Some(identity) => (
                identity.manufacturer,
                identity
                    .name
                    .unwrap_or_else(|| format!("{:#06x}", identity.product_code)),
            ),
            None => ("unknown".to_string(), "unknown".to_string()),
        };

        let id = OutputId(self.next_output_id);
        self.next_output_id += 1;
        let info = OutputInfo {
            id,
            name: pipeline.connector_name.clone(),
            make,
            model,
            size: (width, height),
            scale: self.config.scale,
            refresh_mhz: pipeline.mode.refresh_mhz(),
            position: (0, 0),
            image_description: self.config.image_description,
            format: OutputFormat::Bgra8Unorm,
            supported_image_descriptions: vec![self.config.image_description],
        };
        tracing::info!(
            connector = %pipeline.connector_name,
            crtc = pipeline.crtc,
            plane = pipeline.primary_plane,
            mode = ?pipeline.mode.size(),
            refresh_mhz = pipeline.mode.refresh_mhz(),
            scanout = scanout.name(),
            "KMS output"
        );
        self.events
            .push_back(BackendEvent::OutputAdded(info.clone()));
        let mut pacer = FramePacer::new(self.config.flip_timeout);
        if !self.session_active {
            pacer.session_paused();
        }
        self.output = Some(Output {
            info,
            pipeline,
            mode_blob,
            scanout,
            buffers,
            offscreen,
            back: 0,
            needs_modeset: true,
            pacer,
            flips: VecDeque::new(),
            presented: 0,
            last_sequence: 0,
        });
        Ok(())
    }

    fn create_dmabuf_buffers(
        &mut self,
        targets: &mut (dyn TargetProvider + 'static),
        desc: &TargetDesc,
        modifiers: &[u64],
    ) -> Result<Vec<Buffer>, BackendError> {
        let mut buffers: Vec<Buffer> = Vec::new();
        for _ in 0..BUFFER_COUNT {
            let result = (|| -> Result<Buffer, BackendError> {
                let (target, plane) = targets.create_scanout_target(desc, modifiers)?;
                debug_assert_eq!(plane.fourcc, DRM_FORMAT_XRGB8888);
                use std::os::fd::AsFd;
                let framebuffer = self
                    .device
                    .add_dmabuf_framebuffer(&DmabufDescriptor {
                        fd: plane.fd.as_fd(),
                        width: plane.width,
                        height: plane.height,
                        format: Fourcc(plane.fourcc),
                        modifier: plane.modifier,
                        stride: plane.stride,
                        offset: plane.offset,
                    })
                    .map_err(|e| {
                        targets.destroy_target(target);
                        BackendError::Device(format!("importing the scanout image: {e}"))
                    })?;
                // The framebuffer holds its own reference to the memory;
                // the file descriptor is not needed any more.
                Ok(Buffer {
                    framebuffer,
                    target: Some(target),
                    dumb: None,
                })
            })();
            match result {
                Ok(buffer) => buffers.push(buffer),
                Err(error) => {
                    for buffer in buffers {
                        let _ = self.device.remove_framebuffer(buffer.framebuffer);
                        if let Some(target) = buffer.target {
                            targets.destroy_target(target);
                        }
                    }
                    return Err(error);
                }
            }
        }
        Ok(buffers)
    }

    fn create_dumb_buffers(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<Vec<Buffer>, BackendError> {
        let mut buffers: Vec<Buffer> = Vec::new();
        for _ in 0..BUFFER_COUNT {
            let result = (|| -> std::io::Result<Buffer> {
                let dumb = self.device.create_dumb_buffer(width, height)?;
                // Start black rather than with whatever the memory holds.
                let cleared = self.device.write_dumb_buffer(&dumb, &mut |memory| {
                    memory.fill(0);
                    Ok(())
                });
                let framebuffer = cleared.and_then(|()| self.device.add_dumb_framebuffer(&dumb));
                match framebuffer {
                    Ok(framebuffer) => Ok(Buffer {
                        framebuffer,
                        target: None,
                        dumb: Some(dumb),
                    }),
                    Err(error) => {
                        let _ = self.device.destroy_dumb_buffer(&dumb);
                        Err(error)
                    }
                }
            })();
            match result {
                Ok(buffer) => buffers.push(buffer),
                Err(error) => {
                    for buffer in buffers {
                        let _ = self.device.remove_framebuffer(buffer.framebuffer);
                        if let Some(dumb) = buffer.dumb {
                            let _ = self.device.destroy_dumb_buffer(&dumb);
                        }
                    }
                    return Err(BackendError::Device(format!(
                        "creating a dumb buffer: {error}"
                    )));
                }
            }
        }
        Ok(buffers)
    }

    /// Frees the output's resources. With `disable` the output is turned
    /// off first (when the session allows it).
    fn teardown_output(&mut self, mut targets: Targets<'_>, disable: bool) {
        let Some(output) = self.output.take() else {
            return;
        };
        if disable && self.session_active {
            let flags = CommitFlags {
                page_flip_event: false,
                ..CommitFlags::MODESET
            };
            if let Err(error) = self
                .device
                .commit(&disable_request(&output.pipeline), flags)
            {
                tracing::debug!("turning the output off failed: {error}");
            }
        }
        for buffer in output.buffers {
            let _ = self.device.remove_framebuffer(buffer.framebuffer);
            if let Some(dumb) = buffer.dumb {
                let _ = self.device.destroy_dumb_buffer(&dumb);
            }
            if let (Some(target), Some(targets)) = (buffer.target, targets.as_deref_mut()) {
                targets.destroy_target(target);
            }
        }
        if let (Some(target), Some(targets)) = (output.offscreen, targets) {
            targets.destroy_target(target);
        }
        let _ = self.device.destroy_blob(output.mode_blob);
        self.events
            .push_back(BackendEvent::OutputRemoved(output.info.id));
    }

    fn handle_device_events(&mut self) -> Result<(), BackendError> {
        let events = self
            .device
            .read_events()
            .map_err(|e| BackendError::Device(format!("reading device events: {e}")))?;
        for event in events {
            let DeviceEvent::PageFlip {
                crtc,
                sequence,
                time,
            } = event;
            let Some(output) = &mut self.output else {
                continue;
            };
            if crtc != output.pipeline.crtc {
                continue;
            }
            let completed = output.pacer.flip_completed();
            if !completed.expected {
                // A flip from before a pause, or one already given up on.
                continue;
            }
            output.presented += 1;
            output.last_sequence = sequence;
            output.flips.push_back(time);
            if output.flips.len() > FLIP_HISTORY {
                output.flips.pop_front();
            }
            let id = output.info.id;
            let presentation = Presentation {
                time,
                refresh: output.info.frame_interval(),
                sequence: sequence as u64,
            };
            self.events.push_back(BackendEvent::Presented {
                output: id,
                presentation,
            });
            if completed.grant {
                self.events.push_back(BackendEvent::Frame { output: id });
            }
        }
        Ok(())
    }
}

/// Scanout buffers per output: one on screen, one being drawn.
const BUFFER_COUNT: usize = 2;

impl<D: KmsDevice, C: Clock> Backend for KmsBackend<D, C> {
    fn name(&self) -> &'static str {
        "kms"
    }

    fn outputs(&self) -> Vec<OutputInfo> {
        self.inner
            .borrow()
            .output
            .iter()
            .map(|o| o.info.clone())
            .collect()
    }

    fn is_active(&self) -> bool {
        self.inner.borrow().session_active
    }

    fn render_target(&mut self, output: OutputId) -> Option<TargetId> {
        let inner = self.inner.borrow();
        if !inner.session_active {
            return None;
        }
        let state = inner.output.as_ref().filter(|o| o.info.id == output)?;
        match state.scanout {
            ActiveScanout::Dmabuf => state.buffers[state.back].target,
            ActiveScanout::CpuCopy => state.offscreen,
        }
    }

    fn next_presentation(&self, output: OutputId) -> Option<Duration> {
        let inner = self.inner.borrow();
        let state = inner.output.as_ref().filter(|o| o.info.id == output)?;
        let last = *state.flips.back()?;
        Some(super::frame::next_vblank(
            last,
            state.info.frame_interval(),
            inner.clock.now(),
            state.pacer.flip_pending(),
        ))
    }

    fn request_frame(&mut self, output: OutputId) {
        let mut inner = self.inner.borrow_mut();
        let Some(state) = inner.output.as_mut().filter(|o| o.info.id == output) else {
            return;
        };
        if state.pacer.request_frame() {
            inner.events.push_back(BackendEvent::Frame { output });
        }
    }

    fn present(&mut self, targets: Targets<'_>, output: OutputId) -> Result<(), BackendError> {
        let mut inner = self.inner.borrow_mut();
        let inner = &mut *inner;
        if !inner.session_active {
            return Err(BackendError::SessionInactive);
        }
        let state = inner
            .output
            .as_mut()
            .filter(|o| o.info.id == output)
            .ok_or(BackendError::UnknownOutput)?;
        if !state.pacer.can_present() {
            // The core presents only in answer to a Frame event, so this is
            // a frame drawn out of turn (a verification capture). It will
            // be shown with the next regular frame.
            state.pacer.owe_frame();
            return Ok(());
        }

        let buffer = &state.buffers[state.back];
        if let (ActiveScanout::CpuCopy, Some(target), Some(dumb), Some(targets)) =
            (state.scanout, state.offscreen, buffer.dumb, targets)
        {
            let stride = dumb.stride as usize;
            let mut render_error = None;
            inner
                .device
                .write_dumb_buffer(&dumb, &mut |memory| {
                    if let Err(error) = targets.read_encoded_into(target, memory, stride) {
                        render_error = Some(error);
                    }
                    Ok(())
                })
                .map_err(|e| BackendError::Device(format!("writing the dumb buffer: {e}")))?;
            match render_error {
                // Nothing was rendered yet: show the buffer as it is.
                Some(crate::target::TargetError::TargetNotRendered) | None => {}
                Some(error) => return Err(error.into()),
            }
        }

        let (request, flags) = if state.needs_modeset {
            (
                modeset_request(&state.pipeline, buffer.framebuffer, state.mode_blob),
                CommitFlags::MODESET,
            )
        } else {
            (
                flip_request(&state.pipeline, buffer.framebuffer),
                CommitFlags::FLIP,
            )
        };
        match inner.device.commit(&request, flags) {
            Ok(()) => {
                state.needs_modeset = false;
                state.pacer.flip_submitted(inner.clock.now());
                state.back = (state.back + 1) % state.buffers.len();
                Ok(())
            }
            Err(error) => {
                inner.failed_commits += 1;
                match error.raw_os_error() {
                    // Lost DRM master before being told: the pause
                    // notification is on its way. The frame is redrawn
                    // after the session resumes.
                    Some(EACCES) => {
                        tracing::debug!("commit refused without DRM master");
                        state.needs_modeset = true;
                        state.pacer.owe_frame();
                        Ok(())
                    }
                    // The device still has a flip in flight that we did not
                    // know of. Wait for it (or for the timeout).
                    Some(EBUSY) => {
                        tracing::debug!("commit refused: a flip is still pending");
                        state.pacer.flip_submitted(inner.clock.now());
                        state.pacer.owe_frame();
                        Ok(())
                    }
                    _ => {
                        // Anything else: try again from a known state.
                        tracing::warn!("atomic commit failed: {error}; will retry with a modeset");
                        let was_modeset = state.needs_modeset;
                        state.needs_modeset = true;
                        if was_modeset {
                            // Even a full modeset is refused: report it.
                            Err(BackendError::Device(format!(
                                "atomic modeset failed: {error}"
                            )))
                        } else {
                            // Nothing is in flight, so the frame can be
                            // tried again right away.
                            let id = state.info.id;
                            inner.events.push_back(BackendEvent::Frame { output: id });
                            Ok(())
                        }
                    }
                }
            }
        }
    }

    fn reconfigure(&mut self, mut targets: Targets<'_>) -> Result<(), BackendError> {
        let mut inner = self.inner.borrow_mut();
        if !std::mem::take(&mut inner.rescan) || !inner.session_active {
            // Looked at when the session comes back.
            if !inner.session_active {
                inner.rescan = true;
            }
            return Ok(());
        }
        let snapshot = inner
            .device
            .snapshot()
            .map_err(|e| BackendError::Device(format!("reading the device resources: {e}")))?;
        match &mut inner.output {
            Some(output) => {
                // Whoever had the display in the meantime may have left
                // planes on our CRTC; the next modeset takes them off.
                output.pipeline.stale_planes = stale_planes(
                    &snapshot,
                    output.pipeline.crtc,
                    output.pipeline.primary_plane,
                );
                let connected = snapshot
                    .connector(output.pipeline.connector)
                    .is_some_and(|c| c.connected);
                if !connected {
                    tracing::info!(connector = %output.pipeline.connector_name, "display unplugged");
                    inner.teardown_output(targets.as_deref_mut(), true);
                    // Another display may be there to take over.
                    inner.setup_output(targets, false)?;
                }
            }
            None => inner.setup_output(targets, false)?,
        }
        Ok(())
    }

    fn shutdown(&mut self, targets: Targets<'_>) {
        KmsBackend::shutdown(self, targets);
    }

    fn describe(&self) -> serde_json::Value {
        let status = self.status();
        serde_json::json!({
            "driver": status.driver,
            "session_active": status.session_active,
            "failed_commits": status.failed_commits,
            "output": status.output.map(|output| serde_json::json!({
                "connector": output.connector,
                "crtc": output.crtc,
                "plane": output.plane,
                "mode": [output.mode.0, output.mode.1],
                "refresh_mhz": output.refresh_mhz,
                "scanout": output.scanout.name(),
                "frames_presented": output.frames_presented,
                "flip_interval_ms": {
                    "count": output.flip_statistics.count,
                    "mean": output.flip_statistics.mean_ms,
                    "median": output.flip_statistics.median_ms,
                    "min": output.flip_statistics.min_ms,
                    "max": output.flip_statistics.max_ms,
                    "late": output.flip_statistics.late,
                },
            })),
        })
    }

    fn drain_events(&mut self) -> Vec<BackendEvent> {
        self.inner.borrow_mut().events.drain(..).collect()
    }
}
