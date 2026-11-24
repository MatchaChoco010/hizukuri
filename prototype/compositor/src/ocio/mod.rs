//! OpenColorIO in the core: conversions as draws ([`pass`]), compiled in the
//! background and kept ready, and which of them each output is shown
//! through.
//!
//! - An [`Engine`] holds one OCIO config. A [`Transform`] names a conversion
//!   of that config (a colour space to another, or to a display and view).
//! - The first time a conversion is asked for, a worker thread takes its
//!   shader out of OCIO, compiles it ([`pass::prepare`]) and runs the SPIR-V
//!   checks of the device ([`pass::Prepared::check`]); the frames go on
//!   meanwhile. When the worker is done, the next frame makes the GPU
//!   objects and starts building the pipeline for the target's format on
//!   another thread ([`Pass::ready`]); once it is built, the conversion is
//!   ready.
//! - An output may be given a display and a view ([`Engine::set_output`]).
//!   [`Engine::present`] then draws the output's image through that
//!   conversion; while a newly chosen one is not ready it draws through the
//!   previous one, and says when there is none (the caller then converts
//!   as it does without OCIO).
//!
//! The values that enter a conversion are linear light in the working
//! colour space of the config ([`Engine::working`]), with 1.0 the reference
//! white. OCIO's displays take 1.0 as 100 cd/m² (HDR displays) or as the
//! display's white (SDR displays); the scale between the two is a
//! multiplication in front of the conversion ([`DrawParams::scale`],
//! [`reference_white_scale`]).

pub mod pass;
pub mod signal;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use proto_color::{Chromaticity, ImageDescription, Primaries, TransferFunction};
use proto_gfx_core::{Format, Gpu, Handle, ShaderChecker};
use proto_ocio::{Config, DisplayRequest, Dynamic, Processor};
use proto_slangc::Compiler;
use serde_json::{Value, json};

pub use pass::DrawParams;
use pass::{Pass, Prepared};
pub use signal::DisplaySignal;

/// Where a conversion ends.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// A colour space (or role) of the config.
    ColorSpace(String),
    /// What a display shows through a view.
    Display { display: String, view: String },
}

/// One conversion of the config.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Transform {
    /// The colour space (or role) of the input.
    pub src: String,
    pub dst: Target,
    /// With an exposure (in front) and a gamma (behind) that can change
    /// without compiling again. Only for [`Target::Display`].
    pub dynamic: bool,
}

impl Transform {
    fn describe(&self) -> String {
        match &self.dst {
            Target::ColorSpace(dst) => format!("{} -> {dst}", self.src),
            Target::Display { display, view } => {
                format!("{} -> {display} / {view}", self.src)
            }
        }
    }
}

/// How an [`Engine`] is set up.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// A config file or an `ocio://` name. Without it: what the environment
    /// variable `OCIO` names, or else OCIO's default built-in config.
    pub config: Option<String>,
    /// The colour space of the config that the compositor works in. Without
    /// it: `ACEScg`, or else the config's `scene_linear` role.
    pub working: Option<String>,
    /// Where compiled shaders are kept. Without it: `hizukuri/ocio` under
    /// `XDG_CACHE_HOME` (or `~/.cache`); nowhere when neither is known.
    pub cache_dir: Option<PathBuf>,
    /// `(output name, display, view)`: an output without a name sets every
    /// output that has no setting of its own. A missing display or view is
    /// the config's default.
    pub outputs: Vec<(Option<String>, Option<String>, Option<String>)>,
    /// `(output name, exposure, gamma)`: the controls of an output (of
    /// every output without a setting of its own when it has no name).
    pub controls: Vec<(Option<String>, Option<f64>, Option<f64>)>,
    /// The `slangc` to compile with. Without it: [`find_slangc`].
    pub slangc: Option<PathBuf>,
    /// For tests: every compilation takes this much longer.
    pub compile_delay: Option<Duration>,
    /// Whether an output that is given a display is also driven, and
    /// described to applications, as that display's signal (when the signal
    /// can be told and the backend can do it).
    pub outputs_follow_display: bool,
}

/// The built-in config used when nothing names one.
const DEFAULT_CONFIG: &str = "ocio://default";

/// Finds `slangc` for a running compositor: the file the environment
/// variable names, one next to the executable, the one the compositor was
/// built with (when it is still where it was), one on `PATH`.
///
/// `PATH` comes last: a `slangc` found there may be of any version (one
/// from an older Vulkan SDK does not read its source from standard input,
/// which is how the shaders are handed over).
pub fn find_slangc() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(proto_slangc::SLANGC_VARIABLE) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
        tracing::warn!(
            "{} names {}, which is not a file",
            proto_slangc::SLANGC_VARIABLE,
            path.display()
        );
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let beside = dir.join("slangc");
        if beside.is_file() {
            return Some(beside);
        }
    }
    let built_with = PathBuf::from(env!("HIZUKURI_BUILD_SLANGC"));
    if built_with.is_file() {
        return Some(built_with);
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join("slangc"))
        .find(|candidate| candidate.is_file())
}

fn default_cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("hizukuri").join("ocio"))
}

/// What multiplies working-space values (1.0 is the reference white) in
/// front of a conversion to an OCIO display, for an output driven as
/// `description` says.
///
/// OCIO's HDR displays (PQ, HLG) take 1.0 as 100 cd/m², so the reference
/// white of the output, in cd/m², is divided by 100. Its SDR displays take
/// 1.0 as the display's white, so the scale is where the reference white
/// sits between the output's black and white (1 for an ordinary SDR
/// output).
pub fn reference_white_scale(description: &ImageDescription) -> f32 {
    let l = &description.luminance;
    let scale = match description.transfer {
        TransferFunction::Pq | TransferFunction::Hlg => l.reference / 100.0,
        _ => (l.reference - l.min) / (l.max - l.min),
    };
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

/// The primaries a 3x3 matrix from RGB to CIE XYZ stands for.
fn primaries_of(m: &[[f64; 3]; 3]) -> Option<Primaries> {
    let xy = |v: [f64; 3]| {
        let sum = v[0] + v[1] + v[2];
        (sum.abs() > 1e-9).then(|| Chromaticity::new(v[0] / sum, v[1] / sum))
    };
    let column = |c: usize| [m[0][c], m[1][c], m[2][c]];
    let white = [0, 1, 2].map(|r| m[r][0] + m[r][1] + m[r][2]);
    let primaries = Primaries {
        red: xy(column(0))?,
        green: xy(column(1))?,
        blue: xy(column(2))?,
        white: xy(white)?,
    };
    primaries.rgb_to_xyz()?;
    Some(primaries)
}

struct Job {
    transform: Transform,
    /// The SPIR-V checks of the device the conversion is for.
    checker: ShaderChecker,
}

struct Done {
    transform: Transform,
    result: Result<Prepared, String>,
    took: Duration,
}

fn make_processor(config: &Config, transform: &Transform) -> Result<Processor, String> {
    match &transform.dst {
        Target::ColorSpace(dst) => config.processor(&transform.src, dst),
        Target::Display { display, view } => {
            let mut request = DisplayRequest::new(&transform.src, display, view);
            request.dynamic_exposure = transform.dynamic;
            request.dynamic_gamma = transform.dynamic;
            config.display_processor(&request)
        }
    }
    .map_err(|e| format!("OCIO has no conversion {}: {e}", transform.describe()))
}

fn worker(
    config: Arc<Config>,
    compiler: Option<Compiler>,
    cache_dir: Option<PathBuf>,
    delay: Option<Duration>,
    jobs: Receiver<Job>,
    done: Sender<Done>,
) {
    while let Ok(job) = jobs.recv() {
        let started = Instant::now();
        let result = make_processor(&config, &job.transform)
            .and_then(|processor| {
                pass::prepare(&processor, compiler.as_ref(), cache_dir.as_deref())
            })
            .and_then(|mut prepared| {
                prepared.check(&job.checker)?;
                Ok(prepared)
            });
        if let Some(delay) = delay {
            std::thread::sleep(delay);
        }
        let done_message = Done {
            transform: job.transform,
            result,
            took: started.elapsed(),
        };
        if done.send(done_message).is_err() {
            break;
        }
    }
}

enum Entry {
    /// The worker has it.
    Pending,
    Ready(Box<Pass>),
    Failed(String),
}

/// What an output is to be shown through: what was chosen for it, or for
/// every output.
#[derive(Clone, Debug, Default, PartialEq)]
struct OutputState {
    /// `(display, view)`.
    setting: Option<(String, String)>,
    exposure: f64,
    gamma: f64,
    /// Whether exposure or gamma was ever set: the conversion then has the
    /// controls.
    dynamic: bool,
}

/// What was chosen for one output apart from every output. What is `None`
/// follows the setting for every output.
#[derive(Clone, Debug, Default, PartialEq)]
struct OutputOverride {
    /// `Some(None)`: this output is not shown through OCIO.
    setting: Option<Option<(String, String)>>,
    exposure: Option<f64>,
    gamma: Option<f64>,
}

/// What an output was last shown through.
#[derive(Clone, Debug, Default, PartialEq)]
struct Shown {
    /// The conversion the output was last drawn through.
    current: Option<Transform>,
    /// The failure that was last reported for this output.
    reported: Option<String>,
}

/// What [`Engine::present`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presented {
    /// The output has no display and view: nothing was drawn.
    NotConfigured,
    /// Nothing is ready for the output (the first conversion is still being
    /// compiled, or failed): nothing was drawn.
    NotReady { pending: bool },
    /// Drawn. `pending`: through the previous conversion, the chosen one is
    /// still being compiled.
    Drawn { pending: bool },
}

/// Counters, for the state dump and the tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Conversions the worker compiled with `slangc`.
    pub compiled: u64,
    /// Conversions whose SPIR-V came from the cache on disk.
    pub from_cache: u64,
    pub failed: u64,
    /// Draws through a conversion.
    pub draws: u64,
    /// Draws through the previous conversion while another was compiled.
    pub draws_while_pending: u64,
}

/// Conversions kept ready beyond the ones outputs use.
const KEPT_PASSES: usize = 16;

pub struct Engine {
    config: Arc<Config>,
    config_name: String,
    working: String,
    working_primaries: Option<Primaries>,
    slangc: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    jobs: Sender<Job>,
    done: Receiver<Done>,
    /// Results of the worker that wait for a frame to make GPU objects in.
    arrived: Vec<Done>,
    entries: HashMap<Transform, Entry>,
    /// When each ready conversion was last drawn (a count of draws).
    used: HashMap<Transform, u64>,
    vertex: Option<Handle>,
    /// Outputs with a setting of their own, by name.
    outputs: HashMap<String, OutputOverride>,
    /// The setting of outputs that have none of their own.
    default_output: OutputState,
    /// The outputs that were presented, by name.
    shown: HashMap<String, Shown>,
    /// What the signal of a display colour space is, for those that were
    /// chosen for an output: probed once.
    signals: HashMap<String, Option<DisplaySignal>>,
    stats: Stats,
}

impl Engine {
    pub fn new(options: &Options) -> Result<Self, String> {
        proto_ocio::quiet();
        let (config, spec) = match &options.config {
            Some(spec) => (Config::from_file(spec), spec.clone()),
            None if std::env::var_os("OCIO").is_some_and(|v| !v.is_empty()) => {
                (Config::from_env(), "$OCIO".to_string())
            }
            None => (
                Config::from_file(DEFAULT_CONFIG),
                DEFAULT_CONFIG.to_string(),
            ),
        };
        let config = config.map_err(|e| format!("cannot read the OCIO config {spec}: {e}"))?;
        let working = match &options.working {
            Some(name) => config
                .resolve(name)
                .ok_or_else(|| format!("the OCIO config {spec} has no colour space {name:?}"))?,
            None => config
                .resolve("ACEScg")
                .or_else(|| config.resolve("scene_linear"))
                .ok_or_else(|| {
                    format!("the OCIO config {spec} has neither ACEScg nor a scene_linear role")
                })?,
        };
        let working_primaries = working_primaries(&config, &working);
        if working_primaries.is_none() {
            tracing::warn!(
                "the OCIO colour space {working} is not a matrix away from CIE XYZ: its \
                 primaries are unknown"
            );
        }
        let slangc = match &options.slangc {
            Some(path) => path.is_file().then(|| path.clone()),
            None => find_slangc(),
        };
        if slangc.is_none() {
            tracing::error!(
                "slangc, the Slang compiler, was not found ({}, next to the executable, PATH): \
                 OCIO conversions cannot be compiled, and outputs are converted without OCIO",
                proto_slangc::SLANGC_VARIABLE
            );
        }
        let cache_dir = options.cache_dir.clone().or_else(default_cache_dir);
        let config = Arc::new(config);
        let (jobs, job_receiver) = channel();
        let (done_sender, done) = channel();
        {
            let config = Arc::clone(&config);
            let compiler = slangc.clone().map(Compiler::new);
            let cache_dir = cache_dir.clone();
            let delay = options.compile_delay;
            std::thread::Builder::new()
                .name("ocio-compile".into())
                .spawn(move || {
                    worker(
                        config,
                        compiler,
                        cache_dir,
                        delay,
                        job_receiver,
                        done_sender,
                    )
                })
                .map_err(|e| format!("cannot start the OCIO compile thread: {e}"))?;
        }
        let mut engine = Self {
            config_name: {
                let name = config.name();
                if name.is_empty() { spec.clone() } else { name }
            },
            config,
            working,
            working_primaries,
            slangc,
            cache_dir,
            jobs,
            done,
            arrived: Vec::new(),
            entries: HashMap::new(),
            used: HashMap::new(),
            vertex: None,
            outputs: HashMap::new(),
            default_output: OutputState {
                gamma: 1.0,
                ..Default::default()
            },
            shown: HashMap::new(),
            signals: HashMap::new(),
            stats: Stats::default(),
        };
        for (output, display, view) in &options.outputs {
            engine.set_output(output.as_deref(), display.as_deref(), view.as_deref())?;
        }
        for (output, exposure, gamma) in &options.controls {
            engine.set_output_controls(output.as_deref(), *exposure, *gamma)?;
        }
        Ok(engine)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The colour space of the config the compositor works in.
    pub fn working(&self) -> &str {
        &self.working
    }

    /// The primaries of the working colour space as the config has them:
    /// what a display-referred colour has to be converted to so that the
    /// view without tone mapping gives it back.
    pub fn working_primaries(&self) -> Option<Primaries> {
        self.working_primaries
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Whether a conversion is being compiled, or a pipeline built.
    pub fn busy(&self) -> bool {
        !self.arrived.is_empty()
            || self.entries.values().any(|e| match e {
                Entry::Pending => true,
                Entry::Ready(pass) => pass.is_building(),
                Entry::Failed(_) => false,
            })
    }

    /// What an output is shown through: the setting for every output, with
    /// what was chosen for this one in its place.
    fn output_state(&self, output: &str) -> OutputState {
        let mut state = self.default_output.clone();
        if let Some(own) = self.outputs.get(output) {
            if let Some(setting) = &own.setting {
                state.setting.clone_from(setting);
            }
            if let Some(exposure) = own.exposure {
                state.exposure = exposure;
            }
            if let Some(gamma) = own.gamma {
                state.gamma = gamma;
            }
            state.dynamic |= own.exposure.is_some() || own.gamma.is_some();
        }
        state
    }

    /// Chooses the display and view of an output (of every output without a
    /// setting of its own when `output` is `None`). A missing display is the
    /// config's default display, a missing view the display's default view;
    /// with neither, the output goes back to being converted without OCIO.
    pub fn set_output(
        &mut self,
        output: Option<&str>,
        display: Option<&str>,
        view: Option<&str>,
    ) -> Result<(), String> {
        // What an output's own setting leaves out comes from the setting
        // for every output, when there is one.
        let inherited = match output {
            Some(name) => self.output_state(name).setting,
            None => self.default_output.setting.clone(),
        };
        let setting = if display.is_none() && view.is_none() {
            None
        } else {
            let display = match (display, &inherited) {
                (Some(display), _) => display.to_string(),
                (None, Some((display, _))) => display.clone(),
                (None, None) => self
                    .config
                    .default_display()
                    .ok_or("the OCIO config has no display")?,
            };
            if !self.config.displays().contains(&display) {
                return Err(format!(
                    "the OCIO config has no display {display:?} (it has: {})",
                    self.config.displays().join(", ")
                ));
            }
            let views = self.config.views(&display);
            let view = match (view, &inherited) {
                (Some(view), _) => view.to_string(),
                (None, Some((_, view))) if views.contains(view) => view.clone(),
                _ => self
                    .config
                    .default_view(&display)
                    .ok_or_else(|| format!("the display {display:?} has no view"))?,
            };
            if !views.contains(&view) {
                return Err(format!(
                    "the display {display:?} has no view {view:?} (it has: {})",
                    views.join(", ")
                ));
            }
            if let Some(space) = signal::display_color_space(&self.config, &display, &view)
                && !self.signals.contains_key(&space)
            {
                let probed = signal::probe(&self.config, &space);
                self.signals.insert(space, probed);
            }
            Some((display, view))
        };
        match output {
            None => self.default_output.setting = setting,
            Some(name) => {
                self.outputs.entry(name.to_string()).or_default().setting = Some(setting);
            }
        }
        // A failure is reported again after a new choice.
        for shown in self.shown.values_mut() {
            shown.reported = None;
        }
        Ok(())
    }

    /// Sets the exposure (in stops, in front of the view) and the gamma
    /// (behind it) of an output. The first time, the output's conversion is
    /// compiled again with the controls; after that a change costs nothing.
    pub fn set_output_controls(
        &mut self,
        output: Option<&str>,
        exposure: Option<f64>,
        gamma: Option<f64>,
    ) -> Result<(), String> {
        if exposure.is_some_and(|v| !v.is_finite()) {
            return Err("the exposure is not a number".into());
        }
        if gamma.is_some_and(|v| !(v.is_finite() && v > 0.0)) {
            return Err("the gamma is not a positive number".into());
        }
        match output {
            None => {
                let state = &mut self.default_output;
                if let Some(exposure) = exposure {
                    state.exposure = exposure;
                }
                if let Some(gamma) = gamma {
                    state.gamma = gamma;
                }
                state.dynamic = true;
            }
            Some(name) => {
                let own = self.outputs.entry(name.to_string()).or_default();
                own.exposure = exposure.or(own.exposure);
                own.gamma = gamma.or(own.gamma);
            }
        }
        Ok(())
    }

    fn wanted(&self, state: &OutputState) -> Option<Transform> {
        let (display, view) = state.setting.clone()?;
        Some(Transform {
            src: self.working.clone(),
            dst: Target::Display { display, view },
            dynamic: state.dynamic,
        })
    }

    /// Whether an output is shown through OCIO.
    pub fn output_is_configured(&self, output: &str) -> bool {
        self.output_state(output).setting.is_some()
    }

    fn signal_of(&self, state: &OutputState) -> Option<DisplaySignal> {
        let (display, view) = state.setting.as_ref()?;
        let space = signal::display_color_space(&self.config, display, view)?;
        self.signals.get(&space).copied().flatten()
    }

    /// What the signal of an output is when it is shown through its OCIO
    /// display: the primaries and transfer function of the display's colour
    /// space. `None` without a display, and for a display whose colour
    /// space is not a known curve per channel and a matrix.
    pub fn output_signal(&self, output: &str) -> Option<DisplaySignal> {
        self.signal_of(&self.output_state(output))
    }

    /// Starts compiling a conversion for the device of `gpu`, unless it is
    /// known already.
    fn ensure(&mut self, gpu: &Gpu, transform: &Transform) {
        if self.entries.contains_key(transform) {
            return;
        }
        let entry = match self.jobs.send(Job {
            transform: transform.clone(),
            checker: gpu.shader_checker(),
        }) {
            Ok(()) => Entry::Pending,
            Err(_) => Entry::Failed("the OCIO compile thread is gone".into()),
        };
        self.entries.insert(transform.clone(), entry);
    }

    /// Takes what the worker finished. With a frame being recorded (and no
    /// pass open), also makes the GPU objects.
    fn collect(&mut self, gpu: &mut Gpu) {
        while let Ok(done) = self.done.try_recv() {
            self.arrived.push(done);
        }
        if self.arrived.is_empty() || !gpu.is_recording() || gpu.is_in_pass() {
            return;
        }
        for done in std::mem::take(&mut self.arrived) {
            let what = done.transform.describe();
            let entry = done
                .result
                .and_then(|prepared| {
                    let vertex = match self.vertex {
                        Some(vertex) => vertex,
                        None => {
                            let vertex = pass::create_vertex_shader(gpu)?;
                            self.vertex = Some(vertex);
                            vertex
                        }
                    };
                    let (cached, timing) = (prepared.cached, prepared.timing);
                    let started = Instant::now();
                    let pass = Pass::new(gpu, vertex, prepared)?;
                    if cached {
                        self.stats.from_cache += 1;
                    } else {
                        self.stats.compiled += 1;
                    }
                    tracing::info!(
                        "OCIO conversion {what} is ready: {} (OCIO {:.1} ms, slangc {:.1} ms, \
                         SPIR-V checks {:.1} ms, GPU objects {:.1} ms)",
                        if cached { "from the cache" } else { "compiled" },
                        timing.extract.as_secs_f64() * 1e3,
                        timing.compile.as_secs_f64() * 1e3,
                        timing.check.as_secs_f64() * 1e3,
                        started.elapsed().as_secs_f64() * 1e3,
                    );
                    Ok(Entry::Ready(Box::new(pass)))
                })
                .unwrap_or_else(|error| {
                    self.stats.failed += 1;
                    tracing::error!(
                        "OCIO conversion {what} failed after {:.1} ms: {error}",
                        done.took.as_secs_f64() * 1e3
                    );
                    Entry::Failed(error)
                });
            if let Some(Entry::Ready(mut old)) = self.entries.insert(done.transform, entry) {
                old.destroy(gpu);
            }
        }
    }

    /// Whether a conversion can draw into targets of `format`: its GPU
    /// objects are there and so is its pipeline for the format. Starts
    /// building the pipeline on another thread when it is not; a build that
    /// failed makes the conversion a failed one.
    fn pipeline_ready(&mut self, gpu: &mut Gpu, transform: &Transform, format: Format) -> bool {
        let Some(Entry::Ready(pass)) = self.entries.get_mut(transform) else {
            return false;
        };
        match pass.ready(gpu, format, &transform.describe()) {
            Ok(ready) => ready,
            Err(error) => {
                self.stats.failed += 1;
                tracing::error!("OCIO conversion {} failed: {error}", transform.describe());
                if let Some(Entry::Ready(mut pass)) =
                    self.entries.insert(transform.clone(), Entry::Failed(error))
                {
                    pass.destroy(gpu);
                }
                self.used.remove(transform);
                false
            }
        }
    }

    /// Draws `input` through a ready conversion. `Ok(false)`: not ready
    /// (also while its pipeline for the target's format is being built).
    fn draw_ready(
        &mut self,
        gpu: &mut Gpu,
        transform: &Transform,
        controls: Option<(f64, f64)>,
        input: Handle,
        target: Handle,
        params: DrawParams,
    ) -> Result<bool, String> {
        let format = gpu.image_info(target).map_err(|e| e.to_string())?.format;
        if !self.pipeline_ready(gpu, transform, format) {
            return Ok(false);
        }
        let Some(Entry::Ready(pass)) = self.entries.get_mut(transform) else {
            return Ok(false);
        };
        if let Some((exposure, gamma)) = controls
            && transform.dynamic
        {
            pass.set_dynamic(gpu, Dynamic::Exposure, exposure)?;
            pass.set_dynamic(gpu, Dynamic::Gamma, gamma)?;
        }
        pass.draw(gpu, input, target, params)?;
        self.stats.draws += 1;
        self.used.insert(transform.clone(), self.stats.draws);
        Ok(true)
    }

    /// Destroys ready conversions nothing was drawn through for the longest
    /// time, beyond [`KEPT_PASSES`].
    fn evict(&mut self, gpu: &mut Gpu) {
        let in_use: Vec<&Transform> = self
            .shown
            .values()
            .filter_map(|o| o.current.as_ref())
            .collect();
        let mut idle: Vec<(u64, Transform)> = self
            .entries
            .iter()
            .filter(|(t, e)| matches!(e, Entry::Ready(_)) && !in_use.contains(t))
            .map(|(t, _)| (self.used.get(t).copied().unwrap_or(0), t.clone()))
            .collect();
        if idle.len() <= KEPT_PASSES {
            return;
        }
        idle.sort_by_key(|(used, _)| *used);
        let excess = idle.len() - KEPT_PASSES;
        for (_, transform) in idle.into_iter().take(excess) {
            if let Some(Entry::Ready(mut pass)) = self.entries.remove(&transform) {
                pass.destroy(gpu);
            }
            self.used.remove(&transform);
        }
    }

    /// Converts the image behind `input` (linear light in the working
    /// colour space, as large as `target`) to what the output's display
    /// shows through its view, and draws it into `target`.
    ///
    /// `to_working` brings the input's values to the working colour space
    /// of the config (identity when the script works in it); `scale` is
    /// [`reference_white_scale`] of the output.
    ///
    /// Called while a frame is being recorded and no pass is open.
    pub fn present(
        &mut self,
        gpu: &mut Gpu,
        output: &str,
        input: Handle,
        target: Handle,
        to_working: [[f32; 3]; 3],
        scale: f32,
    ) -> Result<Presented, String> {
        let state = self.output_state(output);
        let Some(wanted) = self.wanted(&state) else {
            self.shown.remove(output);
            return Ok(Presented::NotConfigured);
        };
        self.ensure(gpu, &wanted);
        self.collect(gpu);
        // The chosen conversion takes over once its pipeline for the
        // target is built; until then the previous one draws.
        let format = gpu.image_info(target).map_err(|e| e.to_string())?.format;
        let wanted_ready = self.pipeline_ready(gpu, &wanted, format);
        let Shown {
            mut current,
            mut reported,
        } = self.shown.get(output).cloned().unwrap_or_default();
        let pending = match self.entries.get(&wanted) {
            Some(Entry::Ready(_)) if wanted_ready => {
                current = Some(wanted.clone());
                reported = None;
                false
            }
            Some(Entry::Failed(error)) => {
                if reported.as_ref() != Some(error) {
                    tracing::error!(
                        "output {output}: {} cannot be used ({error}); {}",
                        wanted.describe(),
                        if current.is_some() {
                            "the previous conversion stays"
                        } else {
                            "the output is converted without OCIO"
                        }
                    );
                    reported = Some(error.clone());
                }
                false
            }
            _ => true,
        };
        let params = DrawParams {
            scale,
            premultiplied: false,
            matrix: to_working,
        };
        let drawn = match &current {
            Some(transform) => self.draw_ready(
                gpu,
                transform,
                Some((state.exposure, state.gamma)),
                input,
                target,
                params,
            )?,
            None => false,
        };
        if !drawn {
            current = None;
        }
        if drawn && pending {
            self.stats.draws_while_pending += 1;
        }
        self.shown
            .insert(output.to_string(), Shown { current, reported });
        self.evict(gpu);
        Ok(if drawn {
            Presented::Drawn { pending }
        } else {
            Presented::NotReady { pending }
        })
    }

    /// Draws `input` through any conversion of the config into `target`
    /// (an image as large as the input). `Ok(false)` while the conversion
    /// is being compiled: nothing was drawn, ask again in a later frame.
    /// An unknown colour space, display or view, and a failed compilation,
    /// are errors.
    ///
    /// Called while a frame is being recorded and no pass is open.
    pub fn apply(
        &mut self,
        gpu: &mut Gpu,
        transform: &Transform,
        controls: Option<(f64, f64)>,
        input: Handle,
        target: Handle,
        params: DrawParams,
    ) -> Result<bool, String> {
        self.ensure(gpu, transform);
        self.collect(gpu);
        if let Some(Entry::Failed(error)) = self.entries.get(transform) {
            return Err(error.clone());
        }
        let drawn = self.draw_ready(gpu, transform, controls, input, target, params)?;
        // The build of its pipeline may have just failed.
        if let Some(Entry::Failed(error)) = self.entries.get(transform) {
            return Err(error.clone());
        }
        self.evict(gpu);
        Ok(drawn)
    }

    /// The state, for the control interface.
    pub fn state(&self) -> Value {
        let nothing = Shown::default();
        // `name`: an output that was presented, or `None` for the setting
        // of every output without one of its own.
        let describe = |name: Option<&str>| {
            let state = &match name {
                Some(name) => self.output_state(name),
                None => self.default_output.clone(),
            };
            let shown = name.and_then(|n| self.shown.get(n)).unwrap_or(&nothing);
            let wanted = self.wanted(state);
            let status = match wanted.as_ref().map(|t| self.entries.get(t)) {
                None => "off",
                // Its pipeline is being built.
                Some(Some(Entry::Ready(pass))) if pass.is_building() && shown.current != wanted => {
                    "compiling"
                }
                Some(Some(Entry::Ready(_))) if name.is_none() || shown.current == wanted => "ready",
                Some(Some(Entry::Ready(_))) => "ready-not-drawn",
                Some(Some(Entry::Failed(_))) => "failed",
                Some(Some(Entry::Pending)) => "compiling",
                Some(None) => "not-started",
            };
            let error = match wanted.as_ref().and_then(|t| self.entries.get(t)) {
                Some(Entry::Failed(error)) => Some(error.clone()),
                _ => None,
            };
            json!({
                "output": name,
                "display": state.setting.as_ref().map(|s| s.0.clone()),
                "view": state.setting.as_ref().map(|s| s.1.clone()),
                "status": status,
                "error": error,
                "drawn_through": shown.current.as_ref().map(Transform::describe),
                "exposure": state.exposure,
                "gamma": state.gamma,
                "dynamic": state.dynamic,
                "own_setting": name.is_some_and(|n| self.outputs.contains_key(n)),
                "signal": self.signal_of(state).map(|s| s.describe()),
            })
        };
        let mut names: Vec<&String> = self.shown.keys().chain(self.outputs.keys()).collect();
        names.sort();
        names.dedup();
        let outputs: Vec<Value> = names.into_iter().map(|n| describe(Some(n))).collect();
        json!({
            "default": describe(None),
            "config": self.config_name,
            "version": proto_ocio::version(),
            "working": self.working,
            "working_chromaticities": self.working_primaries.map(|p| [
                p.red.x, p.red.y, p.green.x, p.green.y, p.blue.x, p.blue.y, p.white.x, p.white.y,
            ]),
            "slangc": self.slangc.as_ref().map(|p| p.display().to_string()),
            "cache_dir": self.cache_dir.as_ref().map(|p| p.display().to_string()),
            "busy": self.busy(),
            "outputs": outputs,
            "compiled": self.stats.compiled,
            "from_cache": self.stats.from_cache,
            "failed": self.stats.failed,
            "draws": self.stats.draws,
            "draws_while_pending": self.stats.draws_while_pending,
        })
    }
}

/// The primaries of a scene-referred linear colour space of a config, as
/// display-referred colours see it: through the config's conversion to its
/// CIE XYZ (D65) interchange space, when that is a matrix.
fn working_primaries(config: &Config, working: &str) -> Option<Primaries> {
    let xyz = config.role("cie_xyz_d65_interchange")?;
    let matrix = config.processor(working, &xyz).ok()?.as_matrix().ok()??;
    primaries_of(&matrix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        Engine::new(&Options {
            config: Some("ocio://default".into()),
            ..Options::default()
        })
        .unwrap()
    }

    /// What is chosen for one output replaces only that part of the setting
    /// for every output: an output with an exposure of its own still follows
    /// the view of every output.
    #[test]
    fn an_outputs_own_choices_replace_only_themselves() {
        let mut engine = engine();
        let plain = Some(("sRGB - Display".to_string(), "Un-tone-mapped".to_string()));

        engine
            .set_output_controls(Some("A"), Some(1.5), None)
            .unwrap();
        assert!(!engine.output_is_configured("A"));
        engine
            .set_output(None, None, Some("Un-tone-mapped"))
            .unwrap();
        let a = engine.output_state("A");
        assert_eq!(a.setting, plain);
        assert_eq!((a.exposure, a.gamma, a.dynamic), (1.5, 1.0, true));
        let other = engine.output_state("B");
        assert_eq!(other.setting, plain);
        assert_eq!((other.exposure, other.dynamic), (0.0, false));
        engine.set_output(None, None, None).unwrap();
        assert!(!engine.output_is_configured("A"));

        // A view of its own stays when the one for every output changes,
        // and takes the display from it.
        engine
            .set_output(None, Some("Display P3 - Display"), None)
            .unwrap();
        engine.set_output(Some("B"), None, Some("Raw")).unwrap();
        assert_eq!(
            engine.output_state("B").setting,
            Some(("Display P3 - Display".to_string(), "Raw".to_string()))
        );
        engine
            .set_output(None, None, Some("Un-tone-mapped"))
            .unwrap();
        assert_eq!(engine.output_state("B").setting.unwrap().1, "Raw");
        // And an output can be taken out on its own.
        engine.set_output(Some("B"), None, None).unwrap();
        assert!(!engine.output_is_configured("B"));
        assert!(engine.output_is_configured("A"));

        assert!(engine.set_output(None, None, Some("no such view")).is_err());
        assert!(
            engine
                .set_output(Some("A"), Some("no display"), None)
                .is_err()
        );
        assert!(engine.set_output_controls(None, None, Some(0.0)).is_err());
    }
}
