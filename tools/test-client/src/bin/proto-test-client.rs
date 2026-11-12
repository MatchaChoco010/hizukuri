//! Command line front end of the test client.
//!
//! ```text
//! proto-test-client --socket PATH [--title T] [--app-id ID] [--size WxH]
//!                   [--color RRGGBBAA] [--layout argb8888] [--duration SECONDS]
//!                   [--relative-pointer] [--lock-pointer | --confine-pointer]
//!                   [--text-input X,Y,W,H] [--input-method echo] [--no-window]
//!                   [--color-manager VERSION] [--image-description SPEC]
//!                   [--color-info]
//! ```
//!
//! Shows a solid window and prints every event it receives as one JSON
//! object per line on stdout. `--text-input` gives the window a text field
//! with that cursor rectangle; `--input-method echo` makes the client the
//! input method of the seat (it composes letters and commits them in
//! capitals on Return), usually with `--no-window`. `--socket` is required: the client never
//! falls back to `WAYLAND_DISPLAY`, so it cannot open a window on the
//! desktop session by accident. `--color` gives the stored channel values
//! (premultiplied, as `wl_shm` expects).
//!
//! `--image-description` says what the values mean, through
//! color-management-v1: `windows-scrgb`, `windows-bt2100`, or
//! `tf=NAME,primaries=NAME[,lum=MIN:MAX:REF]` with the protocol's names
//! (`tf=st2084_pq,primaries=bt2020`; `tf=power:2.4` for a power function;
//! luminances in cd/m²). `--color-info` reads the image descriptions of the
//! outputs and the one preferred for the window.
//!
//! ```text
//! proto-test-client --as-xwayland-satellite :N -listenfd FD [-listenfd FD]...
//! ```
//!
//! Stands in for xwayland-satellite, for testing how a compositor starts
//! it, without Xwayland: it takes the command line a compositor gives
//! xwayland-satellite, reports what it was given (the display, the sockets
//! behind the descriptors, `DISPLAY` and `WAYLAND_DISPLAY`), connects to
//! the compositor named by `XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY` (which
//! is how the compositor that started it hands it the socket) and shows
//! one window. It accepts connections on the X11 sockets: each one is
//! greeted with a line, and a line `exit CODE` sent back ends the process
//! with that status.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use proto_test_client::{
    ClientEvent, ClientOptions, ColorTarget, ConstraintRequest, CreatorStep, InputMethodMode,
    PixelLayout, TestClient,
};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("proto-test-client: {message}");
            ExitCode::FAILURE
        }
    }
}

/// How the window's image description is made.
enum Description {
    Windows { bt2100: bool },
    Parametric(Vec<CreatorStep>),
}

const TRANSFER_FUNCTIONS: [(&str, u32); 8] = [
    ("bt1886", 1),
    ("gamma22", 2),
    ("gamma28", 3),
    ("ext_linear", 5),
    ("srgb", 9),
    ("st2084_pq", 11),
    ("hlg", 13),
    ("compound_power_2_4", 14),
];

const PRIMARIES: [(&str, u32); 10] = [
    ("srgb", 1),
    ("pal_m", 2),
    ("pal", 3),
    ("ntsc", 4),
    ("generic_film", 5),
    ("bt2020", 6),
    ("cie1931_xyz", 7),
    ("dci_p3", 8),
    ("display_p3", 9),
    ("adobe_rgb", 10),
];

fn parse_description(text: &str) -> Result<Description, String> {
    match text {
        "windows-scrgb" => return Ok(Description::Windows { bt2100: false }),
        "windows-bt2100" => return Ok(Description::Windows { bt2100: true }),
        _ => {}
    }
    let mut steps = Vec::new();
    for part in text.split(',') {
        let (key, value) = part
            .split_once('=')
            .ok_or(format!("invalid image description part {part:?}"))?;
        let step = match key {
            "tf" => match value.strip_prefix("power:") {
                Some(exponent) => {
                    let exponent: f64 = exponent
                        .parse()
                        .map_err(|_| format!("invalid exponent {exponent:?}"))?;
                    CreatorStep::TfPower((exponent * 10000.0).round() as u32)
                }
                None => CreatorStep::TfNamed(
                    TRANSFER_FUNCTIONS
                        .iter()
                        .find(|(n, _)| *n == value)
                        .ok_or(format!("unknown transfer function {value:?}"))?
                        .1,
                ),
            },
            "primaries" => CreatorStep::PrimariesNamed(
                PRIMARIES
                    .iter()
                    .find(|(n, _)| *n == value)
                    .ok_or(format!("unknown primaries {value:?}"))?
                    .1,
            ),
            "lum" => {
                let values: Vec<f64> = value
                    .split(':')
                    .map(|v| v.parse().map_err(|_| format!("invalid luminance {v:?}")))
                    .collect::<Result<_, _>>()?;
                let [min, max, reference] = values[..] else {
                    return Err(format!("lum wants MIN:MAX:REF, not {value:?}"));
                };
                CreatorStep::Luminances(
                    (min * 10000.0).round() as u32,
                    max.round() as u32,
                    reference.round() as u32,
                )
            }
            other => return Err(format!("unknown image description key {other:?}")),
        };
        steps.push(step);
    }
    Ok(Description::Parametric(steps))
}

/// Where the color management of the window stands.
#[derive(Default)]
struct ColorProgress {
    /// The index of the window's image description, once made.
    made: Option<usize>,
    /// Whether it was set on the window.
    set: bool,
    /// Whether the feedback and the outputs' objects were made.
    asked: bool,
    /// Output names asked about.
    outputs: Vec<String>,
    /// Image descriptions whose information is wanted, and whether it was
    /// asked for.
    informed: Vec<(usize, bool)>,
}

fn run() -> Result<(), String> {
    if std::env::args().nth(1).as_deref() == Some("--as-xwayland-satellite") {
        return stand_in_for_satellite(std::env::args().skip(2).collect());
    }
    let mut options = ClientOptions::default();
    let mut socket: Option<PathBuf> = None;
    let mut duration: Option<Duration> = None;
    // What a game asks for: relative motion, and the pointer held in place
    // (or inside the window). Asked for once the window and the pointer
    // exist.
    let mut relative = false;
    let mut constraint: Option<ConstraintRequest> = None;
    let mut description: Option<Description> = None;
    let mut color_info = false;

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
            "--text-input" => {
                let text = value("--text-input")?;
                let parts: Vec<i32> = text
                    .split(',')
                    .map(|p| p.parse::<i32>())
                    .collect::<Result<_, _>>()
                    .map_err(|_| format!("invalid rectangle {text:?}, expected X,Y,W,H"))?;
                let [x, y, w, h] = parts[..] else {
                    return Err(format!("invalid rectangle {text:?}, expected X,Y,W,H"));
                };
                options.text_input_enable_on_enter = Some((x, y, w, h));
            }
            "--input-method" => match value("--input-method")?.as_str() {
                "echo" => options.input_method = Some(InputMethodMode::Echo),
                other => return Err(format!("unknown input method mode {other:?}")),
            },
            "--no-window" => options.window = false,
            "--duration" => {
                let text = value("--duration")?;
                let seconds: f64 = text
                    .parse()
                    .map_err(|_| format!("invalid duration {text:?}"))?;
                duration = Some(Duration::from_secs_f64(seconds));
            }
            "--relative-pointer" => relative = true,
            "--lock-pointer" => constraint = Some(ConstraintRequest::Lock),
            "--confine-pointer" => constraint = Some(ConstraintRequest::Confine),
            "--color-manager" => {
                let text = value("--color-manager")?;
                options.color_manager_version = Some(
                    text.parse()
                        .map_err(|_| format!("invalid version {text:?}"))?,
                );
            }
            "--image-description" => {
                description = Some(parse_description(&value("--image-description")?)?);
            }
            "--color-info" => color_info = true,
            "--help" | "-h" => {
                println!(
                    "usage: proto-test-client --socket PATH [--title T] [--app-id ID] \
                     [--size WxH] [--color RRGGBBAA] [--layout argb8888|xrgb8888|abgr8888|xbgr8888] \
                     [--duration SECONDS] [--relative-pointer] [--lock-pointer | --confine-pointer] \
                     [--text-input X,Y,W,H] [--input-method echo] \
                     [--no-window] \
                     [--color-manager VERSION] [--image-description SPEC] [--color-info]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if (description.is_some() || color_info) && options.color_manager_version.is_none() {
        // The newest version this client knows.
        options.color_manager_version = Some(3);
    }

    let socket =
        socket.ok_or("--socket PATH is required (the client never uses WAYLAND_DISPLAY)")?;
    let mut client = TestClient::connect_path(&socket, options).map_err(|e| e.to_string())?;
    let deadline = duration.map(|d| Instant::now() + d);
    let mut printed = 0;
    let mut printed_ime = 0;
    let mut color = ColorProgress::default();
    let stdout = std::io::stdout();
    loop {
        client
            .pump(Some(Duration::from_millis(100)))
            .map_err(|e| e.to_string())?;
        // Not there yet until the globals, the pointer and the window are.
        if relative && client.request_relative_pointer().is_ok() {
            relative = false;
        }
        if let Some(request) = constraint
            && client.is_drawn()
            && client.constrain_pointer(request, None, true).is_ok()
        {
            constraint = None;
        }
        advance_color(&mut client, &mut color, description.as_ref(), color_info)?;
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
        let ime_events = client.ime_events();
        if printed_ime < ime_events.len() {
            let mut out = stdout.lock();
            for event in &ime_events[printed_ime..] {
                let line = serde_json::to_string(event).map_err(|e| e.to_string())?;
                writeln!(out, "{line}").map_err(|e| e.to_string())?;
            }
            out.flush().map_err(|e| e.to_string())?;
            printed_ime = ime_events.len();
        }
        if client.is_closed() || deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(());
        }
    }
}

/// Takes the next steps of color management that are due.
fn advance_color(
    client: &mut TestClient,
    color: &mut ColorProgress,
    description: Option<&Description>,
    info: bool,
) -> Result<(), String> {
    let error = |e: proto_test_client::ClientError| e.to_string();
    if !client.is_drawn() || client.color_manager_version().is_none() {
        return Ok(());
    }
    let advertised = client
        .events()
        .iter()
        .any(|e| matches!(e, ClientEvent::ColorManagerSupport { .. }));
    if !advertised {
        return Ok(());
    }
    if let Some(wanted) = description {
        if color.made.is_none() {
            color.made = Some(match wanted {
                Description::Windows { bt2100 } => {
                    client.create_windows_description(*bt2100).map_err(error)?
                }
                Description::Parametric(steps) => {
                    client.create_image_description(steps).map_err(error)?
                }
            });
        }
        if let Some(index) = color.made
            && !color.set
            && client.events().iter().any(
                |e| matches!(e, ClientEvent::ImageDescriptionReady { index: i, .. } if *i == index),
            )
        {
            client
                .create_color_surface(ColorTarget::Window)
                .map_err(error)?;
            client.set_image_description(index, 0).map_err(error)?;
            // Applied with the next buffer.
            client.redraw_with(|_| {});
            color.set = true;
        }
    }
    if info {
        if !color.asked {
            client.create_color_feedback().map_err(error)?;
            color.asked = true;
        }
        let names: Vec<String> = client
            .events()
            .iter()
            .filter_map(|e| match e {
                ClientEvent::Output { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        for name in names {
            if !color.outputs.contains(&name) {
                let output = client.create_color_output(&name).map_err(error)?;
                let index = client.get_output_image_description(output).map_err(error)?;
                color.informed.push((index, false));
                color.outputs.push(name);
            }
        }
        let preferred = client
            .events()
            .iter()
            .filter(|e| matches!(e, ClientEvent::PreferredChanged { .. }))
            .count();
        let asked_preferred = color.informed.len() - color.outputs.len();
        for _ in asked_preferred..preferred {
            let index = client.get_preferred(false).map_err(error)?;
            color.informed.push((index, false));
        }
        for (index, asked) in &mut color.informed {
            if !*asked
                && client.events().iter().any(|e| {
                    matches!(e, ClientEvent::ImageDescriptionReady { index: i, .. } if i == index)
                })
            {
                client.get_image_information(*index).map_err(error)?;
                *asked = true;
            }
        }
    }
    Ok(())
}

/// One JSON object per line on stdout.
fn report(value: serde_json::Value) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{value}").map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}

/// See the module documentation.
fn stand_in_for_satellite(args: Vec<String>) -> Result<(), String> {
    use std::io::{BufRead, BufReader};
    use std::os::fd::FromRawFd;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    let display = args
        .first()
        .filter(|a| a.starts_with(':'))
        .ok_or("the first argument must be the display (:N)")?
        .clone();
    let mut listeners = Vec::new();
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        if arg != "-listenfd" {
            return Err(format!("unexpected argument {arg:?}"));
        }
        let fd: i32 = rest
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or("-listenfd needs a descriptor")?;
        // SAFETY: the descriptor was handed over on the command line, to
        // this process alone; nothing else here owns it.
        let listener = unsafe { UnixListener::from_raw_fd(fd) };
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("descriptor {fd}: {e}"))?;
        let address = listener
            .local_addr()
            .map_err(|e| format!("descriptor {fd} is not a listening socket: {e}"))?;
        let path = address.as_pathname().map(|p| p.display().to_string());
        let abstract_name = address
            .as_abstract_name()
            .map(|n| String::from_utf8_lossy(n).into_owned());
        listeners.push((fd, path, abstract_name, listener));
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
    let wayland = std::env::var_os("WAYLAND_DISPLAY").ok_or("WAYLAND_DISPLAY is not set")?;
    // The signals blocked when it was started (a hexadecimal mask), as the
    // kernel shows them.
    let blocked = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("SigBlk:").map(|v| v.trim().to_string()))
        });
    report(serde_json::json!({
        "blocked_signals": blocked,
        "event": "satellite_started",
        "pid": std::process::id(),
        "display": display,
        "listen": listeners.iter().map(|(fd, path, name, _)| serde_json::json!({
            "fd": fd, "path": path, "abstract": name,
        })).collect::<Vec<_>>(),
        "env_display": std::env::var("DISPLAY").ok(),
        "wayland_display": wayland.to_string_lossy(),
        "wayland_socket": std::env::var("WAYLAND_SOCKET").ok(),
    }))?;

    let socket = PathBuf::from(runtime).join(wayland);
    let options = ClientOptions {
        title: format!("X11 window on {display}"),
        app_id: "hizukuri.fake-x11".to_string(),
        ..ClientOptions::default()
    };
    let mut client = TestClient::connect_path(&socket, options).map_err(|e| e.to_string())?;
    let mut connections: Vec<BufReader<UnixStream>> = Vec::new();
    loop {
        client
            .pump(Some(Duration::from_millis(20)))
            .map_err(|e| e.to_string())?;
        for (fd, _, _, listener) in &listeners {
            while let Ok((mut stream, _)) = listener.accept() {
                writeln!(
                    stream,
                    "proto-test-client stands in for xwayland-satellite on {display}"
                )
                .map_err(|e| e.to_string())?;
                stream.set_nonblocking(true).map_err(|e| e.to_string())?;
                report(serde_json::json!({ "event": "x11_client", "fd": fd }))?;
                connections.push(BufReader::new(stream));
            }
        }
        for connection in &mut connections {
            let mut line = String::new();
            if connection.read_line(&mut line).is_ok()
                && let Some(code) = line.trim().strip_prefix("exit ")
            {
                let code: i32 = code
                    .parse()
                    .map_err(|_| format!("bad exit code {code:?}"))?;
                report(serde_json::json!({ "event": "exit", "code": code }))?;
                std::process::exit(code);
            }
        }
    }
}
