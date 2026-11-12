//! `proto-ctl`: talks to the control socket of a running proto-compositor.
//!
//! ```text
//! proto-ctl --socket PATH ping
//! proto-ctl --socket PATH state
//! proto-ctl --socket PATH screenshot out.png [--composite out.exr] [--output NAME]
//! proto-ctl --socket PATH key 30 press|release
//! proto-ctl --socket PATH pointer-motion 100 50
//! proto-ctl --socket PATH pointer-button 272 press|release
//! proto-ctl --socket PATH raw '{"cmd":"state"}'
//! ```
//!
//! Prints the JSON response. Exits with a failure status when the
//! compositor refuses the request.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use proto_compositor::control::{ControlClient, ControlError, Request};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Control a running proto-compositor (verification tool)"
)]
struct Args {
    /// Path of the control socket (`<wayland socket>.control` by default).
    #[arg(long)]
    socket: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Press {
    Press,
    Release,
}

impl Press {
    fn pressed(self) -> bool {
        matches!(self, Press::Press)
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check that the compositor answers.
    Ping,
    /// Dump windows, surfaces, outputs, focus and color state.
    State,
    /// Render the current state and write it to files.
    Screenshot {
        /// PNG file for the encoded output.
        path: PathBuf,
        /// OpenEXR file for the compositing buffer (linear, half float).
        #[arg(long)]
        composite: Option<PathBuf>,
        /// Name of the output. Defaults to the first one.
        #[arg(long)]
        output: Option<String>,
    },
    /// Press or release a key (Linux evdev code; KEY_A is 30).
    Key { keycode: u32, action: Press },
    /// Move the pointer to an absolute position (global logical).
    PointerMotion { x: f64, y: f64 },
    /// Move the pointer by a relative amount.
    PointerMotionRelative {
        #[arg(allow_negative_numbers = true)]
        dx: f64,
        #[arg(allow_negative_numbers = true)]
        dy: f64,
        /// The motion before pointer acceleration, as relative-pointer
        /// clients see it. Defaults to `dx` and `dy`.
        #[arg(long, allow_negative_numbers = true)]
        dx_unaccel: Option<f64>,
        #[arg(long, allow_negative_numbers = true)]
        dy_unaccel: Option<f64>,
    },
    /// Press or release a pointer button (Linux evdev code; BTN_LEFT is 272).
    PointerButton { button: u32, action: Press },
    /// Scroll. Positive is down or right.
    PointerAxis {
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        horizontal: f64,
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        vertical: f64,
        /// High resolution wheel steps (120 per detent); marks the event as
        /// coming from a wheel.
        #[arg(long, allow_negative_numbers = true)]
        v120_vertical: Option<i32>,
        #[arg(long, allow_negative_numbers = true)]
        v120_horizontal: Option<i32>,
    },
    /// Move a window.
    MoveWindow {
        window: u64,
        #[arg(allow_negative_numbers = true)]
        x: i32,
        #[arg(allow_negative_numbers = true)]
        y: i32,
    },
    /// Focus and raise a window. Without an id the focus is cleared.
    FocusWindow { window: Option<u64> },
    /// Ask a window to close.
    CloseWindow { window: u64 },
    /// Set the opacity a window is drawn with.
    SetWindowOpacity { window: u64, opacity: f32 },
    /// Override what the values of a surface mean (preset name).
    SetSurfaceColor {
        surface: u64,
        image_description: String,
        #[arg(long)]
        alpha_mode: Option<String>,
    },
    /// Call a global function of the script. Each argument is JSON
    /// (`3`, `"text"`, `[1, 0, 0, 1, 20, 30]`, `null`).
    ScriptCall { function: String, args: Vec<String> },
    /// Replace the running script with a fresh load of its project, or of
    /// the project in a directory. Windows stay open.
    ScriptReload {
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Put the script in charge (`running`) or the built-in fallback
    /// (`fallback`).
    ScriptMode { mode: String },
    /// Stop the compositor.
    Quit,
    /// Send a raw JSON request.
    Raw { json: String },
}

fn main() -> ExitCode {
    let args = Args::parse();
    let request = match args.command {
        Command::Ping => Request::Ping,
        Command::State => Request::State,
        Command::Screenshot {
            path,
            composite,
            output,
        } => Request::Screenshot {
            // The compositor resolves paths; make them independent of its
            // working directory.
            path: absolute(path),
            composite_path: composite.map(absolute),
            output,
        },
        Command::Key { keycode, action } => Request::Key {
            keycode,
            pressed: action.pressed(),
        },
        Command::PointerMotion { x, y } => Request::PointerMotion { x, y },
        Command::PointerMotionRelative {
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
        } => Request::PointerMotionRelative {
            dx,
            dy,
            dx_unaccel,
            dy_unaccel,
        },
        Command::PointerButton { button, action } => Request::PointerButton {
            button,
            pressed: action.pressed(),
        },
        Command::PointerAxis {
            horizontal,
            vertical,
            v120_vertical,
            v120_horizontal,
        } => Request::PointerAxis {
            horizontal,
            vertical,
            v120_horizontal,
            v120_vertical,
        },
        Command::MoveWindow { window, x, y } => Request::MoveWindow { window, x, y },
        Command::FocusWindow { window } => Request::FocusWindow { window },
        Command::CloseWindow { window } => Request::CloseWindow { window },
        Command::SetWindowOpacity { window, opacity } => {
            Request::SetWindowOpacity { window, opacity }
        }
        Command::SetSurfaceColor {
            surface,
            image_description,
            alpha_mode,
        } => Request::SetSurfaceColor {
            surface,
            image_description,
            alpha_mode,
        },
        Command::ScriptCall { function, args } => {
            let parsed: Result<Vec<serde_json::Value>, _> =
                args.iter().map(|a| serde_json::from_str(a)).collect();
            match parsed {
                Ok(args) => Request::ScriptCall { function, args },
                Err(error) => {
                    eprintln!("proto-ctl: an argument is not JSON: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }
        Command::ScriptReload { path } => Request::ScriptReload {
            path: path.map(|p| std::path::absolute(&p).unwrap_or(p)),
        },
        Command::ScriptMode { mode } => Request::ScriptMode { mode },
        Command::Quit => Request::Quit,
        Command::Raw { json } => match serde_json::from_str(&json) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("proto-ctl: invalid request: {error}");
                return ExitCode::FAILURE;
            }
        },
    };

    let result = ControlClient::connect(&args.socket).and_then(|mut client| client.send(&request));
    match result {
        Ok(response) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&response).unwrap_or_else(|_| response.to_string())
            );
            ExitCode::SUCCESS
        }
        Err(ControlError::Refused(message)) => {
            eprintln!("proto-ctl: {message}");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("proto-ctl: {error}");
            ExitCode::FAILURE
        }
    }
}

fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}
