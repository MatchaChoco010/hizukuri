//! The animation example (`projects/animation`): windows that move, open
//! and close over time, computed by the script from the time of each frame.
//!
//! The sessions run on a manual clock (`--clock manual`): it stands still
//! until the test moves it (`advance_clock`), so every position below is
//! the one for an exact time. Frames are still granted in real time; while
//! the clock stands still they all show the same moment.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{Device, Session, options, window};
use proto_compositor::control::Request;
use proto_compositor::image::EncodedImage;
use proto_test_client::{ClientEvent, TestClient};
use serde_json::{Value, json};

device_tests!(
    a_swap_moves_the_windows_over_time,
    a_second_swap_carries_on_from_where_the_windows_are,
    a_closed_window_fades_out_and_is_let_go,
    input_lands_where_a_moving_window_is_drawn,
);

const RED: [u8; 4] = [210, 60, 50, 255];
const GREEN: [u8; 4] = [60, 190, 70, 255];
const BACKGROUND: [u8; 4] = [0x20, 0x20, 0x20, 255];

const KEY_Q: u32 = 16;
const KEY_H: u32 = 35;
const KEY_L: u32 = 38;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTMETA: u32 = 125;

// The settings of the example (`main.luau`).
const MOVE_SECONDS: f64 = 0.30;
const CLOSE_SECONDS: f64 = 0.20;

// 640x400 with gaps of 8: two tiles of 308x384, at x 8 and 324.
const LEFT: f64 = 8.0;
const RIGHT: f64 = 324.0;

fn project() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("projects/animation")
}

fn start(name: &str, device: Device) -> Session {
    let project = project();
    let mut session = Session::start(
        name,
        device,
        &[
            "--output-size",
            "640x400",
            "--clock",
            "manual",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    assert_eq!(session.state()["clock"]["kind"], "manual");
    session
}

fn advance(session: &mut Session, seconds: f64) {
    session.send(Request::AdvanceClock {
        milliseconds: seconds * 1000.0,
    });
}

fn key(session: &mut Session, keycode: u32, pressed: bool) {
    session.send(Request::Key { keycode, pressed });
}

/// Presses and releases `keycode` with the logo key (and shift) held.
fn logo_key(session: &mut Session, keycode: u32, shift: bool) {
    key(session, KEY_LEFTMETA, true);
    if shift {
        key(session, KEY_LEFTSHIFT, true);
    }
    key(session, keycode, true);
    key(session, keycode, false);
    if shift {
        key(session, KEY_LEFTSHIFT, false);
    }
    key(session, KEY_LEFTMETA, false);
}

fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

/// Where the script draws each window (`animation_state`), by id.
fn drawn(session: &mut Session, id: u64) -> Value {
    let state = session.script("animation_state", json!([]));
    state
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == id)
        .cloned()
        .unwrap_or(Value::Null)
}

#[track_caller]
fn assert_near(actual: &Value, expected: f64, what: &str) {
    let actual = actual
        .as_f64()
        .unwrap_or_else(|| panic!("{what}: not a number"));
    assert!(
        (actual - expected).abs() < 1e-3,
        "{what}: {actual}, expected {expected}"
    );
}

#[track_caller]
fn assert_rgb(image: &EncodedImage, x: u32, y: u32, expected: [u8; 4], what: &str) {
    let actual = image.pixel(x, y);
    let worst = (0..3)
        .map(|i| (actual[i] as i32 - expected[i] as i32).abs())
        .max()
        .unwrap();
    assert!(
        worst <= 2,
        "{what}: pixel ({x}, {y}) is {actual:?}, expected {expected:?}"
    );
}

/// The id of the window with a title, once it is placed at `x` with the
/// size of a tile.
fn tiled(session: &mut Session, clients: &mut [&mut TestClient], title: &str, x: i64) -> u64 {
    let state = session.wait(clients, &format!("{title} tiled at {x}"), |s| {
        window(s, title)
            .is_some_and(|w| w["position"] == json!([x, 8]) && w["size"] == json!([308, 384]))
    });
    window(&state, title).unwrap()["id"].as_u64().unwrap()
}

/// Two windows side by side, `a` (red) left and `b` (green, focused)
/// right, with the open animations over.
fn two_windows(session: &mut Session) -> (TestClient, TestClient, u64, u64) {
    let mut a = session.client(options("a", 100, 60, RED));
    session.wait(&mut [&mut a], "a mapped", |s| {
        window(s, "a").is_some_and(|w| w["surfaces"][0]["buffer_size"].is_array())
    });
    let mut b = session.client(options("b", 100, 60, GREEN));
    let ida = tiled(session, &mut [&mut a, &mut b], "a", 8);
    let idb = tiled(session, &mut [&mut a, &mut b], "b", 324);
    // A window that opens starts transparent and small.
    let opening = drawn(session, idb);
    assert_near(&opening["opacity"], 0.0, "b when it opens");
    assert_near(&opening["scale"], 0.8, "b when it opens");
    advance(session, 1.0);
    settle(session, &mut [&mut a, &mut b]);
    let shown = drawn(session, idb);
    assert_near(&shown["opacity"], 1.0, "b once open");
    assert_near(&shown["scale"], 1.0, "b once open");
    assert_near(&drawn(session, ida)["x"], LEFT, "a");
    (a, b, ida, idb)
}

/// Waits until nothing moves and no frame is owed.
fn settle(session: &mut Session, clients: &mut [&mut TestClient]) -> Value {
    // A frame at the current time has to be drawn first.
    session.screenshot_png("settle");
    let state = session.script("animation_state", json!([]));
    assert!(
        state
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["moving"] == false),
        "still moving: {state:#}"
    );
    // A call into the script through the control interface asks for a
    // frame too (`script_call`); that one has to be drawn.
    session.wait(clients, "no frame owed", |s| s["redraw_pending"] == false);
    state
}

/// Once the animations are over, no more frames are drawn: the script asks
/// for none.
fn assert_idle(session: &mut Session, clients: &mut [&mut TestClient]) {
    settle(session, clients);
    let frames = |session: &mut Session| session.state()["script"]["frames"].as_u64().unwrap();
    let before = frames(session);
    for _ in 0..10 {
        for client in clients.iter_mut() {
            client.pump(Some(Duration::from_millis(20))).unwrap();
        }
    }
    let state = session.state();
    assert_eq!(
        state["script"]["frames"].as_u64().unwrap(),
        before,
        "frames were drawn with nothing moving"
    );
    assert_eq!(state["redraw_pending"], false);
}

/// Logo+shift+H swaps the focused window (b) with its left neighbour. The
/// windows move from their old place to the new one: at the start where
/// they were, half way between, at the end where the tiling put them. Then
/// nothing more is drawn.
fn a_swap_moves_the_windows_over_time(device: Device) {
    let mut session = start("animation-swap", device);
    let (mut a, mut b, ida, idb) = two_windows(&mut session);

    logo_key(&mut session, KEY_H, true);
    // The tiling decided at once: the places in the core's table changed.
    let state = session.state();
    assert_eq!(window(&state, "a").unwrap()["position"], json!([324, 8]));
    assert_eq!(window(&state, "b").unwrap()["position"], json!([8, 8]));

    // The frame at the time of the key: nothing has moved yet.
    let image = session.screenshot_png("start");
    assert_rgb(&image, 100, 200, RED, "a where it was");
    assert_rgb(&image, 500, 200, GREEN, "b where it was");
    assert_near(&drawn(&mut session, ida)["x"], LEFT, "a at the start");
    assert_eq!(drawn(&mut session, ida)["moving"], true);

    // Half the time: with ease-out, most of the way.
    advance(&mut session, MOVE_SECONDS / 2.0);
    let image = session.screenshot_png("middle");
    let k = ease_out_cubic(0.5);
    let ax = LEFT + (RIGHT - LEFT) * k;
    let bx = RIGHT + (LEFT - RIGHT) * k;
    assert_near(&drawn(&mut session, ida)["x"], ax, "a half way");
    assert_near(&drawn(&mut session, idb)["x"], bx, "b half way");
    // a (284.5..592.5) is on top of b (47.5..355.5).
    assert_rgb(
        &image,
        20,
        200,
        BACKGROUND,
        "left of b, which has left the edge",
    );
    assert_rgb(&image, 200, 200, GREEN, "b on its way left");
    assert_rgb(&image, 450, 200, RED, "a on its way right");
    assert_rgb(
        &image,
        610,
        200,
        BACKGROUND,
        "right of a, which is not there yet",
    );

    // Past the end: the new places, and nothing more to draw.
    advance(&mut session, MOVE_SECONDS);
    let image = session.screenshot_png("end");
    assert_rgb(&image, 100, 200, GREEN, "b on the left");
    assert_rgb(&image, 500, 200, RED, "a on the right");
    assert_rgb(&image, 320, 200, BACKGROUND, "the gap");
    assert_near(&drawn(&mut session, ida)["x"], RIGHT, "a at the end");
    assert_near(&drawn(&mut session, idb)["x"], LEFT, "b at the end");
    assert_idle(&mut session, &mut [&mut a, &mut b]);
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// A swap in the middle of a swap: the windows turn back from where they
/// are drawn, without jumping to either end.
fn a_second_swap_carries_on_from_where_the_windows_are(device: Device) {
    let mut session = start("animation-reverse", device);
    let (mut a, mut b, ida, _) = two_windows(&mut session);

    logo_key(&mut session, KEY_H, true);
    session.screenshot_png("start");
    advance(&mut session, MOVE_SECONDS / 2.0);
    let k = ease_out_cubic(0.5);
    let middle = LEFT + (RIGHT - LEFT) * k;
    session.screenshot_png("middle");
    assert_near(&drawn(&mut session, ida)["x"], middle, "a half way");

    // b, still focused, goes back to the right.
    logo_key(&mut session, KEY_L, true);
    let image = session.screenshot_png("turned");
    assert_near(
        &drawn(&mut session, ida)["x"],
        middle,
        "a right after the turn",
    );
    // Where a was drawn, not where either swap would have put it.
    assert_rgb(&image, 450, 200, RED, "a where it was drawn");
    assert_rgb(&image, 610, 200, BACKGROUND, "a not at the right end");

    // From there to its old place, over a whole move.
    advance(&mut session, MOVE_SECONDS / 2.0);
    session.screenshot_png("back-half");
    assert_near(
        &drawn(&mut session, ida)["x"],
        middle + (LEFT - middle) * k,
        "a half way back",
    );
    advance(&mut session, MOVE_SECONDS);
    let image = session.screenshot_png("back");
    assert_rgb(&image, 100, 200, RED, "a back on the left");
    assert_rgb(&image, 500, 200, GREEN, "b back on the right");
    assert_idle(&mut session, &mut [&mut a, &mut b]);
    session.assert_script_healthy();
    session.quit();
}

/// Logo+Q closes b. Its client destroys the window at once, but what it
/// showed is still drawn, fading out, until the close animation ends; then
/// the script lets the core drop it.
fn a_closed_window_fades_out_and_is_let_go(device: Device) {
    let mut session = start("animation-close", device);
    let (mut a, mut b, ida, idb) = two_windows(&mut session);

    logo_key(&mut session, KEY_Q, false);
    b.wait_for(common::TIMEOUT, |e| *e == ClientEvent::CloseRequested)
        .unwrap()
        .expect("b is asked to close");
    let state = session.wait(&mut [&mut a, &mut b], "b gone", |s| {
        window(s, "b").is_none()
    });
    let closed = state["script"]["closed_windows"].as_array().unwrap();
    assert_eq!(closed.len(), 1, "{state:#}");
    assert_eq!(closed[0]["id"], idb);
    // 308x384 pixels at 4 bytes each.
    assert_eq!(closed[0]["bytes"], 308 * 384 * 4);

    // a is told to take the whole output, and draws at that size; its
    // rectangle then grows from where it is drawn (the clock stood still,
    // so that starts at the time of the close).
    session.wait(&mut [&mut a], "a at its new size", |s| {
        window(s, "a").is_some_and(|w| w["size"] == json!([624, 384]))
    });

    // Right after closing, b is drawn as it was, a not grown yet.
    let image = session.screenshot_png("closed");
    assert_rgb(&image, 500, 200, GREEN, "b's last contents");
    assert_near(&drawn(&mut session, ida)["width"], 308.0, "a not grown yet");
    let fading = drawn(&mut session, idb);
    assert_eq!(fading["closing"], true);
    assert_near(&fading["opacity"], 1.0, "b right after closing");

    // Half way: half transparent (ease-in-out is 0.5 at 0.5), over a that
    // has grown beneath it.
    advance(&mut session, CLOSE_SECONDS / 2.0);
    let image = session.screenshot_png("fading");
    let fading = drawn(&mut session, idb);
    assert_near(&fading["opacity"], 0.5, "b half way");
    assert_near(&fading["scale"], 0.9, "b half way");
    let grown = 308.0 + (624.0 - 308.0) * ease_out_cubic(CLOSE_SECONDS / 2.0 / MOVE_SECONDS);
    assert_near(&drawn(&mut session, ida)["width"], grown, "a growing");
    assert!(8.0 + grown > 478.0);
    // Blended in linear light: half of b's green over a's red.
    let linear = |c: u8| (c as f64 / 255.0).powf(2.2);
    let mixed: Vec<u8> = (0..3)
        .map(|i| {
            let v = 0.5 * linear(GREEN[i]) + 0.5 * linear(RED[i]);
            (v.powf(1.0 / 2.2) * 255.0).round() as u8
        })
        .collect();
    let pixel = image.pixel(478, 200);
    assert!(
        (0..3).all(|i| (pixel[i] as i32 - mixed[i] as i32).abs() <= 3),
        "b half transparent over a at its centre: {pixel:?}, expected {mixed:?}"
    );

    // Over: not drawn, let go, and a fills the output.
    advance(&mut session, 1.0);
    let state = settle(&mut session, &mut [&mut a]);
    assert!(state.as_array().unwrap().iter().all(|w| w["id"] != idb));
    let image = session.screenshot_png("alone");
    assert_rgb(&image, 500, 200, RED, "a where b was");
    assert_near(&drawn(&mut session, ida)["width"], 624.0, "a's width");
    assert_eq!(
        session.state()["script"]["closed_windows"],
        json!([]),
        "the closed window is let go"
    );
    assert_idle(&mut session, &mut [&mut a]);
    session.assert_script_healthy();
    session.quit();
}

/// Half way through a swap, the pointer finds each window where it is
/// drawn, and the client sees the point in its own coordinates.
fn input_lands_where_a_moving_window_is_drawn(device: Device) {
    let mut session = start("animation-input", device);
    let (mut a, mut b, _, _) = two_windows(&mut session);

    logo_key(&mut session, KEY_H, true);
    session.screenshot_png("start");
    advance(&mut session, MOVE_SECONDS / 2.0);
    session.screenshot_png("middle");
    let k = ease_out_cubic(0.5);
    let ax = LEFT + (RIGHT - LEFT) * k;
    let bx = RIGHT + (LEFT - RIGHT) * k;

    // On a, which is on top where the two overlap.
    let at = point_at(&mut session, &mut a, (450.0, 200.0));
    assert_local(at, (450.0 - ax, 192.0), "a");
    let at = point_at(&mut session, &mut a, (300.0, 100.0));
    assert_local(at, (300.0 - ax, 92.0), "a over b");
    // On b, left of a.
    let at = point_at(&mut session, &mut b, (100.0, 50.0));
    assert_local(at, (100.0 - bx, 42.0), "b");
    session.assert_script_healthy();
    session.quit();
}

/// Moves the pointer and returns where the client was last told it is.
fn point_at(session: &mut Session, client: &mut TestClient, (x, y): (f64, f64)) -> (f64, f64) {
    client.clear_events();
    session.send(Request::PointerMotion { x, y });
    client
        .wait_for(common::TIMEOUT, |e| *e == ClientEvent::PointerFrame)
        .unwrap()
        .expect("a pointer frame");
    client
        .events()
        .iter()
        .rev()
        .find_map(|e| match e {
            ClientEvent::PointerEnter { x, y } | ClientEvent::PointerMotion { x, y } => {
                Some((*x, *y))
            }
            _ => None,
        })
        .expect("the pointer reached the client")
}

#[track_caller]
fn assert_local(actual: (f64, f64), expected: (f64, f64), what: &str) {
    // Positions travel as 24.8 fixed point.
    assert!(
        (actual.0 - expected.0).abs() < 0.01 && (actual.1 - expected.1).abs() < 0.01,
        "{what}: at {actual:?}, expected {expected:?}"
    );
}

/// What the animations cost the script per frame, by the number of windows:
/// the work of `animate.frame` alone (`animation_bench`, no drawing), and
/// the whole `frame` callback (which also records the drawing). Not a test:
/// run by hand, optimized, on a quiet machine:
///
/// ```sh
/// cargo test --release -p proto-compositor --test animation -- --ignored --nocapture
/// ```
///
/// The table is written to `target/test-output/animation-measure.md`.
#[test]
#[ignore = "a measurement, run by hand (see the documentation)"]
fn animation_cost() {
    use std::fmt::Write as _;
    use std::time::Instant;

    let device = Device::new(common::Gpu::Lavapipe);
    let mut table = String::from(
        "| windows | `animate.frame` alone (µs) | per window (µs) | whole `frame` (µs) |\n| ---: | ---: | ---: | ---: |\n",
    );
    for count in [1usize, 4, 16, 32, 64] {
        let project = project();
        let mut session = Session::start(
            &format!("animation-measure-{count}"),
            device,
            &[
                "--output-size",
                "1920x1080",
                "--clock",
                "manual",
                "--script",
                project.to_str().unwrap(),
            ],
        );
        let mut clients: Vec<TestClient> = Vec::new();
        for index in 0..count {
            let title = format!("w{index}");
            let mut client = session.client(options(&title, 100, 100, RED));
            session.wait(&mut [&mut client], "mapped", |s| {
                window(s, &title).is_some_and(|w| w["surfaces"][0]["buffer_size"].is_array())
            });
            clients.push(client);
        }
        // The clock stands still: every window is still opening, so every
        // frame computes every window, and asks for the next frame.
        let moving = session.script("animation_state", json!([]));
        assert!(
            moving
                .as_array()
                .unwrap()
                .iter()
                .all(|w| w["moving"] == true)
        );

        let call = |session: &mut Session, rounds: u32| {
            let started = Instant::now();
            session.script("animation_bench", json!([rounds]));
            started.elapsed().as_secs_f64() * 1e6
        };
        let rounds = 2000;
        let empty = (0..5)
            .map(|_| call(&mut session, 0))
            .fold(f64::MAX, f64::min);
        let full = (0..5)
            .map(|_| call(&mut session, rounds))
            .fold(f64::MAX, f64::min);
        let alone = (full - empty) / rounds as f64;

        let timing = |session: &mut Session| {
            let t = session.script_state()["timing"]["frame"].take();
            let calls = t["calls"].as_f64().unwrap();
            (calls, t["mean_us"].as_f64().unwrap() * calls)
        };
        let (calls0, total0) = timing(&mut session);
        for _ in 0..50 {
            for client in clients.iter_mut() {
                client.pump(Some(Duration::from_millis(0))).unwrap();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let (calls1, total1) = timing(&mut session);
        let whole = (total1 - total0) / (calls1 - calls0).max(1.0);
        writeln!(
            table,
            "| {count} | {alone:.1} | {:.2} | {whole:.1} |",
            alone / count as f64
        )
        .unwrap();
        drop(clients);
        session.quit();
    }
    println!("{table}");
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .unwrap()
        .join("test-output")
        .join("animation-measure.md");
    std::fs::write(&path, &table).unwrap();
    println!("written to {}", path.display());
}
