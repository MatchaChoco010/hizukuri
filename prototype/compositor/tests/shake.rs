//! Shakes a project: runs it and, for a while, does to it what a person at
//! the keyboard does, in an order drawn from a seed. Clients open, change,
//! close and vanish; keys are tapped, held and hammered; the pointer moves,
//! clicks and drags; the script is reloaded; as a nested compositor, its
//! window is given other sizes.
//!
//! What must hold: the compositor keeps answering, the script stays in
//! charge and reports no failure, drawing reports no validation error (when
//! validation is on), and afterwards nothing is left of the windows that
//! are gone.
//!
//! Not run with the other tests:
//!
//! ```text
//! HIZUKURI_SHAKE_PROJECT=projects/fancy \
//! HIZUKURI_SHAKE_SEED=7 HIZUKURI_SHAKE_SECONDS=120 \
//!     cargo test -p proto-compositor --test shake -- --ignored --nocapture --exact lavapipe
//! ```
//!
//! | Variable | Default | |
//! | --- | --- | --- |
//! | `HIZUKURI_SHAKE_PROJECT` | the built-in project | directory of the project (a relative one is from `prototype/compositor`, where cargo runs the test) |
//! | `HIZUKURI_SHAKE_SEED` | 1 | what the order of the actions is drawn from |
//! | `HIZUKURI_SHAKE_SECONDS` | 30 | how long |
//! | `HIZUKURI_SHAKE_NESTED` | unset | `1`: as a nested compositor in a headless parent, which resizes its window |
//! | `HIZUKURI_SHAKE_HOOK` | unset | a function of the script to call at the end (its answer is printed) |
//! | `HIZUKURI_TEST_VALIDATION` | unset | `1`: with the Vulkan validation layer |
//!
//! The same seed gives the same actions in the same order. Their timing is
//! the machine's, so a failure may need a few runs to show again.
//!
//! Everything a project starts by name (`foot`, other terminals) is a
//! stand-in here: a terminal without configuration, in a home of its own,
//! that runs `sleep`, never a shell of whoever runs the test. The terminals
//! the test starts itself are as isolated.

mod common;

use std::collections::VecDeque;
use std::mem::DropGuard;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{ClientProcess, Device, Gpu, Session};
use proto_compositor::control::Request;
use proto_test_client::{BufferKind, ClientOptions, PopupOptions, TestClient, ToplevelRequest};
use serde_json::{Value, json};

/// A small deterministic generator (splitmix64).
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number in `low..=high`.
    fn range(&mut self, low: i64, high: i64) -> i64 {
        low + (self.next() % (high - low + 1) as u64) as i64
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[(self.next() % items.len() as u64) as usize]
    }
}

const KEY_ESC: u32 = 1;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTALT: u32 = 56;
const KEY_LEFTMETA: u32 = 125;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// The keys projects bind (with the logo key): Return, letters, digits,
/// punctuation, Tab, Space, arrows.
const KEYS: &[u32] = &[
    28, // Return
    35, 36, 37, 38, // H J K L
    50, 33, 16, 47, // M F Q V
    2, 3, 4, 5, 6, // 1..5
    12, 13, 51, 52, // - = , .
    15, 57, KEY_ESC, // Tab Space Escape
    103, 108, 105, 106, // arrows
];

/// The modifiers held with a key, most often the logo key.
const MODIFIERS: &[&[u32]] = &[
    &[KEY_LEFTMETA],
    &[KEY_LEFTMETA],
    &[KEY_LEFTMETA],
    &[KEY_LEFTMETA, KEY_LEFTSHIFT],
    &[KEY_LEFTMETA, KEY_LEFTCTRL],
    &[KEY_LEFTMETA, KEY_LEFTCTRL, KEY_LEFTSHIFT],
    &[],
    &[KEY_LEFTALT],
];

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Quotes a path for `sh`.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Stand-ins for the terminals a project may start, first on `PATH`: each
/// is a terminal without configuration that runs `sleep` (or, where no
/// terminal is installed, just `sleep`). Returns the stand-in to name in
/// `HIZUKURI_TERMINAL`.
///
/// The project starts them from the compositor, whose home is that of
/// whoever runs the test, so each stand-in first moves to what the harness
/// gives the applications it starts ([`Session::client_command`]): a home
/// of its own (empty configuration, data and state), only the system's
/// configuration directory, and a plain shell. It reads no settings and
/// leaves no history behind.
fn terminal_stand_ins(real_foot: Option<&Path>) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let directory =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("shake-bin-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("creating the stand-in directory");
    let home = directory.join("home");
    for sub in ["config", "data", "state"] {
        std::fs::create_dir_all(home.join(sub)).expect("creating the stand-in home");
    }
    // Font caches are safe to share and slow to rebuild (as for the clients
    // the harness starts).
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
    let home_text = shell_quote(&home);
    let mut prelude = format!(
        "export HOME={home_text} XDG_CONFIG_HOME={home_text}/config \
         XDG_DATA_HOME={home_text}/data XDG_STATE_HOME={home_text}/state \
         XDG_CONFIG_DIRS=/etc/xdg SHELL=/bin/sh\n\
         unset ENV BASH_ENV ZDOTDIR\n"
    );
    if let Some(cache) = cache {
        prelude.push_str(&format!("export XDG_CACHE_HOME={}\n", shell_quote(&cache)));
    }
    let script = match real_foot {
        Some(foot) => format!(
            "#!/bin/sh\n{prelude}exec {} --config=/dev/null sh -c 'sleep 8'\n",
            shell_quote(foot)
        ),
        None => "#!/bin/sh\nexec sleep 1\n".to_string(),
    };
    for name in [
        "foot",
        "footclient",
        "alacritty",
        "ghostty",
        "kitty",
        "wezterm",
        "weston-terminal",
        "xterm",
        "ptyxis",
    ] {
        let path = directory.join(name);
        std::fs::write(&path, &script).expect("writing a stand-in");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("making a stand-in executable");
    }
    let mut path = std::ffi::OsString::from(&directory);
    if let Some(rest) = std::env::var_os("PATH") {
        path.push(":");
        path.push(rest);
    }
    // SAFETY: set before any thread of this test reads the environment; the
    // compositors started below inherit it.
    unsafe {
        std::env::set_var("PATH", path);
        std::env::set_var("HIZUKURI_TERMINAL", directory.join("foot"));
    }
    directory.join("foot")
}

/// The program on `PATH` with this name.
fn find_program(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// What is measured while shaking.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Sample {
    script_memory: u64,
    gpu_memory: u64,
    images: u64,
    buffers: u64,
    slots: u64,
    pipelines: u64,
    closed_windows: u64,
    windows: u64,
    frames: u64,
}

fn sample(state: &Value) -> Sample {
    let script = &state["script"];
    let number = |value: &Value| value.as_u64().unwrap_or(0);
    Sample {
        script_memory: number(&script["memory_bytes"]),
        gpu_memory: number(&script["gpu"]["memory_bytes"]),
        images: number(&script["gpu"]["images"]),
        buffers: number(&script["gpu"]["buffers"]),
        slots: number(&script["gpu"]["slots"]),
        pipelines: number(&script["gpu"]["pipelines"]),
        closed_windows: script["closed_windows"]
            .as_array()
            .map_or(0, |a| a.len() as u64),
        windows: state["windows"].as_array().map_or(0, |a| a.len() as u64),
        frames: number(&state["frames_rendered"]),
    }
}

struct Shaker {
    session: Session,
    /// The parent, when the session is a nested compositor.
    parent: Option<Session>,
    random: Random,
    seed: u64,
    clients: Vec<(TestClient, String)>,
    terminals: Vec<ClientProcess>,
    real_foot: Option<PathBuf>,
    /// Logical size of the output, for where the pointer can go.
    size: (i64, i64),
    actions: u64,
    /// The last actions, for the report of a failure.
    recent: VecDeque<String>,
    samples: Vec<(u64, Sample)>,
    allow_dmabuf: bool,
}

impl Shaker {
    fn note(&mut self, action: String) {
        if self.recent.len() == 60 {
            self.recent.pop_front();
        }
        self.recent
            .push_back(format!("{:>5} {action}", self.actions));
    }

    fn fail(&mut self, what: &str, detail: &str) -> ! {
        let recent: Vec<String> = self.recent.iter().cloned().collect();
        panic!(
            "shake: {what}\nseed {} after {} actions\n{detail}\nthe last actions:\n{}",
            self.seed,
            self.actions,
            recent.join("\n")
        );
    }

    fn send(&mut self, request: Request) -> Value {
        match self.session.try_send(request.clone()) {
            Ok(value) => value,
            Err(error) => {
                let log =
                    std::fs::read_to_string(self.session.output_dir.join("proto-compositor.log"))
                        .unwrap_or_default();
                let tail: Vec<&str> = log.lines().rev().take(30).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                self.fail(
                    "the compositor stopped answering",
                    &format!(
                        "{request:?}: {error}\nthe end of its log:\n{}",
                        tail.join("\n")
                    ),
                )
            }
        }
    }

    fn key(&mut self, keycode: u32, pressed: bool) {
        self.send(Request::Key { keycode, pressed });
    }

    fn pump(&mut self, milliseconds: u64) {
        let deadline = Instant::now() + Duration::from_millis(milliseconds);
        loop {
            let mut failed = None;
            for (index, (client, _)) in self.clients.iter_mut().enumerate() {
                if client.pump(Some(Duration::from_millis(1))).is_err() {
                    failed = Some(index);
                }
            }
            // A client the compositor disconnected, or one asked to close.
            if let Some(index) = failed {
                let (_, title) = self.clients.remove(index);
                self.note(format!("client {title} lost its connection"));
            }
            self.clients.retain(|(client, _)| !client.is_closed());
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn point(&mut self) -> (f64, f64) {
        (
            self.random.range(0, self.size.0.max(2) - 1) as f64,
            self.random.range(0, self.size.1.max(2) - 1) as f64,
        )
    }

    fn open_client(&mut self) {
        if self.clients.len() >= 7 {
            return;
        }
        let title = format!("c{}", self.actions);
        let fixed = self.random.chance(12);
        let (width, height) = (
            self.random.range(40, 900) as u32,
            self.random.range(40, 700) as u32,
        );
        let options = ClientOptions {
            title: title.clone(),
            app_id: "shake".to_string(),
            width,
            height,
            color: [
                self.random.range(0, 255) as u8,
                self.random.range(0, 255) as u8,
                self.random.range(0, 255) as u8,
                255,
            ],
            buffer_kind: if self.allow_dmabuf && self.random.chance(20) {
                BufferKind::Dmabuf
            } else {
                BufferKind::Shm
            },
            // A window that cannot be resized (floats in a tiling layout).
            min_size: fixed.then_some((width as i32, height as i32)),
            max_size: fixed.then_some((width as i32, height as i32)),
            ..Default::default()
        };
        self.note(format!(
            "open {title} {width}x{height} {:?}{}",
            options.buffer_kind,
            if fixed { " fixed size" } else { "" }
        ));
        let client = self.session.client(options);
        self.clients.push((client, title));
    }

    fn close_client(&mut self) {
        if self.clients.is_empty() {
            return;
        }
        let index = self.random.range(0, self.clients.len() as i64 - 1) as usize;
        if self.random.chance(50) {
            // The client vanishes (as a killed one does).
            let (_, title) = self.clients.remove(index);
            self.note(format!("drop {title}"));
        } else {
            // The compositor asks it to close; it goes when it is told.
            let title = self.clients[index].1.clone();
            let state = self.send(Request::State);
            if let Some(id) = common::window(&state, &title).and_then(|w| w["id"].as_u64()) {
                self.note(format!("close {title}"));
                self.send(Request::CloseWindow { window: id });
            }
        }
    }

    fn change_client(&mut self) {
        if self.clients.is_empty() {
            return;
        }
        let index = self.random.range(0, self.clients.len() as i64 - 1) as usize;
        let title = self.clients[index].1.clone();
        match self.random.range(0, 5) {
            0 => {
                let (width, height) = (
                    self.random.range(20, 1000) as u32,
                    self.random.range(20, 800) as u32,
                );
                let color = [
                    self.random.range(0, 255) as u8,
                    self.random.range(0, 255) as u8,
                    self.random.range(0, 255) as u8,
                    255,
                ];
                self.note(format!("redraw {title} {width}x{height}"));
                self.clients[index].0.redraw_with(|options| {
                    options.width = width;
                    options.height = height;
                    options.color = color;
                });
            }
            1 => {
                let has = self.clients[index].0.has_popup();
                if has {
                    self.note(format!("close the popup of {title}"));
                    self.clients[index].0.close_popup();
                } else {
                    let options = PopupOptions {
                        x: self.random.range(-50, 300) as i32,
                        y: self.random.range(-50, 300) as i32,
                        width: self.random.range(10, 400) as i32,
                        height: self.random.range(10, 300) as i32,
                        color: [200, 200, 40, 255],
                        grab: false,
                        slide: self.random.chance(50),
                    };
                    self.note(format!("popup on {title} at {},{}", options.x, options.y));
                    // A window that is not mapped yet has no popup.
                    let _ = self.clients[index].0.open_popup(options);
                }
            }
            2 => {
                let (x, y) = (
                    self.random.range(-40, 200) as i32,
                    self.random.range(-40, 200) as i32,
                );
                self.note(format!("subsurface on {title} at {x},{y}"));
                let _ = self.clients[index].0.add_subsurface(
                    x,
                    y,
                    self.random.range(8, 200) as u32,
                    self.random.range(8, 200) as u32,
                    [40, 200, 200, 255],
                );
            }
            _ => {
                let request = self.random.pick(&[
                    ToplevelRequest::Fullscreen,
                    ToplevelRequest::Unfullscreen,
                    ToplevelRequest::Maximize,
                    ToplevelRequest::Unmaximize,
                    ToplevelRequest::Minimize,
                ]);
                self.note(format!("{title} asks for {request:?}"));
                let _ = self.clients[index].0.request(request);
            }
        }
    }

    fn terminal(&mut self) {
        let Some(foot) = self.real_foot.clone() else {
            return;
        };
        if !self.terminals.is_empty() && (self.terminals.len() >= 3 || self.random.chance(40)) {
            let index = self.random.range(0, self.terminals.len() as i64 - 1) as usize;
            self.note("kill a terminal".to_string());
            drop(self.terminals.remove(index));
            return;
        }
        self.note("start a terminal".to_string());
        let name = format!("foot-{}", self.actions);
        let terminal = self.session.spawn_client(
            &name,
            foot.to_str().expect("a UTF-8 path"),
            &["--config=/dev/null", "sh", "-c", "echo shake; sleep 600"],
        );
        self.terminals.push(terminal);
    }

    fn keys(&mut self) {
        let modifiers = self.random.pick(MODIFIERS);
        let key = self.random.pick(KEYS);
        let style = self.random.range(0, 9);
        for modifier in modifiers {
            self.key(*modifier, true);
        }
        match style {
            // Only the modifiers.
            0 => self.note(format!("tap modifiers {modifiers:?}")),
            // Hammered.
            1 | 2 => {
                let times = self.random.range(3, 12);
                self.note(format!("{modifiers:?}+{key} x{times}"));
                for _ in 0..times {
                    self.key(key, true);
                    self.key(key, false);
                }
            }
            // Held.
            3 => {
                let held = self.random.range(100, 700) as u64;
                self.note(format!("{modifiers:?}+{key} held {held} ms"));
                self.key(key, true);
                self.pump(held);
                self.key(key, false);
            }
            // Released in the wrong order: the modifiers first.
            4 => {
                self.note(format!("{modifiers:?}+{key}, modifiers released first"));
                self.key(key, true);
                for modifier in modifiers.iter().rev() {
                    self.key(*modifier, false);
                }
                self.key(key, false);
                return;
            }
            _ => {
                self.note(format!("{modifiers:?}+{key}"));
                self.key(key, true);
                self.key(key, false);
            }
        }
        for modifier in modifiers.iter().rev() {
            self.key(*modifier, false);
        }
    }

    fn pointer(&mut self) {
        match self.random.range(0, 6) {
            0 | 1 => {
                let (x, y) = self.point();
                self.note(format!("pointer to {x},{y}"));
                self.send(Request::PointerMotion { x, y });
            }
            2 => {
                let (x, y) = self.point();
                let button = self.random.pick(&[BTN_LEFT, BTN_LEFT, BTN_RIGHT]);
                self.note(format!("click {button:#x} at {x},{y}"));
                self.send(Request::PointerMotion { x, y });
                self.send(Request::PointerButton {
                    button,
                    pressed: true,
                });
                self.send(Request::PointerButton {
                    button,
                    pressed: false,
                });
            }
            3 | 4 => {
                // A drag, half of the time with the logo key (grabbing a
                // window), sometimes ending past the edge.
                let logo = self.random.chance(50);
                let (x, y) = self.point();
                let steps = self.random.range(2, 14);
                self.note(format!(
                    "drag from {x},{y} in {steps} steps{}",
                    if logo { " with logo" } else { "" }
                ));
                if logo {
                    self.key(KEY_LEFTMETA, true);
                }
                self.send(Request::PointerMotion { x, y });
                self.send(Request::PointerButton {
                    button: BTN_LEFT,
                    pressed: true,
                });
                for _ in 0..steps {
                    let (x, y) = if self.random.chance(15) {
                        (self.random.pick(&[0.0, (self.size.0 - 1) as f64]), y)
                    } else {
                        self.point()
                    };
                    self.send(Request::PointerMotion { x, y });
                    if self.random.chance(30) {
                        let pause = self.random.range(5, 60) as u64;
                        self.pump(pause);
                    }
                }
                if logo && self.random.chance(30) {
                    // The key goes up before the button.
                    self.key(KEY_LEFTMETA, false);
                }
                self.send(Request::PointerButton {
                    button: BTN_LEFT,
                    pressed: false,
                });
                if logo {
                    self.key(KEY_LEFTMETA, false);
                }
            }
            5 => {
                let steps = self.random.range(-3, 3) as i32;
                self.note(format!("scroll {steps}"));
                self.send(Request::PointerAxis {
                    horizontal: 0.0,
                    vertical: f64::from(steps) * 15.0,
                    v120_horizontal: None,
                    v120_vertical: Some(steps * 120),
                });
            }
            _ => {
                // A three finger swipe on the touchpad.
                let cancelled = self.random.chance(20);
                let steps = self.random.range(1, 10);
                self.note(format!("swipe in {steps} steps, cancelled: {cancelled}"));
                self.send(Request::GestureBegin {
                    kind: "swipe".to_string(),
                    fingers: 3,
                });
                for _ in 0..steps {
                    let (dx, dy) = (
                        self.random.range(-80, 80) as f64,
                        self.random.range(-20, 20) as f64,
                    );
                    self.send(Request::GestureUpdate {
                        kind: "swipe".to_string(),
                        dx,
                        dy,
                        scale: 1.0,
                        rotation: 0.0,
                    });
                }
                self.send(Request::GestureEnd {
                    kind: "swipe".to_string(),
                    cancelled,
                });
            }
        }
    }

    /// The parent gives the nested window another size.
    fn resize(&mut self) {
        let (width, height) = (self.random.range(240, 1700), self.random.range(160, 1000));
        let Some(parent) = &mut self.parent else {
            return;
        };
        let state = parent.state();
        let Some(id) = state["windows"]
            .as_array()
            .and_then(|windows| windows.first())
            .and_then(|window| window["id"].as_u64())
        else {
            return;
        };
        parent.script("resize_window", json!([id, width, height]));
        self.note(format!("the window becomes {width}x{height}"));
    }

    fn reload(&mut self) {
        self.note("reload the script".to_string());
        let response = self.session.try_send(Request::ScriptReload { path: None });
        if let Err(error) = response {
            self.fail("reloading the script failed", &error);
        }
    }

    /// Looks at the state: the script is in charge, nothing failed.
    fn check(&mut self) {
        let state = self.send(Request::State);
        let script = &state["script"];
        if script["mode"] != "running" || script["failures"] != 0 {
            let detail = format!(
                "mode {} failures {}\n{:#}",
                script["mode"], script["failures"], script["last_failures"]
            );
            self.fail("the script failed", &detail);
        }
        let errors = &state["gpu"]["validation_errors"];
        if !(errors.is_null() || errors == 0) {
            self.fail("Vulkan validation errors", &errors.to_string());
        }
        if let Some(size) = state["outputs"][0]["logical_size"].as_array() {
            self.size = (
                size[0].as_i64().unwrap_or(1280),
                size[1].as_i64().unwrap_or(720),
            );
        }
        self.samples.push((self.actions, sample(&state)));
    }

    fn step(&mut self) {
        self.actions += 1;
        match self.random.range(0, 99) {
            0..=9 => self.open_client(),
            10..=15 => self.close_client(),
            16..=27 => self.change_client(),
            28..=31 => self.terminal(),
            32..=64 => self.keys(),
            65..=89 => self.pointer(),
            90..=93 => self.resize(),
            94 => self.reload(),
            _ => {
                let pause = self.random.range(50, 600) as u64;
                self.note(format!("nothing for {pause} ms"));
                self.pump(pause);
            }
        }
        let pause = self.random.range(0, 25) as u64;
        self.pump(pause);
        if self.actions.is_multiple_of(20) {
            self.check();
        }
    }
}

fn print_samples(samples: &[(u64, Sample)]) {
    println!(
        "{:>7} {:>9} {:>9} {:>6} {:>7} {:>6} {:>9} {:>6} {:>7} {:>8}",
        "actions",
        "script kB",
        "gpu kB",
        "images",
        "buffers",
        "slots",
        "pipelines",
        "closed",
        "windows",
        "frames"
    );
    let step = (samples.len() / 12).max(1);
    for (index, (actions, s)) in samples.iter().enumerate() {
        if index % step != 0 && index != samples.len() - 1 {
            continue;
        }
        println!(
            "{:>7} {:>9} {:>9} {:>6} {:>7} {:>6} {:>9} {:>6} {:>7} {:>8}",
            actions,
            s.script_memory / 1024,
            s.gpu_memory / 1024,
            s.images,
            s.buffers,
            s.slots,
            s.pipelines,
            s.closed_windows,
            s.windows,
            s.frames
        );
    }
}

fn shake(device: Device) {
    if !device.enabled() {
        eprintln!(
            "skipped: {} is not in HIZUKURI_TEST_DEVICES",
            device.gpu_name()
        );
        return;
    }
    let seed: u64 = variable("HIZUKURI_SHAKE_SEED").map_or(1, |s| s.parse().expect("a seed"));
    let seconds: u64 =
        variable("HIZUKURI_SHAKE_SECONDS").map_or(30, |s| s.parse().expect("seconds"));
    let project = variable("HIZUKURI_SHAKE_PROJECT").map(|p| {
        std::fs::canonicalize(&p).unwrap_or_else(|e| panic!("HIZUKURI_SHAKE_PROJECT {p}: {e}"))
    });
    let nested = variable("HIZUKURI_SHAKE_NESTED").is_some();
    let hook = variable("HIZUKURI_SHAKE_HOOK");

    let real_foot = common::real_client_wanted("foot", &["foot"])
        .then(|| find_program("foot"))
        .flatten();
    // Declared before the sessions, so dropped after them.
    let stand_in = terminal_stand_ins(real_foot.as_deref());
    // Removed when dropped, also when the test fails.
    let _stand_ins = DropGuard::new(
        stand_in
            .parent()
            .expect("the stand-in directory")
            .to_path_buf(),
        |dir| {
            let _ = std::fs::remove_dir_all(dir);
        },
    );
    let allow_dmabuf = common::udmabuf_wanted();

    let name = format!(
        "shake-{}-{seed}{}",
        project
            .as_ref()
            .and_then(|p| p.file_name())
            .map_or("builtin".to_string(), |n| n.to_string_lossy().into_owned()),
        if nested { "-nested" } else { "" }
    );
    // Long enough for the run, the settling and the checks after it.
    let exit_after = (seconds + 120).to_string();
    let mut arguments = vec!["--cursor", "on", "--exit-after", &exit_after];
    if allow_dmabuf {
        arguments.extend(["--dmabuf", "gpu"]);
    }
    // On a GPU the script gets the time it gets in real use; the software
    // rasterizer is slower than any budget meant for a person.
    if device.gpu == Gpu::Hardware {
        arguments.extend(["--script-budget-ms", "250"]);
    }
    let project_text = project.as_ref().map(|p| p.to_str().expect("a UTF-8 path"));
    if let Some(project) = project_text {
        arguments.extend(["--script", project]);
    }
    let (session, parent) = if nested {
        let parent_project = common::script_project(
            &format!("{name}-parent"),
            device,
            &[
                (
                    "main",
                    "hz.include(\"session\")\n\
                     function resize_window(id, width, height)\n\
                     \x20   hz.configure(id, width, height)\n\
                     \x20   return true\n\
                     end\n",
                ),
                ("session", common::DRAW_ONLY_SESSION),
            ],
        );
        let parent = Session::start(
            &format!("{name}-parent"),
            device,
            &[
                "--output-size",
                "1920x1200",
                "--exit-after",
                &exit_after,
                "--script",
                parent_project.to_str().unwrap(),
            ],
        );
        arguments.extend(["--output-size", "1280x720"]);
        let session = if project.is_some() {
            Session::start_nested(&name, device, &parent, &arguments)
        } else {
            Session::start_nested_builtin(&name, device, &parent, &arguments)
        };
        (session, Some(parent))
    } else {
        let session = if project.is_some() {
            Session::start(&name, device, &arguments)
        } else {
            Session::start_builtin(&name, device, &arguments)
        };
        (session, None)
    };

    let mut shaker = Shaker {
        session,
        parent,
        random: Random(seed),
        seed,
        clients: Vec::new(),
        terminals: Vec::new(),
        real_foot,
        size: (1280, 720),
        actions: 0,
        recent: VecDeque::new(),
        samples: Vec::new(),
        allow_dmabuf,
    };
    println!("shake: {name}, seed {seed}, {seconds} s");
    shaker.pump(300);
    shaker.check();

    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        shaker.step();
    }
    shaker.check();
    println!("shake: {} actions", shaker.actions);

    // Everything goes away: the test's clients, its terminals, and what the
    // project started itself (by the process ids the compositor reports).
    shaker.clients.clear();
    shaker.terminals.clear();
    for modifier in [KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_LEFTCTRL, KEY_LEFTALT] {
        shaker.key(modifier, false);
    }
    let state = shaker.send(Request::State);
    let own = std::process::id();
    for started in state["script"]["spawned"].as_array().into_iter().flatten() {
        if let Some(pid) = started["pid"].as_u64().filter(|pid| *pid as u32 != own) {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    let settle = Instant::now() + Duration::from_secs(20);
    let mut state = shaker.send(Request::State);
    while state["windows"].as_array().is_some_and(|w| !w.is_empty()) && Instant::now() < settle {
        std::thread::sleep(Duration::from_millis(100));
        state = shaker.send(Request::State);
    }
    // Whatever animates the departures comes to rest.
    shaker.pump(3000);
    shaker.check();
    let state = shaker.send(Request::State);
    let left: Vec<String> = state["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|w| format!("{} ({})", w["title"], w["app_id"]))
        .collect();

    // Does it keep drawing with nothing to draw? Animations may run on for
    // a while; after that, nothing should change the picture.
    let mut rest = None;
    let mut idle_frames = 0;
    for second in 0..15 {
        let before = sample(&shaker.send(Request::State));
        std::thread::sleep(Duration::from_secs(1));
        let after = sample(&shaker.send(Request::State));
        idle_frames = after.frames - before.frames;
        if idle_frames == 0 {
            rest = Some(second);
            break;
        }
    }

    print_samples(&shaker.samples);
    let (first, last) = (
        shaker.samples[0].1,
        shaker.samples[shaker.samples.len() - 1].1,
    );
    match rest {
        Some(seconds) => println!(
            "shake: it stopped drawing {} s after the last window went",
            seconds + 3
        ),
        None => println!(
            "shake: it still draws 18 s after the last window went ({idle_frames} frames in the last second)"
        ),
    }
    println!(
        "shake: windows left: {left:?}; closed windows kept: {}; validation errors: {}",
        last.closed_windows,
        // null: without the validation layer.
        state["gpu"]["validation_errors"]
    );
    println!(
        "shake: from the start to the end: script memory {} -> {} kB, GPU memory {} -> {} kB, \
         images {} -> {}, buffers {} -> {}, slots {} -> {}, pipelines {} -> {}",
        first.script_memory / 1024,
        last.script_memory / 1024,
        first.gpu_memory / 1024,
        last.gpu_memory / 1024,
        first.images,
        last.images,
        first.buffers,
        last.buffers,
        first.slots,
        last.slots,
        first.pipelines,
        last.pipelines
    );
    if let Some(hook) = hook {
        let answer = shaker.session.script(&hook, json!([]));
        println!("shake: {hook}: {answer:#}");
    }

    assert!(left.is_empty(), "windows are left: {left:?} (seed {seed})");
    assert_eq!(
        last.closed_windows, 0,
        "closed windows are still kept (seed {seed})"
    );
    // Images of windows and their slots go with the windows. What a project
    // makes on demand (a pipeline, a buffer for a size it had not seen) may
    // stay, so the bound is loose.
    assert!(
        last.images <= first.images + 16 && last.slots <= first.slots + 16,
        "GPU objects pile up: images {} -> {}, slots {} -> {} (seed {seed})",
        first.images,
        last.images,
        first.slots,
        last.slots
    );

    let Shaker {
        session, parent, ..
    } = shaker;
    session.quit();
    if let Some(parent) = parent {
        parent.quit();
    }
}

/// Found by shaking: windows that go away together are reported to the
/// script one after the other, and by the first report the others are gone
/// from the core's table while the script still lists them. What the script
/// asks about those is dropped, not held against it.
#[test]
fn windows_that_close_together_do_not_fail_the_script() {
    let device = Device::new(Gpu::Lavapipe);
    if !device.enabled() {
        return;
    }
    let mut session = Session::start_builtin("close-together", device, &[]);
    for round in 0..4 {
        let clients: Vec<TestClient> = (0..4)
            .map(|index| {
                session
                    .mapped_client(common::options(
                        &format!("w{round}-{index}"),
                        200,
                        150,
                        [200, 40, 40, 255],
                    ))
                    .0
            })
            .collect();
        drop(clients);
        session.wait(&mut [], "the windows to go", |state| {
            state["windows"].as_array().is_some_and(|w| w.is_empty())
        });
    }
    session.assert_script_healthy();
    session.quit();
}

#[test]
#[ignore = "takes as long as HIZUKURI_SHAKE_SECONDS says; see the module documentation"]
fn lavapipe() {
    shake(Device::new(Gpu::Lavapipe));
}

#[test]
#[ignore = "takes as long as HIZUKURI_SHAKE_SECONDS says; see the module documentation"]
fn gpu() {
    shake(Device::new(Gpu::Hardware));
}
