//! The part of `hz` about the working space and the colors a script writes
//! down.
//!
//! A number in a script is not a color until the script says so. A color
//! goes through `hz.color`, which reads it in some color space (the one
//! content without color information is taken to be in, unless named) and
//! returns it in the working space. Numbers that are passed on as they are
//! need no function.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, MultiValue, Table, Value};
use proto_color::{
    AlphaMode, CompositeSpace, DecodeTransform, ImageDescription, Primaries, TransferFunction,
};
use proto_gfx_core::{Format, Gpu};

use super::api::{SharedRef, arg_error, primaries};
use crate::scene::SceneRequest;

/// What the values of a color or an image mean, as a script names it:
///
/// - nothing: `default`;
/// - `"srgb"`: what content without color information is taken to be;
/// - the name of a preset image description (`"display-p3"`,
///   `"srgb-piecewise"`, `"bt2020-pq"`, ...);
/// - `"acescg"`, or `"linear-"` and the name of primaries
///   (`"linear-srgb"`): linear values in those primaries;
/// - `{ primaries = <name or eight chromaticities>, transfer = <name, or the
///   exponent of a power function> }`.
pub(super) fn description(
    value: &Value,
    default: ImageDescription,
) -> mlua::Result<ImageDescription> {
    let unknown = |name: &str| {
        arg_error(format!(
            "unknown color space {name:?} (known: srgb, acescg, linear-<primaries>, {})",
            ImageDescription::NAMED
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    };
    match value {
        Value::Nil => Ok(default),
        Value::String(name) => {
            let name = name.to_str()?;
            let name: &str = &name;
            if name == "srgb" {
                return Ok(proto_wl::DEFAULT_SURFACE_IMAGE_DESCRIPTION);
            }
            if let Some(description) = ImageDescription::from_name(name) {
                return Ok(description);
            }
            let linear = match name {
                "acescg" => Some(Primaries::ACES_AP1),
                _ => name.strip_prefix("linear-").and_then(Primaries::from_name),
            };
            linear
                .map(|p| ImageDescription::new(p, TransferFunction::Linear))
                .ok_or_else(|| unknown(name))
        }
        Value::Table(table) => {
            let primaries = primaries(&table.get::<Value>("primaries")?)?;
            let transfer = match table.get::<Value>("transfer")? {
                Value::Nil => default.transfer,
                Value::String(name) => {
                    let name = name.to_str()?;
                    TransferFunction::from_name(&name).ok_or_else(|| {
                        arg_error(format!(
                            "unknown transfer function {:?} (known: {})",
                            &*name,
                            TransferFunction::NAMED
                                .iter()
                                .map(|(n, _)| *n)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                    })?
                }
                Value::Number(exponent) if exponent.is_finite() && exponent > 0.0 => {
                    TransferFunction::Power(exponent as f32)
                }
                Value::Integer(exponent) if exponent > 0 => {
                    TransferFunction::Power(exponent as f32)
                }
                _ => {
                    return Err(arg_error(
                        "`transfer` is a name or the exponent of a power function",
                    ));
                }
            };
            Ok(ImageDescription::new(primaries, transfer))
        }
        _ => Err(arg_error(
            "a color space is a name or { primaries = , transfer = }",
        )),
    }
}

/// `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` as four values in `0..=1`.
fn parse_hex(text: &str) -> Option<[f32; 4]> {
    let digits = text.strip_prefix('#')?;
    if !digits.is_ascii() {
        return None;
    }
    let value = |s: &str| u8::from_str_radix(s, 16).ok();
    let mut out = [0.0, 0.0, 0.0, 1.0];
    match digits.len() {
        3 | 4 => {
            for (i, slot) in out.iter_mut().enumerate().take(digits.len()) {
                *slot = value(&digits[i..i + 1])? as f32 * 17.0 / 255.0;
            }
        }
        6 | 8 => {
            for (i, slot) in out.iter_mut().enumerate().take(digits.len() / 2) {
                *slot = value(&digits[i * 2..i * 2 + 2])? as f32 / 255.0;
            }
        }
        _ => return None,
    }
    Some(out)
}

fn number(value: &Value) -> Option<f32> {
    match value {
        Value::Number(n) => Some(*n as f32),
        Value::Integer(n) => Some(*n as f32),
        _ => None,
    }
    .filter(|n| n.is_finite())
}

/// The format `hz.color_format()` names: the forced one, or the most exact
/// float format the device can draw to, blend in and filter.
pub(super) fn color_format(forced: Option<Format>, gpu: &Gpu) -> Format {
    forced.unwrap_or_else(|| {
        let caps = gpu.format_caps(Format::Rgba32Float);
        if caps.sampled && caps.filter_linear && caps.color_attachment && caps.blend {
            Format::Rgba32Float
        } else {
            Format::Rgba16Float
        }
    })
}

fn primaries_value(lua: &Lua, primaries: &Primaries) -> mlua::Result<Value> {
    Ok(match primaries.name() {
        Some(name) => Value::String(lua.create_string(name)?),
        None => Value::Table(lua.create_sequence_from([
            primaries.red.x,
            primaries.red.y,
            primaries.green.x,
            primaries.green.y,
            primaries.blue.x,
            primaries.blue.y,
            primaries.white.x,
            primaries.white.y,
        ])?),
    })
}

pub(super) fn register(
    lua: &Lua,
    hz: &Table,
    shared: &SharedRef,
    gpu: &Rc<RefCell<Gpu>>,
) -> mlua::Result<()> {
    // The primaries of the working space (a name, or eight chromaticity
    // coordinates), and a number that changes whenever they do.
    {
        let state = Rc::clone(shared);
        hz.set(
            "working_space",
            lua.create_function(move |lua, (): ()| {
                let shared = state.borrow();
                Ok((
                    primaries_value(lua, &shared.working)?,
                    shared.working_generation,
                ))
            })?,
        )?;
    }

    // The weights of red, green and blue in the luminance of a color of the
    // working space (they sum to 1): for a shader that needs to know how
    // bright a color is, whatever the working space.
    {
        let state = Rc::clone(shared);
        hz.set(
            "working_luminance",
            lua.create_function(move |_, (): ()| {
                let y = state
                    .borrow()
                    .working
                    .rgb_to_xyz()
                    .ok_or_else(|| arg_error("degenerate primaries"))?
                    .0[1];
                Ok((y[0], y[1], y[2]))
            })?,
        )?;
    }

    // Changes the working space. Everything a script computed from the old
    // one (colors, images read as colors) keeps its old numbers.
    {
        let state = Rc::clone(shared);
        hz.set(
            "set_working_space",
            lua.create_function(move |_, value: Value| {
                let primaries = primaries(&value)?;
                let mut shared = state.borrow_mut();
                if shared.working != primaries {
                    shared.working = primaries;
                    shared.working_generation += 1;
                    shared.requests.push(SceneRequest::Redraw);
                }
                Ok(())
            })?,
        )?;
    }

    // A color, in the working space: `hz.color("#1e1e2e")`,
    // `hz.color(r, g, b)`, `hz.color(r, g, b, a)`, each with an optional
    // table last: `{ space = <see `description`>, premultiply = false }`.
    // Returns r, g, b, a: linear, with alpha premultiplied unless asked
    // otherwise.
    {
        let state = Rc::clone(shared);
        hz.set(
            "color",
            lua.create_function(move |_, args: MultiValue| {
                let mut args: Vec<Value> = args.into_vec();
                let options = match args.last() {
                    Some(Value::Table(_)) => match args.pop() {
                        Some(Value::Table(t)) => Some(t),
                        _ => None,
                    },
                    _ => None,
                };
                let usage = || {
                    arg_error(
                        "hz.color takes \"#rrggbb\" (or #rgb, #rgba, #rrggbbaa) or r, g, b \
                         and an optional a, then an optional table of options",
                    )
                };
                let [r, g, b, a] = match args.as_slice() {
                    [Value::String(text)] => {
                        let text = text.to_str()?;
                        parse_hex(&text).ok_or_else(|| {
                            arg_error(format!("{:?} is not a color like \"#1e1e2e\"", &*text))
                        })?
                    }
                    [r, g, b] => [
                        number(r).ok_or_else(usage)?,
                        number(g).ok_or_else(usage)?,
                        number(b).ok_or_else(usage)?,
                        1.0,
                    ],
                    [r, g, b, a] => [
                        number(r).ok_or_else(usage)?,
                        number(g).ok_or_else(usage)?,
                        number(b).ok_or_else(usage)?,
                        number(a).ok_or_else(usage)?,
                    ],
                    _ => return Err(usage()),
                };
                let (space, premultiply) = match &options {
                    Some(options) => (
                        options.get::<Value>("space")?,
                        options.get::<Option<bool>>("premultiply")?.unwrap_or(true),
                    ),
                    None => (Value::Nil, true),
                };
                let description = description(&space, proto_wl::DEFAULT_SURFACE_IMAGE_DESCRIPTION)?;
                let working = CompositeSpace {
                    primaries: state.borrow().working,
                };
                let transform = DecodeTransform::new(&description, AlphaMode::Straight, &working)
                    .ok_or_else(|| arg_error("degenerate primaries or luminances"))?;
                let [r, g, b, _] = transform.apply([r, g, b, 1.0]);
                Ok(if premultiply {
                    (r * a, g * a, b * a, a)
                } else {
                    (r, g, b, a)
                })
            })?,
        )?;
    }

    // The format for buffers that hold colors of the working space: what a
    // script uses when it has no reason to choose another.
    {
        let state = Rc::clone(shared);
        let gpu = Rc::clone(gpu);
        hz.set(
            "color_format",
            lua.create_function(move |_, (): ()| {
                let forced = state.borrow().color_format;
                Ok(color_format(forced, &gpu.borrow()).name())
            })?,
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors() {
        assert_eq!(parse_hex("#ff8000"), Some([1.0, 128.0 / 255.0, 0.0, 1.0]));
        assert_eq!(
            parse_hex("#FF800080"),
            Some([1.0, 128.0 / 255.0, 0.0, 128.0 / 255.0])
        );
        assert_eq!(parse_hex("#f80"), Some([1.0, 136.0 / 255.0, 0.0, 1.0]));
        assert_eq!(
            parse_hex("#f808"),
            Some([1.0, 136.0 / 255.0, 0.0, 136.0 / 255.0])
        );
        for bad in ["ff8000", "#ff800", "#gg0000", "#", "#ｆｆ"] {
            assert_eq!(parse_hex(bad), None, "{bad}");
        }
    }
}
