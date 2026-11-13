//! The `proto-compositor` binary: command line, event loop, control socket.
//!
//! ```text
//! proto-compositor [OPTIONS]        run the compositor (headless unless told otherwise)
//! proto-compositor kms-info         print a DRM device's resources as JSON (read-only)
//! proto-compositor kms-selftest     drive one output with a test pattern and write a report
//! ```

use std::io::{self, BufRead, BufReader, Write};
use std::mem::DropGuard;
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction};
use clap::{Args, Parser, Subcommand, ValueEnum};
use proto_backend::SelectionCrossing;
use proto_backend::headless::{HeadlessBackend, HeadlessConfig, HeadlessOutputConfig};
use proto_backend::nested::{LogoKey, LogoKeyChoice, NestedBackend, NestedBuffers, NestedConfig};
use proto_backend::{
    Backend, BackendError, BackendEvent, BackendHost, OutputId, OutputInfo, TimeSource,
};
use proto_backend::{OutputFormat, TargetId, Targets};
use proto_color::ImageDescription;
use proto_compositor::compositor::{Compositor, CompositorOptions};
use proto_compositor::control::{self, Request};
use proto_compositor::fallback;
use proto_compositor::safety::{self, MachineFacts, Watchdog, virtualization};
use proto_compositor::scene::{NullScene, Scene};
use proto_compositor::script::{
    OnError, Project, ScriptOptions, ScriptRuntime, ScriptScene, ScriptWm,
};
use proto_compositor::wm::WindowManager;
use proto_gfx_core::GpuConfig;
use proto_wl::{DmabufConfig, DmabufFormat, ServerConfig, SocketConfig};

mod kms;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum BackendKind {
    /// Virtual outputs rendered offscreen. Nothing is shown anywhere.
    Headless,
    /// A DRM/KMS session on a virtual terminal: takes over the display.
    Kms,
    /// A window of another Wayland compositor (`--nested-socket`, or the
    /// one `WAYLAND_DISPLAY` names).
    Nested,
    /// `kms` when started from a bare console, `headless` when a desktop
    /// session is around (`WAYLAND_DISPLAY` or `DISPLAY` is set).
    Auto,
}

/// `--nested-buffers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum NestedBuffersArg {
    /// dma-bufs when the parent compositor and the device can, else
    /// `wl_shm`.
    Auto,
    /// Copy every frame into shared memory.
    Shm,
    /// dma-bufs, or fail.
    Dmabuf,
}

/// `--nested-logo-key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum NestedLogoKey {
    /// Super while the parent passes its shortcuts to the window
    /// (keyboard-shortcuts-inhibit), Alt while it does not.
    Auto,
    /// The parent's logo key.
    Super,
    Alt,
    Ctrl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CursorMode {
    /// Drawn on KMS and nested, not drawn headless.
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ClockKind {
    /// `CLOCK_MONOTONIC`.
    Monotonic,
    /// Headless only: a clock that stands still until it is moved through
    /// the control interface (`advance_clock`). For tests of what moves
    /// with time.
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ScriptErrorPolicy {
    /// Keep showing the last good frame and retry the script.
    Hold,
    /// Draw with the built-in fallback until the script is reloaded.
    Fallback,
}

/// The verification compositor of hizukuri, a foundation for building
/// Wayland compositors (prototype).
#[derive(Debug, Parser)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    run: RunArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the resources of a DRM device as JSON: connectors, modes,
    /// CRTCs, planes, properties. Opens the device read-only, does not take
    /// DRM master and changes nothing.
    KmsInfo(kms::InfoArgs),
    /// Drive one output with a test pattern for a few seconds, measure the
    /// page flips, write a report and exit. Takes over the display.
    KmsSelftest(kms::SelftestArgs),
    /// Compile the Slang shaders of a project (`<PROJECT>/shaders/*.slang`)
    /// to SPIR-V in `<PROJECT>/.build/shaders/`, one file per entry point.
    /// The compositor reads that SPIR-V; it does not compile shaders itself.
    /// Needs slangc (`HIZUKURI_SLANGC`, or on `PATH`).
    BuildShaders(BuildShadersArgs),
}

#[derive(Debug, Args)]
struct BuildShadersArgs {
    /// The project directory.
    project: PathBuf,
}

/// `proto-compositor build-shaders`: errors go to standard error, one per
/// line, as `file:line:column: error ...`.
fn build_shaders(args: BuildShadersArgs) -> Result<(), String> {
    match proto_compositor::shaders::build_project(&args.project) {
        Ok(built) => {
            for warning in &built.warnings {
                eprintln!("{warning}");
            }
            for (rel, entries) in &built.compiled {
                if entries.is_empty() {
                    println!("shaders/{rel}: a module (no entry point)");
                } else {
                    println!("compiled shaders/{rel}: {}", entries.join(", "));
                }
            }
            if !built.up_to_date.is_empty() {
                println!("{} up to date", built.up_to_date.len());
            }
            Ok(())
        }
        Err(errors) => {
            for error in &errors {
                eprintln!("{error}");
            }
            Err(format!(
                "the shaders of {} did not compile ({} failed)",
                args.project.display(),
                errors.len()
            ))
        }
    }
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Backend. KMS is never chosen implicitly: without this option the
    /// compositor runs headless.
    #[arg(long, value_enum, default_value = "headless")]
    backend: BackendKind,

    /// Name of the Wayland socket in XDG_RUNTIME_DIR, or an absolute path.
    /// Never chosen automatically, so it cannot collide with the socket of
    /// a running desktop session by accident.
    #[arg(long, default_value = "hizukuri-0")]
    socket: String,

    /// Path of the control socket. Defaults to `<socket>.control` next to
    /// the Wayland socket.
    #[arg(long)]
    control_socket: Option<PathBuf>,

    /// Headless: size of the virtual output in physical pixels, as
    /// WIDTHxHEIGHT. Give it several times for several outputs
    /// (`HEADLESS-1`, `HEADLESS-2`, ...), which are placed side by side
    /// from left to right. Nested: the size of the window, in the parent's
    /// logical coordinates, when the parent leaves it to the window.
    #[arg(long, default_value = "1280x720", value_parser = parse_size)]
    output_size: Vec<(u32, u32)>,

    /// Scale of the output. With several headless outputs: one value for
    /// all of them, or one per output.
    #[arg(long, default_value = "1")]
    output_scale: Vec<f64>,

    /// Headless: refresh rate of the virtual output in Hz.
    #[arg(long, default_value_t = 60.0)]
    output_refresh: f64,

    /// What the frames sent to the output mean: one of srgb-gamma22,
    /// srgb-piecewise, display-p3, adobe-rgb, bt2020-gamma24, bt2020-pq,
    /// bt2020-linear, scrgb-linear. Default: srgb-gamma22; an output that is
    /// shown through an OCIO display then follows that display, which one
    /// given here does not.
    #[arg(long, value_parser = parse_image_description)]
    output_image_description: Option<ImageDescription>,

    /// Headless: format of the encoded frames: rgba8, rgba16 or bgra8.
    #[arg(long, default_value = "rgba8", value_parser = parse_output_format)]
    output_format: OutputFormat,

    /// Background color as RRGGBB, interpreted like content without color
    /// information (BT.709 primaries, gamma 2.2).
    #[arg(long, default_value = "202020", value_parser = parse_color)]
    background: [f32; 3],

    /// Draw the pointer cursor into the frame.
    #[arg(long, value_enum, default_value = "auto")]
    cursor: CursorMode,

    /// Vulkan device: auto, gpu, cpu, or text contained in the device or
    /// driver name (e.g. llvmpipe). With KMS, auto means the GPU that
    /// drives the display.
    #[arg(long, default_value = "auto")]
    vulkan_device: String,

    /// Vulkan driver manifest(s) to use, as a `VK_DRIVER_FILES` value. The
    /// loader only sees the drivers listed there. Needed to reach lavapipe
    /// when the environment restricts the loader to another driver.
    #[arg(long)]
    vulkan_icd: Option<String>,

    /// Enable the Vulkan validation layer (must be installed).
    #[arg(long)]
    vulkan_validation: bool,

    /// Run without drawing and without a script: no GPU is used.
    /// Everything works except drawing; the fallback places windows.
    #[arg(long, conflicts_with = "script")]
    no_drawing: bool,

    /// Whether clients may send dmabufs (`zwp_linux_dmabuf_v1`).
    #[arg(long, value_enum, default_value_t = DmabufMode::Off)]
    dmabuf: DmabufMode,

    /// Exit after this many seconds. A safety net for unattended runs.
    #[arg(long)]
    exit_after: Option<f64>,

    /// The clock frames are timed with. `manual` (headless only) stands
    /// still until `advance_clock` moves it; frames are still granted in
    /// real time.
    #[arg(long, value_enum, default_value = "monotonic")]
    clock: ClockKind,

    /// Directory of the script project (with `main.luau`), which draws and
    /// manages the windows. Without it the built-in project is used.
    #[arg(long)]
    script: Option<PathBuf>,

    /// What happens when the drawing part of the script fails.
    #[arg(long, value_enum, default_value = "fallback")]
    script_on_error: ScriptErrorPolicy,

    /// Time one call into the script may take, in milliseconds.
    #[arg(long, default_value_t = 250)]
    script_budget_ms: u64,

    /// Memory the script may use, in MiB.
    #[arg(long, default_value_t = 256)]
    script_memory_mb: usize,

    /// GPU memory (buffers and images) the script may allocate, in MiB.
    /// A 3840x2160 RGBA32F buffer alone takes about 127 MiB, and a renderer
    /// keeps several of them, more while it replaces them on a resize.
    #[arg(long, default_value_t = 4096)]
    script_gpu_memory_mb: u64,

    /// With `--script-on-error hold`: how long a script that failed to draw
    /// is left alone before it is asked again, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    script_retry_ms: u64,

    /// Wait for the GPU after every frame the script draws. For measuring
    /// frame times.
    #[arg(long)]
    script_wait_gpu: bool,

    /// The OpenColorIO config: a file, or an `ocio://` name of a config built
    /// into OCIO (such as `ocio://studio-config-latest`). Without it: what
    /// the environment variable `OCIO` names, or OCIO's default config.
    #[cfg(feature = "ocio")]
    #[arg(long, value_name = "CONFIG")]
    ocio_config: Option<String>,

    /// Show an output through an OCIO display: `<output>=<display>`, or
    /// `<display>` for every output. May be repeated. Without `--view`, the
    /// display's default view is used.
    #[cfg(feature = "ocio")]
    #[arg(long = "display", value_name = "[OUTPUT=]DISPLAY")]
    ocio_display: Vec<String>,

    /// Show an output through an OCIO view: `<output>=<view>`, or `<view>`
    /// for every output. May be repeated. Without `--display`, the config's
    /// default display is used.
    #[cfg(feature = "ocio")]
    #[arg(long = "view", value_name = "[OUTPUT=]VIEW")]
    ocio_view: Vec<String>,

    /// Exposure in front of an output's OCIO view, in stops:
    /// `<output>=<stops>`, or `<stops>` for every output.
    #[cfg(feature = "ocio")]
    #[arg(long, value_name = "[OUTPUT=]STOPS", allow_hyphen_values = true)]
    ocio_exposure: Vec<String>,

    /// Gamma behind an output's OCIO view: `<output>=<gamma>`, or `<gamma>`
    /// for every output.
    #[cfg(feature = "ocio")]
    #[arg(long, value_name = "[OUTPUT=]GAMMA")]
    ocio_gamma: Vec<String>,

    /// The colour space of the OCIO config that the compositor works in.
    /// Default: `ACEScg`, or the config's `scene_linear` role.
    #[cfg(feature = "ocio")]
    #[arg(long, value_name = "COLOR_SPACE")]
    ocio_working: Option<String>,

    /// Where the compiled OCIO shaders are kept. Default: `hizukuri/ocio`
    /// under `XDG_CACHE_HOME`.
    #[cfg(feature = "ocio")]
    #[arg(long, value_name = "DIR")]
    ocio_cache: Option<PathBuf>,

    /// The `slangc` that compiles the OCIO shaders. Default: the one that
    /// `HIZUKURI_SLANGC` names, one next to this executable, one on `PATH`.
    #[cfg(feature = "ocio")]
    #[arg(long, value_name = "FILE")]
    ocio_slangc: Option<PathBuf>,

    /// For tests: make every OCIO compilation take this much longer, in
    /// milliseconds.
    #[cfg(feature = "ocio")]
    #[arg(long, hide = true)]
    ocio_compile_delay_ms: Option<u64>,
    /// Primaries of the linear working space scripts composite in: acescg,
    /// bt2020, srgb, display-p3, adobe-rgb.
    #[arg(long)]
    working_space: Option<String>,

    /// Format of the buffers that hold colors of the working space:
    /// rgba32float, rgba16float, or auto (the most exact one the device can
    /// draw to, blend in and filter).
    #[arg(long)]
    color_format: Option<String>,

    /// The font file scripts get from `hz.load_font()`. Without it, the
    /// environment variable HIZUKURI_FONT, then the first well-known font
    /// found where systems keep them.
    #[arg(long)]
    font: Option<PathBuf>,

    #[command(flatten)]
    kms: kms::KmsArgs,

    #[command(flatten)]
    nested: NestedArgs,

    #[command(flatten)]
    x11: X11Args,
}

/// The nested backend (`--backend nested`).
#[derive(Debug, Args)]
struct NestedArgs {
    /// The Wayland socket of the parent compositor: a name in
    /// XDG_RUNTIME_DIR, or an absolute path. Without it, the one
    /// `WAYLAND_DISPLAY` names.
    #[arg(long, value_name = "SOCKET")]
    nested_socket: Option<String>,

    /// How frames are handed to the parent.
    #[arg(long, value_enum, default_value = "auto")]
    nested_buffers: NestedBuffersArg,

    /// The key of the parent that is the logo (Super) key inside, for a
    /// parent that keeps its Super shortcuts to itself. `auto` follows the
    /// parent: Super while it passes its shortcuts to the window, Alt while
    /// it does not.
    #[arg(long, value_enum, default_value = "auto")]
    nested_logo_key: NestedLogoKey,

    /// Do not ask the parent to pass its keyboard shortcuts to the window
    /// (keyboard-shortcuts-inhibit).
    #[arg(long)]
    nested_keep_parent_shortcuts: bool,

    /// Ask the parent to show the window fullscreen.
    #[arg(long)]
    nested_fullscreen: bool,

    /// Which way copy and paste cross between the parent and the inside.
    #[arg(long, value_enum, default_value = "both")]
    nested_clipboard: NestedClipboard,

    /// Whether the primary selection (select, middle click) crosses too,
    /// the way `--nested-clipboard` says.
    #[arg(long, value_enum, default_value = "on")]
    nested_primary_selection: OnOff,
}

/// `--nested-clipboard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum NestedClipboard {
    /// What is copied inside can be pasted in the parent, and the other
    /// way round.
    Both,
    /// Only what is copied inside reaches the parent.
    ToParent,
    /// Only what is copied in the parent reaches the inside.
    FromParent,
    /// Neither: the selections inside stay inside.
    Off,
}

/// `on` or `off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OnOff {
    On,
    Off,
}

/// Where the parent compositor's socket is: `given`, or else what
/// `WAYLAND_DISPLAY` names. A name is looked up in XDG_RUNTIME_DIR.
fn nested_socket(given: Option<&str>) -> Result<PathBuf, String> {
    let name = match given {
        Some(name) => name.to_string(),
        None => std::env::var("WAYLAND_DISPLAY")
            .ok()
            .filter(|name| !name.is_empty())
            .ok_or("--backend nested: no parent compositor (pass --nested-socket, or set WAYLAND_DISPLAY)")?,
    };
    if name.starts_with('/') {
        return Ok(PathBuf::from(name));
    }
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or("XDG_RUNTIME_DIR is not set; pass --nested-socket with an absolute path")?;
    Ok(PathBuf::from(runtime_dir).join(name))
}

/// X11 applications, through xwayland-satellite.
#[derive(Debug, Args)]
struct X11Args {
    /// Serve an X11 display and start this xwayland-satellite (a path, or a
    /// name looked up in PATH) when the first X11 client connects. Without
    /// it there is no X11 display.
    #[arg(long, value_name = "PROGRAM")]
    xwayland_satellite: Option<PathBuf>,

    /// The directory that stands for /tmp: the X11 sockets go to
    /// DIR/.X11-unix, the display locks to DIR. For tests, which must not
    /// use the displays of the machine.
    #[arg(long, value_name = "DIR", default_value = "/tmp")]
    x11_root: PathBuf,

    /// Append xwayland-satellite's (and Xwayland's) output to this file
    /// instead of discarding it.
    #[arg(long, value_name = "FILE")]
    xwayland_satellite_log: Option<PathBuf>,
}

/// Whatever draws, before it is wrapped into a scene.
pub enum Drawing {
    /// Nothing draws (`--no-drawing`).
    Nothing,
    Script(Box<ScriptScene>, Rc<ScriptRuntime>),
}

impl Drawing {
    pub fn targets(&mut self) -> Targets<'_> {
        match self {
            Drawing::Nothing => None,
            Drawing::Script(scene, _) => scene.targets(),
        }
    }
}

/// How to create what draws, once it is known which GPU drives the display
/// (`None` when headless). Returns the drawing and whether it runs on that
/// GPU.
pub type MakeDrawing<'a> = dyn FnMut(Option<(u32, u32)>) -> Result<(Drawing, bool), String> + 'a;

/// The OCIO part of the arguments. `--display` and `--view` name an output
/// with `<output>=`, or every output without it; the two are put together
/// per output.
#[cfg(feature = "ocio")]
fn ocio_options(args: &RunArgs) -> Result<proto_compositor::ocio::Options, String> {
    type Setting = (Option<String>, Option<String>, Option<String>);
    let mut outputs: Vec<Setting> = Vec::new();
    let split = |text: &str| match text.split_once('=') {
        Some((output, value)) => (Some(output.to_string()), value.to_string()),
        None => (None, text.to_string()),
    };
    let entry = |outputs: &mut Vec<Setting>, output: Option<String>| -> usize {
        match outputs.iter().position(|o| o.0 == output) {
            Some(index) => index,
            None => {
                outputs.push((output, None, None));
                outputs.len() - 1
            }
        }
    };
    for text in &args.ocio_display {
        let (output, display) = split(text);
        let index = entry(&mut outputs, output);
        outputs[index].1 = Some(display);
    }
    for text in &args.ocio_view {
        let (output, view) = split(text);
        let index = entry(&mut outputs, output);
        outputs[index].2 = Some(view);
    }
    // The setting for every output comes first: an output's own setting
    // starts from it.
    outputs.sort_by_key(|o| o.0.is_some());
    let mut controls = Vec::new();
    for (texts, is_exposure) in [(&args.ocio_exposure, true), (&args.ocio_gamma, false)] {
        for text in texts {
            let (output, value) = split(text);
            let value = value.parse::<f64>().map_err(|_| {
                format!(
                    "--ocio-{}: {value:?} is not a number",
                    if is_exposure { "exposure" } else { "gamma" }
                )
            })?;
            controls.push(if is_exposure {
                (output, Some(value), None)
            } else {
                (output, None, Some(value))
            });
        }
    }
    controls.sort_by_key(|c| c.0.is_some());
    Ok(proto_compositor::ocio::Options {
        config: args.ocio_config.clone(),
        working: args.ocio_working.clone(),
        cache_dir: args.ocio_cache.clone(),
        outputs,
        controls,
        slangc: args.ocio_slangc.clone(),
        compile_delay: args.ocio_compile_delay_ms.map(Duration::from_millis),
        // An image description that was asked for stays.
        outputs_follow_display: args.output_image_description.is_none(),
    })
}

fn parse_size(text: &str) -> Result<(u32, u32), String> {
    let (w, h) = text
        .split_once('x')
        .ok_or_else(|| format!("expected WIDTHxHEIGHT, got {text:?}"))?;
    let parse = |v: &str| {
        v.parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("invalid dimension {v:?}"))
    };
    Ok((parse(w)?, parse(h)?))
}

fn parse_image_description(text: &str) -> Result<ImageDescription, String> {
    ImageDescription::from_name(text).ok_or_else(|| {
        let names: Vec<&str> = ImageDescription::NAMED.iter().map(|(n, _)| *n).collect();
        format!(
            "unknown image description {text:?}; known: {}",
            names.join(", ")
        )
    })
}

fn parse_output_format(text: &str) -> Result<OutputFormat, String> {
    OutputFormat::from_name(text)
        .ok_or_else(|| format!("unknown format {text:?}; known: rgba8, rgba16, bgra8"))
}

fn parse_color(text: &str) -> Result<[f32; 3], String> {
    let value = u32::from_str_radix(text, 16)
        .ok()
        .filter(|_| text.len() == 6)
        .ok_or_else(|| format!("expected RRGGBB, got {text:?}"))?;
    let [_, r, g, b] = value.to_be_bytes();
    Ok([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0])
}

/// The backend the compositor runs on, whichever it is.
enum AnyBackend {
    Headless(HeadlessBackend),
    Kms(kms::RealKmsBackend),
    Nested(NestedBackend),
}

impl AnyBackend {
    fn inner(&self) -> &dyn Backend {
        match self {
            AnyBackend::Headless(backend) => backend,
            AnyBackend::Kms(backend) => backend,
            AnyBackend::Nested(backend) => backend,
        }
    }

    fn inner_mut(&mut self) -> &mut dyn Backend {
        match self {
            AnyBackend::Headless(backend) => backend,
            AnyBackend::Kms(backend) => backend,
            AnyBackend::Nested(backend) => backend,
        }
    }
}

impl Backend for AnyBackend {
    fn name(&self) -> &'static str {
        self.inner().name()
    }
    fn outputs(&self) -> Vec<OutputInfo> {
        self.inner().outputs()
    }
    fn is_active(&self) -> bool {
        self.inner().is_active()
    }
    fn render_target(&mut self, output: OutputId) -> Option<TargetId> {
        self.inner_mut().render_target(output)
    }
    fn shown_target(&mut self, output: OutputId) -> Option<TargetId> {
        self.inner_mut().shown_target(output)
    }
    fn request_frame(&mut self, output: OutputId) {
        self.inner_mut().request_frame(output);
    }
    fn present(&mut self, targets: Targets<'_>, output: OutputId) -> Result<(), BackendError> {
        self.inner_mut().present(targets, output)
    }
    fn reconfigure(&mut self, targets: Targets<'_>) -> Result<(), BackendError> {
        self.inner_mut().reconfigure(targets)
    }
    fn shutdown(&mut self, targets: Targets<'_>) {
        self.inner_mut().shutdown(targets);
    }
    fn set_pointer_lock(&mut self, locked: bool) {
        self.inner_mut().set_pointer_lock(locked);
    }
    fn set_text_input(&mut self, state: Option<&proto_backend::TextInputState>) {
        self.inner_mut().set_text_input(state);
    }
    fn offer_selection(
        &mut self,
        kind: proto_backend::SelectionKind,
        mime_types: Option<&[String]>,
    ) {
        self.inner_mut().offer_selection(kind, mime_types);
    }
    fn receive_outer_selection(
        &mut self,
        kind: proto_backend::SelectionKind,
        generation: u64,
        mime_type: &str,
        fd: std::os::fd::OwnedFd,
    ) -> bool {
        self.inner_mut()
            .receive_outer_selection(kind, generation, mime_type, fd)
    }
    fn set_output_image_description(
        &mut self,
        output: OutputId,
        description: ImageDescription,
    ) -> Result<(), BackendError> {
        self.inner_mut()
            .set_output_image_description(output, description)
    }
    fn describe(&self) -> serde_json::Value {
        self.inner().describe()
    }
    fn next_presentation(&self, output: OutputId) -> Option<std::time::Duration> {
        self.inner().next_presentation(output)
    }
    fn drain_events(&mut self) -> Vec<BackendEvent> {
        self.inner_mut().drain_events()
    }
}

/// The data of the event loop.
struct App {
    compositor: Compositor<AnyBackend>,
    signal: LoopSignal,
    /// The first error that made the loop stop.
    failure: Option<String>,
}

impl App {
    fn fail(&mut self, context: &str, error: impl std::fmt::Display) {
        tracing::error!("{context}: {error}");
        if self.failure.is_none() {
            self.failure = Some(format!("{context}: {error}"));
        }
        self.signal.stop();
    }

    fn process(&mut self) {
        if let Err(error) = self.compositor.process() {
            self.fail("compositor error", error);
        }
        if self.compositor.should_quit() {
            self.signal.stop();
        }
    }
}

impl BackendHost for App {
    fn backend_wake(&mut self) {
        self.process();
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // The Vulkan loader reads VK_DRIVER_FILES when it is first loaded.
    // Nothing else runs yet: no threads exist that could read the
    // environment concurrently.
    let icd = match &cli.command {
        Some(Command::KmsSelftest(args)) => args.vulkan_icd.clone(),
        Some(Command::KmsInfo(_) | Command::BuildShaders(_)) => None,
        None => cli.run.vulkan_icd.clone(),
    };
    // What applications started later are given back: the drivers named
    // with `--vulkan-icd` are for this process.
    let outer_drivers = icd.is_some().then(|| std::env::var("VK_DRIVER_FILES").ok());
    if let Some(icd) = icd {
        // SAFETY: single-threaded at this point, see above.
        unsafe { std::env::set_var("VK_DRIVER_FILES", icd) };
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HIZUKURI_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&io::stderr()))
        .with_writer(io::stderr)
        .init();

    let result = match cli.command {
        Some(Command::KmsInfo(args)) => kms::info(args),
        Some(Command::KmsSelftest(args)) => stop_signals().and_then(|s| kms::selftest(args, s)),
        Some(Command::BuildShaders(args)) => build_shaders(args),
        None => stop_signals().and_then(|s| run(cli.run, s, outer_drivers)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            tracing::error!("{message}");
            eprintln!("proto-compositor: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The scale of each of `count` outputs from the `--output-scale` values:
/// one value for all, or one per output.
fn output_scales(given: &[f64], count: usize) -> Result<Vec<f64>, String> {
    match given {
        [one] => Ok(vec![*one; count]),
        many if many.len() == count => Ok(many.to_vec()),
        many => Err(format!(
            "{} --output-scale values for {count} outputs: give one, or one per output",
            many.len()
        )),
    }
}

/// How dmabufs from clients are taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum DmabufMode {
    /// The protocol is not offered. Clients use `wl_shm`; GL and Vulkan
    /// clients fall back to software rendering or do not start.
    Off,
    /// Offered for 8 bit RGB in linear memory. Each buffer is mapped and
    /// copied, like a `wl_shm` buffer. Buffers the driver does not let a
    /// process map (most GPU memory) show nothing. It is for testing the
    /// protocol.
    Cpu,
    /// Offered for what the device the script draws with can import.
    /// Buffers become textures without a copy.
    Gpu,
}

/// The signals that end a run: SIGINT, SIGTERM and SIGHUP, to be read from
/// the event loop so the run ends cleanly (sockets removed, the display
/// released).
///
/// SIGHUP arrives when the terminal the compositor was started from goes
/// away: started from a shell on a virtual terminal, it is in that shell's
/// session, and the kernel hangs up the session when the shell ends (a
/// logout, a stopped systemd unit). Left at its default it would kill the
/// process without any cleanup. Found in the VM, where the launcher script is
/// the session leader.
///
/// This blocks the signals in the calling thread, and threads inherit that
/// from the thread that creates them. It therefore has to be called before
/// anything starts a thread: Vulkan drivers have worker threads (lavapipe
/// does), and a signal that arrives at a thread which does not block it
/// kills the process on the spot.
fn stop_signals() -> Result<Signals, String> {
    Signals::new(&[Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP])
        .map_err(|e| format!("failed to set up signal handling: {e}"))
}

/// Which backend to use. KMS takes over the display, so it is only chosen
/// when asked for: explicitly, or with `auto` from a bare console.
fn choose_backend(kind: BackendKind, desktop_session: bool) -> BackendKind {
    match kind {
        BackendKind::Auto if desktop_session => BackendKind::Headless,
        BackendKind::Auto => BackendKind::Kms,
        other => other,
    }
}

fn desktop_session_present() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// The GPU of the script layer, or of the KMS self test, for a
/// `--vulkan-device` value. `drm_node` is the GPU that drives the display,
/// which is preferred when Vulkan has it.
pub fn gpu_config(device: &str, validation: bool, drm_node: Option<(u32, u32)>) -> GpuConfig {
    GpuConfig {
        validation,
        // The device names the two device selections understand differ;
        // map the common words.
        device_filter: match device {
            "auto" | "gpu" => None,
            "cpu" | "lavapipe" => Some("llvmpipe".to_string()),
            other => Some(other.to_string()),
        },
        drm_node,
        ..GpuConfig::default()
    }
}

fn run(
    args: RunArgs,
    signals: Signals,
    outer_drivers: Option<Option<String>>,
) -> Result<(), String> {
    let backend_kind = choose_backend(args.backend, desktop_session_present());
    let output_image_description = args
        .output_image_description
        .unwrap_or(ImageDescription::SRGB_GAMMA22);

    let socket = if args.socket.contains('/') {
        SocketConfig::Path(PathBuf::from(&args.socket))
    } else {
        SocketConfig::Name(args.socket.clone())
    };
    let control_path = match &args.control_socket {
        Some(path) => path.clone(),
        None => {
            let base = if args.socket.contains('/') {
                PathBuf::from(&args.socket)
            } else {
                let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
                    .ok_or("XDG_RUNTIME_DIR is not set; pass --control-socket")?;
                PathBuf::from(runtime_dir).join(&args.socket)
            };
            let mut name = base.into_os_string();
            name.push(".control");
            PathBuf::from(name)
        }
    };
    if control_path.exists() {
        // A socket nobody listens on was left by a run that was killed.
        match std::os::unix::net::UnixStream::connect(&control_path) {
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                tracing::warn!(
                    "removing the stale control socket {}",
                    control_path.display()
                );
                std::fs::remove_file(&control_path)
                    .map_err(|e| format!("{}: {e}", control_path.display()))?;
            }
            _ => {
                return Err(format!(
                    "the control socket {} already exists; is another proto-compositor running?",
                    control_path.display()
                ));
            }
        }
    }

    let clock = match args.clock {
        ClockKind::Monotonic => TimeSource::Monotonic,
        ClockKind::Manual if backend_kind == BackendKind::Headless => TimeSource::manual(),
        ClockKind::Manual => return Err("--clock manual needs --backend headless".into()),
    };

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().map_err(|e| format!("failed to create the event loop: {e}"))?;
    let handle = event_loop.handle();

    // What draws is created by the backend's setup, because on KMS it should
    // run on the GPU that drives the display, which is only known once the
    // session is open.
    let working = match &args.working_space {
        Some(name) => proto_color::Primaries::from_name(name)
            .ok_or_else(|| format!("--working-space: unknown primaries {name:?}"))?,
        None => proto_compositor::script::DEFAULT_WORKING,
    };
    let script_options = ScriptOptions {
        project: match &args.script {
            Some(dir) => Project::Dir(dir.clone()),
            None => Project::Builtin,
        },
        on_error: match args.script_on_error {
            ScriptErrorPolicy::Hold => OnError::Hold,
            ScriptErrorPolicy::Fallback => OnError::Fallback,
        },
        call_budget: std::time::Duration::from_millis(args.script_budget_ms.max(1)),
        manage_windows: true,
        memory_limit: Some(args.script_memory_mb.max(1) << 20),
        retry_after: std::time::Duration::from_millis(args.script_retry_ms),
        wait_for_gpu: args.script_wait_gpu,
        clock: clock.clone(),
        #[cfg(feature = "ocio")]
        ocio: ocio_options(&args)?,
        // Applications the script starts are clients of this compositor,
        // and of nothing it was itself started in.
        app_env: vec![
            (
                "WAYLAND_DISPLAY".to_string(),
                // A socket given as a path is found by its absolute path.
                Some(if args.socket.contains('/') {
                    std::path::absolute(&args.socket)
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| args.socket.clone())
                } else {
                    args.socket.clone()
                }),
            ),
            ("DISPLAY".to_string(), None),
            ("XDG_ACTIVATION_TOKEN".to_string(), None),
            ("DESKTOP_STARTUP_ID".to_string(), None),
        ]
        .into_iter()
        .chain(outer_drivers.map(|drivers| ("VK_DRIVER_FILES".to_string(), drivers)))
        .collect(),
        working,
        font: args
            .font
            .clone()
            .or_else(|| std::env::var_os("HIZUKURI_FONT").map(PathBuf::from)),
        color_format: match args.color_format.as_deref() {
            None => proto_compositor::script::DEFAULT_COLOR_FORMAT,
            Some("auto") => None,
            Some("rgba32float") => Some(proto_gfx_core::Format::Rgba32Float),
            Some("rgba16float") => Some(proto_gfx_core::Format::Rgba16Float),
            Some(other) => {
                return Err(format!(
                    "--color-format: {other:?} is not rgba32float, rgba16float or auto"
                ));
            }
        },
        ..ScriptOptions::default()
    };
    let mut make_drawing = |display_gpu: Option<(u32, u32)>| -> Result<(Drawing, bool), String> {
        if args.no_drawing {
            return Ok((Drawing::Nothing, false));
        }
        let mut options = script_options.clone();
        options.gpu = gpu_config(&args.vulkan_device, args.vulkan_validation, display_gpu);
        options.gpu.limits.max_memory_bytes = args.script_gpu_memory_mb.max(1) << 20;
        let runtime = ScriptRuntime::new(options)?;
        let device = runtime.device();
        let on_display_gpu = display_gpu.is_some()
            && (device.drm_primary == display_gpu || device.drm_render == display_gpu);
        Ok((
            Drawing::Script(Box::new(ScriptScene::new(Rc::clone(&runtime))), runtime),
            on_display_gpu,
        ))
    };

    // What the state dumps describe: the space scripts start in.
    let composite_space = proto_color::CompositeSpace { primaries: working };
    let (backend, drawing, draw_cursor, watchdog) = match backend_kind {
        BackendKind::Headless => {
            let (mut drawing, _) = make_drawing(None)?;
            let scales = output_scales(&args.output_scale, args.output_size.len())?;
            let config = HeadlessConfig {
                outputs: args
                    .output_size
                    .iter()
                    .zip(scales)
                    .enumerate()
                    .map(|(index, (size, scale))| HeadlessOutputConfig {
                        name: format!("HEADLESS-{}", index + 1),
                        width: size.0,
                        height: size.1,
                        scale,
                        refresh_mhz: (args.output_refresh * 1000.0).round().max(1.0) as u32,
                        image_description: output_image_description,
                        format: args.output_format,
                    })
                    .collect(),
                composite_space,
            };
            let backend = HeadlessBackend::new(&config, drawing.targets())
                .map_err(|e| format!("failed to create the headless backend: {e}"))?
                .with_clock(clock.clone());
            backend
                .insert_sources(&handle)
                .map_err(|e| format!("failed to register the backend: {e}"))?;
            (
                AnyBackend::Headless(backend),
                drawing,
                args.cursor == CursorMode::On,
                None,
            )
        }
        BackendKind::Kms => {
            let started = kms::start(
                &handle,
                &args.kms,
                &mut make_drawing,
                output_scales(&args.output_scale, 1)?[0],
                output_image_description,
                composite_space,
            )?;
            (
                AnyBackend::Kms(started.backend),
                started.drawing,
                args.cursor != CursorMode::Off,
                started.watchdog,
            )
        }
        BackendKind::Nested => {
            let parent = nested_socket(args.nested.nested_socket.as_deref())?;
            // Never a window of itself.
            let own = match &socket {
                SocketConfig::Path(path) => Some(path.clone()),
                SocketConfig::Name(name) => {
                    std::env::var_os("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join(name))
                }
                SocketConfig::None => None,
            };
            if own.as_ref() == Some(&parent) {
                return Err(format!(
                    "--socket and the parent compositor's socket are the same ({})",
                    parent.display()
                ));
            }
            let config = NestedConfig {
                socket: parent,
                size: args.output_size[0],
                title: match &args.script {
                    Some(dir) => format!("hizukuri: {}", dir.display()),
                    None => "hizukuri".to_string(),
                },
                app_id: "hizukuri.nested".to_string(),
                image_description: output_image_description,
                composite_space,
                buffers: match args.nested.nested_buffers {
                    NestedBuffersArg::Auto => NestedBuffers::Auto,
                    NestedBuffersArg::Shm => NestedBuffers::Shm,
                    NestedBuffersArg::Dmabuf => NestedBuffers::Dmabuf,
                },
                logo_key: match args.nested.nested_logo_key {
                    NestedLogoKey::Auto => LogoKeyChoice::Auto,
                    NestedLogoKey::Super => LogoKeyChoice::Fixed(LogoKey::Super),
                    NestedLogoKey::Alt => LogoKeyChoice::Fixed(LogoKey::Alt),
                    NestedLogoKey::Ctrl => LogoKeyChoice::Fixed(LogoKey::Ctrl),
                },
                inhibit_shortcuts: !args.nested.nested_keep_parent_shortcuts,
                hide_parent_cursor: args.cursor != CursorMode::Off,
                fullscreen: args.nested.nested_fullscreen,
                selection: match args.nested.nested_clipboard {
                    NestedClipboard::Both => SelectionCrossing::BOTH,
                    NestedClipboard::ToParent => SelectionCrossing {
                        to_outside: true,
                        from_outside: false,
                    },
                    NestedClipboard::FromParent => SelectionCrossing {
                        to_outside: false,
                        from_outside: true,
                    },
                    NestedClipboard::Off => SelectionCrossing::NONE,
                },
                primary_selection: args.nested.nested_primary_selection == OnOff::On,
            };
            let mut backend = NestedBackend::connect(&config)
                .map_err(|e| format!("failed to create the nested backend: {e}"))?;
            // Draw with the device the parent composites with, so that
            // frames can be handed over without a copy.
            let (mut drawing, on_parent_device) = make_drawing(backend.parent_device())?;
            backend
                .start(drawing.targets(), on_parent_device)
                .map_err(|e| format!("failed to create the nested output: {e}"))?;
            backend
                .insert_sources(&handle)
                .map_err(|e| format!("failed to register the backend: {e}"))?;
            (
                AnyBackend::Nested(backend),
                drawing,
                args.cursor != CursorMode::Off,
                None,
            )
        }
        BackendKind::Auto => unreachable!("resolved by choose_backend"),
    };

    // Clients are pointed at the device the script draws with.
    let dmabuf = match args.dmabuf {
        DmabufMode::Off => None,
        DmabufMode::Cpu => Some(DmabufConfig::cpu_readable(match &drawing {
            Drawing::Nothing => None,
            Drawing::Script(_, runtime) => {
                let device = runtime.device();
                device.drm_render.or(device.drm_primary)
            }
        })),
        DmabufMode::Gpu => match &drawing {
            Drawing::Script(_, runtime) => {
                let device = runtime.device();
                let formats = runtime.dmabuf_formats();
                if formats.is_empty() {
                    return Err(format!(
                        "--dmabuf gpu: {} cannot import dma-bufs",
                        device.name
                    ));
                }
                Some(DmabufConfig {
                    formats: formats
                        .into_iter()
                        .map(|(fourcc, modifier)| DmabufFormat { fourcc, modifier })
                        .collect(),
                    main_device: device.drm_render.or(device.drm_primary),
                })
            }
            Drawing::Nothing => {
                return Err("--dmabuf gpu cannot be combined with --no-drawing".to_string());
            }
        },
    };
    let options = CompositorOptions {
        server: ServerConfig { socket, dmabuf },
        background: args.background,
        composite_space,
        draw_cursor,
        clock,
    };
    let script_runtime = match &drawing {
        Drawing::Script(_, runtime) => Some(Rc::clone(runtime)),
        Drawing::Nothing => None,
    };
    let (scene, wm): (Box<dyn Scene>, Box<dyn WindowManager>) = match drawing {
        Drawing::Nothing => (
            Box::new(NullScene::new()),
            Box::new(fallback::Windows::new()),
        ),
        Drawing::Script(scene, runtime) => (scene, Box::new(ScriptWm::new(runtime)?)),
    };
    let mut compositor = Compositor::with_window_manager(options, backend, scene, wm)
        .map_err(|e| format!("failed to start the compositor: {e}"))?;

    // Wayland: one source for new connections, one for client requests.
    // The file descriptors are duplicated so the sources own what they
    // watch; a duplicate reports readiness of the same underlying object.
    if let Some(fd) = compositor.wl().listen_fd() {
        let fd = fd
            .try_clone_to_owned()
            .map_err(|e| format!("failed to duplicate the listening socket: {e}"))?;
        insert_wayland_source(&handle, fd)?;
    }
    let display_fd = compositor
        .wl()
        .display_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("failed to duplicate the display fd: {e}"))?;
    insert_wayland_source(&handle, display_fd)?;

    // Control socket.
    let listener = UnixListener::bind(&control_path)
        .map_err(|e| format!("failed to bind {}: {e}", control_path.display()))?;
    // Removes the socket on exit.
    let _control_guard = DropGuard::new(control_path.clone(), |path| {
        let _ = std::fs::remove_file(path);
    });
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("failed to configure the control socket: {e}"))?;
    let control_handle = handle.clone();
    handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, app: &mut App| {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Err(error) = insert_control_connection(&control_handle, stream) {
                                tracing::warn!("failed to accept a control connection: {error}");
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            app.fail("control socket error", e);
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| format!("failed to register the control socket: {e}"))?;

    let x11 = start_x11(&handle, &args.x11, &args.socket, &mut compositor)?;
    if let (Some(x11), Some(runtime)) = (&x11, &script_runtime) {
        runtime.set_app_env("DISPLAY", Some(x11.status().display));
    }
    drop(script_runtime);

    insert_common_sources(
        &handle,
        signals,
        args.exit_after,
        watchdog,
        |app: &mut App| app.signal.stop(),
    )?;
    // Wakes the core once a second, also when nothing happens: the frame
    // callbacks of hidden windows are let through that often (the core
    // looks at the time after every wake-up).
    handle
        .insert_source(
            Timer::from_duration(Duration::from_secs(1)),
            |_, _, _: &mut App| TimeoutAction::ToDuration(Duration::from_secs(1)),
        )
        .map_err(|e| format!("failed to register the timer of hidden windows: {e}"))?;

    tracing::info!(
        backend = compositor.backend().name(),
        wayland_socket = %args.socket,
        control_socket = %control_path.display(),
        "proto-compositor is running"
    );

    let mut app = App {
        compositor,
        signal: event_loop.get_signal(),
        failure: None,
    };
    let loop_result = event_loop.run(None, &mut app, |app| app.process());
    // Give the display back in an orderly way, whatever happened.
    app.compositor.shutdown();
    drop(x11);
    loop_result.map_err(|e| format!("event loop error: {e}"))?;

    match app.failure {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

/// Serves the X11 display, when asked to: claims a display number and
/// watches its sockets; the first X11 client starts xwayland-satellite.
fn start_x11(
    handle: &LoopHandle<'static, App>,
    args: &X11Args,
    wayland_socket: &str,
    compositor: &mut Compositor<AnyBackend>,
) -> Result<Option<proto_compositor::x11::X11Display>, String> {
    use proto_compositor::x11::{self, Satellite, SatelliteConfig, X11Socket};
    let Some(program) = &args.xwayland_satellite else {
        return Ok(None);
    };
    let socket = X11Socket::open(&args.x11_root, x11::DISPLAY_NUMBERS)
        .map_err(|e| format!("failed to serve an X11 display: {e}"))?;
    let x11_display = socket.display();
    let satellite = Satellite::new(SatelliteConfig {
        program: program.clone(),
        wayland_display: wayland_socket.into(),
        log: args.xwayland_satellite_log.clone(),
    });
    let display = x11::insert_sources(handle, socket, satellite, |app: &mut App, status| {
        app.compositor.set_x11_status(Some(status));
    })?;
    compositor.set_x11_status(Some(display.status()));
    tracing::info!(
        "serving X11 clients on DISPLAY={x11_display}; xwayland-satellite starts with the first one"
    );
    Ok(Some(display))
}

/// Sources every mode of running wants: signals that stop the loop (from
/// [`stop_signals`]), the `--exit-after` timer, and the watchdog's heartbeat.
fn insert_common_sources<D: 'static>(
    handle: &LoopHandle<'static, D>,
    signals: Signals,
    exit_after: Option<f64>,
    watchdog: Option<Watchdog>,
    stop: fn(&mut D),
) -> Result<(), String> {
    handle
        .insert_source(signals, move |event, _, data: &mut D| {
            tracing::info!(signal = ?event.signal(), "stopping");
            stop(data);
        })
        .map_err(|e| format!("failed to register signal handling: {e}"))?;

    if let Some(seconds) = exit_after {
        handle
            .insert_source(
                Timer::from_duration(Duration::from_secs_f64(seconds.max(0.0))),
                move |_, _, data: &mut D| {
                    tracing::info!("--exit-after elapsed, stopping");
                    stop(data);
                    TimeoutAction::Drop
                },
            )
            .map_err(|e| format!("failed to register the exit timer: {e}"))?;
    }

    if let Some(watchdog) = watchdog {
        watchdog.feed();
        handle
            .insert_source(
                Timer::from_duration(Duration::from_millis(500)),
                move |_, _, _: &mut D| {
                    watchdog.feed();
                    TimeoutAction::ToDuration(Duration::from_millis(500))
                },
            )
            .map_err(|e| format!("failed to register the watchdog: {e}"))?;
    }
    Ok(())
}

fn insert_wayland_source(
    handle: &LoopHandle<'static, App>,
    fd: std::os::fd::OwnedFd,
) -> Result<(), String> {
    handle
        .insert_source(
            Generic::new(fd, Interest::READ, Mode::Level),
            |_, _, app: &mut App| {
                if let Err(error) = app.compositor.dispatch_clients() {
                    app.fail("failed to dispatch Wayland clients", error);
                }
                Ok(PostAction::Continue)
            },
        )
        .map(|_| ())
        .map_err(|e| format!("failed to register a Wayland source: {e}"))
}

/// Serves one control connection: one JSON request per line, one JSON
/// response per line.
fn insert_control_connection(
    handle: &LoopHandle<'static, App>,
    stream: UnixStream,
) -> io::Result<()> {
    // Requests are short and arrive whole; reading a line blocks only for
    // a client that stops in the middle of one, which the timeout bounds.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let watched = stream.as_fd().try_clone_to_owned()?;
    let mut reader = BufReader::new(stream);
    handle
        .insert_source(
            Generic::new(watched, Interest::READ, Mode::Level),
            move |_, _, app: &mut App| {
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => return Ok(PostAction::Remove),
                        Ok(_) => {}
                        Err(error) => {
                            tracing::debug!("control connection closed: {error}");
                            return Ok(PostAction::Remove);
                        }
                    }
                    let response = match serde_json::from_str::<Request>(line.trim()) {
                        Ok(request) => {
                            tracing::debug!(?request, "control request");
                            app.compositor.control(&request)
                        }
                        Err(error) => control::error(format!("invalid request: {error}")),
                    };
                    let mut text = response.to_string();
                    text.push('\n');
                    if reader.get_mut().write_all(text.as_bytes()).is_err() {
                        return Ok(PostAction::Remove);
                    }
                    if app.compositor.should_quit() {
                        app.signal.stop();
                    }
                    // Further complete lines may already sit in the buffer;
                    // they would not make the descriptor readable again.
                    if !reader.buffer().contains(&b'\n') {
                        return Ok(PostAction::Continue);
                    }
                }
            },
        )
        .map(|_| ())
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Refuses to take over a display on real hardware unless that was asked
/// for explicitly, device and display included. Returns the name of the
/// virtualization, if any.
fn check_real_hardware(request: &safety::TakeoverRequest<'_>) -> Result<Option<String>, String> {
    let virtual_machine = virtualization(&MachineFacts::read());
    safety::check_takeover(virtual_machine.as_deref(), request)?;
    match &virtual_machine {
        Some(name) => tracing::info!(virtualization = %name, "running in a virtual machine"),
        None => tracing::warn!(
            device = ?request.device,
            output = ?request.output,
            "taking over a display on real hardware, as asked"
        ),
    }
    Ok(virtual_machine)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kms_is_only_chosen_on_purpose() {
        use BackendKind::*;
        // The default is headless, with or without a desktop session.
        assert_eq!(choose_backend(Headless, true), Headless);
        assert_eq!(choose_backend(Headless, false), Headless);
        // Asked for explicitly: KMS even inside a desktop session.
        assert_eq!(choose_backend(Kms, true), Kms);
        assert_eq!(choose_backend(Kms, false), Kms);
        // Auto never takes the display away from a desktop session.
        assert_eq!(choose_backend(Auto, true), Headless);
        assert_eq!(choose_backend(Auto, false), Kms);
    }

    #[test]
    fn the_command_line_defaults_to_headless() {
        let cli = Cli::try_parse_from(["proto-compositor"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.run.backend, BackendKind::Headless);
        assert_eq!(cli.run.cursor, CursorMode::Auto);
        assert!(!cli.run.kms.allow_real_hardware);
        assert_eq!(cli.run.kms.watchdog, 15.0);

        let cli = Cli::try_parse_from([
            "proto-compositor",
            "--backend",
            "kms",
            "--kms-output",
            "DP-1",
            "--kms-mode",
            "1920x1080@60",
            "--exit-after",
            "30",
        ])
        .unwrap();
        assert_eq!(cli.run.backend, BackendKind::Kms);
        assert_eq!(cli.run.kms.kms_output.as_deref(), Some("DP-1"));
        assert_eq!(cli.run.exit_after, Some(30.0));
    }

    #[test]
    fn subcommands_parse() {
        let cli =
            Cli::try_parse_from(["proto-compositor", "kms-info", "--device", "/dev/dri/card1"]);
        assert!(matches!(cli.unwrap().command, Some(Command::KmsInfo(_))));
        let cli = Cli::try_parse_from([
            "proto-compositor",
            "kms-selftest",
            "--output",
            "Virtual-1",
            "--seconds",
            "5",
            "--report",
            "/tmp/report",
        ]);
        assert!(matches!(
            cli.unwrap().command,
            Some(Command::KmsSelftest(_))
        ));
        // The self test always needs to be told which output and for how
        // long: nothing about taking over a display is implicit.
        assert!(Cli::try_parse_from(["proto-compositor", "kms-selftest"]).is_err());
    }

    #[test]
    fn option_values_are_validated() {
        assert_eq!(parse_size("1280x720"), Ok((1280, 720)));
        assert!(parse_size("1280").is_err());
        assert!(parse_size("0x720").is_err());
        assert_eq!(parse_color("ff8000"), Ok([1.0, 128.0 / 255.0, 0.0]));
        assert!(parse_color("ff80").is_err());
        assert!(parse_image_description("display-p3").is_ok());
        assert!(parse_image_description("nope").is_err());
        assert!(parse_output_format("bgra8").is_ok());
        assert_eq!(output_scales(&[2.0], 3), Ok(vec![2.0, 2.0, 2.0]));
        assert_eq!(output_scales(&[1.0, 1.5], 2), Ok(vec![1.0, 1.5]));
        assert!(output_scales(&[1.0, 1.5], 3).is_err());
    }
}
