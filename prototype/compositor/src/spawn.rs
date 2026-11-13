//! Starting child processes: what every process the compositor starts
//! needs, in one place.
//!
//! - The event loop blocks the signals it handles (SIGTERM, SIGINT,
//!   SIGHUP). A child would inherit that mask and could then not be stopped
//!   with them, so the mask is emptied between fork and exec.
//! - A child gets a process group of its own: a signal meant for the
//!   compositor's terminal does not reach it.
//! - `WAYLAND_SOCKET` is not handed on: it would win over `WAYLAND_DISPLAY`.

use std::io;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Prepares `command` as described in the module documentation.
/// `inherited` are descriptors of this process the child keeps open.
pub fn prepare(command: &mut Command, inherited: &[RawFd]) {
    command.env_remove("WAYLAND_SOCKET").process_group(0);
    let inherited = inherited.to_vec();
    // SAFETY: only fcntl and pthread_sigmask, which are async-signal-safe,
    // run between fork and exec. The descriptors stay open in the parent
    // until then.
    unsafe {
        command.pre_exec(move || {
            for &fd in &inherited {
                let fd = std::os::fd::BorrowedFd::borrow_raw(fd);
                rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::empty())?;
            }
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            if libc::pthread_sigmask(libc::SIG_SETMASK, &none, std::ptr::null_mut()) != 0 {
                return Err(io::Error::other("pthread_sigmask failed"));
            }
            Ok(())
        });
    }
}

/// An application to start: a program a user of the compositor runs in it
/// (a terminal bound to a key, a launcher).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct App {
    /// The program (looked up in `PATH`) and its arguments.
    pub argv: Vec<String>,
    /// Variables set (or, with `None`, removed) on top of the compositor's
    /// environment.
    pub env: Vec<(String, Option<String>)>,
    pub cwd: Option<PathBuf>,
}

/// Starts an application and returns its process id, without waiting for
/// it. The application is not stopped when the compositor exits; its exit
/// status is collected whenever it ends.
pub fn spawn_app(app: &App) -> io::Result<u32> {
    let Some((program, arguments)) = app.argv.split_first() else {
        return Err(io::Error::other("no program to start"));
    };
    let mut command = Command::new(program);
    command.args(arguments).stdin(Stdio::null());
    for (name, value) in &app.env {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    if let Some(cwd) = &app.cwd {
        command.current_dir(cwd);
    }
    prepare(&mut command, &[]);
    let mut child = command.spawn()?;
    let pid = child.id();
    // Nobody else waits for it; without this it would stay a zombie.
    std::thread::Builder::new()
        .name("reap".into())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_app_gets_its_environment_and_an_empty_signal_mask() {
        let directory = std::env::temp_dir().join(format!("hzk-spawn-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let out = directory.join("out");
        // Block a signal like the event loop does.
        // SAFETY: plain libc calls on this thread's mask.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        }
        let pid = spawn_app(&App {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "{ echo \"$HZK_TEST\"; echo \"${HZK_GONE-unset}\"; grep SigBlk /proc/self/status; } \
                 > out.tmp && mv out.tmp out"
                    .into(),
            ],
            env: vec![
                ("HZK_TEST".into(), Some("value".into())),
                ("HZK_GONE".into(), None),
            ],
            cwd: Some(directory.clone()),
        })
        .unwrap();
        assert!(pid > 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !out.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let text = std::fs::read_to_string(&out).unwrap();
        let _ = std::fs::remove_dir_all(&directory);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "value");
        assert_eq!(lines[1], "unset");
        assert_eq!(lines[2].split_whitespace().last(), Some("0000000000000000"));
    }

    #[test]
    fn a_missing_program_is_an_error() {
        assert!(spawn_app(&App::default()).is_err());
        assert!(
            spawn_app(&App {
                argv: vec!["hizukuri-no-such-program".into()],
                ..App::default()
            })
            .is_err()
        );
    }
}
