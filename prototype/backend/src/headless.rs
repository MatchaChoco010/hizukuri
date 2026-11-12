//! The headless backend: virtual outputs rendered offscreen.
//!
//! Nothing is shown anywhere. Frames stay in the render target, from where
//! the control interface reads them back. A virtual frame clock stands in
//! for vblank: a requested frame is granted at the next tick of the clock,
//! and a presented frame is reported as shown right away.
//!
//! There are no input devices and no session. Input reaches the compositor
//! through the control interface, which feeds the same handler that backend
//! input events go to.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::target::{OutputFormat, TargetDesc, TargetId, Targets};
use calloop::LoopHandle;
use calloop::timer::{TimeoutAction, Timer};
use proto_color::{CompositeSpace, ImageDescription};

use crate::{
    Backend, BackendError, BackendEvent, BackendHost, OutputId, OutputInfo, Presentation,
    monotonic_now,
};

/// Configuration of one virtual output.
#[derive(Debug, Clone, PartialEq)]
pub struct HeadlessOutputConfig {
    pub name: String,
    /// Size in physical pixels.
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    /// Refresh rate in mHz.
    pub refresh_mhz: u32,
    /// What the encoded frames mean.
    pub image_description: ImageDescription,
    pub format: OutputFormat,
}

impl Default for HeadlessOutputConfig {
    fn default() -> Self {
        Self {
            name: "HEADLESS-1".to_string(),
            width: 1280,
            height: 720,
            scale: 1.0,
            refresh_mhz: 60_000,
            // An SDR display as content without color information assumes
            // it: BT.709 primaries, gamma 2.2. Such content passes through
            // unchanged.
            image_description: ImageDescription::SRGB_GAMMA22,
            format: OutputFormat::Rgba8Unorm,
        }
    }
}

/// Configuration of the headless backend.
#[derive(Debug, Clone, PartialEq)]
pub struct HeadlessConfig {
    /// Outputs, laid out left to right in the global logical space.
    pub outputs: Vec<HeadlessOutputConfig>,
    /// The space surfaces are blended in.
    pub composite_space: CompositeSpace,
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            outputs: vec![HeadlessOutputConfig::default()],
            composite_space: CompositeSpace::default(),
        }
    }
}

struct Output {
    info: OutputInfo,
    target: Option<TargetId>,
    sequence: u64,
    frame_requested: bool,
}

/// State shared between the backend object and its timer callbacks.
struct Shared {
    outputs: Vec<Output>,
    events: VecDeque<BackendEvent>,
}

impl Shared {
    /// One tick of the frame clock of an output. Returns whether an event
    /// was queued.
    fn tick(&mut self, output: OutputId) -> bool {
        match self.outputs.iter_mut().find(|o| o.info.id == output) {
            Some(o) if o.frame_requested => {
                o.frame_requested = false;
                self.events.push_back(BackendEvent::Frame { output });
                true
            }
            _ => false,
        }
    }
}

/// See the module documentation.
pub struct HeadlessBackend {
    shared: Rc<RefCell<Shared>>,
}

impl HeadlessBackend {
    /// Creates the backend and, when a target provider is given, one render target
    /// per output. Without one the backend still provides outputs
    /// and frame timing, which is enough for protocol tests.
    ///
    /// The outputs are announced with [`BackendEvent::OutputAdded`].
    pub fn new(config: &HeadlessConfig, mut targets: Targets<'_>) -> Result<Self, BackendError> {
        if config.outputs.is_empty() {
            return Err(BackendError::InvalidConfig(
                "at least one output is required".to_string(),
            ));
        }
        let mut outputs = Vec::new();
        let mut events = VecDeque::new();
        let mut x = 0;
        for (index, output) in config.outputs.iter().enumerate() {
            if output.width == 0 || output.height == 0 {
                return Err(BackendError::InvalidConfig(format!(
                    "output {} has an empty size",
                    output.name
                )));
            }
            if !(output.scale.is_finite() && output.scale > 0.0) {
                return Err(BackendError::InvalidConfig(format!(
                    "output {} has an invalid scale {}",
                    output.name, output.scale
                )));
            }
            if output.refresh_mhz == 0 {
                return Err(BackendError::InvalidConfig(format!(
                    "output {} has a zero refresh rate",
                    output.name
                )));
            }
            let target = match targets.as_deref_mut() {
                Some(targets) => Some(targets.create_target(&TargetDesc {
                    width: output.width,
                    height: output.height,
                    format: output.format,
                    image_description: output.image_description,
                    composite_space: config.composite_space,
                })?),
                None => None,
            };
            let info = OutputInfo {
                id: OutputId(index as u64 + 1),
                name: output.name.clone(),
                make: "hizukuri".to_string(),
                model: "headless".to_string(),
                size: (output.width, output.height),
                scale: output.scale,
                refresh_mhz: output.refresh_mhz,
                position: (x, 0),
                image_description: output.image_description,
                format: output.format,
                // A virtual output can be driven with anything; it is
                // configured with exactly one image description.
                supported_image_descriptions: vec![output.image_description],
            };
            x += info.logical_size().0;
            events.push_back(BackendEvent::OutputAdded(info.clone()));
            outputs.push(Output {
                info,
                target,
                sequence: 0,
                frame_requested: false,
            });
        }
        Ok(Self {
            shared: Rc::new(RefCell::new(Shared { outputs, events })),
        })
    }

    /// Registers the virtual frame clocks on an event loop.
    pub fn insert_sources<D: BackendHost + 'static>(
        &self,
        handle: &LoopHandle<'static, D>,
    ) -> Result<(), BackendError> {
        let clocks: Vec<_> = self
            .shared
            .borrow()
            .outputs
            .iter()
            .map(|o| (o.info.id, o.info.frame_interval()))
            .collect();
        for (output, interval) in clocks {
            let shared = self.shared.clone();
            handle
                .insert_source(Timer::from_duration(interval), move |_, _, data: &mut D| {
                    // Release the borrow before calling into the host, which
                    // will call back into the backend.
                    let queued = shared.borrow_mut().tick(output);
                    if queued {
                        data.backend_wake();
                    }
                    TimeoutAction::ToDuration(interval)
                })
                .map_err(|e| BackendError::EventLoop(e.to_string()))?;
        }
        Ok(())
    }

    /// Advances the virtual frame clock of every output by one tick, as the
    /// timers registered by [`Self::insert_sources`] do. For callers that
    /// drive the compositor without an event loop (tests).
    pub fn tick(&mut self) {
        let mut shared = self.shared.borrow_mut();
        let ids: Vec<OutputId> = shared.outputs.iter().map(|o| o.info.id).collect();
        for id in ids {
            shared.tick(id);
        }
    }
}

impl Backend for HeadlessBackend {
    fn name(&self) -> &'static str {
        "headless"
    }

    /// The render target keeps the description it was made with; what is
    /// encoded follows the output's [`OutputInfo`].
    fn set_output_image_description(
        &mut self,
        output: OutputId,
        description: ImageDescription,
    ) -> Result<(), BackendError> {
        let mut shared = self.shared.borrow_mut();
        let Some(o) = shared.outputs.iter_mut().find(|o| o.info.id == output) else {
            return Err(BackendError::InvalidConfig(format!(
                "unknown output {}",
                output.0
            )));
        };
        if o.info.image_description != description {
            o.info.image_description = description;
            o.info.supported_image_descriptions = vec![description];
            let info = o.info.clone();
            shared.events.push_back(BackendEvent::OutputChanged(info));
        }
        Ok(())
    }

    fn outputs(&self) -> Vec<OutputInfo> {
        self.shared
            .borrow()
            .outputs
            .iter()
            .map(|o| o.info.clone())
            .collect()
    }

    fn is_active(&self) -> bool {
        // There is no session to lose.
        true
    }

    fn render_target(&mut self, output: OutputId) -> Option<TargetId> {
        self.shared
            .borrow()
            .outputs
            .iter()
            .find(|o| o.info.id == output)
            .and_then(|o| o.target)
    }

    fn request_frame(&mut self, output: OutputId) {
        if let Some(o) = self
            .shared
            .borrow_mut()
            .outputs
            .iter_mut()
            .find(|o| o.info.id == output)
        {
            o.frame_requested = true;
        }
    }

    fn present(&mut self, _targets: Targets<'_>, output: OutputId) -> Result<(), BackendError> {
        let mut shared = self.shared.borrow_mut();
        let o = shared
            .outputs
            .iter_mut()
            .find(|o| o.info.id == output)
            .ok_or(BackendError::UnknownOutput)?;
        // There is nowhere to show the frame; it stays in the render target
        // and counts as shown immediately.
        o.sequence += 1;
        let presentation = Presentation {
            time: monotonic_now(),
            refresh: o.info.frame_interval(),
            sequence: o.sequence,
        };
        shared.events.push_back(BackendEvent::Presented {
            output,
            presentation,
        });
        Ok(())
    }

    fn drain_events(&mut self) -> Vec<BackendEvent> {
        self.shared.borrow_mut().events.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn added(event: &BackendEvent) -> &OutputInfo {
        match event {
            BackendEvent::OutputAdded(info) => info,
            other => panic!("expected OutputAdded, got {other:?}"),
        }
    }

    #[test]
    fn outputs_are_announced_and_laid_out_left_to_right() {
        let config = HeadlessConfig {
            outputs: vec![
                HeadlessOutputConfig {
                    name: "A".into(),
                    width: 1920,
                    height: 1080,
                    scale: 2.0,
                    ..Default::default()
                },
                HeadlessOutputConfig {
                    name: "B".into(),
                    width: 800,
                    height: 600,
                    image_description: ImageDescription::BT2020_PQ,
                    format: OutputFormat::Rgba16Unorm,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut backend = HeadlessBackend::new(&config, None).unwrap();
        let events = backend.drain_events();
        assert_eq!(events.len(), 2);
        let (a, b) = (added(&events[0]), added(&events[1]));
        assert_eq!(a.logical_size(), (960, 540));
        assert_eq!(a.position, (0, 0));
        assert_eq!(b.position, (960, 0));
        assert_eq!(b.image_description, ImageDescription::BT2020_PQ);
        assert_eq!(b.format, OutputFormat::Rgba16Unorm);
        assert_eq!(backend.outputs(), vec![a.clone(), b.clone()]);
        // Without a target provider there is nothing to draw into.
        assert_eq!(backend.render_target(a.id), None);
        assert!(backend.is_active());
    }

    #[test]
    fn default_output_is_bt709_gamma22() {
        let backend = HeadlessBackend::new(&HeadlessConfig::default(), None).unwrap();
        let output = &backend.outputs()[0];
        assert_eq!(output.image_description, ImageDescription::SRGB_GAMMA22);
        assert_eq!(output.size, (1280, 720));
        assert_eq!(output.frame_interval(), Duration::from_nanos(16_666_666));
    }

    #[test]
    fn frames_are_granted_only_when_requested() {
        let mut backend = HeadlessBackend::new(&HeadlessConfig::default(), None).unwrap();
        let id = backend.outputs()[0].id;
        backend.drain_events();

        backend.tick();
        assert!(backend.drain_events().is_empty(), "no request, no frame");

        backend.request_frame(id);
        backend.request_frame(id);
        backend.request_frame(OutputId(99)); // unknown: ignored
        backend.tick();
        assert_eq!(
            backend.drain_events(),
            vec![BackendEvent::Frame { output: id }]
        );
        backend.tick();
        assert!(backend.drain_events().is_empty(), "one request, one frame");
    }

    #[test]
    fn present_reports_presentation_and_counts_frames() {
        let mut backend = HeadlessBackend::new(&HeadlessConfig::default(), None).unwrap();
        let id = backend.outputs()[0].id;
        backend.drain_events();
        backend.present(None, id).unwrap();
        backend.present(None, id).unwrap();
        let events = backend.drain_events();
        let sequences: Vec<u64> = events
            .iter()
            .map(|e| match e {
                BackendEvent::Presented {
                    output,
                    presentation,
                } => {
                    assert_eq!(*output, id);
                    assert_eq!(presentation.refresh, Duration::from_nanos(16_666_666));
                    presentation.sequence
                }
                other => panic!("unexpected event {other:?}"),
            })
            .collect();
        assert_eq!(sequences, [1, 2]);
        assert!(matches!(
            backend.present(None, OutputId(99)),
            Err(BackendError::UnknownOutput)
        ));
    }

    struct Host {
        backend: HeadlessBackend,
        frames: Vec<OutputId>,
    }

    impl BackendHost for Host {
        fn backend_wake(&mut self) {
            for event in self.backend.drain_events() {
                if let BackendEvent::Frame { output } = event {
                    self.frames.push(output);
                }
            }
        }
    }

    #[test]
    fn frame_clock_runs_on_an_event_loop() {
        let config = HeadlessConfig {
            outputs: vec![HeadlessOutputConfig {
                refresh_mhz: 1_000_000, // 1 kHz keeps the test short
                ..Default::default()
            }],
            ..Default::default()
        };
        let backend = HeadlessBackend::new(&config, None).unwrap();
        let id = backend.outputs()[0].id;
        let mut event_loop = calloop::EventLoop::<Host>::try_new().unwrap();
        backend.insert_sources(&event_loop.handle()).unwrap();
        let mut host = Host {
            backend,
            frames: Vec::new(),
        };
        host.backend.request_frame(id);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while host.frames.is_empty() && std::time::Instant::now() < deadline {
            event_loop
                .dispatch(Duration::from_millis(10), &mut host)
                .unwrap();
        }
        assert_eq!(host.frames, [id]);
    }

    #[test]
    fn invalid_configurations_are_rejected() {
        let bad = |output: HeadlessOutputConfig| {
            HeadlessBackend::new(
                &HeadlessConfig {
                    outputs: vec![output],
                    ..Default::default()
                },
                None,
            )
            .is_err()
        };
        assert!(bad(HeadlessOutputConfig {
            width: 0,
            ..Default::default()
        }));
        assert!(bad(HeadlessOutputConfig {
            scale: 0.0,
            ..Default::default()
        }));
        assert!(bad(HeadlessOutputConfig {
            refresh_mhz: 0,
            ..Default::default()
        }));
        assert!(
            HeadlessBackend::new(
                &HeadlessConfig {
                    outputs: vec![],
                    ..Default::default()
                },
                None
            )
            .is_err()
        );
    }
}
