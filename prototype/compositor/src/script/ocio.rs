//! The OCIO part of the `hz` table, and of the control interface.
//!
//! - `hz.ocio_present(slot, output, target[, working])`: the way out through
//!   the output's OCIO display and view. `false` when the output has none
//!   (or none is ready yet): the script then converts as it does without
//!   OCIO. The built-in `color.present` asks this first.
//! - `hz.ocio_apply({...})`: any conversion of the config as a draw from
//!   one image into another, for a renderer that wants a view on part of
//!   what it draws.
//! - `hz.ocio_info()`, `hz.ocio_set_output(...)`, `hz.ocio_set_controls(...)`:
//!   what the config has, and choosing an output's display, view, exposure
//!   and gamma from a script.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Table, Value};
use proto_color::{ImageDescription, Primaries};
use proto_gfx_core::{ErrorCode, GfxError, Gpu, Handle};
use serde_json::json;

use super::Shared;
use crate::ocio::{DrawParams, Engine, Options, Presented, Target, Transform};
use crate::scene::SceneRequest;

type SharedRef = Rc<RefCell<Shared>>;

fn arg_error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::external(GfxError {
        code: ErrorCode::InvalidArgument,
        message: message.into(),
    })
}

/// The engine of a runtime, and what was last complained about (so that a
/// failure that repeats every frame is logged once).
pub(crate) struct OcioState {
    pub engine: Option<Engine>,
    last_error: Option<String>,
    /// The disagreements between an output's image description and its OCIO
    /// display that were warned about.
    warned: Vec<String>,
    /// Whether outputs are driven as their OCIO display's signal says.
    follow: bool,
    /// The outputs that were asked to follow their display, by name.
    followed: std::collections::HashMap<String, Followed>,
}

/// An output whose image description was asked to follow its OCIO display.
struct Followed {
    /// What the output was driven as before.
    original: ImageDescription,
    /// What was asked for.
    requested: ImageDescription,
}

impl OcioState {
    /// Reads the config. When nothing was asked of OCIO, a config that
    /// cannot be read is logged and the runtime goes on without OCIO; when
    /// something was (a config, a working colour space, an output's display
    /// or view), it is an error.
    pub fn new(options: &Options) -> Result<Self, String> {
        let asked = options.config.is_some()
            || options.working.is_some()
            || !options.outputs.is_empty()
            || !options.controls.is_empty();
        let engine = match Engine::new(options) {
            Ok(engine) => Some(engine),
            Err(error) if asked => return Err(error),
            Err(error) => {
                tracing::error!("OCIO is not available: {error}");
                None
            }
        };
        Ok(Self {
            engine,
            last_error: None,
            warned: Vec::new(),
            follow: options.outputs_follow_display,
            followed: Default::default(),
        })
    }

    /// The image description to ask the core for, so that an output is
    /// driven, and described to applications, as the signal of its OCIO
    /// display: once per output and signal. Without a display (or with one
    /// whose signal cannot be told), what the output was before.
    fn follow_display(
        &mut self,
        output: &str,
        description: &ImageDescription,
    ) -> Option<ImageDescription> {
        if !self.follow {
            return None;
        }
        let wanted = self
            .engine
            .as_ref()
            .and_then(|engine| engine.output_signal(output))
            .and_then(|signal| signal.image_description());
        match wanted {
            Some(wanted) if wanted != *description => {
                let known = self.followed.get(output);
                // Asked before: the backend refused, or something else
                // described the output since.
                if known.is_some_and(|f| f.requested == wanted) {
                    return None;
                }
                let original = known.map_or(*description, |f| f.original);
                self.followed.insert(
                    output.to_string(),
                    Followed {
                        original,
                        requested: wanted,
                    },
                );
                Some(wanted)
            }
            Some(_) => None,
            None => {
                let followed = self.followed.remove(output)?;
                (followed.requested == *description && followed.original != *description)
                    .then_some(followed.original)
            }
        }
    }

    /// Says, once, that an output is driven with one signal while
    /// applications are told another: the output's image description is
    /// set apart from its OCIO display.
    fn check_signal(&mut self, output: &str, description: &ImageDescription) {
        let Some(signal) = self
            .engine
            .as_ref()
            .and_then(|engine| engine.output_signal(output))
        else {
            return;
        };
        if signal.matches(description) != Some(false) {
            return;
        }
        let told = description.name().unwrap_or("a custom image description");
        let key = format!("{output}: {told} / {}", signal.describe());
        if !self.warned.contains(&key) {
            tracing::warn!(
                "output {output} is shown through an OCIO display whose signal is {}, but \
                 applications are told the output is {told}",
                signal.describe()
            );
            self.warned.push(key);
        }
    }

    fn report(&mut self, error: String) {
        if self.last_error.as_ref() != Some(&error) {
            tracing::error!("the OCIO conversion was not drawn: {error}");
            self.last_error = Some(error);
        }
    }
}

/// The matrix from the script's working primaries to the config's working
/// colour space, row-major.
fn to_config_working(script: &Primaries, engine: &Engine) -> [[f32; 3]; 3] {
    engine
        .working_primaries()
        .and_then(|config| script.conversion_to(&config))
        .map_or(crate::ocio::pass::IDENTITY, |m| m.to_f32())
}

fn output_name(shared: &Shared, value: &Value) -> mlua::Result<Option<String>> {
    match value {
        Value::Nil => Ok(None),
        Value::String(name) => Ok(Some(name.to_str()?.to_string())),
        Value::Integer(_) | Value::Number(_) => {
            let id = match value {
                Value::Integer(i) => *i as u64,
                Value::Number(n) => *n as u64,
                _ => unreachable!(),
            };
            shared
                .world
                .outputs
                .iter()
                .find(|o| o.id.0 == id)
                .map(|o| Some(o.name.clone()))
                .ok_or_else(|| arg_error(format!("unknown output {id}")))
        }
        _ => Err(arg_error("an output is its id, its name, or nil for all")),
    }
}

/// `(true)` or `(false, message)`.
fn outcome(lua: &Lua, result: Result<bool, String>) -> mlua::Result<(bool, Value)> {
    Ok(match result {
        Ok(done) => (done, Value::Nil),
        Err(message) => (false, Value::String(lua.create_string(&message)?)),
    })
}

pub(crate) fn register(
    lua: &Lua,
    hz: &Table,
    shared: &SharedRef,
    gpu: &Rc<RefCell<Gpu>>,
) -> mlua::Result<()> {
    // The way out of an output through its OCIO display and view: converts
    // the image behind `slot` (linear light in the primaries `working`, the
    // size of the output) into `target`. Returns whether it drew.
    {
        let (state, gpu) = (Rc::clone(shared), Rc::clone(gpu));
        hz.set(
            "ocio_present",
            lua.create_function(
                move |_, (slot, output, target, working): (i64, u64, i64, Value)| {
                    // Nil: the session's working space.
                    let script_working = super::api::working(&state, &working)?;
                    let mut shared = state.borrow_mut();
                    let Some(info) = shared.world.outputs.iter().find(|o| o.id.0 == output) else {
                        return Err(arg_error(format!("unknown output {output}")));
                    };
                    let (name, description) = (info.name.clone(), info.image_description);
                    // The output is driven as its display's signal says.
                    let asked = shared.ocio.follow_display(&name, &description);
                    if let Some(wanted) = asked {
                        shared.requests.push(SceneRequest::OutputImageDescription {
                            output: name.clone(),
                            description: wanted,
                        });
                    }
                    let Some(engine) = shared.ocio.engine.as_mut() else {
                        return Ok(false);
                    };
                    if !engine.output_is_configured(&name) {
                        return Ok(false);
                    }
                    let matrix = to_config_working(&script_working, engine);
                    let scale = crate::ocio::reference_white_scale(&description);
                    let presented = engine.present(
                        &mut gpu.borrow_mut(),
                        &name,
                        Handle::from_bits(slot),
                        Handle::from_bits(target),
                        matrix,
                        scale,
                    );
                    let (drawn, pending) = match presented {
                        Ok(Presented::Drawn { pending }) => (true, pending),
                        Ok(Presented::NotReady { pending }) => (false, pending),
                        Ok(Presented::NotConfigured) => (false, false),
                        Err(error) => {
                            shared.ocio.report(error);
                            (false, false)
                        }
                    };
                    if pending {
                        // Keep the frames coming until the conversion that
                        // is being compiled can take over.
                        shared.requests.push(SceneRequest::Redraw);
                    }
                    if drawn && asked.is_none() {
                        shared.ocio.check_signal(&name, &description);
                    }
                    Ok(drawn)
                },
            )?,
        )?;
    }

    // Any conversion of the config as a draw: from the image behind
    // `input` (a texture slot) into `target` (an image of the same size).
    //
    //   hz.ocio_apply({ input = slot, target = image,
    //                   from = "ACEScg",              -- default: the working colour space
    //                   to = "ACEScct",               -- a colour space, or
    //                   display = "...", view = "...", -- a display and view
    //                   premultiplied = false, scale = 1,
    //                   exposure = 0, gamma = 1 })    -- with a display and view only
    //
    // Returns true when it drew; false while the conversion is being
    // compiled (ask again next frame; `hz.request_frame()` is called for
    // you); false and a message when the conversion cannot be made.
    {
        let (state, gpu) = (Rc::clone(shared), Rc::clone(gpu));
        hz.set(
            "ocio_apply",
            lua.create_function(move |lua, args: Table| {
                let input: i64 = args.get("input")?;
                let target: i64 = args.get("target")?;
                let from: Option<String> = args.get("from")?;
                let to: Option<String> = args.get("to")?;
                let display: Option<String> = args.get("display")?;
                let view: Option<String> = args.get("view")?;
                let exposure: Option<f64> = args.get("exposure")?;
                let gamma: Option<f64> = args.get("gamma")?;
                let params = DrawParams {
                    scale: args.get::<Option<f32>>("scale")?.unwrap_or(1.0),
                    premultiplied: args.get::<Option<bool>>("premultiplied")?.unwrap_or(false),
                    ..DrawParams::default()
                };
                let mut shared = state.borrow_mut();
                let Some(engine) = shared.ocio.engine.as_mut() else {
                    return outcome(lua, Err("OCIO is not available".into()));
                };
                let dst = match (to, display, view) {
                    (Some(to), None, None) => Target::ColorSpace(to),
                    (None, Some(display), view) => {
                        let view = match view.or_else(|| engine.config().default_view(&display)) {
                            Some(view) => view,
                            None => {
                                return outcome(
                                    lua,
                                    Err(format!("the display {display:?} has no view")),
                                );
                            }
                        };
                        Target::Display { display, view }
                    }
                    _ => {
                        return Err(arg_error(
                            "ocio_apply wants `to` (a colour space) or `display` (and `view`)",
                        ));
                    }
                };
                let dynamic = (exposure.is_some() || gamma.is_some())
                    && matches!(dst, Target::Display { .. });
                let transform = Transform {
                    src: from.unwrap_or_else(|| engine.working().to_string()),
                    dst,
                    dynamic,
                };
                let controls = dynamic.then(|| (exposure.unwrap_or(0.0), gamma.unwrap_or(1.0)));
                let result = engine.apply(
                    &mut gpu.borrow_mut(),
                    &transform,
                    controls,
                    Handle::from_bits(input),
                    Handle::from_bits(target),
                    params,
                );
                if result == Ok(false) {
                    shared.requests.push(SceneRequest::Redraw);
                }
                outcome(lua, result)
            })?,
        )?;
    }

    // What the config has: its name, the working colour space and its
    // primaries (eight chromaticity coordinates, as `color.set_working`
    // takes them), the displays with their views, the colour spaces. Nil
    // without OCIO.
    {
        let state = Rc::clone(shared);
        hz.set(
            "ocio_info",
            lua.create_function(move |lua, (): ()| {
                let shared = state.borrow();
                let Some(engine) = shared.ocio.engine.as_ref() else {
                    return Ok(Value::Nil);
                };
                let config = engine.config();
                let t = lua.create_table()?;
                t.set("config", config.name())?;
                t.set("working", engine.working())?;
                if let Some(p) = engine.working_primaries() {
                    t.set(
                        "working_chromaticities",
                        lua.create_sequence_from([
                            p.red.x, p.red.y, p.green.x, p.green.y, p.blue.x, p.blue.y, p.white.x,
                            p.white.y,
                        ])?,
                    )?;
                }
                let displays = lua.create_table()?;
                for display in config.displays() {
                    let d = lua.create_table()?;
                    d.set("name", display.as_str())?;
                    d.set("views", lua.create_sequence_from(config.views(&display))?)?;
                    d.set("default_view", config.default_view(&display))?;
                    displays.push(d)?;
                }
                t.set("displays", displays)?;
                t.set("default_display", config.default_display())?;
                t.set(
                    "color_spaces",
                    lua.create_sequence_from(config.color_spaces())?,
                )?;
                Ok(Value::Table(t))
            })?,
        )?;
    }

    // Chooses the display and view an output is shown through. `output` is
    // an id, a name, or nil for every output without a setting of its own.
    // A nil display is the config's default, a nil view the display's
    // default; both nil: back to the conversion without OCIO.
    // Returns true, or false and a message.
    {
        let state = Rc::clone(shared);
        hz.set(
            "ocio_set_output",
            lua.create_function(
                move |lua, (output, display, view): (Value, Option<String>, Option<String>)| {
                    let mut shared = state.borrow_mut();
                    let output = output_name(&shared, &output)?;
                    let Some(engine) = shared.ocio.engine.as_mut() else {
                        return outcome(lua, Err("OCIO is not available".into()));
                    };
                    let result = engine
                        .set_output(output.as_deref(), display.as_deref(), view.as_deref())
                        .map(|()| true);
                    shared.requests.push(SceneRequest::Redraw);
                    outcome(lua, result)
                },
            )?,
        )?;
    }

    // Sets an output's exposure (stops, before the view) and gamma (after
    // it): `hz.ocio_set_controls(output, { exposure = 1.5, gamma = 1.0 })`.
    {
        let state = Rc::clone(shared);
        hz.set(
            "ocio_set_controls",
            lua.create_function(move |lua, (output, controls): (Value, Table)| {
                let exposure: Option<f64> = controls.get("exposure")?;
                let gamma: Option<f64> = controls.get("gamma")?;
                let mut shared = state.borrow_mut();
                let output = output_name(&shared, &output)?;
                let Some(engine) = shared.ocio.engine.as_mut() else {
                    return outcome(lua, Err("OCIO is not available".into()));
                };
                let result = engine
                    .set_output_controls(output.as_deref(), exposure, gamma)
                    .map(|()| true);
                shared.requests.push(SceneRequest::Redraw);
                outcome(lua, result)
            })?,
        )?;
    }
    Ok(())
}

/// The control requests `ocio_set` and `ocio_state`.
pub(crate) fn control(
    shared: &SharedRef,
    command: &str,
    args: &serde_json::Value,
) -> Option<Result<serde_json::Value, String>> {
    let mut shared = shared.borrow_mut();
    Some(match command {
        "ocio_state" => Ok(match shared.ocio.engine.as_ref() {
            Some(engine) => engine.state(),
            None => json!({ "available": false }),
        }),
        "ocio_set" => (|| {
            let text = |key: &str| args[key].as_str();
            let engine = shared
                .ocio
                .engine
                .as_mut()
                .ok_or("OCIO is not available in this session")?;
            let output = text("output");
            if args["display"].is_string() || args["view"].is_string() {
                engine.set_output(output, text("display"), text("view"))?;
            } else if args["clear"].as_bool() == Some(true) {
                engine.set_output(output, None, None)?;
            }
            let (exposure, gamma) = (args["exposure"].as_f64(), args["gamma"].as_f64());
            if exposure.is_some() || gamma.is_some() {
                engine.set_output_controls(output, exposure, gamma)?;
            }
            let state = engine.state();
            shared.requests.push(SceneRequest::Redraw);
            Ok(state)
        })(),
        _ => return None,
    })
}
