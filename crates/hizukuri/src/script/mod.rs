//! Drawing, hit testing and window management written in a script.
//!
//! This is the other implementation of the [`Scene`] seam: instead of the
//! Rust renderer drawing flat rectangles, a script
//! receives the windows and their textures and builds the frame itself with
//! the checked GPU API of `gfx-core`.
//!
//! # What stays in the core
//!
//! - Client buffers are turned into textures here ([`Scene::surface_contents`]
//!   uploads them). A script sees a texture handle; it cannot destroy or
//!   overwrite it.
//! - The images of the outputs are created here, on request of the backend
//!   ([`TargetProvider`]). A script draws into the image it is handed.
//! - Every call into the script runs under a time budget, and every failure
//!   comes back here as a value. What happens next is decided here, not by
//!   the script: keep the last frame, or draw the windows with the small
//!   built-in fallback ([`fallback`]).
//!
//! The script language is provisional (Luau through `gfx-lua`).

mod api;
mod dmabuf;
mod fallback;
mod wm;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gfx_core::{ErrorCode, Format, GfxError, Gpu, GpuConfig, Handle, ImageDesc, ImageUsage};
use gfx_lua::{HostOptions, ScriptError, ScriptHost};
use hizukuri_backend::OutputInfo;
use hizukuri_color::{AlphaMode, CompositeSpace, ImageDescription, Primaries};
use hizukuri_layout::WindowId;
use hizukuri_render::{
    CompositeImage, DRM_FORMAT_XRGB8888, DmabufPlane, EncodedImage, OutputFormat, RenderError,
    TargetDesc, TargetId, TargetProvider, Targets,
};
use hizukuri_wl::{
    BufferContents, BufferSource, BufferTransform, BufferUpdate, DamageRect, ShmFormat, SurfaceId,
};
use serde_json::{Value, json};

use crate::scene::{
    Capture, CursorView, DrawStatus, Hit, Scene, SceneError, SceneRequest, VisualWindow,
    WindowView, WorldSnapshot, flat_hit_test, flat_locate,
};

pub use wm::ScriptWm;

/// Where the script comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Project {
    /// The scripts compiled into the binary (`crates/hizukuri/scripts`).
    Builtin,
    /// A directory with `main.luau` and whatever it includes. Read again on
    /// every reload.
    Dir(PathBuf),
}

/// The built-in project: module name and source.
const BUILTIN: &[(&str, &str)] = &[
    ("main", include_str!("../../scripts/main.luau")),
    ("session", include_str!("../../scripts/session.luau")),
    ("color", include_str!("../../scripts/color.luau")),
    ("affine", include_str!("../../scripts/affine.luau")),
    ("render", include_str!("../../scripts/render.luau")),
    ("tile", include_str!("../../scripts/tile.luau")),
];

impl Project {
    /// The source of a module of the project.
    pub fn read(&self, name: &str) -> Result<String, String> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!("invalid module name {name:?}"));
        }
        match self {
            Project::Builtin => BUILTIN
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, source)| (*source).to_owned())
                .ok_or_else(|| format!("the built-in project has no module {name:?}")),
            Project::Dir(dir) => {
                let path = dir.join(format!("{name}.luau"));
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Project::Builtin => "builtin".to_owned(),
            Project::Dir(dir) => dir.display().to_string(),
        }
    }
}

/// What the core does when the drawing part of the script fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    /// Keep showing the last good frame and try the script again later.
    Hold,
    /// Draw the windows with the built-in fallback until the script is
    /// reloaded.
    Safe,
}

/// Which code currently draws and decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The script.
    Running,
    /// The script failed to draw; the last good frame stays on screen and
    /// the script is retried from time to time.
    Hold,
    /// The built-in fallback draws, hit tests and places windows.
    Safe,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Running => "running",
            Mode::Hold => "hold",
            Mode::Safe => "safe",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScriptOptions {
    pub project: Project,
    pub on_error: OnError,
    /// Time one call into the script may take (a frame, an event, a hit
    /// test).
    pub call_budget: Duration,
    /// Time loading the script and its `init` may take.
    pub init_budget: Duration,
    pub memory_limit: Option<usize>,
    pub gpu: GpuConfig,
    /// How long a script that failed to draw is left alone in [`Mode::Hold`]
    /// before it is tried again.
    pub retry_after: Duration,
    /// Whether the script also manages the windows ([`ScriptWm`]). A script
    /// asks with `hz.manages_windows()` while it loads, so this has to be
    /// known before the script starts.
    pub manage_windows: bool,
    /// Wait for the GPU to finish every frame before returning from
    /// [`Scene::draw`], also when nothing needs the result right away. For
    /// measurements: without it the time of a frame is the time to record
    /// and submit it.
    pub wait_for_gpu: bool,
}

impl Default for ScriptOptions {
    fn default() -> Self {
        Self {
            project: Project::Builtin,
            on_error: OnError::Safe,
            call_budget: Duration::from_millis(250),
            init_budget: Duration::from_secs(10),
            memory_limit: Some(256 << 20),
            gpu: GpuConfig::default(),
            retry_after: Duration::from_secs(1),
            manage_windows: false,
            wait_for_gpu: false,
        }
    }
}

/// The texture of a client surface.
pub(crate) struct SurfaceTexture {
    pub image: Handle,
    pub slot: Handle,
    pub width: u32,
    pub height: u32,
    pub format: Format,
    pub opaque: bool,
    /// Counts replacements of the image object (a new size or format).
    pub generation: u64,
    /// Counts uploads.
    pub serial: u64,
    /// What changed with the last upload, in buffer pixels. Empty when the
    /// client did not say (treat as everything).
    pub damage: Vec<[i32; 4]>,
    /// Index into [`Shared::colors`].
    pub color: u32,
    /// The image is an imported dma-buf (owned by the import cache), not an
    /// upload (owned by this texture).
    pub imported: bool,
    /// How the client turned the contents inside the texture. Uploads are
    /// upright; an imported buffer is as the client made it.
    pub transform: BufferTransform,
    /// The first row of the texture is the bottom row of the image.
    pub y_inverted: bool,
}

/// The image(s) of an output.
pub(crate) struct OutputTarget {
    pub desc: TargetDesc,
    /// One image for a scanout target (the backend rotates between targets),
    /// two for an offscreen one: the script draws into the back image, and a
    /// frame that fails never replaces the front image.
    pub images: Vec<Handle>,
    pub front: usize,
    pub rendered: bool,
}

/// One recorded failure of the script.
#[derive(Debug, Clone)]
pub(crate) struct Failure {
    pub what: String,
    pub kind: &'static str,
    pub message: String,
}

/// Everything the script-facing functions read and write. Kept apart from
/// the VM so that the functions (which the VM owns) can share it.
pub(crate) struct Shared {
    pub world: WorldSnapshot,
    pub opacity: HashMap<WindowId, f32>,
    pub textures: HashMap<SurfaceId, SurfaceTexture>,
    /// Distinct (image description, alpha mode) pairs seen so far. Scripts
    /// refer to them by index, which makes caching per color cheap.
    pub colors: Vec<(ImageDescription, AlphaMode)>,
    pub arrow: Option<(Handle, Handle)>,
    pub targets: HashMap<u64, OutputTarget>,
    pub next_target: u64,
    pub requests: Vec<SceneRequest>,
    /// The compositing buffer the script offers for verification captures.
    pub debug_composite: Option<(Handle, Primaries)>,
    pub project: Project,
    pub started: Instant,
    pub wm: wm::WmState,
    pub dmabuf: dmabuf::DmabufState,

    pub mode: Mode,
    pub failures: Vec<Failure>,
    pub failure_count: u64,
    pub hold_since: Option<Instant>,
    pub reloads: u64,
    pub frames: u64,
    pub fallback_frames: u64,
    pub kept_frames: u64,
    /// Callbacks the loaded script defines.
    pub defined: Vec<String>,
    /// Failures in a row of the callbacks that answer events (everything
    /// but `frame`). See [`ScriptRuntime::record_event_failure`].
    pub strikes: u32,
    /// How long calls into the script took, by callback.
    pub timing: Vec<(&'static str, CallTiming)>,
}

/// Time spent in one callback of the script.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CallTiming {
    pub calls: u64,
    pub total: Duration,
    pub max: Duration,
}

impl Shared {
    pub(crate) fn color_id(&mut self, description: ImageDescription, alpha: AlphaMode) -> u32 {
        match self.colors.iter().position(|c| *c == (description, alpha)) {
            Some(index) => index as u32,
            None => {
                self.colors.push((description, alpha));
                self.colors.len() as u32 - 1
            }
        }
    }
}

/// Failures in a row of event callbacks after which the script is taken
/// out of charge.
const MAX_STRIKES: u32 = 8;

/// The names of the functions the core calls in a script.
const CALLBACKS: &[&str] = &[
    "init",
    "unload",
    "frame",
    "hit_test",
    "locate",
    "window_opened",
    "window_changed",
    "window_closed",
    "focus_changed",
    "outputs_changed",
    "key",
    "pointer_button",
];

/// The VM, the GPU and the shared state. Held by the scene and by the window
/// manager, which are two views of the same script.
pub struct ScriptRuntime {
    pub(crate) host: ScriptHost,
    pub(crate) shared: Rc<RefCell<Shared>>,
    pub(crate) options: ScriptOptions,
    fallback: RefCell<fallback::Fallback>,
}

fn gfx(context: &str, error: GfxError) -> String {
    format!("{context}: {error}")
}

impl ScriptRuntime {
    /// Creates the GPU device and the VM, loads the project and runs its
    /// `init`. A script that fails to load is not an error here: the
    /// runtime starts in the fallback mode and can be reloaded.
    pub fn new(options: ScriptOptions) -> Result<Rc<Self>, String> {
        let gpu = Gpu::new(options.gpu.clone()).map_err(|e| gfx("creating the GPU device", e))?;
        let shared = Rc::new(RefCell::new(Shared {
            world: WorldSnapshot::default(),
            opacity: HashMap::new(),
            textures: HashMap::new(),
            colors: Vec::new(),
            arrow: None,
            targets: HashMap::new(),
            next_target: 1,
            requests: Vec::new(),
            debug_composite: None,
            project: options.project.clone(),
            started: Instant::now(),
            wm: wm::WmState::new(options.manage_windows),
            dmabuf: dmabuf::DmabufState::default(),
            mode: Mode::Running,
            failures: Vec::new(),
            failure_count: 0,
            hold_since: None,
            reloads: 0,
            frames: 0,
            fallback_frames: 0,
            kept_frames: 0,
            defined: Vec::new(),
            strikes: 0,
            timing: Vec::new(),
        }));
        let for_api = Rc::clone(&shared);
        let host = ScriptHost::with_setup(
            gpu,
            HostOptions {
                memory_limit: options.memory_limit,
                time_budget: Some(options.call_budget),
                sandbox: true,
            },
            move |lua, gpu| api::register(lua, &for_api, gpu),
        )
        .map_err(|e| format!("creating the script VM: {e}"))?;
        let runtime = Rc::new(Self {
            host,
            shared,
            options,
            fallback: RefCell::new(fallback::Fallback::default()),
        });
        if let Err(error) = runtime.load(None) {
            tracing::error!("the script failed to load: {error}");
            runtime.record("load", &error);
            runtime.shared.borrow_mut().mode = Mode::Safe;
        }
        Ok(runtime)
    }

    pub(crate) fn gpu(&self) -> std::cell::RefMut<'_, Gpu> {
        self.host.gpu()
    }

    /// The GPU device the script draws with.
    pub fn device(&self) -> gfx_core::DeviceDescription {
        self.gpu().device_description()
    }

    pub fn mode(&self) -> Mode {
        self.shared.borrow().mode
    }

    pub(crate) fn defines(&self, callback: &str) -> bool {
        self.shared.borrow().defined.iter().any(|d| d == callback)
    }

    /// Loads the project's `main` and runs `init`. `saved` is what the
    /// previous script's `unload` returned, if anything.
    fn load(&self, saved: Option<mlua::Value>) -> Result<(), ScriptError> {
        let project = self.shared.borrow().project.clone();
        let source = project.read("main").map_err(ScriptError::Runtime)?;
        self.host.run(Some(self.options.init_budget), |lua| {
            api::clear_modules(lua)?;
            lua.load(source.as_str()).set_name("main.luau").exec()
        })?;
        let defined: Vec<String> = CALLBACKS
            .iter()
            .filter(|name| self.host.has_function(name))
            .map(|name| (*name).to_owned())
            .collect();
        self.shared.borrow_mut().defined = defined;
        if self.defines("init") {
            // `init` may upload images, which needs an open frame.
            self.ensure_frame().map_err(ScriptError::Gfx)?;
            let result = self.host.call_function::<_, ()>(
                "init",
                saved.unwrap_or(mlua::Value::Nil),
                Some(self.options.init_budget),
            );
            self.finish_frame(false);
            result?;
        }
        Ok(())
    }

    /// Replaces the running script with a fresh load of the project (or of
    /// `project`, when given): the script's state and GPU objects are
    /// dropped, the windows and their textures stay. Returns to
    /// [`Mode::Running`] when the new script loads.
    pub fn reload(&self, project: Option<Project>) -> Result<(), String> {
        // A script that does not even compile never replaces the one that
        // runs. (Only `main` can be checked this way: its modules are read
        // when it runs.)
        let candidate = project
            .clone()
            .unwrap_or_else(|| self.shared.borrow().project.clone());
        let checked = candidate
            .read("main")
            .map_err(ScriptError::Runtime)
            .and_then(|source| {
                self.host.run(None, |lua| {
                    lua.load(source.as_str())
                        .set_name("main.luau")
                        .into_function()
                        .map(|_| ())
                })
            });
        if let Err(error) = checked {
            self.record("load", &error);
            return Err(format!(
                "the script does not compile; the running one was kept: {error}"
            ));
        }
        // Let the old script hand over what it wants to keep.
        let saved = if self.mode() == Mode::Running && self.defines("unload") {
            match self
                .host
                .call_function::<_, mlua::Value>("unload", (), None)
            {
                Ok(value) => Some(value),
                Err(error) => {
                    self.record("unload", &error);
                    None
                }
            }
        } else {
            None
        };
        // Close whatever the old script left open before its objects go.
        self.finish_frame(false);
        self.host
            .reset()
            .map_err(|e| format!("resetting the script VM: {e}"))?;
        {
            let mut shared = self.shared.borrow_mut();
            if let Some(project) = project {
                shared.project = project;
            }
            shared.debug_composite = None;
            shared.defined.clear();
            shared.reloads += 1;
            shared.hold_since = None;
            shared.strikes = 0;
        }
        match self.load(saved) {
            Ok(()) => {
                let mut shared = self.shared.borrow_mut();
                shared.mode = Mode::Running;
                shared.requests.push(SceneRequest::Redraw);
                tracing::info!("the script was reloaded");
                Ok(())
            }
            Err(error) => {
                self.record("load", &error);
                let mut shared = self.shared.borrow_mut();
                shared.mode = Mode::Safe;
                shared.requests.push(SceneRequest::Redraw);
                Err(format!("the script failed to load: {error}"))
            }
        }
    }

    /// Switches between the script and the built-in fallback by hand.
    pub fn set_mode(&self, mode: Mode) {
        let mut shared = self.shared.borrow_mut();
        shared.mode = mode;
        shared.strikes = 0;
        shared.hold_since = (mode == Mode::Hold).then(Instant::now);
        shared.requests.push(SceneRequest::Redraw);
    }

    /// Remembers a failure of the script.
    pub(crate) fn record(&self, what: &str, error: &ScriptError) {
        let (kind, message) = match error {
            ScriptError::Syntax(m) => ("syntax", m.clone()),
            ScriptError::Runtime(m) => ("runtime", m.clone()),
            ScriptError::Gfx(e) => ("gfx", e.to_string()),
            ScriptError::Timeout(m) => ("timeout", m.clone()),
            ScriptError::Memory(m) => ("memory", m.clone()),
        };
        tracing::warn!(what, kind, "script failure: {message}");
        let mut shared = self.shared.borrow_mut();
        shared.failure_count += 1;
        if shared.failures.len() >= 16 {
            shared.failures.remove(0);
        }
        shared.failures.push(Failure {
            what: what.to_owned(),
            kind,
            message,
        });
    }

    /// Remembers a failure of a callback that answers an event (a hit test,
    /// a key, a window that opened), and decides whether the script stays
    /// in charge.
    ///
    /// One failure costs nothing: the core uses its own answer for that one
    /// event. But these callbacks run for every pointer motion and every
    /// key, so a script that fails every time, or that takes its whole time
    /// budget each time, makes the session unusable while every single
    /// event is "handled". A timeout therefore ends the script's turn at
    /// once, and so do [`MAX_STRIKES`] failures in a row. The session goes
    /// on with the built-in behaviour until the script is reloaded.
    pub(crate) fn record_event_failure(&self, what: &str, error: &ScriptError) {
        self.record(what, error);
        let mut shared = self.shared.borrow_mut();
        shared.strikes += 1;
        let timeout = matches!(error, ScriptError::Timeout(_));
        if shared.mode != Mode::Safe && (timeout || shared.strikes >= MAX_STRIKES) {
            tracing::error!(
                what,
                strikes = shared.strikes,
                "the script keeps failing on events; the built-in behaviour takes over"
            );
            shared.mode = Mode::Safe;
            shared.requests.push(SceneRequest::Redraw);
        }
    }

    /// Notes how long a call into the script took.
    pub(crate) fn time_call(&self, name: &'static str, started: Instant) {
        let took = started.elapsed();
        let mut shared = self.shared.borrow_mut();
        let index = match shared.timing.iter().position(|(n, _)| *n == name) {
            Some(index) => index,
            None => {
                shared.timing.push((name, CallTiming::default()));
                shared.timing.len() - 1
            }
        };
        let timing = &mut shared.timing[index].1;
        timing.calls += 1;
        timing.total += took;
        timing.max = timing.max.max(took);
    }

    /// Notes that an event callback answered.
    pub(crate) fn event_ok(&self) {
        self.shared.borrow_mut().strikes = 0;
    }

    /// Calls a callback of the script, if the script defines it and the
    /// script is in charge. A failure is recorded and returned.
    pub(crate) fn call<A: mlua::IntoLuaMulti, R: mlua::FromLuaMulti>(
        &self,
        name: &'static str,
        args: A,
    ) -> Option<Result<R, ScriptError>> {
        if self.mode() == Mode::Safe || !self.defines(name) {
            return None;
        }
        let started = Instant::now();
        let result = self.host.call_function::<A, R>(name, args, None);
        self.time_call(name, started);
        match &result {
            Ok(_) => self.event_ok(),
            Err(error) => self.record_event_failure(name, error),
        }
        Some(result)
    }

    /// Calls any global function of the script with JSON arguments. For the
    /// control interface: tests and tools reach script-defined hooks with it.
    pub fn call_json(&self, function: &str, args: &[Value]) -> Result<Value, String> {
        use mlua::LuaSerdeExt;
        let result = self.host.run(None, |lua| {
            let f: mlua::Function = lua.globals().get(function)?;
            // JSON `null` is `nil` for the script, not a placeholder object.
            let options = mlua::serde::ser::Options::new()
                .serialize_none_to_null(false)
                .serialize_unit_to_null(false);
            let args = args
                .iter()
                .map(|a| lua.to_value_with(a, options))
                .collect::<mlua::Result<mlua::Variadic<mlua::Value>>>()?;
            let value: mlua::Value = f.call(args)?;
            lua.from_value::<Value>(value)
        });
        result.map_err(|error| {
            self.record(function, &error);
            error.to_string()
        })
    }

    /// Opens a GPU frame if none is open.
    pub(crate) fn ensure_frame(&self) -> Result<(), GfxError> {
        let mut gpu = self.gpu();
        if !gpu.is_recording() {
            gpu.begin_frame()?;
        }
        Ok(())
    }

    /// Ends the open GPU frame, if any. An open render pass (a script that
    /// failed half-way) is closed by `end_frame`. With `wait`, returns once
    /// the GPU has finished the frame.
    pub(crate) fn finish_frame(&self, wait: bool) {
        let mut gpu = self.gpu();
        if gpu.is_recording()
            && let Err(error) = gpu.end_frame()
            && error.code != ErrorCode::InvalidState
        {
            tracing::warn!("ending the GPU frame failed: {error}");
        }
        if wait && let Err(error) = gpu.wait_idle() {
            tracing::warn!("waiting for the GPU failed: {error}");
        }
        drop(gpu);
        self.collect_retired_buffers();
    }

    /// The dma-buf formats and modifiers this device imports, for
    /// `zwp_linux_dmabuf_v1`.
    pub fn dmabuf_formats(&self) -> Vec<(u32, u64)> {
        self.gpu()
            .dmabuf_import_formats()
            .iter()
            .map(|f| (f.fourcc, f.modifier))
            .collect()
    }

    fn describe(&self) -> Value {
        let gpu = self.gpu();
        let device = gpu.device_description();
        let shared = self.shared.borrow();
        json!({
            "kind": "script",
            "renderer": {
                "device": device.name,
                "kind": if device.is_cpu { "cpu" } else { "gpu" },
                "driver": device.driver,
                "driver_info": Value::Null,
                "validation_errors":
                    self.options.gpu.validation.then(gfx_core::validation_error_count),
                "validation_warnings": Value::Null,
                "dmabuf_export": device.dmabuf_export,
            },
            "frames_rendered": shared.frames + shared.fallback_frames,
            "script": {
                "language": gfx_lua::VM_NAME,
                "project": shared.project.describe(),
                "mode": shared.mode.name(),
                "on_error": match self.options.on_error {
                    OnError::Hold => "hold",
                    OnError::Safe => "safe",
                },
                "frames": shared.frames,
                "fallback_frames": shared.fallback_frames,
                "kept_frames": shared.kept_frames,
                "failures": shared.failure_count,
                "last_failures": shared.failures.iter().map(|f| json!({
                    "what": f.what,
                    "kind": f.kind,
                    "message": f.message.lines().next().unwrap_or(""),
                })).collect::<Vec<_>>(),
                "reloads": shared.reloads,
                "strikes": shared.strikes,
                "timing": shared.timing.iter().map(|(name, t)| {
                    ((*name).to_string(), json!({
                        "calls": t.calls,
                        "mean_us": t.total.as_secs_f64() * 1e6 / t.calls.max(1) as f64,
                        "max_us": t.max.as_secs_f64() * 1e6,
                    }))
                }).collect::<serde_json::Map<String, Value>>(),
                "dmabuf": shared.dmabuf.describe(),
                "callbacks": shared.defined,
                "memory_bytes": self.host.lua().used_memory(),
                "gpu": {
                    "memory_bytes": gpu.stats().memory_bytes,
                    "images": gpu.stats().live_images,
                    "buffers": gpu.stats().live_buffers,
                    "slots": gpu.stats().live_slots,
                    "pipelines": gpu.stats().live_pipelines,
                },
            },
        })
    }
}

fn output_format(format: OutputFormat) -> Format {
    match format {
        OutputFormat::Rgba8Unorm => Format::Rgba8Unorm,
        OutputFormat::Rgba16Unorm => Format::Rgba16Unorm,
        OutputFormat::Bgra8Unorm => Format::Bgra8Unorm,
    }
}

fn provider_error(context: &str, error: GfxError) -> RenderError {
    RenderError::Provider(format!("{context}: {error}"))
}

/// The output images, as the backend sees them.
pub struct ScriptTargets {
    runtime: Rc<ScriptRuntime>,
}

impl TargetProvider for ScriptTargets {
    fn create_target(&mut self, desc: &TargetDesc) -> Result<TargetId, RenderError> {
        let mut gpu = self.runtime.gpu();
        let mut images = Vec::new();
        for _ in 0..2 {
            let image = gpu
                .create_image(&ImageDesc {
                    format: output_format(desc.format),
                    width: desc.width,
                    height: desc.height,
                    usage: ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC,
                })
                .map_err(|e| provider_error("creating an output image", e))?;
            gpu.protect(image);
            images.push(image);
        }
        drop(gpu);
        let mut shared = self.runtime.shared.borrow_mut();
        let id = shared.next_target;
        shared.next_target += 1;
        shared.targets.insert(
            id,
            OutputTarget {
                desc: *desc,
                images,
                front: 0,
                rendered: false,
            },
        );
        Ok(TargetId::from_raw(id))
    }

    fn scanout_modifiers(&self) -> Vec<u64> {
        self.runtime.gpu().scanout_modifiers()
    }

    fn create_scanout_target(
        &mut self,
        desc: &TargetDesc,
        allowed_modifiers: &[u64],
    ) -> Result<(TargetId, DmabufPlane), RenderError> {
        if desc.format != OutputFormat::Bgra8Unorm {
            return Err(RenderError::UnsupportedFormat(
                "scanout targets must be bgra8 (DRM XRGB8888)",
            ));
        }
        let mut gpu = self.runtime.gpu();
        let (image, plane) = gpu
            .create_scanout_image(desc.width, desc.height, allowed_modifiers)
            .map_err(|e| match e.code {
                ErrorCode::Unsupported => RenderError::ScanoutUnsupported,
                _ => provider_error("creating a scanout image", e),
            })?;
        gpu.protect(image);
        drop(gpu);
        let mut shared = self.runtime.shared.borrow_mut();
        let id = shared.next_target;
        shared.next_target += 1;
        shared.targets.insert(
            id,
            OutputTarget {
                desc: *desc,
                images: vec![image],
                front: 0,
                rendered: false,
            },
        );
        Ok((
            TargetId::from_raw(id),
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

    fn destroy_target(&mut self, id: TargetId) {
        let target = self.runtime.shared.borrow_mut().targets.remove(&id.raw());
        if let Some(target) = target {
            let mut gpu = self.runtime.gpu();
            for image in target.images {
                let _ = gpu.destroy(image);
            }
        }
    }

    fn read_encoded_into(
        &mut self,
        target: TargetId,
        destination: &mut [u8],
        stride: usize,
    ) -> Result<(), RenderError> {
        let image = {
            let shared = self.runtime.shared.borrow();
            let target = shared
                .targets
                .get(&target.raw())
                .ok_or(RenderError::UnknownTarget)?;
            if !target.rendered {
                return Err(RenderError::TargetNotRendered);
            }
            target.images[target.front]
        };
        self.runtime.finish_frame(false);
        self.runtime
            .gpu()
            .read_image_into(image, destination, stride)
            .map_err(|e| provider_error("reading an output image", e))
    }
}

/// The [`Scene`] backed by a script.
pub struct ScriptScene {
    runtime: Rc<ScriptRuntime>,
    targets: ScriptTargets,
}

impl ScriptScene {
    pub fn new(runtime: Rc<ScriptRuntime>) -> Self {
        Self {
            targets: ScriptTargets {
                runtime: Rc::clone(&runtime),
            },
            runtime,
        }
    }

    fn position(&self, window: WindowId) -> Option<(f64, f64)> {
        self.runtime
            .shared
            .borrow()
            .world
            .windows
            .iter()
            .find(|w| w.window == window)
            .map(|w| (w.position.0 as f64, w.position.1 as f64))
    }

    /// Whether the script should be asked to draw now.
    fn script_draws(&self) -> bool {
        let mut shared = self.runtime.shared.borrow_mut();
        match shared.mode {
            Mode::Running => true,
            Mode::Safe => false,
            Mode::Hold => {
                let due = shared
                    .hold_since
                    .is_none_or(|since| since.elapsed() >= self.runtime.options.retry_after);
                if due {
                    shared.hold_since = Some(Instant::now());
                }
                due
            }
        }
    }

    /// Asks the script for a frame. `Ok(false)` when the script does not
    /// define `frame`.
    fn script_frame(&self, output: &OutputInfo, image: Handle) -> Result<bool, ScriptError> {
        if !self.runtime.defines("frame") {
            return Ok(false);
        }
        let started = Instant::now();
        let result = self.runtime.host.run(None, |lua| {
            let function: mlua::Function = lua.globals().get("frame")?;
            let output = api::output_table(lua, output)?;
            function.call::<()>((output, image.to_bits()))
        });
        self.runtime.time_call("frame", started);
        result?;
        // A frame that returns in the middle of a render pass did not
        // finish what it was drawing.
        if self.runtime.gpu().is_in_pass() {
            return Err(ScriptError::Runtime(
                "frame returned with a render pass still open".to_string(),
            ));
        }
        Ok(true)
    }
}

impl Scene for ScriptScene {
    fn window_added(&mut self, window: WindowId, _position: (i32, i32)) {
        self.runtime.shared.borrow_mut().opacity.insert(window, 1.0);
    }

    fn window_moved(&mut self, _window: WindowId, _position: (i32, i32)) {
        // Positions arrive with the world snapshot.
    }

    fn window_removed(&mut self, window: WindowId) {
        self.runtime.shared.borrow_mut().opacity.remove(&window);
    }

    fn set_window_opacity(&mut self, window: WindowId, opacity: f32) -> bool {
        match self.runtime.shared.borrow_mut().opacity.get_mut(&window) {
            Some(value) => {
                *value = opacity.clamp(0.0, 1.0);
                true
            }
            None => false,
        }
    }

    fn visual(&self, window: WindowId) -> Option<VisualWindow> {
        let opacity = *self.runtime.shared.borrow().opacity.get(&window)?;
        Some(VisualWindow {
            position: self.position(window)?,
            opacity,
        })
    }

    fn update_world(&mut self, world: &WorldSnapshot) {
        let mut shared = self.runtime.shared.borrow_mut();
        if shared.world != *world {
            shared.world = world.clone();
        }
    }

    fn take_requests(&mut self) -> Vec<SceneRequest> {
        let mut shared = self.runtime.shared.borrow_mut();
        // A held script is retried on a timer, which needs frames to happen.
        if shared.mode == Mode::Hold
            && shared
                .hold_since
                .is_some_and(|since| since.elapsed() >= self.runtime.options.retry_after)
        {
            shared.requests.push(SceneRequest::Redraw);
        }
        std::mem::take(&mut shared.requests)
    }

    fn surface_contents(
        &mut self,
        surface: SurfaceId,
        contents: &BufferContents,
        damage: &[DamageRect],
        image_description: ImageDescription,
        alpha_mode: AlphaMode,
    ) -> Result<(), SceneError> {
        let (format, opaque) = match contents.format {
            ShmFormat::Argb8888 => (Format::Bgra8Unorm, false),
            ShmFormat::Xrgb8888 => (Format::Bgra8Unorm, true),
            ShmFormat::Abgr8888 => (Format::Rgba8Unorm, false),
            ShmFormat::Xbgr8888 => (Format::Rgba8Unorm, true),
        };
        let fail = |context: &str, e: GfxError| SceneError::Other(gfx(context, e));
        self.runtime
            .ensure_frame()
            .map_err(|e| fail("opening a GPU frame", e))?;

        let (width, height) = (contents.width, contents.height);
        let reusable = self
            .runtime
            .shared
            .borrow()
            .textures
            .get(&surface)
            .is_some_and(|t| {
                !t.imported
                    && (t.width, t.height, t.format, t.opaque) == (width, height, format, opaque)
            });
        if !reusable {
            let previous = self.runtime.shared.borrow_mut().textures.remove(&surface);
            let generation = match previous {
                Some(old) if old.imported => {
                    // The surface went from dma-bufs to pixels.
                    self.runtime.release_surface_buffers(surface);
                    old.generation + 1
                }
                Some(old) => {
                    let mut gpu = self.runtime.gpu();
                    let _ = gpu.destroy(old.slot);
                    let _ = gpu.destroy(old.image);
                    old.generation + 1
                }
                None => 1,
            };
            let mut gpu = self.runtime.gpu();
            let image = gpu
                .create_image(&ImageDesc {
                    format,
                    width,
                    height,
                    usage: ImageUsage::SAMPLED | ImageUsage::COPY_DST | ImageUsage::COPY_SRC,
                })
                .map_err(|e| fail("creating a surface texture", e))?;
            let slot = if opaque {
                gpu.create_opaque_texture_slot(image)
            } else {
                gpu.create_texture_slot(image)
            };
            let slot = match slot {
                Ok(slot) => slot,
                Err(e) => {
                    let _ = gpu.destroy(image);
                    return Err(fail("creating a surface texture slot", e));
                }
            };
            gpu.protect(image);
            gpu.protect(slot);
            drop(gpu);
            self.runtime.shared.borrow_mut().textures.insert(
                surface,
                SurfaceTexture {
                    image,
                    slot,
                    width,
                    height,
                    format,
                    opaque,
                    generation,
                    serial: 0,
                    damage: Vec::new(),
                    color: 0,
                    imported: false,
                    transform: BufferTransform::Normal,
                    y_inverted: false,
                },
            );
        }

        // The upload wants tightly packed rows.
        let row = width as usize * 4;
        let packed;
        let pixels: &[u8] = if contents.stride as usize == row {
            &contents.data[..row * height as usize]
        } else {
            let mut data = Vec::with_capacity(row * height as usize);
            for y in 0..height as usize {
                let start = y * contents.stride as usize;
                data.extend_from_slice(&contents.data[start..start + row]);
            }
            packed = data;
            &packed
        };
        let image = self.runtime.shared.borrow().textures[&surface].image;
        self.runtime
            .gpu()
            .upload_image(image, pixels)
            .map_err(|e| fail("uploading a surface", e))?;

        let mut shared = self.runtime.shared.borrow_mut();
        let color = shared.color_id(image_description, alpha_mode);
        // Buffer damage is passed on as it is. Surface damage would need the
        // buffer scale and viewport to convert; it is reported as "all".
        let rects: Option<Vec<[i32; 4]>> = damage
            .iter()
            .map(|d| match *d {
                DamageRect::Buffer {
                    x,
                    y,
                    width,
                    height,
                } => Some([x, y, width, height]),
                DamageRect::Surface { .. } => None,
            })
            .collect();
        let texture = shared.textures.get_mut(&surface).expect("inserted above");
        texture.serial += 1;
        texture.color = color;
        texture.damage = rects.unwrap_or_default();
        Ok(())
    }

    fn surface_buffer(
        &mut self,
        surface: SurfaceId,
        update: BufferUpdate,
        image_description: ImageDescription,
        alpha_mode: AlphaMode,
    ) -> Result<(), SceneError> {
        self.runtime.collect_retired_buffers();
        if let BufferSource::Dmabuf(contents) = &update.source {
            match self.runtime.import_surface(
                surface,
                contents,
                &update,
                image_description,
                alpha_mode,
            ) {
                Ok(()) => {
                    self.runtime.hold_buffer(surface, update);
                    return Ok(());
                }
                Err(reason) => {
                    if self.runtime.note_dmabuf_fallback(surface) {
                        tracing::info!(
                            ?surface,
                            reason,
                            "a dma-buf is not imported; reading it through a mapping"
                        );
                    }
                }
            }
        }
        // Pixels in CPU memory: `wl_shm`, or a dma-buf that can be mapped.
        let contents = update
            .upright_pixels()
            .map_err(|e| SceneError::Unreadable(e.to_string()))?;
        // Damage in buffer coordinates describes the buffer before the
        // transform was undone; rather than map it, report "everything".
        let damage: &[DamageRect] = match update.transform {
            BufferTransform::Normal => &update.damage,
            _ => &[],
        };
        self.surface_contents(surface, &contents, damage, image_description, alpha_mode)
    }

    fn surface_removed(&mut self, surface: SurfaceId) {
        let removed = self.runtime.shared.borrow_mut().textures.remove(&surface);
        match removed {
            Some(texture) if !texture.imported => {
                let mut gpu = self.runtime.gpu();
                let _ = gpu.destroy(texture.slot);
                let _ = gpu.destroy(texture.image);
            }
            _ => {}
        }
        self.runtime.release_surface_buffers(surface);
    }

    fn has_contents(&self, surface: SurfaceId) -> bool {
        self.runtime.shared.borrow().textures.contains_key(&surface)
    }

    fn draw(
        &mut self,
        output: &OutputInfo,
        target: Option<TargetId>,
        background: [f32; 3],
        windows: &[WindowView],
        cursor: Option<&CursorView>,
    ) -> Result<DrawStatus, SceneError> {
        let runtime = Rc::clone(&self.runtime);
        let Some(target) = target else {
            runtime.finish_frame(false);
            return Ok(DrawStatus::Drawn);
        };
        // The color of a surface can change without new pixels.
        {
            let mut shared = runtime.shared.borrow_mut();
            for surface in windows.iter().flat_map(|w| w.surfaces.iter()) {
                let color = shared.color_id(surface.image_description, surface.alpha_mode);
                if let Some(texture) = shared.textures.get_mut(&surface.surface) {
                    texture.color = color;
                }
            }
        }
        let (image, scanout) = {
            let shared = runtime.shared.borrow();
            let Some(target) = shared.targets.get(&target.raw()) else {
                return Err(SceneError::Render(RenderError::UnknownTarget));
            };
            // Draw into the image that is not shown.
            let back = (target.front + 1) % target.images.len();
            (target.images[back], target.images.len() == 1)
        };
        if let Err(error) = runtime.ensure_frame() {
            // The device is gone or stuck. Nothing can be drawn; keep going
            // so that clients and input stay alive.
            runtime.record("frame", &ScriptError::Gfx(error));
            runtime.shared.borrow_mut().kept_frames += 1;
            return Ok(DrawStatus::Kept);
        }

        let mut drawn_by_script = false;
        if self.script_draws() {
            match self.script_frame(output, image) {
                Ok(true) => {
                    drawn_by_script = true;
                    let mut shared = runtime.shared.borrow_mut();
                    if shared.mode == Mode::Hold {
                        tracing::info!("the script draws again");
                        shared.mode = Mode::Running;
                        shared.hold_since = None;
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    runtime.record("frame", &error);
                    let mut shared = runtime.shared.borrow_mut();
                    match runtime.options.on_error {
                        OnError::Hold => {
                            shared.mode = Mode::Hold;
                            shared.hold_since = Some(Instant::now());
                        }
                        OnError::Safe => shared.mode = Mode::Safe,
                    }
                }
            }
        }

        let status = if drawn_by_script {
            runtime.shared.borrow_mut().frames += 1;
            DrawStatus::Drawn
        } else if runtime.mode() == Mode::Hold {
            // What was asked for is still not on screen. Ask for a frame
            // again, so that the script gets its next try when the retry
            // time has passed, and not only when something else happens to
            // need a frame.
            let mut shared = runtime.shared.borrow_mut();
            shared.kept_frames += 1;
            shared.requests.push(SceneRequest::Redraw);
            DrawStatus::Kept
        } else {
            // The script does not draw (it failed, or it defines no `frame`):
            // the built-in fallback does. Close what the script left open
            // first.
            runtime.finish_frame(false);
            let drew = runtime.ensure_frame().and_then(|()| {
                runtime
                    .fallback
                    .borrow_mut()
                    .draw(&runtime, output, image, background, windows, cursor)
            });
            match drew {
                Ok(()) => {
                    runtime.shared.borrow_mut().fallback_frames += 1;
                    DrawStatus::Drawn
                }
                Err(error) => {
                    runtime.record("fallback", &ScriptError::Gfx(error));
                    runtime.shared.borrow_mut().kept_frames += 1;
                    DrawStatus::Kept
                }
            }
        };

        // A scanout image is handed to the display right after this, so the
        // GPU must be done with it.
        runtime
            .finish_frame((scanout || runtime.options.wait_for_gpu) && status == DrawStatus::Drawn);
        if status == DrawStatus::Drawn {
            let mut shared = runtime.shared.borrow_mut();
            if let Some(target) = shared.targets.get_mut(&target.raw()) {
                target.front = (target.front + 1) % target.images.len();
                target.rendered = true;
            }
        }
        Ok(status)
    }

    fn targets(&mut self) -> Targets<'_> {
        Some(&mut self.targets)
    }

    fn hit_test(
        &self,
        point: (f64, f64),
        windows: &[WindowView],
        accepts_input: &dyn Fn(SurfaceId, f64, f64) -> bool,
    ) -> Option<Hit> {
        let runtime = &self.runtime;
        if runtime.mode() != Mode::Safe && runtime.defines("hit_test") {
            // The script gets a function to ask what only the protocol side
            // knows: whether a surface takes input at a point.
            let started = Instant::now();
            let result = runtime.host.run(None, |lua| {
                lua.scope(|scope| {
                    let accepts = scope.create_function(|_, (s, x, y): (u64, f64, f64)| {
                        Ok(accepts_input(SurfaceId(s), x, y))
                    })?;
                    let function: mlua::Function = lua.globals().get("hit_test")?;
                    function.call::<(Option<u64>, Option<f64>, Option<f64>)>((
                        point.0, point.1, accepts,
                    ))
                })
            });
            runtime.time_call("hit_test", started);
            match result {
                Ok((Some(surface), Some(x), Some(y))) => {
                    runtime.event_ok();
                    let surface = SurfaceId(surface);
                    // Only a surface of a window that exists can be hit.
                    return windows
                        .iter()
                        .find(|w| w.surfaces.iter().any(|s| s.surface == surface))
                        .map(|w| Hit {
                            window: w.window,
                            surface,
                            local: (x, y),
                        });
                }
                Ok(_) => {
                    runtime.event_ok();
                    return None;
                }
                Err(error) => runtime.record_event_failure("hit_test", &error),
            }
        }
        flat_hit_test(|w| self.position(w), point, windows, accepts_input)
    }

    fn locate(
        &self,
        point: (f64, f64),
        surface: SurfaceId,
        windows: &[WindowView],
    ) -> Option<(f64, f64)> {
        let runtime = &self.runtime;
        if runtime.mode() != Mode::Safe && runtime.defines("locate") {
            match runtime.host.call_function::<_, (Option<f64>, Option<f64>)>(
                "locate",
                (surface.0, point.0, point.1),
                None,
            ) {
                Ok((Some(x), Some(y))) => {
                    runtime.event_ok();
                    return Some((x, y));
                }
                Ok(_) => {
                    runtime.event_ok();
                    return None;
                }
                Err(error) => runtime.record_event_failure("locate", &error),
            }
        }
        flat_locate(|w| self.position(w), point, surface, windows)
    }

    fn capture(&mut self, target: TargetId) -> Result<Capture, SceneError> {
        let (desc, image, composite) = {
            let shared = self.runtime.shared.borrow();
            let target = shared
                .targets
                .get(&target.raw())
                .ok_or(RenderError::UnknownTarget)?;
            if !target.rendered {
                return Err(RenderError::TargetNotRendered.into());
            }
            (
                target.desc,
                target.images[target.front],
                shared.debug_composite,
            )
        };
        self.runtime.finish_frame(false);
        let mut gpu = self.runtime.gpu();
        let fail = |context: &str, e: GfxError| SceneError::Other(gfx(context, e));
        let data = gpu
            .read_image(image)
            .map_err(|e| fail("reading the output image", e))?;
        let encoded = EncodedImage {
            width: desc.width,
            height: desc.height,
            format: desc.format,
            image_description: desc.image_description,
            data,
        };
        // The compositing buffer is the script's own. It is captured only
        // when the script offers one of the right shape.
        let composite = composite.and_then(|(image, primaries)| {
            let info = gpu.image_info(image).ok()?;
            if info.format != Format::Rgba16Float
                || (info.width, info.height) != (desc.width, desc.height)
            {
                return None;
            }
            let bytes = gpu.read_image(image).ok()?;
            Some(CompositeImage {
                width: desc.width,
                height: desc.height,
                space: CompositeSpace { primaries },
                data: bytes
                    .chunks_exact(2)
                    .map(|b| u16::from_ne_bytes([b[0], b[1]]))
                    .collect(),
            })
        });
        Ok(Capture { encoded, composite })
    }

    fn describe(&self) -> Value {
        self.runtime.describe()
    }

    fn control(&mut self, command: &str, args: &Value) -> Option<Result<Value, String>> {
        let runtime = &self.runtime;
        Some(match command {
            "script_call" => {
                let function = args["function"].as_str().unwrap_or_default();
                let list = args["args"].as_array().cloned().unwrap_or_default();
                runtime.call_json(function, &list)
            }
            "script_reload" => {
                let project = args["path"]
                    .as_str()
                    .map(|p| Project::Dir(PathBuf::from(p)));
                runtime.reload(project).map(|()| json!({}))
            }
            "script_mode" => match args["mode"].as_str() {
                Some("running") => {
                    runtime.set_mode(Mode::Running);
                    Ok(json!({}))
                }
                Some("safe") => {
                    runtime.set_mode(Mode::Safe);
                    Ok(json!({}))
                }
                other => Err(format!("unknown mode {other:?}")),
            },
            _ => return None,
        })
    }
}
