//! Applications a script starts (`hz.spawn`).

mod common;

use common::{Device, Session, script_project, wait_for_file};
use proto_compositor::control::Request;
use serde_json::json;

const KEY_ENTER: u32 = 28;
const KEY_LEFTMETA: u32 = 125;

/// A script starts a program: it gets the environment that leads to this
/// compositor, and the script gets its process id. A program that cannot
/// be started is reported, not raised.
fn a_script_starts_a_program(device: Device) {
    let project = script_project(
        "spawn",
        device,
        &[(
            "main",
            r#"
hz.include("session")
function start(argv, options)
    local pid, problem = hz.spawn(argv, options)
    return { pid = pid, problem = problem }
end
"#,
        )],
    );
    let mut session = Session::start("spawn", device, &["--script", project.to_str().unwrap()]);
    let out = session.runtime_dir.join("spawned");
    // Written under another name first, so that it is read whole.
    let command = "echo \"$WAYLAND_DISPLAY ${DISPLAY-unset} ${WAYLAND_SOCKET-unset} $EXTRA\" \
                   > spawned.tmp && grep SigBlk /proc/self/status >> spawned.tmp \
                   && mv spawned.tmp spawned";
    let result = session.script(
        "start",
        json!([
            ["sh", "-c", command],
            { "env": { "EXTRA": "extra" }, "cwd": session.runtime_dir },
        ]),
    );
    let pid = result["pid"].as_u64().expect("a process id");
    assert!(result["problem"].is_null(), "{result}");
    let text = wait_for_file(&out);
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("hizukuri-test unset unset extra"));
    // The signals the event loop blocks are not blocked in the program.
    assert_eq!(
        lines.next().and_then(|l| l.split_whitespace().last()),
        Some("0000000000000000")
    );
    let state = session.script_state();
    assert_eq!(state["spawned"][0]["pid"].as_u64(), Some(pid), "{state:#}");

    let result = session.script("start", json!([["hizukuri-no-such-program"]]));
    assert!(result["pid"].is_null(), "{result}");
    assert!(result["problem"].is_string(), "{result}");
    session.assert_script_healthy();
    session.quit();
}

/// The built-in project starts a terminal with logo+Return: the command in
/// `HIZUKURI_TERMINAL`.
fn the_built_in_project_starts_a_terminal(device: Device) {
    common::use_terminal_stand_in();
    let mut session = Session::start_builtin("spawn-terminal", device, &[]);
    for (keycode, pressed) in [
        (KEY_LEFTMETA, true),
        (KEY_ENTER, true),
        (KEY_ENTER, false),
        (KEY_LEFTMETA, false),
    ] {
        session.send(Request::Key { keycode, pressed });
    }
    let text = wait_for_file(&session.runtime_dir.join("terminal"));
    assert_eq!(text.trim(), "hizukuri-test");
    session.assert_script_healthy();
    session.quit();
}

device_tests!(
    a_script_starts_a_program,
    the_built_in_project_starts_a_terminal
);
