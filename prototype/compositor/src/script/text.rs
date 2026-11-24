//! `hz.load_font` and `hz.render_text`: a line of text as a texture.
//!
//! The core turns a string into coverage; where it goes on the screen, in
//! what color, and when it is drawn again is the script's.
//!
//! A font is a file: one of the project (`hz.load_font("fonts/x.ttf")`), or
//! the session's (`hz.load_font()`), which is the file the session was
//! started with (`--font`), or else the one fontconfig picks for
//! sans-serif, or else the first of a few well-known fonts found where
//! systems keep them. A session without any font has no text:
//! `hz.load_font()` returns nil then, and a project has to do without.

use std::cell::RefCell;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use fontdue::layout::{CoordinateSystem, Layout, LayoutSettings, TextStyle};
use mlua::{Lua, Table};
use proto_gfx_core::{Format, Gpu, ImageDesc, ImageUsage};

use super::Project;
use super::api::{SharedRef, arg_error};

/// The largest font file read.
const MAX_FONT_BYTES: u64 = 64 << 20;
/// The largest size of text in pixels, and the largest side of its texture.
const MAX_SIZE: f32 = 512.0;
const MAX_SIDE: u32 = 8192;
/// The longest string rendered at once, in bytes.
const MAX_TEXT_BYTES: usize = 4096;

/// How fontconfig is asked for the session's font: its `fc-match`, which
/// answers with the file and the face of the best match. The family the
/// person's configuration (and locale) chooses for sans-serif first; when
/// that one cannot be read (a collection of faces, such as many CJK fonts),
/// the one it chooses for English text.
const FONTCONFIG: &[&str] = &["fc-match", "--format", "%{file}\t%{index}\n"];
const FONTCONFIG_PATTERNS: &[&str] = &["sans-serif", "sans-serif:lang=en"];
/// How long fontconfig may take to answer (it may build its cache first).
const FONTCONFIG_TIMEOUT: Duration = Duration::from_secs(5);
/// The files of one TrueType or OpenType face, which fontdue reads.
const FONT_EXTENSIONS: &[&str] = &["ttf", "otf"];

/// Where systems keep fonts, and the files looked for there, in order of
/// preference, when there is no fontconfig.
const FONT_DIRS: &[&str] = &["/usr/share/fonts", "/usr/local/share/fonts"];
const FONT_NAMES: &[&str] = &[
    "DejaVuSans.ttf",
    "NotoSans-Regular.ttf",
    "LiberationSans-Regular.ttf",
    "Roboto-Regular.ttf",
    "FreeSans.ttf",
];

/// The first of `names` (in their order) found beneath `dirs`, looking a
/// few directories deep.
pub(crate) fn find_font(dirs: &[&Path], names: &[&str]) -> Option<PathBuf> {
    fn walk(dir: &Path, depth: u32, budget: &mut u32, found: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if *budget == 0 {
                return;
            }
            *budget -= 1;
            let path = entry.path();
            if path.is_dir() {
                if depth > 0 {
                    walk(&path, depth - 1, budget, found);
                }
            } else {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    let mut budget = 50_000;
    for dir in dirs {
        walk(dir, 4, &mut budget, &mut found);
    }
    names.iter().find_map(|name| {
        found
            .iter()
            .find(|path| path.file_name().is_some_and(|f| f == *name))
            .cloned()
    })
}

/// The session's font when it was given none: the one fontconfig picks, or
/// else the first well-known one. Looked up once, when it is first asked
/// for.
fn system_font() -> Option<PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            FONTCONFIG_PATTERNS
                .iter()
                .find_map(|pattern| {
                    let mut command = FONTCONFIG.to_vec();
                    command.push(pattern);
                    usable_font(&answer(&command, FONTCONFIG_TIMEOUT)?)
                })
                .or_else(|| {
                    let dirs: Vec<&Path> = FONT_DIRS.iter().map(Path::new).collect();
                    find_font(&dirs, FONT_NAMES)
                })
        })
        .clone()
}

/// What `command` prints. `None` when the program is missing, fails, or
/// does not end within `timeout` (it is killed then).
pub(crate) fn answer(command: &[&str], timeout: Duration) -> Option<String> {
    let (program, arguments) = command.split_first()?;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::spawn::prepare(&mut command, &[]);
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    // Read on another thread, so that the pipe never fills while this one
    // waits for the program to end.
    let reader = std::thread::Builder::new()
        .name("fontconfig".into())
        .spawn(move || {
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            text
        })
        .ok()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!("{program} did not answer within {timeout:?}");
                return None;
            }
        }
    };
    let text = reader.join().ok()?;
    status.success().then_some(text)
}

/// The file fontconfig answered with (`file<TAB>index`), when fontdue reads
/// it: a file of one TrueType or OpenType face, there. The index holds the
/// face in its low 16 bits and a named instance of a variable font above
/// them; fontdue draws the default instance.
pub(crate) fn usable_font(answer: &str) -> Option<PathBuf> {
    let (file, index) = answer.lines().next()?.split_once('\t')?;
    let path = Path::new(file);
    let one_face = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| FONT_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)));
    let first_face = index.trim().parse::<u32>().is_ok_and(|i| i & 0xffff == 0);
    (one_face && first_face && path.is_file()).then(|| path.to_path_buf())
}

fn read_font(path: &Path) -> Result<fontdue::Font, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if size > MAX_FONT_BYTES {
        return Err(format!(
            "{}: {size} bytes; the limit is {MAX_FONT_BYTES}",
            path.display()
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Coverage of `text` set in `font` at `size` pixels: width, height, and one
/// byte per pixel, top row first. `None` when nothing is covered.
pub(crate) fn rasterize(
    font: &fontdue::Font,
    text: &str,
    size: f32,
    max_width: Option<f32>,
) -> Option<(u32, u32, Vec<u8>)> {
    let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
    layout.reset(&LayoutSettings {
        max_width,
        ..LayoutSettings::default()
    });
    layout.append(&[font], &TextStyle::new(text, size, 0));
    let glyphs = layout.glyphs();
    // Glyphs without an outline (spaces) take room but cover nothing.
    let right = glyphs
        .iter()
        .filter(|g| g.width > 0 && g.height > 0)
        .map(|g| g.x + g.width as f32)
        .fold(0.0f32, f32::max);
    let bottom = glyphs
        .iter()
        .map(|g| g.y + g.height as f32)
        .fold(layout.height(), f32::max);
    let (width, height) = (right.ceil() as u32, bottom.ceil() as u32);
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return None;
    }
    let mut coverage = vec![0u8; width as usize * height as usize];
    for glyph in glyphs {
        if glyph.width == 0 || glyph.height == 0 {
            continue;
        }
        let (metrics, bitmap) = font.rasterize_config(glyph.key);
        let (left, top) = (glyph.x.round() as i64, glyph.y.round() as i64);
        for row in 0..metrics.height {
            let y = top + row as i64;
            if y < 0 || y >= height as i64 {
                continue;
            }
            for column in 0..metrics.width {
                let x = left + column as i64;
                if x < 0 || x >= width as i64 {
                    continue;
                }
                // Glyphs that touch keep the stronger coverage.
                let at = &mut coverage[y as usize * width as usize + x as usize];
                *at = (*at).max(bitmap[row * metrics.width + column]);
            }
        }
    }
    Some((width, height, coverage))
}

pub(super) fn register(
    lua: &Lua,
    hz: &Table,
    shared: &SharedRef,
    gpu: &Rc<RefCell<Gpu>>,
) -> mlua::Result<()> {
    // hz.load_font(path): a font file of the project. hz.load_font(): the
    // session's font, or nil when the session has none. Returns a number
    // that names the font until the script is loaded again.
    {
        let state = Rc::clone(shared);
        hz.set(
            "load_font",
            lua.create_function(move |_, path: Option<String>| {
                let font = match path {
                    Some(path) => {
                        let project = state.borrow().project.clone();
                        let Project::Dir(dir) = &project else {
                            return Err(arg_error(format!(
                                "{path}: the built-in project has no files"
                            )));
                        };
                        let file = super::images::project_file(dir, &path).map_err(arg_error)?;
                        read_font(&file).map_err(arg_error)?
                    }
                    None => {
                        let configured = state.borrow().font.clone();
                        let file = configured.or_else(system_font);
                        let Some(file) = file else {
                            return Ok(None);
                        };
                        match read_font(&file) {
                            Ok(font) => font,
                            Err(error) => {
                                tracing::warn!("the session's font cannot be used: {error}");
                                return Ok(None);
                            }
                        }
                    }
                };
                let mut shared = state.borrow_mut();
                shared.fonts.push(font);
                Ok(Some(shared.fonts.len()))
            })?,
        )?;
    }

    // hz.render_text(font, text, options): the text as a texture the script
    // owns: { image, texture, width, height, format }, or nil when there is
    // nothing to see (an empty string, only spaces).
    //
    // options.size: the size of the text in pixels (16 when left out).
    // options.max_width: where lines are broken, in pixels.
    // options.format: "rgba8unorm" (the default: white with the coverage as
    //   alpha, premultiplied, to be multiplied by a color) or "r8unorm" (the
    //   coverage alone).
    {
        let state = Rc::clone(shared);
        let gpu = Rc::clone(gpu);
        hz.set(
            "render_text",
            lua.create_function(
                move |lua, (font, text, options): (usize, String, Option<Table>)| {
                    let (size, max_width, format) = match &options {
                        Some(options) => (
                            options.get::<Option<f32>>("size")?.unwrap_or(16.0),
                            options.get::<Option<f32>>("max_width")?,
                            options.get::<Option<String>>("format")?,
                        ),
                        None => (16.0, None, None),
                    };
                    if !(size.is_finite() && size > 0.0 && size <= MAX_SIZE) {
                        return Err(arg_error(format!(
                            "the size of text is between 0 and {MAX_SIZE} pixels"
                        )));
                    }
                    if text.len() > MAX_TEXT_BYTES {
                        return Err(arg_error(format!(
                            "text of {} bytes; the limit is {MAX_TEXT_BYTES}",
                            text.len()
                        )));
                    }
                    let format = match format.as_deref() {
                        None | Some("rgba8unorm") => Format::Rgba8Unorm,
                        Some("r8unorm") => Format::R8Unorm,
                        Some(other) => {
                            return Err(arg_error(format!(
                                "text is rendered as rgba8unorm or r8unorm, not {other}"
                            )));
                        }
                    };
                    let rasterized = {
                        let shared = state.borrow();
                        let font = font
                            .checked_sub(1)
                            .and_then(|i| shared.fonts.get(i))
                            .ok_or_else(|| arg_error(format!("unknown font {font}")))?;
                        rasterize(font, &text, size, max_width.filter(|w| w.is_finite()))
                    };
                    let Some((width, height, coverage)) = rasterized else {
                        return Ok(None);
                    };
                    let bytes: Vec<u8> = match format {
                        Format::R8Unorm => coverage,
                        _ => coverage.iter().flat_map(|c| [*c; 4]).collect(),
                    };
                    let mut gpu = gpu.borrow_mut();
                    let image = gpu
                        .create_image(&ImageDesc {
                            format,
                            width,
                            height,
                            usage: ImageUsage::SAMPLED | ImageUsage::COPY_DST,
                        })
                        .map_err(mlua::Error::external)?;
                    let filled = (|| {
                        if !gpu.is_recording() {
                            gpu.begin_frame()?;
                        }
                        gpu.upload_image(image, &bytes)?;
                        gpu.create_texture_slot(image)
                    })();
                    let slot = match filled {
                        Ok(slot) => slot,
                        Err(error) => {
                            let _ = gpu.destroy(image);
                            return Err(mlua::Error::external(error));
                        }
                    };
                    let t = lua.create_table()?;
                    t.set("image", image.to_bits())?;
                    t.set("texture", slot.to_bits())?;
                    t.set("width", width)?;
                    t.set("height", height)?;
                    t.set("format", format.name())?;
                    Ok(Some(t))
                },
            )?,
        )?;
    }

    // hz.missing_glyphs(font, text): how many characters of the text the
    // font has no glyph for (spaces and control characters not counted).
    // A script that gets more than none shows something else (another
    // string, another font) rather than empty boxes.
    {
        let state = Rc::clone(shared);
        hz.set(
            "missing_glyphs",
            lua.create_function(move |_, (font, text): (usize, String)| {
                let shared = state.borrow();
                let font = font
                    .checked_sub(1)
                    .and_then(|i| shared.fonts.get(i))
                    .ok_or_else(|| arg_error(format!("unknown font {font}")))?;
                Ok(missing_glyphs(font, &text))
            })?,
        )?;
    }
    Ok(())
}

fn missing_glyphs(font: &fontdue::Font, text: &str) -> usize {
    text.chars()
        .filter(|c| !c.is_whitespace() && !c.is_control() && font.lookup_glyph_index(*c) == 0)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The font the tests render with: one glyph, a square.
    include!("../../tests/common/square_font.rs");

    #[test]
    fn a_square_glyph_covers_a_square() {
        let font =
            fontdue::Font::from_bytes(square_font(), fontdue::FontSettings::default()).unwrap();
        // 600 units of 1000 at 20 pixels: a square of 12 pixels, 2 pixels in.
        let (width, height, coverage) = rasterize(&font, "A", 20.0, None).unwrap();
        assert!((12..=16).contains(&width), "{width}x{height}");
        let covered = coverage.iter().filter(|c| **c > 200).count();
        assert!(
            (130..=150).contains(&covered),
            "{covered} of {width}x{height}"
        );
        // Two of them: twice as wide, as much again covered.
        let (two, _, coverage) = rasterize(&font, "AA", 20.0, None).unwrap();
        assert!(two >= width + 14, "{two}");
        assert_eq!(coverage.iter().filter(|c| **c > 200).count(), covered * 2);
        // Two lines: as wide as one glyph, twice as high. A width breaks
        // lines the same way.
        let (wide, high, coverage) = rasterize(&font, "A\nA", 20.0, None).unwrap();
        assert_eq!((wide, high), (width, height * 2));
        assert_eq!(coverage.iter().filter(|c| **c > 200).count(), covered * 2);
        let (wide, high, _) = rasterize(&font, "A A", 20.0, Some(30.0)).unwrap();
        assert_eq!((wide, high), (width, height * 2));
        // Nothing to see.
        assert!(rasterize(&font, "", 20.0, None).is_none());
        assert!(rasterize(&font, "b", 20.0, None).is_none());
        // What the font has no glyph for, spaces aside.
        assert_eq!(missing_glyphs(&font, "A A\n"), 0);
        assert_eq!(missing_glyphs(&font, "Ab あ"), 2);
    }

    #[test]
    fn fonts_are_found_by_name_in_order_of_preference() {
        let dir = std::env::temp_dir().join(format!("hz-fonts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/Second.ttf"), b"").unwrap();
        std::fs::write(dir.join("a/b/First.ttf"), b"").unwrap();
        let found = find_font(&[&dir], &["First.ttf", "Second.ttf"]);
        assert_eq!(found, Some(dir.join("a/b/First.ttf")));
        assert_eq!(find_font(&[&dir], &["Third.ttf"]), None);
        assert_eq!(find_font(&[&dir.join("none")], &["First.ttf"]), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_font_fontconfig_answers_with_is_used_when_fontdue_reads_it() {
        let dir = std::env::temp_dir().join(format!("hz-fontconfig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["Sans.OTF", "bitmap.pcf.gz", "collection.ttc"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let answer = |name: &str, index: u32| format!("{}\t{index}\n", dir.join(name).display());
        assert_eq!(
            usable_font(&answer("Sans.OTF", 0)),
            Some(dir.join("Sans.OTF"))
        );
        // A named instance of a variable font: the default one is drawn.
        assert_eq!(
            usable_font(&answer("Sans.OTF", 3 << 16)),
            Some(dir.join("Sans.OTF"))
        );
        // Not a file of one face that fontdue reads, not its first face,
        // not there.
        assert_eq!(usable_font(&answer("bitmap.pcf.gz", 0)), None);
        assert_eq!(usable_font(&answer("collection.ttc", 0)), None);
        assert_eq!(usable_font(&answer("Sans.OTF", 1)), None);
        assert_eq!(usable_font(&answer("gone.ttf", 0)), None);
        assert_eq!(usable_font(""), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fontconfig_is_asked_with_a_time_limit() {
        let timeout = Duration::from_secs(10);
        assert_eq!(answer(&["echo", "a"], timeout).as_deref(), Some("a\n"));
        assert_eq!(answer(&["false"], timeout), None);
        assert_eq!(answer(&["hizukuri-no-such-program"], timeout), None);
        let began = Instant::now();
        assert_eq!(answer(&["sleep", "30"], Duration::from_millis(200)), None);
        assert!(began.elapsed() < Duration::from_secs(10));
    }
}
