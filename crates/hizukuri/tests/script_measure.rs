//! Measurements of the script layer: the time of a frame with the Rust
//! renderer and with the script renderer, the cost of the script's hit
//! test, and how long a reload takes.
//!
//! Not a test: it asserts nothing about the numbers and is ignored by
//! default. Run it on a quiet machine, optimized:
//!
//! ```sh
//! cargo test --release -p hizukuri --test script_measure -- --ignored --nocapture
//! ```
//!
//! The table is printed and written to
//! `target/test-output/script-measure.md`.

mod common;

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use common::{Device, Drawing, Gpu, Session, options};
use hizukuri::control::Request;
use hizukuri_test_client::TestClient;
use serde_json::Value;

const FRAMES: usize = 300;

struct Row {
    device: String,
    windows: usize,
    draw_median: f64,
    draw_p99: f64,
    total_median: f64,
    script_frame_us: Option<f64>,
    hit_test_us: Option<f64>,
    reload_ms: Option<f64>,
    first_frame_ms: f64,
}

/// `wait`: the script renderer waits for the GPU after every frame, which
/// the Rust renderer always does.
fn measure(device: Device, windows: usize, wait: bool) -> Row {
    let started = Instant::now();
    let mut args = vec!["--output-size", "1920x1080"];
    if wait {
        args.push("--script-wait-gpu");
    }
    let mut session = Session::start(&format!("measure-{windows}"), device, &args);
    let mut clients: Vec<TestClient> = Vec::new();
    for index in 0..windows {
        let title = format!("w{index}");
        let color = [
            60 + (index * 37 % 180) as u8,
            60 + (index * 91 % 180) as u8,
            60 + (index * 53 % 180) as u8,
            255,
        ];
        let (client, id) = session.mapped_client(options(&title, 300, 200, color));
        session.send(Request::MoveWindow {
            window: id,
            x: 20 + (index % 6) as i32 * 310,
            y: 20 + (index / 6 % 5) as i32 * 205,
        });
        clients.push(client);
    }
    // The first frame includes compiling shaders and pipelines.
    session.screenshot("first");
    let first_frame_ms = started.elapsed().as_secs_f64() * 1e3;

    // Frames: one client redraws, every time in another color.
    for frame in 0..FRAMES {
        let level = 40 + (frame % 200) as u8;
        session.recolor(&mut clients[0], "w0", [level, 255 - level, 90, 255]);
    }
    let state = session.state();
    let times = &state["frame_time_ms"];
    let number = |v: &Value| v.as_f64().unwrap_or(f64::NAN);

    // Hit tests: the pointer crosses the windows.
    let (mut hit_test_us, mut script_frame_us, mut reload_ms) = (None, None, None);
    if device.is_script() {
        for step in 0..400 {
            session.send(Request::PointerMotion {
                x: 10.0 + step as f64 * 4.7,
                y: 10.0 + (step % 100) as f64 * 10.3,
            });
        }
        let timing = session.script_state()["timing"].take();
        hit_test_us = timing["hit_test"]["mean_us"].as_f64();
        script_frame_us = timing["frame"]["mean_us"].as_f64();
        let started = Instant::now();
        session.send(Request::ScriptReload { path: None });
        session.screenshot("reloaded");
        reload_ms = Some(started.elapsed().as_secs_f64() * 1e3);
        session.assert_script_healthy();
    }
    let row = Row {
        device: match (device.is_script(), wait) {
            (false, _) => format!("{}, Rust renderer", device.gpu_name()),
            (true, true) => format!("{}, script", device.gpu_name()),
            (true, false) => format!("{}, script, not waiting for the GPU", device.gpu_name()),
        },
        windows,
        draw_median: number(&times["draw"]["median"]),
        draw_p99: number(&times["draw"]["p99"]),
        total_median: number(&times["total"]["median"]),
        script_frame_us,
        hit_test_us,
        reload_ms,
        first_frame_ms,
    };
    session.quit();
    // Let the device settle before the next session.
    std::thread::sleep(Duration::from_millis(100));
    row
}

#[test]
#[ignore = "a measurement, run by hand (see the module documentation)"]
fn frame_times() {
    let mut rows = Vec::new();
    for gpu in [Gpu::Hardware, Gpu::Lavapipe] {
        for windows in [1, 8, 30] {
            for (drawing, wait) in [
                (Drawing::Rust, false),
                (Drawing::Script, true),
                (Drawing::Script, false),
            ] {
                let device = Device::new(gpu, drawing);
                if device.enabled() {
                    rows.push(measure(device, windows, wait));
                }
            }
        }
    }
    let mut table = String::new();
    let optional = |v: Option<f64>, digits: usize| match v {
        Some(v) => format!("{v:.digits$}"),
        None => "-".to_string(),
    };
    writeln!(
        table,
        "| device | windows | draw median (ms) | draw p99 (ms) | draw+present median (ms) | script `frame` (µs) | script `hit_test` (µs) | reload + first frame (ms) | start to first frame (ms) |"
    )
    .unwrap();
    writeln!(
        table,
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    )
    .unwrap();
    for row in &rows {
        writeln!(
            table,
            "| {} | {} | {:.3} | {:.3} | {:.3} | {} | {} | {} | {:.0} |",
            row.device,
            row.windows,
            row.draw_median,
            row.draw_p99,
            row.total_median,
            optional(row.script_frame_us, 1),
            optional(row.hit_test_us, 1),
            optional(row.reload_ms, 1),
            row.first_frame_ms,
        )
        .unwrap();
    }
    println!("{table}");
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .unwrap()
        .join("test-output")
        .join("script-measure.md");
    std::fs::write(&path, &table).unwrap();
    println!("written to {}", path.display());
}
