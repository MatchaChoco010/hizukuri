//! `hz.load_image`: a file of the project as a texture.
//!
//! An image is not always a color. The script says which it is when it loads
//! one:
//!
//! - a color image (the default) becomes a texture whose values are linear
//!   light in the working space with alpha premultiplied, so that a shader
//!   reads it like everything else it draws with. What the stored values
//!   mean comes from the file, from the script (`space`), or is what
//!   content without color information is taken to be;
//! - a data image (`data = true`) is uploaded as it is stored.
//!
//! Formats: PNG (8 and 16 bit), Radiance RGBE (`.hdr`, floating point), and
//! raw little-endian `f32` values (`raw = { width, height, channels }`).

use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use mlua::{Lua, Table, Value};
use proto_color::{
    AlphaMode, Chromaticity, CompositeSpace, DecodeTransform, ImageDescription, Primaries,
    TransferFunction,
};
use proto_gfx_core::{Format, Gpu, Handle, ImageDesc, ImageExtra, ImageUsage};

use super::Project;
use super::api::{SharedRef, arg_error};
use crate::image::f32_to_half;

/// The largest file read.
const MAX_FILE_BYTES: u64 = 1 << 30;
/// The largest width and height read.
const MAX_SIDE: u32 = 16384;

/// The pixels of a file, top row first.
#[derive(Debug, PartialEq)]
pub(crate) enum Pixels {
    /// RGBA, 8 bit, straight alpha.
    Rgba8(Vec<u8>),
    /// RGBA, 16 bit, straight alpha.
    Rgba16(Vec<u16>),
    /// `channels` floats per pixel.
    Float { channels: u32, values: Vec<f32> },
}

#[derive(Debug)]
pub(crate) struct Decoded {
    pub width: u32,
    pub height: u32,
    pub pixels: Pixels,
    /// What the file says its values mean, when it says.
    pub description: Option<ImageDescription>,
    /// Color information of the file that was not used.
    pub ignored: Option<&'static str>,
}

// ----- PNG ---------------------------------------------------------------------

/// The primaries of a code point of Rec. ITU-T H.273, table 2.
fn h273_primaries(code: u8) -> Option<Primaries> {
    Some(match code {
        1 => Primaries::SRGB,
        5 => Primaries::PAL,
        6 | 7 => Primaries::NTSC,
        9 => Primaries::BT2020,
        10 => Primaries::CIE1931_XYZ,
        11 => Primaries::DCI_P3,
        12 => Primaries::DISPLAY_P3,
        _ => return None,
    })
}

/// The transfer function of a code point of Rec. ITU-T H.273, table 3. The
/// camera functions (BT.709 and its relatives) are read like content
/// without color information.
fn h273_transfer(code: u8, fallback: TransferFunction) -> Option<TransferFunction> {
    Some(match code {
        1 | 6 | 14 | 15 => fallback,
        4 => TransferFunction::Gamma22,
        5 => TransferFunction::Gamma28,
        8 => TransferFunction::Linear,
        13 => TransferFunction::SrgbPiecewise,
        16 => TransferFunction::Pq,
        18 => TransferFunction::Hlg,
        _ => return None,
    })
}

fn png_description(info: &png::Info) -> (Option<ImageDescription>, Option<&'static str>) {
    let default = proto_wl::DEFAULT_SURFACE_IMAGE_DESCRIPTION;
    if let Some(cicp) = info.coding_independent_code_points {
        return match (
            h273_primaries(cicp.color_primaries),
            h273_transfer(cicp.transfer_function, default.transfer),
        ) {
            (Some(primaries), Some(transfer)) => {
                (Some(ImageDescription::new(primaries, transfer)), None)
            }
            _ => (
                None,
                Some("a cICP chunk with code points that are not known"),
            ),
        };
    }
    if info.icc_profile.is_some() {
        return (None, Some("an ICC profile (not read)"));
    }
    if info.srgb.is_some() {
        return (Some(default), None);
    }
    if info.gama_chunk.is_none() && info.chrm_chunk.is_none() {
        return (None, None);
    }
    let transfer = match info.gama_chunk.map(|g| 1.0 / g.into_value()) {
        None => default.transfer,
        Some(e) if !e.is_finite() || e <= 0.0 => {
            return (None, Some("a gAMA chunk that makes no sense"));
        }
        Some(e) if (e - 2.2).abs() < 0.005 => TransferFunction::Gamma22,
        Some(e) if (e - 1.0).abs() < 0.0005 => TransferFunction::Linear,
        Some(e) => TransferFunction::Power(e),
    };
    let primaries = match info.chrm_chunk {
        None => default.primaries,
        Some(c) => {
            let at = |p: (png::ScaledFloat, png::ScaledFloat)| {
                Chromaticity::new(p.0.into_value() as f64, p.1.into_value() as f64)
            };
            let primaries = Primaries {
                red: at(c.red),
                green: at(c.green),
                blue: at(c.blue),
                white: at(c.white),
            };
            if primaries.rgb_to_xyz().is_none() {
                return (None, Some("a cHRM chunk with degenerate primaries"));
            }
            primaries
        }
    };
    (Some(ImageDescription::new(primaries, transfer)), None)
}

pub(crate) fn read_png(bytes: &[u8]) -> Result<Decoded, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Palettes and low bit depths become 8 bit samples, tRNS becomes alpha.
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let (description, ignored) = png_description(reader.info());
    let size = reader
        .output_buffer_size()
        .ok_or("the image is too large")?;
    {
        let info = reader.info();
        if info.width > MAX_SIDE || info.height > MAX_SIDE {
            return Err(format!(
                "{}x{}: larger than {MAX_SIDE} on a side",
                info.width, info.height
            ));
        }
    }
    let mut buffer = vec![0; size];
    let info = reader.next_frame(&mut buffer).map_err(|e| e.to_string())?;
    buffer.truncate(info.buffer_size());
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        other => return Err(format!("PNG color type {other:?}")),
    };
    // Any layout of samples to R, G, B, A.
    fn expand<T: Copy>(samples: &[T], channels: usize, opaque: T) -> Vec<T> {
        let mut out = Vec::with_capacity(samples.len() / channels * 4);
        for p in samples.chunks_exact(channels) {
            out.extend_from_slice(&match channels {
                1 => [p[0], p[0], p[0], opaque],
                2 => [p[0], p[0], p[0], p[1]],
                3 => [p[0], p[1], p[2], opaque],
                _ => [p[0], p[1], p[2], p[3]],
            });
        }
        out
    }
    let pixels = match info.bit_depth {
        png::BitDepth::Eight => Pixels::Rgba8(expand(&buffer, channels, u8::MAX)),
        png::BitDepth::Sixteen => {
            let samples: Vec<u16> = buffer
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_be_bytes(*c))
                .collect();
            Pixels::Rgba16(expand(&samples, channels, u16::MAX))
        }
        other => return Err(format!("PNG bit depth {other:?}")),
    };
    Ok(Decoded {
        width: info.width,
        height: info.height,
        pixels,
        description,
        ignored,
    })
}

// ----- Radiance RGBE -------------------------------------------------------------

/// One RGBE pixel as linear RGB.
fn rgbe(p: [u8; 4], scale: f32) -> [f32; 3] {
    if p[3] == 0 {
        return [0.0; 3];
    }
    // The mantissas are the lower edges of their intervals.
    let f = 2.0f32.powi(p[3] as i32 - 136) * scale;
    [
        (p[0] as f32 + 0.5) * f,
        (p[1] as f32 + 0.5) * f,
        (p[2] as f32 + 0.5) * f,
    ]
}

/// Reads a Radiance picture (`#?RADIANCE`, `FORMAT=32-bit_rle_rgbe`):
/// run-length encoded or flat scanlines, rows from the top or the bottom.
pub(crate) fn read_hdr(bytes: &[u8]) -> Result<Decoded, String> {
    let mut pos = 0;
    let mut line = || -> Result<&str, String> {
        let rest = &bytes[pos..];
        let end = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or("a header that does not end")?;
        pos += end + 1;
        std::str::from_utf8(&rest[..end]).map_err(|_| "a header that is not text".to_string())
    };
    if !line()?.starts_with("#?") {
        return Err("not a Radiance picture (no #? line)".into());
    }
    let mut exposure = 1.0f32;
    loop {
        let text = line()?.trim();
        if text.is_empty() {
            break;
        }
        if let Some(format) = text.strip_prefix("FORMAT=") {
            if format.trim() != "32-bit_rle_rgbe" {
                return Err(format!(
                    "the format {format:?} (only 32-bit_rle_rgbe is read)"
                ));
            }
        } else if let Some(value) = text.strip_prefix("EXPOSURE=") {
            // The values were multiplied by every exposure listed.
            let value: f32 = value.trim().parse().map_err(|_| "a bad EXPOSURE line")?;
            if value.is_finite() && value > 0.0 {
                exposure *= value;
            }
        }
    }
    let resolution: Vec<&str> = line()?.split_whitespace().collect();
    let (flip_y, height, flip_x, width) = match resolution.as_slice() {
        [y @ ("-Y" | "+Y"), h, x @ ("+X" | "-X"), w] => (
            *y == "+Y",
            h.parse::<u32>().map_err(|_| "a bad resolution line")?,
            *x == "-X",
            w.parse::<u32>().map_err(|_| "a bad resolution line")?,
        ),
        _ => return Err("a resolution line that is not `-Y height +X width`".into()),
    };
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(format!(
            "{width}x{height}: empty, or larger than {MAX_SIDE} on a side"
        ));
    }
    let (w, h) = (width as usize, height as usize);
    let truncated = || "pixel data that ends early".to_string();
    let mut data = &bytes[pos..];
    let mut take = |n: usize| -> Result<&[u8], String> {
        if data.len() < n {
            return Err(truncated());
        }
        let (head, rest) = data.split_at(n);
        data = rest;
        Ok(head)
    };
    let scale = 1.0 / exposure;
    let mut values = vec![0.0f32; w * h * 3];
    let mut scanline = vec![[0u8; 4]; w];
    for row in 0..h {
        let head: [u8; 4] = take(4)?.try_into().unwrap();
        let rle = (8..=0x7fff).contains(&w)
            && head[0] == 2
            && head[1] == 2
            && ((head[2] as usize) << 8 | head[3] as usize) == w;
        if rle {
            // Each channel on its own, in runs.
            for channel in 0..4 {
                let mut x = 0;
                while x < w {
                    let count = take(1)?[0] as usize;
                    if count > 128 {
                        let count = count - 128;
                        let value = take(1)?[0];
                        if x + count > w {
                            return Err("a run that leaves its scanline".into());
                        }
                        for pixel in &mut scanline[x..x + count] {
                            pixel[channel] = value;
                        }
                        x += count;
                    } else {
                        if count == 0 || x + count > w {
                            return Err("a run that leaves its scanline".into());
                        }
                        for (pixel, value) in scanline[x..x + count].iter_mut().zip(take(count)?) {
                            pixel[channel] = *value;
                        }
                        x += count;
                    }
                }
            }
        } else {
            // Flat pixels. A pixel (1, 1, 1, n) repeats the one before it.
            let mut x = 0;
            let mut next = Some(head);
            let mut shift = 0;
            while x < w {
                let p: [u8; 4] = match next.take() {
                    Some(p) => p,
                    None => take(4)?.try_into().unwrap(),
                };
                if p[0] == 1 && p[1] == 1 && p[2] == 1 && x > 0 {
                    let count = (p[3] as usize) << shift;
                    if x + count > w {
                        return Err("a run that leaves its scanline".into());
                    }
                    let previous = scanline[x - 1];
                    scanline[x..x + count].fill(previous);
                    x += count;
                    shift += 8;
                } else {
                    scanline[x] = p;
                    x += 1;
                    shift = 0;
                }
            }
        }
        let y = if flip_y { h - 1 - row } else { row };
        for (x, p) in scanline.iter().enumerate() {
            let x = if flip_x { w - 1 - x } else { x };
            values[(y * w + x) * 3..][..3].copy_from_slice(&rgbe(*p, scale));
        }
    }
    Ok(Decoded {
        width,
        height,
        pixels: Pixels::Float {
            channels: 3,
            values,
        },
        // Radiance pictures hold linear values; their primaries are those
        // of BT.709 unless a PRIMARIES line says otherwise (not read).
        description: Some(ImageDescription::new(
            Primaries::SRGB,
            TransferFunction::Linear,
        )),
        ignored: None,
    })
}

// ----- raw floats ----------------------------------------------------------------

pub(crate) fn read_raw(
    bytes: &[u8],
    width: u32,
    height: u32,
    channels: u32,
) -> Result<Decoded, String> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err(format!(
            "{width}x{height}: empty, or larger than {MAX_SIDE} on a side"
        ));
    }
    if !(1..=4).contains(&channels) {
        return Err(format!("{channels} channels (1 to 4 are read)"));
    }
    let expected = width as usize * height as usize * channels as usize * 4;
    if bytes.len() != expected {
        return Err(format!(
            "{} bytes, but {width}x{height} pixels of {channels} f32 are {expected}",
            bytes.len()
        ));
    }
    Ok(Decoded {
        width,
        height,
        pixels: Pixels::Float {
            channels,
            values: bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect(),
        },
        description: None,
        ignored: None,
    })
}

// ----- to a texture ----------------------------------------------------------------

/// Every pixel as four floats: R, G, B, A as stored (integers normalized;
/// missing channels are 0, a missing alpha is 1).
fn to_floats(pixels: &Pixels) -> Vec<f32> {
    match pixels {
        Pixels::Rgba8(v) => v.iter().map(|c| *c as f32 / 255.0).collect(),
        Pixels::Rgba16(v) => v.iter().map(|c| *c as f32 / 65535.0).collect(),
        Pixels::Float { channels, values } => {
            let n = *channels as usize;
            let mut out = Vec::with_capacity(values.len() / n * 4);
            for p in values.chunks_exact(n) {
                let at = |i: usize, missing: f32| p.get(i).copied().unwrap_or(missing);
                out.extend_from_slice(&[at(0, 0.0), at(1, 0.0), at(2, 0.0), at(3, 1.0)]);
            }
            out
        }
    }
}

/// The pixels as premultiplied linear light in `working`, four floats each.
pub(crate) fn to_working(
    pixels: &Pixels,
    description: &ImageDescription,
    working: Primaries,
) -> Result<Vec<f32>, String> {
    let transform = DecodeTransform::new(
        description,
        AlphaMode::Straight,
        &CompositeSpace { primaries: working },
    )
    .ok_or("degenerate primaries or luminances")?;
    let m = transform.matrix;
    // Integer samples through a table of the transfer function; what mixes
    // the channels before the matrix (the OOTF of HLG) goes the long way.
    let table = |max: usize| -> Vec<f32> {
        (0..=max)
            .map(|v| {
                transform
                    .transfer
                    .decode_with(v as f32 / max as f32, transform.transfer_param)
            })
            .collect()
    };
    let finish = |linear: [f32; 3], a: f32| {
        [
            (m[0][0] * linear[0] + m[0][1] * linear[1] + m[0][2] * linear[2]) * a,
            (m[1][0] * linear[0] + m[1][1] * linear[1] + m[1][2] * linear[2]) * a,
            (m[2][0] * linear[0] + m[2][1] * linear[1] + m[2][2] * linear[2]) * a,
            a,
        ]
    };
    Ok(match pixels {
        Pixels::Rgba8(v) if transform.ootf.is_none() => {
            let table = table(255);
            v.as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| {
                    finish(
                        [
                            table[p[0] as usize],
                            table[p[1] as usize],
                            table[p[2] as usize],
                        ],
                        p[3] as f32 / 255.0,
                    )
                })
                .collect()
        }
        Pixels::Rgba16(v) if transform.ootf.is_none() => {
            let table = table(65535);
            v.as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| {
                    finish(
                        [
                            table[p[0] as usize],
                            table[p[1] as usize],
                            table[p[2] as usize],
                        ],
                        p[3] as f32 / 65535.0,
                    )
                })
                .collect()
        }
        other => to_floats(other)
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| transform.apply(*p))
            .collect(),
    })
}

/// Four floats per pixel as the bytes of `format`.
pub(crate) fn pack(rgba: &[f32], format: Format) -> Result<Vec<u8>, String> {
    let unorm = |v: f32, max: f32| (v.clamp(0.0, 1.0) * max + 0.5).floor();
    let pixels = rgba.as_chunks::<4>().0.iter();
    Ok(match format {
        Format::Rgba32Float => rgba.iter().flat_map(|v| v.to_ne_bytes()).collect(),
        Format::Rgba16Float => rgba
            .iter()
            .flat_map(|v| f32_to_half(*v).to_ne_bytes())
            .collect(),
        Format::R32Float => pixels.flat_map(|p| p[0].to_ne_bytes()).collect(),
        Format::R16Float => pixels
            .flat_map(|p| f32_to_half(p[0]).to_ne_bytes())
            .collect(),
        Format::Rg16Float => pixels
            .flat_map(|p| [f32_to_half(p[0]), f32_to_half(p[1])])
            .flat_map(u16::to_ne_bytes)
            .collect(),
        Format::Rgba8Unorm => rgba.iter().map(|v| unorm(*v, 255.0) as u8).collect(),
        Format::R8Unorm => pixels.map(|p| unorm(p[0], 255.0) as u8).collect(),
        Format::Rg8Unorm => pixels
            .flat_map(|p| [unorm(p[0], 255.0) as u8, unorm(p[1], 255.0) as u8])
            .collect(),
        Format::Rgba16Unorm => rgba
            .iter()
            .flat_map(|v| (unorm(*v, 65535.0) as u16).to_ne_bytes())
            .collect(),
        other => {
            return Err(format!(
                "an image cannot be loaded as {} (use rgba32float, rgba16float, r32float, \
                 r16float, rg16float, rgba8unorm, rg8unorm, r8unorm or rgba16unorm)",
                other.name()
            ));
        }
    })
}

/// The file `relative` of the project in `dir`. A path that leaves the
/// project (an absolute one, `..`, a link to the outside) is refused.
pub(super) fn project_file(dir: &Path, relative: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    if relative.is_empty()
        || !path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!(
            "{relative:?}: name a file of the project by its path inside the project"
        ));
    }
    let root = dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let file = root
        .join(path)
        .canonicalize()
        .map_err(|e| format!("{relative}: {e}"))?;
    if !file.starts_with(&root) {
        return Err(format!("{relative}: leads outside the project"));
    }
    Ok(file)
}

fn read_file(project: &Project, relative: &str) -> Result<Vec<u8>, String> {
    let Project::Dir(dir) = project else {
        return Err(format!("{relative}: the built-in project has no files"));
    };
    let file = project_file(dir, relative)?;
    let size = std::fs::metadata(&file)
        .map_err(|e| format!("{relative}: {e}"))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(format!(
            "{relative}: {size} bytes; the limit is {MAX_FILE_BYTES}"
        ));
    }
    std::fs::read(&file).map_err(|e| format!("{relative}: {e}"))
}

/// What `hz.load_image` was asked for.
struct Request {
    data: bool,
    space: Value,
    format: Option<Format>,
    raw: Option<(u32, u32, u32)>,
    mipmaps: bool,
}

fn request(options: Option<&Table>) -> mlua::Result<Request> {
    let Some(options) = options else {
        return Ok(Request {
            data: false,
            space: Value::Nil,
            format: None,
            raw: None,
            mipmaps: false,
        });
    };
    let raw = match options.get::<Option<Table>>("raw")? {
        Some(raw) => Some((
            raw.get::<u32>("width")?,
            raw.get::<u32>("height")?,
            raw.get::<Option<u32>>("channels")?.unwrap_or(4),
        )),
        None => None,
    };
    let format = match options.get::<Option<String>>("format")? {
        Some(name) => Some(
            Format::from_name(&name)
                .ok_or_else(|| arg_error(format!("unknown format {name:?}")))?,
        ),
        None => None,
    };
    let space = options.get::<Value>("space")?;
    Ok(Request {
        // Raw values are data unless the script says what colors they are.
        data: options
            .get::<Option<bool>>("data")?
            .unwrap_or(raw.is_some() && space.is_nil()),
        space,
        format,
        raw,
        mipmaps: options.get::<Option<bool>>("mipmaps")?.unwrap_or(false),
    })
}

/// Reads and converts: the format, the size and the bytes to upload, and
/// what the values are (`"data"`, or the name of the color space read).
fn prepare(
    project: &Project,
    path: &str,
    request: &Request,
    working: Primaries,
) -> mlua::Result<(Format, u32, u32, Vec<u8>, String)> {
    let fail = |message: String| arg_error(format!("{path}: {message}"));
    let bytes = read_file(project, path).map_err(arg_error)?;
    let decoded = if let Some((width, height, channels)) = request.raw {
        read_raw(&bytes, width, height, channels)
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        read_png(&bytes)
    } else if bytes.starts_with(b"#?") {
        read_hdr(&bytes)
    } else {
        Err("neither a PNG nor a Radiance picture (for raw f32 values, give `raw`)".into())
    }
    .map_err(fail)?;
    if let Some(ignored) = decoded.ignored
        && !request.data
        && request.space.is_nil()
    {
        tracing::warn!("{path}: has {ignored}; read as content without color information");
    }
    let (format, bytes, meaning) = if request.data {
        match (&decoded.pixels, request.format) {
            (Pixels::Rgba8(v), None | Some(Format::Rgba8Unorm)) => {
                (Format::Rgba8Unorm, v.clone(), "data")
            }
            (Pixels::Rgba16(v), None | Some(Format::Rgba16Unorm)) => (
                Format::Rgba16Unorm,
                v.iter().flat_map(|c| c.to_ne_bytes()).collect(),
                "data",
            ),
            (pixels, format) => {
                let format = format.unwrap_or(match pixels {
                    Pixels::Float { channels: 1, .. } => Format::R32Float,
                    _ => Format::Rgba32Float,
                });
                (
                    format,
                    pack(&to_floats(pixels), format).map_err(fail)?,
                    "data",
                )
            }
        }
    } else {
        let description = match &request.space {
            Value::Nil => decoded
                .description
                .unwrap_or(proto_wl::DEFAULT_SURFACE_IMAGE_DESCRIPTION),
            space => {
                super::color_api::description(space, proto_wl::DEFAULT_SURFACE_IMAGE_DESCRIPTION)?
            }
        };
        let format = request.format.unwrap_or(Format::Rgba16Float);
        if !matches!(format, Format::Rgba16Float | Format::Rgba32Float) {
            return Err(fail(format!(
                "a color image is loaded as rgba16float or rgba32float, not {} \
                 (with `data = true`, any format)",
                format.name()
            )));
        }
        let rgba = to_working(&decoded.pixels, &description, working).map_err(fail)?;
        (
            format,
            pack(&rgba, format).map_err(fail)?,
            description.name().unwrap_or("custom"),
        )
    };
    Ok((
        format,
        decoded.width,
        decoded.height,
        bytes,
        meaning.to_owned(),
    ))
}

/// Creates the image, with every mip level when `mipmaps`, fills it and
/// returns it with a texture slot and its number of mip levels. The levels
/// below the first are scaled down on the GPU, from values that are already
/// what a shader reads (for a color image: linear light).
fn upload(
    gpu: &mut Gpu,
    format: Format,
    width: u32,
    height: u32,
    bytes: &[u8],
    mipmaps: bool,
) -> Result<(Handle, Handle, u32), proto_gfx_core::GfxError> {
    let mip_levels = if mipmaps {
        width.max(height).bit_width()
    } else {
        1
    };
    let image = gpu.create_image_with(
        &ImageDesc {
            format,
            width,
            height,
            usage: ImageUsage::SAMPLED | ImageUsage::COPY_DST | ImageUsage::COPY_SRC,
        },
        &ImageExtra {
            mip_levels,
            ..ImageExtra::default()
        },
    )?;
    let filled = (|| {
        if !gpu.is_recording() {
            gpu.begin_frame()?;
        }
        gpu.upload_image(image, bytes)?;
        if mip_levels > 1 {
            gpu.generate_mipmaps(image)?;
        }
        gpu.create_texture_slot(image)
    })();
    match filled {
        Ok(slot) => Ok((image, slot, mip_levels)),
        Err(error) => {
            let _ = gpu.destroy(image);
            Err(error)
        }
    }
}

pub(super) fn register(
    lua: &Lua,
    hz: &Table,
    shared: &SharedRef,
    gpu: &Rc<RefCell<Gpu>>,
) -> mlua::Result<()> {
    // hz.load_image(path, options): a file of the project (a path inside
    // it) as a texture the script owns. Returns { image, texture, width,
    // height, format, mip_levels, space, filterable }.
    //
    // options.data: true to upload the values as stored (no conversion).
    // options.space: what the stored values mean, when not what the file
    //   says (see `color_api::description`).
    // options.format: the format of the texture.
    // options.raw = { width, height, channels }: the file is raw
    //   little-endian f32 values.
    // options.mipmaps: true to make every mip level (outside a pass).
    let state = Rc::clone(shared);
    let gpu = Rc::clone(gpu);
    hz.set(
        "load_image",
        lua.create_function(move |lua, (path, options): (String, Option<Table>)| {
            let request = request(options.as_ref())?;
            let (project, working) = {
                let shared = state.borrow();
                (shared.project.clone(), shared.working)
            };
            let (format, width, height, bytes, meaning) =
                prepare(&project, &path, &request, working)?;
            let mut gpu = gpu.borrow_mut();
            let caps = gpu.format_caps(format);
            if !caps.sampled {
                return Err(arg_error(format!(
                    "{path}: this device cannot sample {}",
                    format.name()
                )));
            }
            let (image, slot, mip_levels) =
                upload(&mut gpu, format, width, height, &bytes, request.mipmaps)
                    .map_err(mlua::Error::external)?;
            let t = lua.create_table()?;
            t.set("image", image.to_bits())?;
            t.set("texture", slot.to_bits())?;
            t.set("width", width)?;
            t.set("height", height)?;
            t.set("format", format.name())?;
            t.set("mip_levels", mip_levels)?;
            t.set("space", meaning)?;
            t.set("filterable", caps.filter_linear)?;
            Ok(t)
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(
        color: png::ColorType,
        depth: png::BitDepth,
        size: (u32, u32),
        data: &[u8],
        setup: impl FnOnce(&mut png::Encoder<&mut Vec<u8>>),
        chunks: &[(&[u8; 4], &[u8])],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, size.0, size.1);
        encoder.set_color(color);
        encoder.set_depth(depth);
        setup(&mut encoder);
        let mut writer = encoder.write_header().unwrap();
        for (name, data) in chunks {
            writer
                .write_chunk(png::chunk::ChunkType(**name), data)
                .unwrap();
        }
        writer.write_image_data(data).unwrap();
        drop(writer);
        out
    }

    /// The layouts of PNG samples, and what a PNG says about its colors.
    #[test]
    fn png_pictures() {
        use png::{BitDepth, ColorType};
        let default = proto_wl::DEFAULT_SURFACE_IMAGE_DESCRIPTION;

        // 16 bit RGB with a gamma and chromaticities of its own.
        let adobe = Primaries::ADOBE_RGB;
        let at = |c: Chromaticity| (c.x as f32, c.y as f32);
        let bytes = png_bytes(
            ColorType::Rgb,
            BitDepth::Sixteen,
            (1, 1),
            &[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc],
            |e| {
                e.set_source_gamma(png::ScaledFloat::new(1.0 / 1.8));
                e.set_source_chromaticities(png::SourceChromaticities::new(
                    at(adobe.white),
                    at(adobe.red),
                    at(adobe.green),
                    at(adobe.blue),
                ));
            },
            &[],
        );
        let decoded = read_png(&bytes).unwrap();
        assert_eq!(
            decoded.pixels,
            Pixels::Rgba16(vec![0x1234, 0x5678, 0x9abc, 0xffff])
        );
        let description = decoded.description.unwrap();
        let TransferFunction::Power(exponent) = description.transfer else {
            panic!("{:?}", description.transfer);
        };
        assert!((exponent - 1.8).abs() < 1e-3, "{exponent}");
        assert!((description.primaries.green.y - adobe.green.y).abs() < 1e-4);

        // A palette with transparency.
        let bytes = png_bytes(
            ColorType::Indexed,
            BitDepth::Two,
            (2, 1),
            &[0b0110_0000],
            |e| {
                e.set_palette(vec![1, 2, 3, 10, 20, 30, 100, 110, 120]);
                e.set_trns(vec![255, 128]);
            },
            &[],
        );
        let decoded = read_png(&bytes).unwrap();
        assert_eq!(
            decoded.pixels,
            Pixels::Rgba8(vec![10, 20, 30, 128, 100, 110, 120, 255])
        );
        assert_eq!(decoded.description, None);

        // Gray with alpha, marked sRGB: read like content without color
        // information.
        let bytes = png_bytes(
            ColorType::GrayscaleAlpha,
            BitDepth::Eight,
            (1, 1),
            &[77, 200],
            |e| e.set_source_srgb(png::SrgbRenderingIntent::Perceptual),
            &[],
        );
        let decoded = read_png(&bytes).unwrap();
        assert_eq!(decoded.pixels, Pixels::Rgba8(vec![77, 77, 77, 200]));
        assert_eq!(decoded.description, Some(default));

        // cICP: BT.2020 primaries with PQ; and code points nobody knows.
        let pixel = [1u8, 2, 3];
        let cicp = |code: &[u8]| {
            read_png(&png_bytes(
                ColorType::Rgb,
                BitDepth::Eight,
                (1, 1),
                &pixel,
                |_| {},
                &[(b"cICP", code)],
            ))
            .unwrap()
        };
        assert_eq!(
            cicp(&[9, 16, 0, 1]).description,
            Some(ImageDescription::BT2020_PQ)
        );
        let unknown = cicp(&[200, 16, 0, 1]);
        assert_eq!(unknown.description, None);
        assert!(unknown.ignored.is_some());

        assert!(read_png(b"\x89PNG\r\n\x1a\nnot a png").is_err());
    }

    /// A Radiance picture with new-style runs, and the same pixels flat.
    #[test]
    fn radiance_pictures() {
        let (w, h) = (8usize, 2usize);
        let pixel = |x: usize, y: usize| [(x * 30) as u8, (y * 100 + 20) as u8, 7, 128 + y as u8];
        let header =
            |extra: &str| format!("#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n{extra}\n-Y {h} +X {w}\n");
        let mut flat = header("").into_bytes();
        let mut runs = header("EXPOSURE=2.0\n").into_bytes();
        for y in 0..h {
            runs.extend_from_slice(&[2, 2, 0, w as u8]);
            for channel in 0..4 {
                if channel == 0 {
                    // Literals.
                    runs.push(w as u8);
                    runs.extend((0..w).map(|x| pixel(x, y)[0]));
                } else {
                    // One run.
                    runs.extend_from_slice(&[128 + w as u8, pixel(0, y)[channel]]);
                }
            }
            for x in 0..w {
                flat.extend_from_slice(&pixel(x, y));
            }
        }
        let flat = read_hdr(&flat).unwrap();
        let runs = read_hdr(&runs).unwrap();
        assert_eq!((flat.width, flat.height), (8, 2));
        let (Pixels::Float { values: a, .. }, Pixels::Float { values: b, .. }) =
            (&flat.pixels, &runs.pixels)
        else {
            panic!("not floats");
        };
        // (30 + 0.5) * 2^(128 - 136) at x = 1, y = 0; half of it with the
        // exposure of 2 undone.
        assert_eq!(a[3], 30.5 / 256.0);
        assert_eq!(a[(w + 1) * 3 + 1], 120.5 / 128.0);
        for (a, b) in a.iter().zip(b) {
            assert_eq!(*a, b * 2.0);
        }
        assert!(read_hdr(b"#?RADIANCE\n\n-Y 1 +X 1\n").is_err());
        assert!(read_hdr(b"P6\n").is_err());
    }

    #[test]
    fn raw_floats() {
        let values = [0.25f32, 1.5, -2.0, 1.0e-3, 4.0, 5.0];
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let decoded = read_raw(&bytes, 3, 1, 2).unwrap();
        assert_eq!(
            to_floats(&decoded.pixels),
            [
                0.25, 1.5, 0.0, 1.0, -2.0, 1.0e-3, 0.0, 1.0, 4.0, 5.0, 0.0, 1.0
            ]
        );
        assert!(read_raw(&bytes, 3, 1, 3).is_err());
        assert!(read_raw(&bytes, 0, 1, 2).is_err());
        let packed = pack(&to_floats(&decoded.pixels), Format::R32Float).unwrap();
        assert_eq!(packed.len(), 12);
        assert_eq!(f32::from_ne_bytes(packed[4..8].try_into().unwrap()), -2.0);
    }

    /// The table for integer samples gives what the transform gives.
    #[test]
    fn integer_samples_match_the_transform() {
        let description = ImageDescription::DISPLAY_P3;
        let working = Primaries::ACES_AP1;
        let pixels = Pixels::Rgba8(vec![200, 100, 50, 128, 0, 255, 17, 255]);
        let converted = to_working(&pixels, &description, working).unwrap();
        let transform = DecodeTransform::new(
            &description,
            AlphaMode::Straight,
            &CompositeSpace { primaries: working },
        )
        .unwrap();
        for (i, p) in [[200u8, 100, 50, 128], [0, 255, 17, 255]]
            .iter()
            .enumerate()
        {
            let expected = transform.apply(p.map(|v| v as f32 / 255.0));
            for c in 0..4 {
                assert!(
                    (converted[i * 4 + c] - expected[c]).abs() <= 1e-6,
                    "{converted:?} {expected:?}"
                );
            }
        }
    }

    #[test]
    fn paths_stay_inside_the_project() {
        let dir = std::env::temp_dir().join(format!("hz-images-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project/textures")).unwrap();
        std::fs::write(dir.join("project/textures/a.bin"), [0u8; 4]).unwrap();
        std::fs::write(dir.join("outside.bin"), [0u8; 4]).unwrap();
        let project = dir.join("project");
        assert!(project_file(&project, "textures/a.bin").is_ok());
        assert!(project_file(&project, "./textures/a.bin").is_ok());
        for bad in [
            "../outside.bin",
            "/etc/hostname",
            "textures/../../outside.bin",
            "",
        ] {
            assert!(project_file(&project, bad).is_err(), "{bad}");
        }
        std::os::unix::fs::symlink(dir.join("outside.bin"), project.join("link.bin")).unwrap();
        assert!(project_file(&project, "link.bin").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
