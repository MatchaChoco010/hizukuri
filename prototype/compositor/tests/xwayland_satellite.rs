//! The X11 display the compositor serves, and how it starts
//! xwayland-satellite (`--xwayland-satellite`).
//!
//! Neither Xwayland nor xwayland-satellite runs here: `proto-test-client
//! --as-xwayland-satellite` stands in for xwayland-satellite. It reports
//! the command line and the sockets it was given, connects back to the
//! compositor like xwayland-satellite does and shows a window, and greets
//! whoever connects to the X11 sockets.
//!
//! The X11 sockets and locks go to a directory of the test
//! (`--x11-root`), never to `/tmp`, where the X11 displays of the machine
//! are.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use common::{Device, Gpu, Session, TIMEOUT};
use serde_json::Value;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// The device these tests run on: one is enough, they are not about
/// drawing.
fn device() -> Device {
    let gpu = Device::new(Gpu::Hardware);
    if gpu.enabled() {
        gpu
    } else {
        Device::new(Gpu::Lavapipe)
    }
}

/// A directory that stands for /tmp, removed when dropped. Short, because
/// the socket paths inside it are limited to about 100 bytes.
struct X11Root(PathBuf);

impl X11Root {
    fn new() -> Self {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!(
            "hzk-x-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("creating the X11 root");
        Self(path)
    }

    fn socket(&self, number: u32) -> PathBuf {
        self.0.join(format!(".X11-unix/X{number}"))
    }

    fn lock(&self, number: u32) -> PathBuf {
        self.0.join(format!(".X{number}-lock"))
    }
}

impl Drop for X11Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A script that runs the stand-in with the command line the compositor
/// gives xwayland-satellite.
fn stand_in(root: &X11Root) -> PathBuf {
    let client =
        Path::new(env!("CARGO_BIN_EXE_proto-compositor")).with_file_name("proto-test-client");
    assert!(
        client.exists(),
        "{} is missing: the workspace's binaries are built by `cargo test --workspace` \
         (or `cargo build -p proto-test-client`)",
        client.display()
    );
    let script = root.0.join("xwayland-satellite");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec '{}' --as-xwayland-satellite \"$@\"\n",
            client.display()
        ),
    )
    .expect("writing the stand-in script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("making the stand-in script executable");
    script
}

fn start(name: &str, root: &X11Root, program: &Path) -> Session {
    let log = root.0.join("satellite.log");
    Session::start(
        name,
        device(),
        &[
            "--xwayland-satellite",
            program.to_str().unwrap(),
            "--x11-root",
            root.0.to_str().unwrap(),
            "--xwayland-satellite-log",
            log.to_str().unwrap(),
        ],
    )
}

fn wait_state<T>(
    session: &mut Session,
    what: &str,
    mut done: impl FnMut(&Value) -> Option<T>,
) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let state = session.state();
        if let Some(value) = done(&state) {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for: {what}\nstate: {state:#}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Connects like an X11 client and reads the greeting. `None` when the
/// connection was closed without one.
fn connect(stream: UnixStream) -> (Option<String>, UnixStream) {
    stream
        .set_read_timeout(Some(TIMEOUT))
        .expect("setting a timeout");
    let mut reader = BufReader::new(stream.try_clone().expect("cloning the stream"));
    let mut line = String::new();
    let greeting = match reader.read_line(&mut line) {
        Ok(n) if n > 0 => Some(line.trim().to_string()),
        _ => None,
    };
    (greeting, stream)
}

fn by_path(root: &X11Root, number: u32) -> UnixStream {
    UnixStream::connect(root.socket(number)).expect("connecting to the X11 socket")
}

fn by_name(root: &X11Root, number: u32) -> UnixStream {
    let name = root.socket(number).into_os_string().into_encoded_bytes();
    let address = SocketAddr::from_abstract_name(&name).expect("an abstract address");
    UnixStream::connect_addr(&address).expect("connecting to the abstract X11 socket")
}

/// The lines the stand-in printed, as JSON.
fn reports(root: &X11Root) -> Vec<Value> {
    std::fs::read_to_string(root.0.join("satellite.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[test]
fn the_first_x11_client_starts_xwayland_satellite_on_the_display_sockets() {
    let root = X11Root::new();
    let program = stand_in(&root);
    let mut session = start("x11-start", &root, &program);

    // The display is claimed, nothing is started yet.
    let state = session.state();
    assert_eq!(state["x11"]["display"], ":0", "{:#}", state["x11"]);
    assert!(state["x11"]["satellite_pid"].is_null());
    assert_eq!(state["x11"]["satellite_starts"], 0);
    assert!(root.socket(0).exists());
    let lock = std::fs::read_to_string(root.lock(0)).expect("the lock file");
    assert_eq!(lock.len(), 11, "ten characters and a newline: {lock:?}");
    assert!(lock.trim().parse::<u32>().is_ok(), "{lock:?}");

    // An X11 client connects: xwayland-satellite starts and gets it.
    let (greeting, _first) = connect(by_path(&root, 0));
    assert_eq!(
        greeting.as_deref(),
        Some("proto-test-client stands in for xwayland-satellite on :0")
    );
    let (pid, window) = wait_state(&mut session, "the X11 window", |state| {
        let pid = state["x11"]["satellite_pid"].as_u64()?;
        let window = state["windows"]
            .as_array()?
            .iter()
            .find(|w| w["app_id"] == "hizukuri.fake-x11" && w["mapped"] == true)?
            .clone();
        Some((pid, window))
    });
    assert_eq!(session.state()["x11"]["satellite_starts"], 1);
    // Its windows are known as X11 windows, by the client's process id.
    assert_eq!(window["x11"], true, "{window:#}");
    assert_eq!(window["client_pid"].as_u64(), Some(pid));
    assert_eq!(window["title"], "X11 window on :0");

    // What it was given: the display, both sockets (the file and the
    // abstract one of the same name), DISPLAY, and the compositor's socket
    // by name.
    let started = reports(&root)
        .into_iter()
        .find(|r| r["event"] == "satellite_started")
        .expect("the stand-in reported its start");
    let path = root.socket(0).display().to_string();
    assert_eq!(started["pid"].as_u64(), Some(pid));
    assert_eq!(started["display"], ":0");
    assert_eq!(started["listen"][0]["path"], path.as_str(), "{started:#}");
    assert_eq!(
        started["listen"][1]["abstract"],
        path.as_str(),
        "{started:#}"
    );
    assert_eq!(started["env_display"], ":0");
    assert_eq!(started["wayland_display"], "hizukuri-test");
    assert!(started["wayland_socket"].is_null());
    // The compositor blocks the signals its event loop handles; the child
    // must not keep that mask, or SIGTERM would not stop it.
    assert_eq!(
        started["blocked_signals"], "0000000000000000",
        "{started:#}"
    );

    // While it runs, it accepts on both sockets itself.
    let (greeting, mut second) = connect(by_name(&root, 0));
    assert!(greeting.is_some(), "the abstract socket reaches it too");
    assert_eq!(session.state()["x11"]["satellite_starts"], 1);

    // It ends: its windows go, the compositor stays, and the next X11
    // client starts it again.
    writeln!(second, "exit 3").expect("asking the stand-in to exit");
    let exits = wait_state(&mut session, "xwayland-satellite to end", |state| {
        state["x11"]["satellite_pid"]
            .is_null()
            .then(|| state["x11"]["satellite_exits"].clone())
    });
    assert!(
        exits[0]
            .as_str()
            .is_some_and(|e| e.starts_with("exited with 3")),
        "{exits:#}"
    );
    wait_state(&mut session, "the X11 window to go", |state| {
        state["windows"]
            .as_array()?
            .iter()
            .all(|w| w["app_id"] != "hizukuri.fake-x11")
            .then_some(())
    });
    let (greeting, _third) = connect(by_path(&root, 0));
    assert!(greeting.is_some(), "started again");
    wait_state(&mut session, "the second start", |state| {
        (state["x11"]["satellite_starts"] == 2 && state["x11"]["satellite_pid"].is_u64())
            .then_some(())
    });

    // The compositor ends it, and removes its socket and lock.
    session.quit();
    assert!(!root.socket(0).exists(), "the X11 socket was left behind");
    assert!(!root.lock(0).exists(), "the lock was left behind");
}

#[test]
fn a_display_that_is_taken_is_skipped() {
    let root = X11Root::new();
    let program = stand_in(&root);
    // Another X server holds :0: its lock names a running process (this
    // test).
    std::fs::write(root.lock(0), format!("{:>10}\n", std::process::id())).unwrap();
    let mut session = start("x11-taken", &root, &program);
    assert_eq!(session.state()["x11"]["display"], ":1");
    assert!(!root.socket(0).exists());
    assert!(root.socket(1).exists());
    session.quit();
    assert!(root.lock(0).exists(), "somebody else's lock stays");
}

#[test]
fn x11_clients_are_dropped_when_xwayland_satellite_cannot_start() {
    let root = X11Root::new();
    let missing = root.0.join("no-such-program");
    let mut session = start("x11-missing", &root, &missing);
    // The connection is accepted and closed, not left waiting (which would
    // start the program again and again).
    for _ in 0..2 {
        let (greeting, _) = connect(by_path(&root, 0));
        assert_eq!(greeting, None);
    }
    let state = session.state();
    assert_eq!(state["x11"]["satellite_starts"], 0, "{:#}", state["x11"]);
    assert!(state["x11"]["satellite_pid"].is_null());
    session.quit();
}
