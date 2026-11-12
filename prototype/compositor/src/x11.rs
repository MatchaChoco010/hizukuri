//! X11 applications, through xwayland-satellite.
//!
//! The compositor has no X11 window manager of its own. It owns the X11
//! display (the sockets X11 clients connect to, and the lock that claims the
//! display number), and starts xwayland-satellite when the first X11 client
//! connects. xwayland-satellite starts Xwayland on those sockets
//! (`-listenfd`), acts as Xwayland's window manager, and connects to the
//! compositor as an ordinary Wayland client: X11 windows arrive as
//! `xdg_toplevel`s and `xdg_popup`s of that one client.
//!
//! While xwayland-satellite runs, the compositor does not watch the sockets;
//! Xwayland accepts on them. When it ends, the connections that are still
//! waiting are accepted and dropped (otherwise they would start it again at
//! once, in a loop, when it cannot start), and the compositor watches the
//! sockets again: the next X11 client starts it anew.
//!
//! [`X11Socket`] and [`Satellite`] know nothing of the event loop, so they
//! can be tested on their own; [`insert_sources`] puts them on one.

use std::cell::RefCell;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::RangeInclusive;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixListener};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::rc::Rc;
use std::time::Instant;

use calloop::generic::Generic;
use calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use rustix::process::{Pid, PidfdFlags, Signal};

/// The display numbers tried, lowest first.
pub const DISPLAY_NUMBERS: RangeInclusive<u32> = 0..=32;

/// The sockets and the lock of one X11 display (`:N`).
///
/// Under `root` (`/tmp` for a real session): the socket
/// `.X11-unix/X<N>`, an abstract socket of the same name, and the lock file
/// `.X<N>-lock`, which holds the process id of the owner. Dropping it
/// removes the socket file and the lock.
#[derive(Debug)]
pub struct X11Socket {
    number: u32,
    lock: PathBuf,
    path: PathBuf,
    listeners: Vec<UnixListener>,
}

impl X11Socket {
    /// Claims the lowest display number in `numbers` that no running
    /// process holds. A lock whose process is gone, and the socket left
    /// next to it, are removed and the number is taken.
    pub fn open(root: &Path, numbers: RangeInclusive<u32>) -> io::Result<Self> {
        let directory = root.join(".X11-unix");
        ensure_socket_directory(&directory)?;
        let pid = std::process::id();
        for number in numbers {
            let lock = root.join(format!(".X{number}-lock"));
            if !take_lock(&lock, pid)? {
                continue;
            }
            let path = directory.join(format!("X{number}"));
            match bind(&path) {
                Ok(listeners) => {
                    return Ok(Self {
                        number,
                        lock,
                        path,
                        listeners,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                    // Somebody serves this display without its lock.
                    let _ = fs::remove_file(&lock);
                }
                Err(error) => {
                    let _ = fs::remove_file(&lock);
                    return Err(error);
                }
            }
        }
        Err(io::Error::other(format!(
            "no free X11 display number under {}",
            root.display()
        )))
    }

    pub fn number(&self) -> u32 {
        self.number
    }

    /// The value of `DISPLAY` for clients: `:N`.
    pub fn display(&self) -> String {
        format!(":{}", self.number)
    }

    /// The socket file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The listening sockets: the socket file, then the abstract socket.
    pub fn listeners(&self) -> &[UnixListener] {
        &self.listeners
    }

    /// Accepts the connections that wait on the sockets and closes them.
    /// Returns how many there were.
    pub fn drain(&self) -> usize {
        let mut count = 0;
        for listener in &self.listeners {
            while let Ok((stream, _)) = listener.accept() {
                drop(stream);
                count += 1;
            }
        }
        count
    }
}

impl X11Socket {
    /// Removes the socket file and the lock: the display is free for
    /// others. The listening sockets stay open until dropped.
    pub fn remove_files(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(&self.lock);
    }
}

impl Drop for X11Socket {
    fn drop(&mut self) {
        self.remove_files();
    }
}

/// `/tmp/.X11-unix` is shared by every user; it is made world writable and
/// sticky when this is the first X11 display of the machine.
fn ensure_socket_directory(directory: &Path) -> io::Result<()> {
    match fs::create_dir(directory) {
        Ok(()) => fs::set_permissions(directory, fs::Permissions::from_mode(0o1777)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Takes the lock of a display number. `Ok(false)` when a running process
/// holds it.
fn take_lock(lock: &Path, pid: u32) -> io::Result<bool> {
    for _ in 0..2 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(lock)
        {
            Ok(mut file) => {
                // The format X servers use: the id right aligned in ten
                // characters, and a newline.
                file.write_all(format!("{pid:>10}\n").as_bytes())?;
                return Ok(true);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if lock_is_held(lock) {
                    return Ok(false);
                }
                tracing::info!(lock = %lock.display(), "removing a stale X11 display lock");
                match fs::remove_file(lock) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    // Not ours to remove (another user's): taken.
                    Err(_) => return Ok(false),
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

/// Whether the process a lock file names is running. A lock that cannot
/// be read, or names no process id, counts as held: it is not removed.
fn lock_is_held(lock: &Path) -> bool {
    let mut text = String::new();
    if File::open(lock)
        .and_then(|mut file| file.read_to_string(&mut text))
        .is_err()
    {
        return true;
    }
    let Some(pid) = text.trim().parse::<i32>().ok().and_then(Pid::from_raw) else {
        return true;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        // It exists, under another user.
        Err(rustix::io::Errno::PERM) => true,
        Err(_) => false,
    }
}

/// Binds the socket file (removing one left behind: the lock is ours) and
/// the abstract socket of the same name. Both are non-blocking.
fn bind(path: &Path) -> io::Result<Vec<UnixListener>> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let file = UnixListener::bind(path)?;
    let name = path.as_os_str().as_encoded_bytes();
    let address = SocketAddr::from_abstract_name(name)?;
    let abstract_socket = match UnixListener::bind_addr(&address) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = fs::remove_file(path);
            return Err(error);
        }
    };
    for listener in [&file, &abstract_socket] {
        listener.set_nonblocking(true)?;
    }
    Ok(vec![file, abstract_socket])
}

/// How xwayland-satellite is started.
#[derive(Debug, Clone)]
pub struct SatelliteConfig {
    /// The executable (a path, or a name looked up in `PATH`).
    pub program: PathBuf,
    /// The compositor's Wayland socket, as `WAYLAND_DISPLAY` names it.
    pub wayland_display: OsString,
    /// Where its output goes. Without one it is discarded, so that it does
    /// not mix with the compositor's.
    pub log: Option<PathBuf>,
}

/// xwayland-satellite: at most one running at a time.
#[derive(Debug)]
pub struct Satellite {
    config: SatelliteConfig,
    child: Option<(Child, Instant)>,
    starts: u32,
    exits: Vec<String>,
}

/// What the state dump shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct X11Status {
    /// `:N`.
    pub display: String,
    /// The process id of the running xwayland-satellite.
    pub satellite_pid: Option<u32>,
    /// How many times it was started.
    pub starts: u32,
    /// How the recent runs ended, oldest first.
    pub exits: Vec<String>,
}

/// How many endings [`X11Status::exits`] keeps.
const KEPT_EXITS: usize = 8;

impl Satellite {
    pub fn new(config: SatelliteConfig) -> Self {
        Self {
            config,
            child: None,
            starts: 0,
            exits: Vec::new(),
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|(child, _)| child.id())
    }

    /// The command line it is started with, for `socket`:
    /// `:N -listenfd FD -listenfd FD`. The descriptors are those of the
    /// listening sockets, which the child inherits.
    pub fn arguments(socket: &X11Socket) -> Vec<String> {
        let mut arguments = vec![socket.display()];
        for listener in socket.listeners() {
            arguments.push("-listenfd".to_string());
            arguments.push(listener.as_raw_fd().to_string());
        }
        arguments
    }

    /// Starts it for `socket`. Returns a pidfd of the process, which
    /// becomes readable when it ends.
    pub fn spawn(&mut self, socket: &X11Socket) -> io::Result<OwnedFd> {
        if self.child.is_some() {
            return Err(io::Error::other("xwayland-satellite is already running"));
        }
        let mut command = Command::new(&self.config.program);
        command
            .args(Self::arguments(socket))
            .env("WAYLAND_DISPLAY", &self.config.wayland_display)
            .env("DISPLAY", socket.display())
            // WAYLAND_SOCKET would win over WAYLAND_DISPLAY; it is not
            // ours to hand on.
            .env_remove("WAYLAND_SOCKET")
            .stdin(Stdio::null())
            // Its own process group: a signal meant for the compositor's
            // terminal does not reach it.
            .process_group(0);
        match &self.config.log {
            Some(path) => {
                let file = OpenOptions::new().create(true).append(true).open(path)?;
                command.stdout(file.try_clone()?).stderr(file);
            }
            None => {
                command.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        let inherited: Vec<i32> = socket.listeners().iter().map(|l| l.as_raw_fd()).collect();
        // SAFETY: only fcntl and pthread_sigmask, which are
        // async-signal-safe, run between fork and exec. The descriptors stay
        // open in the parent until then.
        unsafe {
            command.pre_exec(move || {
                for &fd in &inherited {
                    let fd = std::os::fd::BorrowedFd::borrow_raw(fd);
                    rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty())?;
                }
                // The event loop blocks the signals it handles (SIGTERM,
                // SIGINT, SIGHUP); a child would inherit that mask, and
                // could then not be stopped with them.
                let mut none: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut none);
                if libc::pthread_sigmask(libc::SIG_SETMASK, &none, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::other("pthread_sigmask failed"));
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        let pid = Pid::from_raw(child.id() as i32).expect("a child has a process id");
        let pidfd = match rustix::process::pidfd_open(pid, PidfdFlags::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                let mut child = child;
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        };
        self.starts += 1;
        self.child = Some((child, Instant::now()));
        Ok(pidfd)
    }

    /// Collects the exit status once the process ended. Returns how it
    /// ended, or `None` while it runs (or when none was started).
    pub fn reap(&mut self) -> Option<String> {
        let (child, started) = self.child.as_mut()?;
        let status = child.try_wait().ok()??;
        let ending = format!(
            "{} after {:.1} s",
            describe_status(status),
            started.elapsed().as_secs_f64()
        );
        self.child = None;
        self.exits.push(ending.clone());
        if self.exits.len() > KEPT_EXITS {
            self.exits.remove(0);
        }
        Some(ending)
    }

    pub fn status(&self, socket: &X11Socket) -> X11Status {
        X11Status {
            display: socket.display(),
            satellite_pid: self.pid(),
            starts: self.starts,
            exits: self.exits.clone(),
        }
    }

    /// Asks the running process to end (SIGTERM, by the id it was started
    /// with). Does not wait.
    pub fn terminate(&mut self) {
        if let Some((child, _)) = &mut self.child
            && let Ok(None) = child.try_wait()
            && let Some(pid) = Pid::from_raw(child.id() as i32)
        {
            let _ = rustix::process::kill_process(pid, Signal::TERM);
        }
    }
}

impl Drop for Satellite {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn describe_status(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exited with {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        _ => format!("ended ({status})"),
    }
}

/// The X11 display and xwayland-satellite, shared by the event loop's
/// sources.
struct Shared {
    socket: X11Socket,
    satellite: Satellite,
    listening: Vec<RegistrationToken>,
}

/// The X11 display on an event loop. Dropping it gives the display up:
/// the socket file and the lock are removed and xwayland-satellite is asked
/// to end. (The loop's sources cannot be relied on for that: they live as
/// long as the loop, and sources that hold the loop's handle keep it alive.)
pub struct X11Display {
    shared: Rc<RefCell<Shared>>,
}

impl X11Display {
    pub fn status(&self) -> X11Status {
        let shared = self.shared.borrow();
        shared.satellite.status(&shared.socket)
    }
}

impl Drop for X11Display {
    fn drop(&mut self) {
        let mut shared = self.shared.borrow_mut();
        shared.satellite.terminate();
        shared.socket.remove_files();
    }
}

/// Puts the X11 display on the event loop: the sockets are watched, the
/// first connection starts xwayland-satellite, and its end makes the
/// sockets watched again. `report` is called with the status whenever it
/// changes.
pub fn insert_sources<D: 'static>(
    handle: &LoopHandle<'static, D>,
    socket: X11Socket,
    satellite: Satellite,
    report: fn(&mut D, X11Status),
) -> Result<X11Display, String> {
    let fds: Vec<OwnedFd> = socket
        .listeners()
        .iter()
        .map(|listener| listener.as_fd().try_clone_to_owned())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("failed to duplicate an X11 socket: {e}"))?;
    let shared = Rc::new(RefCell::new(Shared {
        socket,
        satellite,
        listening: Vec::new(),
    }));
    for fd in fds {
        let shared_for_source = Rc::clone(&shared);
        let loop_handle = handle.clone();
        let token = handle
            .insert_source(
                Generic::new(fd, Interest::READ, Mode::Level),
                move |_, _, data: &mut D| {
                    start(&loop_handle, &shared_for_source, data, report);
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| format!("failed to register an X11 socket: {e}"))?;
        shared.borrow_mut().listening.push(token);
    }
    Ok(X11Display { shared })
}

/// An X11 client connected: start xwayland-satellite, stop watching the
/// sockets until it ends.
fn start<D: 'static>(
    handle: &LoopHandle<'static, D>,
    shared: &Rc<RefCell<Shared>>,
    data: &mut D,
    report: fn(&mut D, X11Status),
) {
    let mut guard = shared.borrow_mut();
    let state = &mut *guard;
    if state.satellite.pid().is_some() {
        return;
    }
    match state.satellite.spawn(&state.socket) {
        Ok(pidfd) => {
            // Xwayland accepts on the sockets from now on. (A source may
            // disable itself from its callback; it may not enable itself.)
            for token in &state.listening {
                handle.disable(token).ok();
            }
            tracing::info!(
                display = %state.socket.display(),
                pid = state.satellite.pid(),
                "an X11 client connected; started xwayland-satellite"
            );
            let shared_for_exit = Rc::clone(shared);
            let loop_handle = handle.clone();
            let inserted = handle.insert_source(
                Generic::new(pidfd, Interest::READ, Mode::Level),
                move |_, _, data: &mut D| {
                    ended(&loop_handle, &shared_for_exit, data, report);
                    Ok(PostAction::Remove)
                },
            );
            if let Err(error) = inserted {
                tracing::error!("failed to watch xwayland-satellite: {error}");
            }
        }
        Err(error) => {
            tracing::error!(
                program = %state.satellite.config.program.display(),
                "failed to start xwayland-satellite: {error}"
            );
            // Otherwise they would come back at once, and again.
            let dropped = state.socket.drain();
            tracing::warn!(dropped, "dropped the X11 connections that waited");
        }
    }
    let status = state.satellite.status(&state.socket);
    drop(guard);
    report(data, status);
}

/// xwayland-satellite ended: drop what waits, watch the sockets again.
fn ended<D: 'static>(
    handle: &LoopHandle<'static, D>,
    shared: &Rc<RefCell<Shared>>,
    data: &mut D,
    report: fn(&mut D, X11Status),
) {
    let mut guard = shared.borrow_mut();
    let state = &mut *guard;
    let ending = state.satellite.reap();
    let dropped = state.socket.drain();
    tracing::warn!(
        ending = ending.as_deref().unwrap_or("ended"),
        dropped,
        "xwayland-satellite ended; the next X11 client starts it again"
    );
    for token in &state.listening {
        handle.enable(token).ok();
    }
    let status = state.satellite.status(&state.socket);
    drop(guard);
    report(data, status);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "hzk-x11-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_lowest_free_number_is_taken_and_given_back() {
        let root = TempDir::new("lowest");
        let first = X11Socket::open(&root.0, 3..=5).unwrap();
        assert_eq!(first.display(), ":3");
        let lock = fs::read_to_string(root.0.join(".X3-lock")).unwrap();
        assert_eq!(lock, format!("{:>10}\n", std::process::id()));
        assert!(root.0.join(".X11-unix/X3").exists());
        let mode = fs::metadata(root.0.join(".X11-unix"))
            .unwrap()
            .permissions();
        assert_eq!(mode.mode() & 0o7777, 0o1777);

        // Held by this process: the next one goes to :4.
        let second = X11Socket::open(&root.0, 3..=5).unwrap();
        assert_eq!(second.display(), ":4");
        drop(first);
        assert!(!root.0.join(".X3-lock").exists());
        assert!(!root.0.join(".X11-unix/X3").exists());
        let third = X11Socket::open(&root.0, 3..=5).unwrap();
        assert_eq!(third.display(), ":3");
    }

    #[test]
    fn a_lock_of_a_process_that_is_gone_is_replaced() {
        let root = TempDir::new("stale");
        fs::create_dir_all(root.0.join(".X11-unix")).unwrap();
        // A process id that is not running: a child that was waited for.
        let mut child = Command::new("true").spawn().unwrap();
        let gone = child.id();
        child.wait().unwrap();
        fs::write(root.0.join(".X0-lock"), format!("{gone:>10}\n")).unwrap();
        fs::write(root.0.join(".X11-unix/X0"), b"left behind").unwrap();
        // A lock nobody can read the owner of is left alone.
        fs::write(root.0.join(".X1-lock"), b"garbage").unwrap();

        let socket = X11Socket::open(&root.0, 0..=2).unwrap();
        assert_eq!(socket.display(), ":0");
        let again = X11Socket::open(&root.0, 0..=2).unwrap();
        assert_eq!(
            again.display(),
            ":2",
            ":1 has a lock that is not understood"
        );
    }

    #[test]
    fn clients_reach_both_sockets_and_waiting_ones_are_dropped() {
        let root = TempDir::new("connect");
        let socket = X11Socket::open(&root.0, 0..=0).unwrap();
        let _by_path = UnixStream::connect(socket.path()).unwrap();
        let name = socket.path().as_os_str().as_encoded_bytes().to_vec();
        let address = SocketAddr::from_abstract_name(&name).unwrap();
        let _by_name = UnixStream::connect_addr(&address).unwrap();
        assert_eq!(socket.drain(), 2);
        assert_eq!(socket.drain(), 0);
    }

    #[test]
    fn a_display_number_served_without_a_lock_is_skipped() {
        let root = TempDir::new("unlocked");
        fs::create_dir_all(root.0.join(".X11-unix")).unwrap();
        // Somebody listens on the abstract socket of :0 without a lock.
        let path = root.0.join(".X11-unix/X0");
        let name = path.as_os_str().as_encoded_bytes().to_vec();
        let _other = UnixListener::bind_addr(&SocketAddr::from_abstract_name(&name).unwrap());
        let socket = X11Socket::open(&root.0, 0..=1).unwrap();
        assert_eq!(socket.display(), ":1");
        assert!(!root.0.join(".X0-lock").exists());
    }

    #[test]
    fn the_command_line_passes_the_sockets() {
        let root = TempDir::new("arguments");
        let socket = X11Socket::open(&root.0, 7..=7).unwrap();
        let fds: Vec<String> = socket
            .listeners()
            .iter()
            .map(|l| l.as_raw_fd().to_string())
            .collect();
        assert_eq!(
            Satellite::arguments(&socket),
            vec![
                ":7".to_string(),
                "-listenfd".to_string(),
                fds[0].clone(),
                "-listenfd".to_string(),
                fds[1].clone()
            ]
        );
    }
}
