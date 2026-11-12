//! Harness for tests that run the real `proto-compositor` binary.
//!
//! Every [`Session`] starts its own compositor process:
//!
//! - in its own `XDG_RUNTIME_DIR` (mode 0700), with `WAYLAND_DISPLAY` and
//!   `DISPLAY` removed from the environment, so nothing can reach the
//!   desktop session and no two tests share a socket,
//! - with `--exit-after`, so a process that the test fails to stop does not
//!   linger,
//! - on the Vulkan device under test.
//!
//! Test clients run inside the test process and connect to the session's
//! socket by path.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use proto_color::ImageDescription;
use proto_compositor::control::{ControlClient, Request};
use proto_compositor::image::{CompositeImage, EncodedImage};
use proto_test_client::{ClientOptions, TestClient};
use serde_json::Value;

pub mod icd;

pub use icd::find_icd_manifest;

/// How long to wait for the compositor or a client to reach a state.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The Vulkan device a session runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gpu {
    /// A hardware device visible to the Vulkan loader.
    Hardware,
    /// Mesa's software rasterizer.
    Lavapipe,
}

/// What a scenario runs on: the Vulkan device the script draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device {
    pub gpu: Gpu,
}

impl Device {
    pub const fn new(gpu: Gpu) -> Self {
        Device { gpu }
    }

    /// The name of the Vulkan device, as listed in `HIZUKURI_TEST_DEVICES`.
    pub fn gpu_name(self) -> &'static str {
        match self.gpu {
            Gpu::Hardware => "gpu",
            Gpu::Lavapipe => "lavapipe",
        }
    }

    /// A name for directories and messages.
    pub fn name(self) -> String {
        self.gpu_name().to_string()
    }

    /// Whether the device is listed in `HIZUKURI_TEST_DEVICES` (comma
    /// separated, default `lavapipe`, so that the tests do the same on
    /// every machine). A listed device that is missing makes tests fail; it
    /// is not skipped silently.
    pub fn enabled(self) -> bool {
        let list = std::env::var("HIZUKURI_TEST_DEVICES").unwrap_or_else(|_| "lavapipe".into());
        list.split(',').any(|d| d.trim() == self.gpu_name())
    }
}

/// Defines one `#[test]` per scenario in a module, for one [`Device`].
#[macro_export]
macro_rules! device_module {
    ($module:ident, $gpu:ident, [$($scenario:ident),*]) => {
        mod $module {
            $(
                #[test]
                fn $scenario() {
                    let device = $crate::common::Device::new($crate::common::Gpu::$gpu);
                    if device.enabled() {
                        super::$scenario(device);
                    } else {
                        eprintln!(
                            "skipped: {} is not in HIZUKURI_TEST_DEVICES",
                            device.gpu_name()
                        );
                    }
                }
            )*
        }
    };
}

/// Defines a `#[test]` per Vulkan device for a scenario function taking a
/// [`Device`].
#[macro_export]
macro_rules! device_tests {
    ($($scenario:ident),* $(,)?) => {
        $crate::device_module!(gpu, Hardware, [$($scenario),*]);
        $crate::device_module!(lavapipe, Lavapipe, [$($scenario),*]);
    };
}

/// The real applications the tests use when `HIZUKURI_TEST_REAL_CLIENTS`
/// is not set: the ones that the `ci` development shell of the repository
/// provides.
pub const DEFAULT_REAL_CLIENTS: &str = "weston-terminal,wl-clipboard,foot,script-tiles";

/// Whether the tests that use a real application run.
///
/// `HIZUKURI_TEST_REAL_CLIENTS` lists the names (comma separated, default
/// [`DEFAULT_REAL_CLIENTS`]). A name that is not listed is skipped with a
/// note. A listed name whose programs are not on `PATH` is a failure: a
/// test that silently does nothing would look like a test that passed.
pub fn real_client_wanted(name: &str, programs: &[&str]) -> bool {
    let list = std::env::var("HIZUKURI_TEST_REAL_CLIENTS")
        .unwrap_or_else(|_| DEFAULT_REAL_CLIENTS.to_string());
    if !list.split(',').any(|entry| entry.trim() == name) {
        eprintln!("skipped: {name} is not in HIZUKURI_TEST_REAL_CLIENTS");
        return false;
    }
    for program in programs {
        let found = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
        });
        assert!(
            found,
            "{program} is not on PATH, but {name} is in HIZUKURI_TEST_REAL_CLIENTS"
        );
    }
    true
}

/// Whether the tests that need `/dev/udmabuf` run.
///
/// The kernel's udmabuf device turns system memory into a dma-buf. The
/// tests make real dma-bufs with it without a GPU, and lavapipe exports its
/// images as dma-bufs through it. These tests fail when the device cannot
/// be opened. Where it cannot be made available, set
/// `HIZUKURI_TEST_SKIP_UDMABUF=1`: the tests are then left out, with a
/// note.
pub fn udmabuf_wanted() -> bool {
    if std::env::var_os("HIZUKURI_TEST_SKIP_UDMABUF").is_some() {
        eprintln!("skipped: HIZUKURI_TEST_SKIP_UDMABUF is set");
        return false;
    }
    if let Err(error) = std::fs::File::options()
        .read(true)
        .write(true)
        .open("/dev/udmabuf")
    {
        panic!(
            "/dev/udmabuf cannot be opened ({error}). This test needs it; set \
             HIZUKURI_TEST_SKIP_UDMABUF=1 to run without the tests that do"
        );
    }
    true
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// The variables through which a process finds a session that is already
/// running where the tests are started: a Wayland compositor (by name or by
/// an inherited socket), an X server, the D-Bus session bus (through which
/// an application can ask an instance that is already running to open the
/// window instead), and a pending startup notification.
const SESSION_VARIABLES: [&str; 7] = [
    "WAYLAND_DISPLAY",
    "WAYLAND_SOCKET",
    "DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_ACTIVATION_TOKEN",
    "DESKTOP_STARTUP_ID",
];

/// Whether the value of an environment variable names something inside
/// `directory`: a path in it, or an address such as `unix:path=<path>`.
fn points_into(value: &std::ffi::OsStr, directory: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let (value, directory) = (value.as_bytes(), directory.as_bytes());
    if directory.len() < 2 {
        return false;
    }
    let inside = |at: usize| {
        let rest = &value[at + directory.len()..];
        rest.is_empty() || rest[0] == b'/'
    };
    (value.starts_with(directory) && inside(0))
        || (1..value.len().saturating_sub(directory.len() - 1))
            .any(|at| value[at - 1] == b'=' && value[at..].starts_with(directory) && inside(at))
}

/// Cuts a process that a test starts off from any session that is running
/// where the tests are started.
///
/// - Every variable of [`SESSION_VARIABLES`] is removed.
/// - Every variable whose value points into the runtime directory the
///   process would have inherited is removed. That is where the sockets of
///   a session are (the control sockets of compositors, agents, sound
///   servers), whichever programs they belong to.
/// - The runtime directory, where Wayland sockets are looked up by name, is
///   the test's own.
///
/// What the process can still reach is whatever the test then names
/// explicitly.
pub fn isolate(command: &mut Command, runtime_dir: &Path) {
    // What the process would see: this process's environment, with what
    // was already set on the command.
    let mut inherited: std::collections::HashMap<_, _> = std::env::vars_os().collect();
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => inherited.insert(name.to_owned(), value.to_owned()),
            None => inherited.remove(name),
        };
    }
    for variable in SESSION_VARIABLES {
        command.env_remove(variable);
    }
    if let Some(outer) = inherited.get(std::ffi::OsStr::new("XDG_RUNTIME_DIR")) {
        for (name, value) in &inherited {
            if points_into(value, outer) && !points_into(value, runtime_dir.as_os_str()) {
                command.env_remove(name);
            }
        }
    }
    command.env("XDG_RUNTIME_DIR", runtime_dir);
}

/// A running compositor process and the files that belong to it.
pub struct Session {
    child: Option<Child>,
    control: ControlClient,
    pub runtime_dir: PathBuf,
    /// Where screenshots and the compositor log of this session go.
    pub output_dir: PathBuf,
    wayland_socket: PathBuf,
    pub control_socket: PathBuf,
    pub device: Device,
}

/// Which project a session runs when the test gives no `--script`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefaultProject {
    /// [`draw_only_project`]: the script draws, the fallback places windows.
    DrawOnly,
    /// The built-in project, which also tiles the windows.
    Builtin,
}

impl Session {
    /// Starts a compositor for the test `name` with extra command line
    /// arguments. Unless the arguments name a project (`--script`), it runs
    /// [`draw_only_project`]: the script draws and the fallback places the
    /// windows.
    pub fn start(name: &str, device: Device, extra_args: &[&str]) -> Session {
        Self::start_with(name, device, extra_args, DefaultProject::DrawOnly)
    }

    /// Like [`Self::start`], with the built-in project, which also manages
    /// the windows (tiles them), unless the arguments name a project.
    pub fn start_builtin(name: &str, device: Device, extra_args: &[&str]) -> Session {
        Self::start_with(name, device, extra_args, DefaultProject::Builtin)
    }

    fn start_with(
        name: &str,
        device: Device,
        extra_args: &[&str],
        project: DefaultProject,
    ) -> Session {
        let unique = format!(
            "hzk-t-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        // Unix socket paths are limited to about 100 bytes, so the runtime
        // directory must be short: under the real runtime directory when
        // there is one (it is per user and on tmpfs), else under /tmp.
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let runtime_dir = base.join(unique);
        std::fs::create_dir(&runtime_dir).expect("creating the runtime directory");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))
                .expect("restricting the runtime directory");
        }
        Self::launch(name, device, extra_args, runtime_dir, project)
    }

    /// Like [`Self::start`], in a runtime directory that already exists
    /// (and may hold what an earlier compositor left there).
    pub fn start_in(
        name: &str,
        device: Device,
        extra_args: &[&str],
        runtime_dir: PathBuf,
    ) -> Session {
        Self::launch(
            name,
            device,
            extra_args,
            runtime_dir,
            DefaultProject::DrawOnly,
        )
    }

    fn launch(
        name: &str,
        device: Device,
        extra_args: &[&str],
        runtime_dir: PathBuf,
        project: DefaultProject,
    ) -> Session {
        // target/tmp is CARGO_TARGET_TMPDIR; results go next to it.
        let output_dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .parent()
            .expect("target directory")
            .join("test-output")
            .join(format!("{name}-{}", device.name()));
        let _ = std::fs::remove_dir_all(&output_dir);
        std::fs::create_dir_all(&output_dir).expect("creating the output directory");

        let socket_name = "hizukuri-test";
        let wayland_socket = runtime_dir.join(socket_name);
        let control_socket = runtime_dir.join(format!("{socket_name}.control"));

        let mut command = Command::new(env!("CARGO_BIN_EXE_proto-compositor"));
        command
            .args(["--backend", "headless", "--socket", socket_name])
            .args(["--exit-after", "120"])
            .args(extra_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(
                std::fs::File::create(output_dir.join("proto-compositor.log")).expect("log file"),
            );
        isolate(&mut command, &runtime_dir);
        if project == DefaultProject::DrawOnly && !extra_args.contains(&"--script") {
            command.arg("--script").arg(draw_only_project());
        }
        // The compositor reads a project's shaders as SPIR-V and never
        // compiles them: build them first, as the build tool would.
        let script = extra_args
            .iter()
            .position(|a| *a == "--script")
            .map(|i| PathBuf::from(extra_args[i + 1]))
            .or((project == DefaultProject::DrawOnly).then(draw_only_project));
        if let Some(dir) = script {
            build_shaders(&dir);
        }
        // The tests are not about how fast the machine is: a call into the
        // script only times out when it is stuck. A scenario about the
        // budget itself passes its own.
        if !extra_args.contains(&"--script-budget-ms") {
            command.args(["--script-budget-ms", "120000"]);
        }
        match device.gpu {
            Gpu::Hardware => {
                command.args(["--vulkan-device", "gpu"]);
            }
            Gpu::Lavapipe => {
                let manifest =
                    find_icd_manifest("lvp").expect("the lavapipe ICD manifest is installed");
                command
                    .args(["--vulkan-device", "cpu", "--vulkan-icd"])
                    .arg(manifest);
            }
        }
        if std::env::var("HIZUKURI_TEST_VALIDATION").is_ok_and(|v| v == "1") {
            command.arg("--vulkan-validation");
        }
        let mut child = command.spawn().expect("starting proto-compositor");

        // The control socket appears once the compositor is up.
        let deadline = Instant::now() + TIMEOUT;
        let control = loop {
            if let Ok(client) = ControlClient::connect(&control_socket) {
                break client;
            }
            if let Some(status) = child.try_wait().expect("polling proto-compositor") {
                let log = std::fs::read_to_string(output_dir.join("proto-compositor.log"))
                    .unwrap_or_default();
                let _ = std::fs::remove_dir_all(&runtime_dir);
                panic!("proto-compositor exited during startup with {status}:\n{log}");
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&runtime_dir);
                panic!("proto-compositor did not create its control socket");
            }
            std::thread::sleep(Duration::from_millis(10));
        };

        let mut session = Session {
            child: Some(child),
            control,
            runtime_dir,
            output_dir,
            wayland_socket,
            control_socket,
            device,
        };
        let state = session.state();
        let kind = state["gpu"]["kind"].as_str().unwrap_or("none").to_string();
        match device.gpu {
            Gpu::Hardware => assert_ne!(kind, "cpu", "expected a hardware device: {state:#}"),
            Gpu::Lavapipe => assert_eq!(kind, "cpu", "expected lavapipe: {state:#}"),
        }
        assert!(
            state["script"].is_object(),
            "the script does not draw: {state:#}"
        );
        session
    }

    /// A command for a real application, set up to reach this session and
    /// nothing else.
    ///
    /// - The desktop session is out of reach ([`isolate`]): the only
    ///   Wayland socket it can find is this session's.
    /// - Its home and configuration are empty directories of this session,
    ///   so it neither reads the settings of whoever runs the tests nor
    ///   writes history or state into their home. The cache directory is
    ///   left alone (font and shader caches are safe to share and slow to
    ///   rebuild).
    /// - The system's configuration directory is the only other one it
    ///   looks in: the list it inherited may hold directories of the person
    ///   who runs the tests (a per-user profile).
    /// - A terminal started without a command runs `/bin/sh`, not their
    ///   login shell, and nothing points a shell at their startup files.
    pub fn client_command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        isolate(&mut command, &self.runtime_dir);
        for variable in ["ZDOTDIR", "ENV", "BASH_ENV"] {
            command.env_remove(variable);
        }
        command
            .env("SHELL", "/bin/sh")
            .env("XDG_CONFIG_DIRS", "/etc/xdg");
        let home = self.runtime_dir.join("home");
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
        for (variable, directory) in [
            ("HOME", home.clone()),
            ("XDG_CONFIG_HOME", home.join("config")),
            ("XDG_DATA_HOME", home.join("data")),
            ("XDG_STATE_HOME", home.join("state")),
        ] {
            std::fs::create_dir_all(&directory).expect("creating the client's home");
            command.env(variable, directory);
        }
        if let Some(cache) = cache {
            command.env("XDG_CACHE_HOME", cache);
        }
        command
            .env("WAYLAND_DISPLAY", "hizukuri-test")
            .stdin(Stdio::null());
        command
    }

    /// Starts a real application in this session. Its output goes to
    /// `<name>.log` in the output directory. It is stopped when the
    /// returned value is dropped.
    pub fn spawn_client(&self, name: &str, program: &str, args: &[&str]) -> ClientProcess {
        use std::os::unix::process::CommandExt;
        let log = std::fs::File::create(self.output_dir.join(format!("{name}.log")))
            .expect("creating the client's log");
        let child = self
            .client_command(program)
            .args(args)
            .stdout(log.try_clone().expect("duplicating the log"))
            .stderr(log)
            // A process group of its own, so that what it starts in turn
            // can be stopped with it.
            .process_group(0)
            .spawn()
            .unwrap_or_else(|e| panic!("starting {program}: {e}"));
        ClientProcess(child)
    }

    /// Connects a test client to this session's Wayland socket.
    pub fn client(&self, options: ClientOptions) -> TestClient {
        TestClient::connect_path(&self.wayland_socket, options).expect("connecting a client")
    }

    /// Sends a control request that must succeed.
    pub fn send(&mut self, request: Request) -> Value {
        self.control
            .send(&request)
            .unwrap_or_else(|e| panic!("control request {request:?} failed: {e}"))
    }

    pub fn state(&mut self) -> Value {
        self.send(Request::State)
    }

    /// Sends a control request that may be refused.
    pub fn try_send(&mut self, request: Request) -> Result<Value, String> {
        self.control.send(&request).map_err(|e| e.to_string())
    }

    /// Calls a global function of the script and returns what it returned.
    pub fn script(&mut self, function: &str, args: Value) -> Value {
        let args = args.as_array().cloned().expect("an array of arguments");
        self.send(Request::ScriptCall {
            function: function.to_string(),
            args,
        })["result"]
            .take()
    }

    /// Asserts that the script is in charge and never failed. For every
    /// scenario that does not make it fail on purpose: a failure does not
    /// stop the session (the built-in fallback takes over), so it has to be
    /// looked for.
    #[track_caller]
    pub fn assert_script_healthy(&mut self) {
        self.assert_script_running(0);
    }

    /// Like [`Self::assert_script_healthy`], for scenarios that call into
    /// the script wrongly on purpose a known number of times (a refused
    /// call through the control interface counts as a failure too).
    #[track_caller]
    pub fn assert_script_running(&mut self, failures: u64) {
        let script = self.script_state();
        assert_eq!(script["mode"], "running", "{script:#}");
        assert_eq!(script["failures"], failures, "{script:#}");
    }

    /// The `script` section of the state: mode, counters, failures.
    pub fn script_state(&mut self) -> Value {
        self.state()["script"].take()
    }

    /// Pumps the clients and polls the compositor state until `done`
    /// accepts it. Returns that state.
    pub fn wait(
        &mut self,
        clients: &mut [&mut TestClient],
        what: &str,
        done: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            for client in clients.iter_mut() {
                client
                    .pump(Some(Duration::from_millis(5)))
                    .expect("pumping a client");
            }
            let state = self.state();
            if done(&state) {
                return state;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for: {what}\nstate: {state:#}");
            }
        }
    }

    /// Connects a client and waits until its window is mapped. Returns the
    /// client and the id of its window.
    pub fn mapped_client(&mut self, options: ClientOptions) -> (TestClient, u64) {
        let title = options.title.clone();
        let mut client = self.client(options);
        let state = self.wait(&mut [&mut client], "window mapped", |state| {
            window(state, &title).is_some_and(|w| w["surfaces"][0]["buffer_size"].is_array())
        });
        let id = window(&state, &title).unwrap()["id"].as_u64().unwrap();
        (client, id)
    }

    /// Changes the fill color of a client and waits until the compositor
    /// has the new contents.
    pub fn recolor(&mut self, client: &mut TestClient, title: &str, color: [u8; 4]) {
        let before =
            window(&self.state(), title).expect("window exists")["surfaces"][0]["generation"]
                .as_u64()
                .unwrap();
        client.set_color(color);
        self.wait(&mut [client], "new contents", |state| {
            window(state, title)
                .is_some_and(|w| w["surfaces"][0]["generation"].as_u64().unwrap() > before)
        });
    }

    /// Changes what a client draws and waits until the compositor has the
    /// new contents.
    pub fn redraw(
        &mut self,
        client: &mut TestClient,
        title: &str,
        change: impl FnOnce(&mut ClientOptions),
    ) -> Value {
        let before =
            window(&self.state(), title).expect("window exists")["surfaces"][0]["generation"]
                .as_u64()
                .unwrap();
        client.redraw_with(change);
        self.wait(&mut [client], "new contents", |state| {
            window(state, title)
                .is_some_and(|w| w["surfaces"][0]["generation"].as_u64().unwrap() > before)
        })
    }

    /// Captures the output: the encoded image and the compositing buffer.
    /// The files stay in [`Self::output_dir`] as `<name>.png` / `<name>.exr`.
    pub fn screenshot(&mut self, name: &str) -> (EncodedImage, CompositeImage) {
        self.screenshot_of(name, None)
    }

    /// Captures only the encoded output. For states in which nothing offers
    /// a compositing buffer (the script layer's built-in fallback).
    pub fn screenshot_png(&mut self, name: &str) -> EncodedImage {
        let path = self.output_dir.join(format!("{name}.png"));
        let response = self.send(Request::Screenshot {
            path: path.clone(),
            composite_path: None,
            output: None,
        });
        let description_name = response["image_description"]["name"]
            .as_str()
            .expect("the output uses a preset image description");
        let description = ImageDescription::from_name(description_name).expect("known preset name");
        EncodedImage::read_png(&path, description).expect("reading the screenshot")
    }

    /// Like [`Self::screenshot`], of the output with the given name (the
    /// first one with `None`).
    pub fn screenshot_of(
        &mut self,
        name: &str,
        output: Option<&str>,
    ) -> (EncodedImage, CompositeImage) {
        let path = self.output_dir.join(format!("{name}.png"));
        let composite_path = self.output_dir.join(format!("{name}.exr"));
        let response = self.send(Request::Screenshot {
            path: path.clone(),
            composite_path: Some(composite_path.clone()),
            output: output.map(str::to_string),
        });
        let description_name = response["image_description"]["name"]
            .as_str()
            .expect("the output uses a preset image description");
        let description = ImageDescription::from_name(description_name).expect("known preset name");
        let encoded = EncodedImage::read_png(&path, description).expect("reading the screenshot");
        let composite = CompositeImage::read_exr(&composite_path).expect("reading the EXR");
        assert_eq!(
            (encoded.width, encoded.height),
            (composite.width, composite.height)
        );
        (encoded, composite)
    }

    /// Asserts that the drawing reported no Vulkan validation errors (when
    /// validation is enabled) and that it drew at all.
    pub fn assert_drawing_clean(&mut self) {
        let state = self.state();
        assert!(state["frames_rendered"].as_u64().unwrap() > 0);
        let errors = &state["gpu"]["validation_errors"];
        assert!(
            errors.is_null() || errors == 0,
            "Vulkan validation errors: {errors} (see {})",
            self.output_dir.join("proto-compositor.log").display()
        );
    }

    /// Stops the compositor with a `quit` request and checks that it exits
    /// cleanly and removes its sockets.
    pub fn quit(mut self) {
        self.send(Request::Quit);
        self.expect_clean_exit();
    }

    /// Sends a signal to the compositor (`TERM`, `KILL`, ...).
    pub fn signal(&self, signal: &str) {
        let pid = self.child.as_ref().expect("still running").id();
        let status = Command::new("kill")
            .args([format!("-{signal}"), pid.to_string()])
            .status()
            .expect("running kill");
        assert!(status.success(), "kill -{signal} {pid} failed");
    }

    /// Ends the session after the compositor was killed: waits for it and
    /// returns the runtime directory with whatever is left in it. The
    /// caller removes the directory.
    pub fn abandon(mut self) -> PathBuf {
        let mut child = self.child.take().expect("still running");
        let _ = child.wait();
        std::mem::take(&mut self.runtime_dir)
    }

    /// Waits for the compositor to exit by itself, successfully and
    /// without leaving its sockets behind.
    pub fn expect_clean_exit(mut self) {
        let mut child = self.child.take().expect("still running");
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait().expect("polling proto-compositor") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("proto-compositor did not exit");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "proto-compositor exited with {status}");
        assert!(
            !self.wayland_socket.exists(),
            "the Wayland socket was left behind"
        );
        assert!(
            !self.control_socket.exists(),
            "the control socket was left behind"
        );
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Reached with a live child only when the test failed before
        // `quit`. Make sure nothing is left running.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Empty after `abandon`, which hands the directory to the caller.
        if !self.runtime_dir.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.runtime_dir);
        }
    }
}

/// A real application started for a test, in a process group of its own.
///
/// Dropping it stops the application and whatever it started in that
/// group. Processes are only ever addressed by the id this test got when
/// it started them, never by name: the machine the tests run on may have
/// a session with programs of the same names (a clipboard watcher running
/// `wl-paste`, terminals).
pub struct ClientProcess(Child);

impl Drop for ClientProcess {
    fn drop(&mut self) {
        // The group id is the child's process id (`process_group(0)`).
        // While the child has not been waited for, that id cannot have been
        // reused, so the group is still ours.
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{}", self.0.id())])
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Finds a mapped window by title in a state dump.
pub fn window<'a>(state: &'a Value, title: &str) -> Option<&'a Value> {
    state["windows"]
        .as_array()?
        .iter()
        .find(|w| w["title"] == title)
}

pub fn options(title: &str, width: u32, height: u32, color: [u8; 4]) -> ClientOptions {
    ClientOptions {
        title: title.to_string(),
        width,
        height,
        color,
        ..Default::default()
    }
}

/// The project in `projects/draw-only`: the built-in drawing, and no
/// window management, so the core's fallback places the windows. Sessions
/// start with it unless a test gives `--script` or asks for the built-in
/// project ([`Session::start_builtin`]).
pub fn draw_only_project() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("projects/draw-only")
}

/// The `session` module of [`draw_only_project`]. A test project that
/// includes `session` and is to run without the tiling has this module as
/// its `session`.
pub const DRAW_ONLY_SESSION: &str = include_str!("../../projects/draw-only/session.luau");

/// Compiles the shaders of the project in `dir` (`proto-compositor
/// build-shaders`); a project without `shaders/` is left as it is.
pub fn build_shaders(dir: &Path) {
    if let Err(errors) = proto_compositor::shaders::build_project(dir) {
        panic!(
            "the shaders of {} did not compile:\n{}",
            dir.display(),
            errors.join("\n")
        );
    }
}

/// Writes a script project (module name, source) for a test and returns its
/// directory. Modules it does not have come from the built-in project.
pub fn script_project(name: &str, device: Device, modules: &[(&str, &str)]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("script-projects")
        .join(format!("{name}-{}", device.name()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creating the project directory");
    for (module, source) in modules {
        std::fs::write(dir.join(format!("{module}.luau")), source).expect("writing a module");
    }
    dir
}
