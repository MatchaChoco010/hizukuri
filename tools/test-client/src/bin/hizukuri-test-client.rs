//! Command line front end of the test client.
//!
//! ```text
//! hizukuri-test-client --socket PATH [--title T] [--app-id ID] [--size WxH]
//!                      [--color RRGGBBAA] [--layout argb8888] [--duration SECONDS]
//! ```
//!
//! Shows a solid window and prints every event it receives as one JSON
//! object per line on stdout. `--socket` is required: the client never
//! falls back to `WAYLAND_DISPLAY`, so it cannot open a window on the
//! desktop session by accident. `--color` gives the stored channel values
//! (premultiplied, as `wl_shm` expects).

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use hizukuri_test_client::{ClientOptions, PixelLayout, TestClient};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("hizukuri-test-client: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut options = ClientOptions::default();
    let mut socket: Option<PathBuf> = None;
    let mut duration: Option<Duration> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--socket" => socket = Some(PathBuf::from(value("--socket")?)),
            "--title" => options.title = value("--title")?,
            "--app-id" => options.app_id = value("--app-id")?,
            "--size" => {
                let text = value("--size")?;
                let (w, h) = text
                    .split_once('x')
                    .ok_or(format!("invalid size {text:?}, expected WxH"))?;
                options.width = w.parse().map_err(|_| format!("invalid width {w:?}"))?;
                options.height = h.parse().map_err(|_| format!("invalid height {h:?}"))?;
            }
            "--color" => {
                let text = value("--color")?;
                let parsed = u32::from_str_radix(&text, 16)
                    .ok()
                    .filter(|_| text.len() == 8)
                    .ok_or(format!("invalid color {text:?}, expected RRGGBBAA"))?;
                options.color = parsed.to_be_bytes();
            }
            "--layout" => {
                let text = value("--layout")?;
                options.layout = PixelLayout::from_name(&text)
                    .ok_or(format!("unknown pixel layout {text:?}"))?;
            }
            "--duration" => {
                let text = value("--duration")?;
                let seconds: f64 = text
                    .parse()
                    .map_err(|_| format!("invalid duration {text:?}"))?;
                duration = Some(Duration::from_secs_f64(seconds));
            }
            "--help" | "-h" => {
                println!(
                    "usage: hizukuri-test-client --socket PATH [--title T] [--app-id ID] \
                     [--size WxH] [--color RRGGBBAA] [--layout argb8888|xrgb8888|abgr8888|xbgr8888] \
                     [--duration SECONDS]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }

    let socket =
        socket.ok_or("--socket PATH is required (the client never uses WAYLAND_DISPLAY)")?;
    let mut client = TestClient::connect_path(&socket, options).map_err(|e| e.to_string())?;
    let deadline = duration.map(|d| Instant::now() + d);
    let mut printed = 0;
    let stdout = std::io::stdout();
    loop {
        client
            .pump(Some(Duration::from_millis(100)))
            .map_err(|e| e.to_string())?;
        let events = client.events();
        if printed < events.len() {
            let mut out = stdout.lock();
            for event in &events[printed..] {
                let line = serde_json::to_string(event).map_err(|e| e.to_string())?;
                writeln!(out, "{line}").map_err(|e| e.to_string())?;
            }
            out.flush().map_err(|e| e.to_string())?;
            printed = events.len();
        }
        if client.is_closed() || deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(());
        }
    }
}
