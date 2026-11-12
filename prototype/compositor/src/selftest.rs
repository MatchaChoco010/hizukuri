//! The KMS self test: drive one output with a test pattern for a while and
//! write down what happened.
//!
//! This is for the things that cannot be checked without a real display
//! path: that the mode can be set, that frames reach the screen through the
//! chosen scanout path, and how regular the page flips are. One command runs
//! it, it ends on its own, and it leaves a report (`report.json`) that says
//! what passed. Items that are not implemented yet (HDR output, variable
//! refresh, tearing) are listed as such, so the report has the same shape
//! when they are added.
//!
//! The test itself ([`SelfTest`]) is generic over the backend. On hardware
//! it is driven by the session's event loop; in the test suite it is driven
//! by the mock device, with or without a renderer.
//!
//! The pattern is drawn by [`PatternRenderer`], straight on the checked GPU
//! API of `proto-gfx-core`, without a script. It is a diagnostic tool, not
//! the drawing of a compositor: it writes the code values of the pattern
//! into the output image as they are, without color conversion.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use proto_backend::{Backend, BackendEvent, OutputInfo};
use proto_gfx_core::{
    ColorAttachment, ColorTarget, CullMode, DeviceDescription, Format, GfxError, Gpu, GpuConfig,
    Handle, LoadOp, RenderPipelineDesc, ShaderLanguage, ShaderSource, ShaderStage, Topology,
};
use proto_render::{
    DmabufPlane, EncodedImage, RenderError, TargetDesc, TargetId, TargetProvider, Targets,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::targets::OutputImages;
use crate::timing::FrameTimes;

/// The outcome of one item of the self test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    /// It worked, but something is off and a person should look.
    Warn,
    /// Not checked in this run (for example: no renderer).
    Skipped,
    /// hizukuri does not do this yet.
    NotImplemented,
}

/// One item of the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

/// Colors of the bars of the test pattern, as stored (sRGB-like) values.
const BARS: [[u8; 3]; 8] = [
    [255, 255, 255],
    [255, 255, 0],
    [0, 255, 255],
    [0, 255, 0],
    [255, 0, 255],
    [255, 0, 0],
    [0, 0, 255],
    [64, 128, 192],
];
/// Color of the square that moves by one step per frame.
const MARKER: [u8; 3] = [255, 128, 0];
const MARKER_SIZE: u32 = 32;
/// The background, visible below the bars.
const BACKGROUND: [u8; 3] = [32, 32, 32];

/// Where the marker is in a given frame.
fn marker_position(width: u32, height: u32, frame: u64) -> (u32, u32) {
    let span = width.saturating_sub(MARKER_SIZE).max(1) as u64;
    (((frame * 8) % span) as u32, height * 3 / 4)
}

/// The rectangles of the pattern in a frame, back to front: (x, y, width,
/// height, color).
fn pattern_rects(width: u32, height: u32, frame: u64) -> Vec<([u32; 4], [u8; 3])> {
    let bar_width = width / BARS.len() as u32;
    let mut rects: Vec<([u32; 4], [u8; 3])> = BARS
        .iter()
        .enumerate()
        .map(|(index, color)| ([index as u32 * bar_width, 0, bar_width, height / 2], *color))
        .collect();
    let (x, y) = marker_position(width, height, frame);
    rects.push(([x, y, MARKER_SIZE, MARKER_SIZE], MARKER));
    rects
}

const PATTERN_SHADER: &str = r#"
struct Push {
    rect: vec4<f32>,        // x, y, width, height in target pixels
    color: vec4<f32>,
    target_size: vec4<f32>, // width, height, unused, unused
}
var<immediate> pc: Push;

@vertex
fn vs(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let corner = vec2<f32>(f32(index & 1u), f32((index >> 1u) & 1u));
    return vec4((pc.rect.xy + corner * pc.rect.zw) / pc.target_size.xy * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs() -> @location(0) vec4<f32> {
    return pc.color;
}
"#;

/// Draws the test pattern into output images, on its own GPU device. It
/// is the [`TargetProvider`] the backend makes its targets with.
pub struct PatternRenderer {
    gpu: Gpu,
    targets: OutputImages,
    shaders: Option<(Handle, Handle)>,
    pipelines: HashMap<Format, Handle>,
}

fn gfx(context: &str, error: GfxError) -> String {
    format!("{context}: {error}")
}

impl PatternRenderer {
    pub fn new(config: GpuConfig) -> Result<Self, String> {
        Ok(Self {
            gpu: Gpu::new(config).map_err(|e| gfx("creating the GPU device", e))?,
            targets: OutputImages::new(),
            shaders: None,
            pipelines: HashMap::new(),
        })
    }

    pub fn device(&self) -> DeviceDescription {
        self.gpu.device_description()
    }

    /// Borrows the renderer as the backend's [`Targets`].
    pub fn targets(&mut self) -> Targets<'_> {
        Some(self)
    }

    fn pipeline(&mut self, format: Format) -> Result<Handle, GfxError> {
        if let Some(pipeline) = self.pipelines.get(&format) {
            return Ok(*pipeline);
        }
        let (vertex, fragment) = match self.shaders {
            Some(shaders) => shaders,
            None => {
                let mut shader = |stage, entry| {
                    self.gpu.create_shader(&ShaderSource {
                        stage,
                        language: ShaderLanguage::Wgsl,
                        source: PATTERN_SHADER,
                        entry,
                    })
                };
                let shaders = (
                    shader(ShaderStage::Vertex, "vs")?,
                    shader(ShaderStage::Fragment, "fs")?,
                );
                self.shaders = Some(shaders);
                shaders
            }
        };
        let pipeline = self.gpu.create_render_pipeline(&RenderPipelineDesc {
            vertex,
            fragment,
            vertex_buffers: Vec::new(),
            targets: vec![ColorTarget {
                format,
                blend: None,
            }],
            topology: Topology::TriangleStrip,
            cull: CullMode::None,
        })?;
        self.pipelines.insert(format, pipeline);
        Ok(pipeline)
    }

    /// Draws frame `frame` of the pattern into a target and waits until
    /// the GPU is done, so the frame can be shown or read right away.
    pub fn draw(&mut self, target: TargetId, frame: u64) -> Result<(), String> {
        let (image, desc) = {
            let target = self
                .targets
                .get(target)
                .ok_or_else(|| RenderError::UnknownTarget.to_string())?;
            (target.back(), target.desc)
        };
        let drawn = self.record(image, &desc, frame);
        // Close a frame left open by a failure before reporting it.
        if self.gpu.is_recording() {
            let _ = self.gpu.end_frame();
        }
        drawn.map_err(|e| gfx("drawing the test pattern", e))?;
        self.gpu
            .wait_idle()
            .map_err(|e| gfx("waiting for the GPU", e))?;
        if let Some(target) = self.targets.get_mut(target) {
            target.flip();
        }
        Ok(())
    }

    fn record(&mut self, image: Handle, desc: &TargetDesc, frame: u64) -> Result<(), GfxError> {
        let pipeline = self.pipeline(crate::targets::output_format(desc.format))?;
        let unit = |c: [u8; 3]| {
            [
                c[0] as f32 / 255.0,
                c[1] as f32 / 255.0,
                c[2] as f32 / 255.0,
            ]
        };
        let size = [desc.width as f32, desc.height as f32, 0.0, 0.0];
        let gpu = &mut self.gpu;
        gpu.begin_frame()?;
        let [r, g, b] = unit(BACKGROUND);
        gpu.begin_pass(&[ColorAttachment {
            image,
            load: LoadOp::Clear([r, g, b, 1.0]),
            store: true,
        }])?;
        gpu.set_pipeline(pipeline)?;
        for (rect, color) in pattern_rects(desc.width, desc.height, frame) {
            let [r, g, b] = unit(color);
            let push: Vec<u8> = rect
                .map(|v| v as f32)
                .iter()
                .chain(&[r, g, b, 1.0])
                .chain(&size)
                .flat_map(|v| v.to_ne_bytes())
                .collect();
            gpu.push(0, &push)?;
            gpu.draw(4, 1, 0, 0)?;
        }
        gpu.end_pass()?;
        gpu.end_frame()
    }

    /// The last frame drawn into a target.
    pub fn read(&mut self, target: TargetId) -> Result<EncodedImage, String> {
        let desc = self
            .targets
            .get(target)
            .ok_or_else(|| RenderError::UnknownTarget.to_string())?
            .desc;
        let image = self.targets.shown(target).map_err(|e| e.to_string())?;
        let data = self
            .gpu
            .read_image(image)
            .map_err(|e| gfx("reading an output image", e))?;
        Ok(EncodedImage {
            width: desc.width,
            height: desc.height,
            format: desc.format,
            image_description: desc.image_description,
            data,
        })
    }
}

impl TargetProvider for PatternRenderer {
    fn create_target(&mut self, desc: &TargetDesc) -> Result<TargetId, RenderError> {
        self.targets.create(&mut self.gpu, desc)
    }

    fn scanout_modifiers(&self) -> Vec<u64> {
        self.gpu.scanout_modifiers()
    }

    fn create_scanout_target(
        &mut self,
        desc: &TargetDesc,
        allowed_modifiers: &[u64],
    ) -> Result<(TargetId, DmabufPlane), RenderError> {
        self.targets
            .create_scanout(&mut self.gpu, desc, allowed_modifiers)
    }

    fn destroy_target(&mut self, id: TargetId) {
        self.targets.destroy(&mut self.gpu, id);
    }

    fn read_encoded_into(
        &mut self,
        target: TargetId,
        destination: &mut [u8],
        stride: usize,
    ) -> Result<(), RenderError> {
        self.targets
            .read_into(&mut self.gpu, target, destination, stride)
    }
}

fn as_targets(renderer: Option<&mut PatternRenderer>) -> Targets<'_> {
    renderer.map(|r| r as &mut (dyn TargetProvider + 'static))
}

/// See the module documentation.
pub struct SelfTest {
    output: Option<OutputInfo>,
    frames_drawn: u64,
    presented: Vec<Duration>,
    quit_requested: bool,
    session_pauses: u32,
    errors: Vec<String>,
    /// Result of comparing the last rendered frame with the pattern.
    contents: Option<Result<(), String>>,
    /// How long frames took, without the ones that were also compared.
    frame_times: FrameTimes,
}

impl Default for SelfTest {
    fn default() -> Self {
        Self::new()
    }
}

impl SelfTest {
    pub fn new() -> Self {
        Self {
            output: None,
            frames_drawn: 0,
            presented: Vec::new(),
            quit_requested: false,
            session_pauses: 0,
            errors: Vec::new(),
            contents: None,
            frame_times: FrameTimes::new(),
        }
    }

    /// Whether the user asked to stop (Ctrl+Alt+Backspace).
    pub fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    pub fn frames_presented(&self) -> usize {
        self.presented.len()
    }

    fn draw<B: Backend>(
        &mut self,
        backend: &mut B,
        renderer: Option<&mut PatternRenderer>,
    ) -> Result<(), String> {
        let Some(output) = self.output.clone() else {
            return Ok(());
        };
        let (width, height) = output.size;
        let started = Instant::now();
        let mut compared = false;
        let renderer = match renderer {
            Some(renderer) => {
                if let Some(target) = backend.render_target(output.id) {
                    renderer.draw(target, self.frames_drawn)?;
                    // Compare what was rendered with the pattern now and
                    // then; every frame would distort the timing.
                    if self.frames_drawn.is_multiple_of(60) {
                        compared = true;
                        let frame = self.frames_drawn;
                        self.contents = Some(renderer.read(target).and_then(|image| {
                            let pixel = |x: u32, y: u32| {
                                let p = image.pixel(x, y);
                                [p[0] as u8, p[1] as u8, p[2] as u8]
                            };
                            check_pattern(width, height, frame, &pixel)
                        }));
                    }
                }
                Some(renderer)
            }
            None => None,
        };
        let drawn = Instant::now();
        backend
            .present(as_targets(renderer), output.id)
            .map_err(|e| e.to_string())?;
        if !compared {
            self.frame_times.record(drawn - started, drawn.elapsed());
        }
        self.frames_drawn += 1;
        Ok(())
    }

    /// Handles what the backend reports. Call whenever the backend may have
    /// events: it draws a frame each time the output can take one.
    pub fn process<B: Backend>(
        &mut self,
        backend: &mut B,
        mut renderer: Option<&mut PatternRenderer>,
    ) {
        // Handling an event can produce more (presenting queues the
        // presentation on backends that report it at once).
        for _ in 0..8 {
            let events = backend.drain_events();
            if events.is_empty() {
                break;
            }
            for event in events {
                match event {
                    BackendEvent::OutputAdded(info) => {
                        backend.request_frame(info.id);
                        self.output = Some(info);
                    }
                    BackendEvent::OutputChanged(info) => self.output = Some(info),
                    BackendEvent::OutputRemoved(id) => {
                        if self.output.as_ref().is_some_and(|o| o.id == id) {
                            self.output = None;
                            self.errors
                                .push("the output went away during the test".to_string());
                        }
                    }
                    BackendEvent::Frame { output } => {
                        if let Err(error) = self.draw(backend, renderer.as_deref_mut()) {
                            // Keep going: a single failure should not end
                            // the run, but it does fail the test.
                            if self.errors.len() < 16 {
                                self.errors.push(error);
                            }
                        }
                        // Keep the frames coming.
                        backend.request_frame(output);
                    }
                    BackendEvent::Presented { presentation, .. } => {
                        self.presented.push(presentation.time);
                    }
                    BackendEvent::SessionPaused => self.session_pauses += 1,
                    BackendEvent::SessionResumed => {
                        if let Some(output) = &self.output {
                            backend.request_frame(output.id);
                        }
                    }
                    BackendEvent::Reconfigure => {
                        if let Err(error) = backend.reconfigure(as_targets(renderer.as_deref_mut()))
                        {
                            self.errors.push(error.to_string());
                        }
                    }
                    BackendEvent::QuitRequested => self.quit_requested = true,
                    BackendEvent::Input(_) => {}
                }
            }
        }
    }

    /// Builds the report. `context` is added as it is (device, renderer,
    /// command line).
    pub fn report<B: Backend>(&self, backend: &B, context: Value) -> Value {
        let status = backend.describe();
        let output = &status["output"];
        let refresh_mhz = output["refresh_mhz"].as_u64().unwrap_or(0);
        let nominal_ms = if refresh_mhz > 0 {
            1_000_000.0 / refresh_mhz as f64
        } else {
            0.0
        };
        let intervals = &output["flip_interval_ms"];
        let median = intervals["median"].as_f64().unwrap_or(0.0);
        let count = intervals["count"].as_u64().unwrap_or(0);
        let late = intervals["late"].as_u64().unwrap_or(0);
        let presented = self.presented.len();

        let mut checks = vec![
            Check {
                name: "modeset",
                status: if presented > 0 {
                    Status::Pass
                } else {
                    Status::Fail
                },
                detail: match &self.output {
                    Some(info) => format!(
                        "{} {}x{} at {:.3} Hz",
                        info.name,
                        info.size.0,
                        info.size.1,
                        info.refresh_mhz as f64 / 1000.0
                    ),
                    None => "no output".to_string(),
                },
            },
            Check {
                name: "scanout_path",
                status: if output["scanout"].is_string() {
                    Status::Pass
                } else {
                    Status::Fail
                },
                detail: output["scanout"].as_str().unwrap_or("none").to_string(),
            },
            Check {
                name: "frames_presented",
                status: if presented >= 2 {
                    Status::Pass
                } else {
                    Status::Fail
                },
                detail: format!("{presented} frames presented, {} drawn", self.frames_drawn),
            },
            Check {
                name: "page_flip_timing",
                status: if count == 0 || nominal_ms == 0.0 {
                    Status::Fail
                } else if (median - nominal_ms).abs() <= nominal_ms * 0.2 && late * 20 <= count {
                    Status::Pass
                } else {
                    // Flips happen, but not at the display's pace. That is
                    // expected in some virtual machines and worth a look on
                    // hardware.
                    Status::Warn
                },
                detail: format!(
                    "median {median:.3} ms, nominal {nominal_ms:.3} ms, {late} of {count} late"
                ),
            },
            Check {
                name: "frame_contents",
                status: match &self.contents {
                    Some(Ok(())) => Status::Pass,
                    Some(Err(_)) => Status::Fail,
                    None => Status::Skipped,
                },
                detail: match &self.contents {
                    Some(Ok(())) => "the rendered frame matches the test pattern".to_string(),
                    Some(Err(error)) => error.clone(),
                    None => "no renderer, nothing was drawn".to_string(),
                },
            },
            Check {
                name: "commits",
                status: if status["failed_commits"].as_u64().unwrap_or(0) == 0
                    && self.errors.is_empty()
                {
                    Status::Pass
                } else {
                    Status::Fail
                },
                detail: if self.errors.is_empty() {
                    format!("{} commits refused", status["failed_commits"])
                } else {
                    self.errors.join("; ")
                },
            },
        ];
        // Making a frame has to fit into the refresh period, or frames are
        // dropped. Slower is not a failure of the display path, so: a
        // warning.
        checks.push(match self.frame_times.median_total_ms() {
            Some(total) => Check {
                name: "frame_time",
                status: if nominal_ms > 0.0 && total <= nominal_ms {
                    Status::Pass
                } else {
                    Status::Warn
                },
                detail: format!(
                    "median {total:.3} ms to draw and present a frame, refresh period \
                     {nominal_ms:.3} ms"
                ),
            },
            None => Check {
                name: "frame_time",
                status: Status::Skipped,
                detail: "no frame was timed".to_string(),
            },
        });
        for name in ["hdr_output", "variable_refresh", "tearing"] {
            checks.push(Check {
                name,
                status: Status::NotImplemented,
                detail: "hizukuri does not implement this yet".to_string(),
            });
        }
        let passed = !checks.iter().any(|c| c.status == Status::Fail);

        json!({
            "tool": "proto-compositor kms-selftest",
            "passed": passed,
            "context": context,
            "backend": status,
            "frames_drawn": self.frames_drawn,
            "frames_presented": presented,
            "session_pauses": self.session_pauses,
            "frame_time_ms": self.frame_times.describe(),
            "checks": checks,
        })
    }
}

/// Compares a rendered frame with the test pattern. `pixel` returns
/// `[r, g, b]` at a position.
pub fn check_pattern(
    width: u32,
    height: u32,
    frame: u64,
    pixel: &dyn Fn(u32, u32) -> [u8; 3],
) -> Result<(), String> {
    // The values are written as they are; a few code values of error are
    // allowed for drivers that round differently.
    const TOLERANCE: i32 = 4;
    let close = |actual: [u8; 3], expected: [u8; 3]| {
        (0..3).all(|i| (actual[i] as i32 - expected[i] as i32).abs() <= TOLERANCE)
    };
    let bar_width = width / BARS.len() as u32;
    for (index, expected) in BARS.iter().enumerate() {
        let (x, y) = (index as u32 * bar_width + bar_width / 2, height / 4);
        let actual = pixel(x, y);
        if !close(actual, *expected) {
            return Err(format!(
                "bar {index} at ({x}, {y}) is {actual:?}, expected {expected:?}"
            ));
        }
    }
    let (marker_x, marker_y) = marker_position(width, height, frame);
    let center = (marker_x + MARKER_SIZE / 2, marker_y + MARKER_SIZE / 2);
    let actual = pixel(center.0, center.1);
    if !close(actual, MARKER) {
        return Err(format!(
            "the marker of frame {frame} at {center:?} is {actual:?}, expected {MARKER:?}"
        ));
    }
    // Below the bars and away from the marker: background.
    let actual = pixel(width - 1, height - 1);
    if !close(actual, BACKGROUND) {
        return Err(format!(
            "the background at the bottom right is {actual:?}, expected {BACKGROUND:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pattern drawn by hand, to check the checker.
    fn pattern(width: u32, height: u32, frame: u64) -> impl Fn(u32, u32) -> [u8; 3] {
        move |x, y| {
            let (mx, my) = marker_position(width, height, frame);
            if x >= mx && x < mx + MARKER_SIZE && y >= my && y < my + MARKER_SIZE {
                MARKER
            } else if y < height / 2 {
                BARS[((x / (width / 8)) as usize).min(7)]
            } else {
                BACKGROUND
            }
        }
    }

    #[test]
    fn the_checker_accepts_the_pattern_it_describes() {
        for frame in [0, 1, 59, 1000] {
            check_pattern(640, 480, frame, &pattern(640, 480, frame)).unwrap();
        }
    }

    #[test]
    fn the_checker_notices_wrong_frames() {
        // The marker of another frame.
        let error = check_pattern(640, 480, 5, &pattern(640, 480, 40)).unwrap_err();
        assert!(error.contains("marker"), "{error}");
        // Red and blue swapped.
        let swapped = |x, y| {
            let [r, g, b] = pattern(640, 480, 0)(x, y);
            [b, g, r]
        };
        let error = check_pattern(640, 480, 0, &swapped).unwrap_err();
        assert!(error.contains("bar"), "{error}");
        // All black.
        assert!(check_pattern(640, 480, 0, &|_, _| [0, 0, 0]).is_err());
    }

    #[test]
    fn the_marker_stays_on_screen() {
        for frame in 0..2000 {
            let (x, y) = marker_position(640, 480, frame);
            assert!(x + MARKER_SIZE <= 640 && y + MARKER_SIZE <= 480);
        }
    }
}
