//! The command line of `proto-test-client --as-xwayland-satellite`, the
//! stand-in for xwayland-satellite that the compositor's tests start. (What
//! it does once started is checked there, in
//! `prototype/compositor/tests/xwayland_satellite.rs`, which also needs
//! this test: it makes `cargo test` build the binary.)

use std::process::Command;

fn run(args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_proto-test-client"))
        .args(args)
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("DISPLAY")
        .output()
        .expect("running proto-test-client");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn the_stand_in_wants_the_command_line_of_xwayland_satellite() {
    let (ok, said) = run(&["--as-xwayland-satellite"]);
    assert!(!ok);
    assert!(
        said.contains("the first argument must be the display"),
        "{said}"
    );

    let (ok, said) = run(&["--as-xwayland-satellite", ":3", "-listenfd"]);
    assert!(!ok);
    assert!(said.contains("-listenfd needs a descriptor"), "{said}");

    let (ok, said) = run(&["--as-xwayland-satellite", ":3", "-wm", "4"]);
    assert!(!ok);
    assert!(said.contains("unexpected argument"), "{said}");
}
