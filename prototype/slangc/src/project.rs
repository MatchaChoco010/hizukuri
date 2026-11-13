//! The shaders of a project: their names, where their SPIR-V goes, and
//! building it.
//!
//! A project keeps its Slang sources under `shaders/`. A script names one by
//! that path and an entry point (`shaders/surface.slang`, `vs_main`). The
//! SPIR-V of each entry point is a file of its own:
//! `<output>/surface.slang.vs_main.spv`, where `<output>` is the project's
//! `.build/shaders/` (written by `proto-compositor build-shaders`), or a
//! directory of the build for the built-in project. The compositor reads
//! SPIR-V only; it never compiles.
//!
//! A source without an entry point is a module of the project: the other
//! sources of its directory import it by its file name (`import ltc;` for
//! `ltc.slang`), and nothing is compiled from it alone.
//!
//! Build scripts use it for the shaders a crate carries (the compositor's
//! built-in project, the examples of `proto-gfx-lua`), and
//! `proto-compositor build-shaders` for any project.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{Compiler, Input};

/// Where a project's Slang sources are, relative to the project.
pub const SOURCE_DIR: &str = "shaders";
/// Where `build-shaders` writes the SPIR-V, relative to the project.
pub const OUTPUT_DIR: &str = ".build/shaders";
/// Where `build-shaders` puts the core's modules, relative to the project.
pub const MODULES_DIR: &str = ".build/modules";

fn valid_part(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Checks the name a script gives a shader (`shaders/<path>.slang`) and
/// returns `<path>.slang`.
pub fn relative_name(shader: &str) -> Result<&str, String> {
    let rel = shader
        .strip_prefix("shaders/")
        .filter(|rel| rel.ends_with(".slang") && rel.split('/').all(valid_part))
        .ok_or_else(|| {
            format!(
                "invalid shader name {shader:?}: name a file of the project's shaders/ \
                 directory, as in \"shaders/surface.slang\""
            )
        })?;
    Ok(rel)
}

/// Checks the name of an entry point.
pub fn check_entry(entry: &str) -> Result<(), String> {
    let mut chars = entry.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if first_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(())
    } else {
        Err(format!("invalid entry point name {entry:?}"))
    }
}

/// The SPIR-V file of one entry point of `rel` (a path under `shaders/`).
pub fn spirv_file(output: &Path, rel: &str, entry: &str) -> PathBuf {
    output.join(format!("{rel}.{entry}.spv"))
}

/// The list of the entry points of `rel`, written when they were built.
fn entries_file(output: &Path, rel: &str) -> PathBuf {
    output.join(format!("{rel}.entries"))
}

/// What one build did.
#[derive(Debug, Default)]
pub struct Built {
    /// `(path under shaders/, entry points)` of every source compiled now.
    pub compiled: Vec<(String, Vec<String>)>,
    /// Sources whose SPIR-V was already newer than every source.
    pub up_to_date: Vec<(String, Vec<String>)>,
    pub warnings: Vec<String>,
}

impl Built {
    /// Every `(path under shaders/, entry point)` the output holds.
    pub fn all(&self) -> impl Iterator<Item = (&str, &str)> {
        self.compiled
            .iter()
            .chain(&self.up_to_date)
            .flat_map(|(rel, entries)| entries.iter().map(move |e| (rel.as_str(), e.as_str())))
    }
}

/// The `.slang` files beneath `dir`, as paths relative to it, sorted.
fn slang_files(dir: &Path) -> Result<Vec<String>, String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
        let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
            if path.is_dir() {
                walk(base, &path, out)?;
            } else if path.extension().is_some_and(|e| e == "slang") {
                let rel = path.strip_prefix(base).expect("beneath base");
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    if dir.is_dir() {
        walk(dir, dir, &mut out)?;
    }
    out.sort();
    Ok(out)
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Compiles every `.slang` under `<project>/shaders/` to SPIR-V in `output`,
/// one file per entry point. A source is compiled again only when one of its
/// files is missing, or older than some `.slang` of the project or of the
/// include directories (a change to a module can change any shader). Output
/// of sources that are gone is removed. Errors come as one message each, with
/// the file, line and column.
pub fn build(project: &Path, output: &Path, compiler: &Compiler) -> Result<Built, Vec<String>> {
    let sources_dir = project.join(SOURCE_DIR);
    let sources = slang_files(&sources_dir).map_err(|e| vec![e])?;
    let mut built = Built::default();
    if sources.is_empty() && !output.exists() {
        return Ok(built);
    }
    std::fs::create_dir_all(output).map_err(|e| vec![format!("{}: {e}", output.display())])?;

    // The newest of every source the shaders can read.
    let mut newest = None;
    let mut consider = |path: &Path| {
        let time = modified(path);
        if time > newest {
            newest = time;
        }
    };
    for rel in &sources {
        consider(&sources_dir.join(rel));
    }
    for dir in &compiler.include_dirs {
        for rel in slang_files(dir).map_err(|e| vec![e])? {
            consider(&dir.join(rel));
        }
    }

    let mut errors = Vec::new();
    for rel in &sources {
        let listed = std::fs::read_to_string(entries_file(output, rel)).ok();
        if let Some(listed) = &listed {
            let entries: Vec<String> = listed.lines().map(str::to_owned).collect();
            let fresh = entries.iter().all(|entry| {
                modified(&spirv_file(output, rel, entry)).is_some_and(|t| Some(t) >= newest)
            }) && modified(&entries_file(output, rel))
                .is_some_and(|t| Some(t) >= newest);
            if fresh {
                built.up_to_date.push((rel.clone(), entries));
                continue;
            }
        }
        match compile_one(&sources_dir.join(rel), rel, output, compiler) {
            Ok((entries, warnings)) => {
                built.warnings.extend(warnings);
                built.compiled.push((rel.clone(), entries));
            }
            Err(e) => errors.push(e),
        }
    }
    remove_stale(output, &sources);
    if errors.is_empty() {
        Ok(built)
    } else {
        Err(errors)
    }
}

/// Compiles every entry point of one source. Its old output goes first, so
/// that an entry point that was removed from the source does not stay.
fn compile_one(
    path: &Path,
    rel: &str,
    output: &Path,
    compiler: &Compiler,
) -> Result<(Vec<String>, Vec<String>), String> {
    let target_dir = spirv_file(output, rel, "x")
        .parent()
        .expect("in the output")
        .to_path_buf();
    std::fs::create_dir_all(&target_dir).map_err(|e| format!("{}: {e}", target_dir.display()))?;
    remove_output_of(output, rel);
    let scratch = output.join(format!("{rel}.reflection.json"));
    let entries = compiler
        .entry_points(path, &scratch)
        .map_err(|e| e.to_string())?;
    // A source without an entry point is a module of the project: the other
    // sources import it (`import name;` finds it next to them), and nothing
    // is compiled from it alone. Its empty list still records that it was
    // looked at.
    let mut warnings = Vec::new();
    for (entry, stage) in &entries {
        let compiled = compiler
            .compile(Input::File(path), *stage, entry)
            .map_err(|e| e.to_string())?;
        warnings.extend(compiled.warnings.iter().map(|w| w.to_string()));
        let file = spirv_file(output, rel, entry);
        std::fs::write(&file, compiled.spirv).map_err(|e| format!("{}: {e}", file.display()))?;
    }
    let names: Vec<String> = entries.into_iter().map(|(name, _)| name).collect();
    let list = entries_file(output, rel);
    let text: String = names.iter().map(|name| format!("{name}\n")).collect();
    std::fs::write(&list, text).map_err(|e| format!("{}: {e}", list.display()))?;
    Ok((names, warnings))
}

/// The source a file of the output belongs to (`a/b.slang` for
/// `a/b.slang.main.spv` and `a/b.slang.entries`).
fn source_of(file: &str) -> Option<&str> {
    let end = file.find(".slang.")? + ".slang".len();
    Some(&file[..end])
}

fn remove_output_of(output: &Path, rel: &str) {
    let mut stale = Vec::new();
    collect_files(output, output, &mut stale);
    for file in stale {
        if source_of(&file) == Some(rel) {
            let _ = std::fs::remove_file(output.join(file));
        }
    }
}

fn remove_stale(output: &Path, sources: &[String]) {
    let mut files = Vec::new();
    collect_files(output, output, &mut files);
    for file in files {
        if source_of(&file).is_none_or(|rel| !sources.iter().any(|s| s == rel)) {
            let _ = std::fs::remove_file(output.join(file));
        }
    }
}

fn collect_files(base: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(base, &path, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}
