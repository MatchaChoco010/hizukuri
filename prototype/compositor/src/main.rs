//! The `proto-compositor` binary: command line, event loop, control socket.
//!
//! ```text
//! proto-compositor [OPTIONS]        run the compositor (headless unless told otherwise)
//! proto-compositor kms-info         print a DRM device's resources as JSON (read-only)
//! proto-compositor kms-selftest     drive one output with a test pattern and write a report
//! ```

use std::io::{self, BufRead, BufReader, Write};
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
use proto_backend::headless::{HeadlessBackend, HeadlessConfig, HeadlessOutputConfig};
use proto_backend::{Backend, BackendError, BackendEvent, BackendHost, OutputId, OutputInfo};
use proto_color::ImageDescription;
use proto_compositor::compositor::{Compositor, CompositorOptions};
use proto_compositor::control::{self, Request};
use proto_compositor::safety::{self, MachineFacts, Watchdog, virtualization};
use proto_compositor::scene::{FlatScene, Scene};
use proto_compositor::script::{
    OnError, Project, ScriptOptions, ScriptRuntime, ScriptScene, ScriptWm,
};
use proto_compositor::wm::WindowManager;
use proto_render::{DeviceSelector, OutputFormat, Renderer, RendererOptions, TargetId, Targets};
use proto_wl::{DmabufConfig, DmabufFormat, ServerConfig, SocketConfig};

mod kms;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum BackendKind {
    /// Virtual outputs rendered offscreen. Nothing is shown anywhere.
    Headless,
    /// A DRM/KMS session on a virtual terminal: takes over the display.
    Kms,
    /// `kms` when started from a bare console, `headless` when a desktop
    /// session is around (`WAYLAND_DISPLAY` or `DISPLAY` is set).
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CursorMode {
    /// Drawn on KMS, not drawn headless.
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum RendererKind {
    /// The renderer written in Rust (`proto-render`).
    Rust,
    /// Drawing assembled by a script through the checked GPU API.
    Script,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum WmKind {
    /// The floating layout written in Rust (`proto-layout`).
    Rust,
    /// Placement, focus and key bindings decided by the script.
    Script,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ScriptErrorPolicy {
    /// Keep showing the last good frame and retry the script.
    Hold,
    /// Draw with the built-in fallback until the script is reloaded.
    Safe,
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
    /// from left to right.
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
    /// bt2020-linear, scrgb-linear.
    #[arg(long, default_value = "srgb-gamma22", value_parser = parse_image_description)]
    output_image_description: ImageDescription,

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

    /// Run without a renderer. Everything works except drawing.
    #[arg(long)]
    no_renderer: bool,

    /// Whether clients may send dmabufs (`zwp_linux_dmabuf_v1`).
    #[arg(long, value_enum, default_value_t = DmabufMode::Off)]
    dmabuf: DmabufMode,

    /// Exit after this many seconds. A safety net for unattended runs.
    #[arg(long)]
    exit_after: Option<f64>,

    /// What draws: the Rust renderer, or a script.
    #[arg(long, value_enum, default_value = "rust")]
    renderer: RendererKind,

    /// What places windows and decides the focus. `script` needs
    /// `--renderer script`.
    #[arg(long, value_enum, default_value = "rust")]
    wm: WmKind,

    /// Directory of the script project (with `main.luau`). Without it the
    /// built-in project is used.
    #[arg(long)]
    script: Option<PathBuf>,

    /// What happens when the drawing part of the script fails.
    #[arg(long, value_enum, default_value = "safe")]
    script_on_error: ScriptErrorPolicy,

    /// Time one call into the script may take, in milliseconds.
    #[arg(long, default_value_t = 250)]
    script_budget_ms: u64,

    /// Memory the script may use, in MiB.
    #[arg(long, default_value_t = 256)]
    script_memory_mb: usize,

    /// With `--script-on-error hold`: how long a script that failed to draw
    /// is left alone before it is asked again, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    script_retry_ms: u64,

    /// Wait for the GPU after every frame the script draws, like the Rust
    /// renderer does. For comparing frame times.
    #[arg(long)]
    script_wait_gpu: bool,

    #[command(flatten)]
    kms: kms::KmsArgs,
}

/// Whatever draws, before it is wrapped into a scene.
pub enum Drawing {
    Rust(Option<Box<Renderer>>),
    Script(Box<ScriptScene>, Rc<ScriptRuntime>),
}

impl Drawing {
    pub fn targets(&mut self) -> Targets<'_> {
        match self {
            Drawing::Rust(renderer) => proto_render::targets_of(renderer.as_deref_mut()),
            Drawing::Script(scene, _) => scene.targets(),
        }
    }
}

/// How to create what draws, once it is known which GPU drives the display
/// (`None` when headless). Returns the drawing and whether it runs on that
/// GPU.
pub type MakeDrawing<'a> = dyn FnMut(Option<(u32, u32)>) -> Result<(Drawing, bool), String> + 'a;

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
}

impl AnyBackend {
    fn inner(&self) -> &dyn Backend {
        match self {
            AnyBackend::Headless(backend) => backend,
            AnyBackend::Kms(backend) => backend,
        }
    }

    fn inner_mut(&mut self) -> &mut dyn Backend {
        match self {
            AnyBackend::Headless(backend) => backend,
            AnyBackend::Kms(backend) => backend,
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
    fn request_frame(&mut self, output: OutputId) {
        self.inner_mut().request_frame(output);
    }
    fn present(&mut self, renderer: Targets<'_>, output: OutputId) -> Result<(), BackendError> {
        self.inner_mut().present(renderer, output)
    }
    fn reconfigure(&mut self, renderer: Targets<'_>) -> Result<(), BackendError> {
        self.inner_mut().reconfigure(renderer)
    }
    fn shutdown(&mut self, renderer: Targets<'_>) {
        self.inner_mut().shutdown(renderer);
    }
    fn describe(&self) -> serde_json::Value {
        self.inner().describe()
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
        Some(Command::KmsInfo(_)) => None,
        None => cli.run.vulkan_icd.clone(),
    };
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
        None => stop_signals().and_then(|s| run(cli.run, s)),
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
    /// process map (most GPU memory) show nothing. This is what the
    /// verification renderer can do; it is for testing the protocol.
    Cpu,
    /// Offered for what the script renderer's device can import. Buffers
    /// become textures without a copy. Needs `--renderer script`.
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

fn create_renderer(
    selector: DeviceSelector,
    validation: bool,
) -> Result<Renderer, proto_render::RenderError> {
    Renderer::new(&RendererOptions {
        device: selector,
        validation,
    })
}

fn run(args: RunArgs, signals: Signals) -> Result<(), String> {
    let backend_kind = choose_backend(args.backend, desktop_session_present());

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

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().map_err(|e| format!("failed to create the event loop: {e}"))?;
    let handle = event_loop.handle();
    let user_selector: DeviceSelector = args
        .vulkan_device
        .parse()
        .expect("device selectors always parse");

    if args.wm == WmKind::Script && args.renderer != RendererKind::Script {
        return Err("--wm script needs --renderer script".to_string());
    }

    // What draws is created by the backend's setup, because on KMS it should
    // run on the GPU that drives the display, which is only known once the
    // session is open.
    let script_options = ScriptOptions {
        project: match &args.script {
            Some(dir) => Project::Dir(dir.clone()),
            None => Project::Builtin,
        },
        on_error: match args.script_on_error {
            ScriptErrorPolicy::Hold => OnError::Hold,
            ScriptErrorPolicy::Safe => OnError::Safe,
        },
        call_budget: std::time::Duration::from_millis(args.script_budget_ms.max(1)),
        manage_windows: args.wm == WmKind::Script,
        memory_limit: Some(args.script_memory_mb.max(1) << 20),
        retry_after: std::time::Duration::from_millis(args.script_retry_ms),
        wait_for_gpu: args.script_wait_gpu,
        ..ScriptOptions::default()
    };
    let mut make_drawing = |display_gpu: Option<(u32, u32)>| -> Result<(Drawing, bool), String> {
        match args.renderer {
            RendererKind::Rust => {
                let (renderer, on_display_gpu) = kms::pick_renderer(
                    &kms::RendererRequest {
                        selector: user_selector.clone(),
                        validation: args.vulkan_validation,
                        disabled: args.no_renderer,
                    },
                    display_gpu,
                )?;
                Ok((Drawing::Rust(renderer.map(Box::new)), on_display_gpu))
            }
            RendererKind::Script => {
                if args.no_renderer {
                    return Err("--no-renderer cannot be combined with --renderer script".into());
                }
                let mut options = script_options.clone();
                options.gpu.validation = args.vulkan_validation;
                // The device names the two device selections understand
                // differ; map the common words.
                options.gpu.device_filter = match args.vulkan_device.as_str() {
                    "auto" | "gpu" => None,
                    "cpu" | "lavapipe" => Some("llvmpipe".to_string()),
                    other => Some(other.to_string()),
                };
                options.gpu.drm_node = display_gpu;
                let runtime = ScriptRuntime::new(options)?;
                let on_display_gpu =
                    display_gpu.is_some() && runtime.device().drm_primary == display_gpu;
                Ok((
                    Drawing::Script(Box::new(ScriptScene::new(Rc::clone(&runtime))), runtime),
                    on_display_gpu,
                ))
            }
        }
    };

    let composite_space = HeadlessConfig::default().composite_space;
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
                        image_description: args.output_image_description,
                        format: args.output_format,
                    })
                    .collect(),
                composite_space,
            };
            let backend = HeadlessBackend::new(&config, drawing.targets())
                .map_err(|e| format!("failed to create the headless backend: {e}"))?;
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
                args.output_image_description,
                composite_space,
            )?;
            (
                AnyBackend::Kms(started.backend),
                started.drawing,
                args.cursor != CursorMode::Off,
                started.watchdog,
            )
        }
        BackendKind::Auto => unreachable!("resolved by choose_backend"),
    };

    // Clients are pointed at the renderer's device, when it has one.
    let dmabuf = match args.dmabuf {
        DmabufMode::Off => None,
        DmabufMode::Cpu => Some(DmabufConfig::cpu_readable(match &drawing {
            Drawing::Rust(renderer) => renderer
                .as_ref()
                .and_then(|r| r.device_info().drm_render.or(r.device_info().drm_primary)),
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
            Drawing::Rust(_) => {
                return Err("--dmabuf gpu needs --renderer script".to_string());
            }
        },
    };
    let options = CompositorOptions {
        server: ServerConfig { socket, dmabuf },
        background: args.background,
        composite_space,
        draw_cursor,
    };
    let (scene, wm): (Box<dyn Scene>, Box<dyn WindowManager>) = match drawing {
        Drawing::Rust(renderer) => (
            Box::new(FlatScene::new(renderer.map(|r| *r))),
            Box::new(proto_layout::Layout::new()),
        ),
        Drawing::Script(scene, runtime) => {
            let wm: Box<dyn WindowManager> = match args.wm {
                WmKind::Script => Box::new(ScriptWm::new(runtime)?),
                WmKind::Rust => Box::new(proto_layout::Layout::new()),
            };
            (scene, wm)
        }
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
    let _control_guard = RemoveOnDrop(control_path.clone());
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

    insert_common_sources(
        &handle,
        signals,
        args.exit_after,
        watchdog,
        |app: &mut App| app.signal.stop(),
    )?;

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
    loop_result.map_err(|e| format!("event loop error: {e}"))?;

    match app.failure {
        Some(message) => Err(message),
        None => Ok(()),
    }
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

/// Removes a file when dropped (the control socket, on exit).
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
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
