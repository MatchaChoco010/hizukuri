//! The control interface for verification.
//!
//! A Unix stream socket carrying one JSON object per line. Each request
//! line gets exactly one response line: `{"ok": true, ...}` on success,
//! `{"ok": false, "error": "..."}` otherwise.
//!
//! This is a testing tool. It is unrelated to how an editor will talk to
//! the engine.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A request to the compositor.
///
/// Key and button codes are Linux evdev codes (`KEY_A` is 30, `BTN_LEFT` is
/// 272). Positions are in the global logical coordinate space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Answers `{"ok": true}`.
    Ping,
    /// Dumps windows, surfaces, outputs, focus and color state.
    State,
    /// Renders the current state and writes the result to files: the
    /// encoded output as PNG to `path` and, when given, the compositing
    /// buffer (linear, half float) as OpenEXR to `composite_path`.
    Screenshot {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        composite_path: Option<PathBuf>,
        /// Name of the output. Defaults to the first one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    Key {
        keycode: u32,
        pressed: bool,
    },
    /// Moves the pointer to an absolute position.
    PointerMotion {
        x: f64,
        y: f64,
    },
    /// Moves the pointer by a relative amount, as a mouse does. `dx` and
    /// `dy` are after pointer acceleration; `dx_unaccel` and `dy_unaccel`
    /// (default: the same) before it.
    PointerMotionRelative {
        dx: f64,
        dy: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dx_unaccel: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dy_unaccel: Option<f64>,
    },
    PointerButton {
        button: u32,
        pressed: bool,
    },
    /// Scrolls. Positive is down or right. With `v120_*` the event is
    /// reported as coming from a wheel.
    PointerAxis {
        #[serde(default)]
        horizontal: f64,
        #[serde(default)]
        vertical: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        v120_horizontal: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        v120_vertical: Option<i32>,
    },
    /// Moves a window (its logical position).
    MoveWindow {
        window: u64,
        x: i32,
        y: i32,
    },
    /// Gives the focus to a window and raises it; `null` clears the focus.
    FocusWindow {
        window: Option<u64>,
    },
    /// Asks a window to close.
    CloseWindow {
        window: u64,
    },
    /// Sets the opacity a window is drawn with (visual state only).
    SetWindowOpacity {
        window: u64,
        opacity: f32,
    },
    /// Overrides what the values of a surface mean, by preset name
    /// (e.g. `display-p3`). Wins over what the client says through
    /// color-management-v1. For tests.
    SetSurfaceColor {
        surface: u64,
        image_description: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alpha_mode: Option<String>,
    },
    /// Changes the image description an output is driven with, by preset
    /// name. Clients are told through color-management-v1. Only the
    /// headless backend can.
    SetOutputImageDescription {
        output: String,
        image_description: String,
    },
    /// Calls a global function of the running script with JSON arguments
    /// and returns what it returns. Not with `--no-drawing`.
    ScriptCall {
        function: String,
        #[serde(default)]
        args: Vec<Value>,
    },
    /// Replaces the running script with a fresh load of its project, or of
    /// the project in the directory `path`. Windows stay open.
    ScriptReload {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
    },
    /// Hands drawing and window management to the script (`running`) or to
    /// the built-in fallback (`fallback`).
    ScriptMode {
        mode: String,
    },
    /// Stops the compositor.
    Quit,
}

/// Builds a success response with extra fields.
pub fn ok(mut fields: Value) -> Value {
    match fields.as_object_mut() {
        Some(map) => {
            map.insert("ok".to_string(), Value::Bool(true));
            fields
        }
        None => serde_json::json!({ "ok": true, "value": fields }),
    }
}

/// Builds an error response.
pub fn error(message: impl Into<String>) -> Value {
    serde_json::json!({ "ok": false, "error": message.into() })
}

/// Errors of the control client.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("I/O error on the control socket: {0}")]
    Io(#[from] io::Error),
    #[error("invalid JSON on the control socket: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the compositor closed the control connection")]
    Closed,
    #[error("the compositor refused the request: {0}")]
    Refused(String),
}

/// A connection to the control socket of a running compositor.
pub struct ControlClient {
    reader: BufReader<UnixStream>,
}

impl ControlClient {
    pub fn connect(path: &Path) -> Result<Self, ControlError> {
        let stream = UnixStream::connect(path)?;
        // A screenshot of a large output on a software rasterizer takes a
        // while, but nothing should take this long.
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        Ok(Self {
            reader: BufReader::new(stream),
        })
    }

    /// Sends a request and returns the response. A response with
    /// `"ok": false` becomes [`ControlError::Refused`].
    pub fn send(&mut self, request: &Request) -> Result<Value, ControlError> {
        let mut line = serde_json::to_string(request)?;
        line.push('\n');
        self.reader.get_mut().write_all(line.as_bytes())?;
        let mut response = String::new();
        if self.reader.read_line(&mut response)? == 0 {
            return Err(ControlError::Closed);
        }
        let value: Value = serde_json::from_str(&response)?;
        if value.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(value)
        } else {
            Err(ControlError::Refused(
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_use_a_cmd_tag() {
        let json = serde_json::to_string(&Request::Key {
            keycode: 30,
            pressed: true,
        })
        .unwrap();
        assert_eq!(json, r#"{"cmd":"key","keycode":30,"pressed":true}"#);
        assert_eq!(
            serde_json::to_string(&Request::Ping).unwrap(),
            r#"{"cmd":"ping"}"#
        );
    }

    #[test]
    fn optional_fields_may_be_omitted() {
        let request: Request =
            serde_json::from_str(r#"{"cmd":"screenshot","path":"/tmp/a.png"}"#).unwrap();
        assert_eq!(
            request,
            Request::Screenshot {
                path: "/tmp/a.png".into(),
                composite_path: None,
                output: None,
            }
        );
        let request: Request =
            serde_json::from_str(r#"{"cmd":"pointer_axis","vertical":15}"#).unwrap();
        assert_eq!(
            request,
            Request::PointerAxis {
                horizontal: 0.0,
                vertical: 15.0,
                v120_horizontal: None,
                v120_vertical: None,
            }
        );
        let request: Request =
            serde_json::from_str(r#"{"cmd":"focus_window","window":null}"#).unwrap();
        assert_eq!(request, Request::FocusWindow { window: None });
    }

    #[test]
    fn unknown_commands_are_rejected() {
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"nope"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"key"}"#).is_err());
    }

    #[test]
    fn responses_carry_ok() {
        assert_eq!(
            ok(serde_json::json!({"a": 1})),
            serde_json::json!({"ok": true, "a": 1})
        );
        assert_eq!(error("no"), serde_json::json!({"ok": false, "error": "no"}));
    }
}
