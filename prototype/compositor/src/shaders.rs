//! A project's shaders at run time, and the tool that builds them.
//!
//! - [`load`]: what `gfx.create_shader({ shader = ..., entry = ... })` reads.
//!   The compositor never compiles: it reads the SPIR-V that
//!   [`build_project`] wrote into the project, or, for the built-in project,
//!   the SPIR-V that `build.rs` put into the binary.
//! - [`build_project`]: `proto-compositor build-shaders <project>`. Compiles
//!   the project's `shaders/` with `slangc`, giving it the core's modules
//!   (`import color;`).
//!
//! Names and places are in [`proto_slangc::project`].

use std::path::{Path, PathBuf};

use proto_slangc::Compiler;

use crate::script::Project;
use proto_slangc::project::{
    Built, MODULES_DIR, OUTPUT_DIR, build, check_entry, relative_name, spirv_file,
};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_shaders.rs"));
}

/// The SPIR-V of `entry` of `shader` (`shaders/<path>.slang`) for `project`.
///
/// A directory's own shaders come from its `.build/shaders/`. A shader the
/// directory does not have (no such file under `shaders/`) comes from the
/// built-in project, as modules do: a project that includes the built-in
/// `render` gets the built-in drawing's shaders.
pub fn load(project: &Project, shader: &str, entry: &str) -> Result<Vec<u8>, String> {
    let rel = relative_name(shader)?;
    check_entry(entry)?;
    if let Project::Dir(dir) = project {
        let file = spirv_file(&dir.join(OUTPUT_DIR), rel, entry);
        match std::fs::metadata(&file) {
            Ok(meta) if meta.len() > proto_gfx_core::MAX_SPIRV_BYTES as u64 => {
                return Err(format!(
                    "{}: {} bytes; the limit is {}",
                    file.display(),
                    meta.len(),
                    proto_gfx_core::MAX_SPIRV_BYTES
                ));
            }
            Ok(_) => {
                return std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", file.display())),
        }
        if dir.join(shader).exists() {
            return Err(format!(
                "{shader} has no compiled entry point `{entry}` (no {}). Compile the \
                 project's shaders with `proto-compositor build-shaders {}`, and check \
                 that the entry point exists",
                file.display(),
                dir.display()
            ));
        }
    }
    embedded::BUILTIN_SHADERS
        .iter()
        .find(|(name, e, _)| *name == shader && *e == entry)
        .map(|(_, _, spirv)| spirv.to_vec())
        .ok_or_else(|| match project {
            Project::Builtin => {
                format!("the built-in project has no shader {shader} with entry point `{entry}`")
            }
            Project::Dir(dir) => format!(
                "{shader}: no such file in {} (nor such a shader in the built-in project)",
                dir.display()
            ),
        })
}

/// Writes the core's modules into `dir`, leaving a file alone when it already
/// holds the same text (its modification time tells the build what changed).
pub fn write_core_modules(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (name, source) in embedded::CORE_MODULES {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(*source) {
            std::fs::write(&path, source).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    Ok(())
}

/// The names of the core's modules (`color.slang`, ...).
pub fn core_modules() -> impl Iterator<Item = &'static str> {
    embedded::CORE_MODULES.iter().map(|(name, _)| *name)
}

/// Compiles the shaders of the project in `dir` (`shaders/*.slang`) into its
/// `.build/shaders/`, with the core's modules in `.build/modules/`. Sources
/// whose SPIR-V is newer than every source are left alone. A project without
/// `shaders/` is left as it is.
pub fn build_project(dir: &Path) -> Result<Built, Vec<String>> {
    if !dir.join(proto_slangc::project::SOURCE_DIR).is_dir() && !dir.join(OUTPUT_DIR).exists() {
        return Ok(Built::default());
    }
    let modules = dir.join(MODULES_DIR);
    write_core_modules(&modules).map_err(|e| vec![e])?;
    let mut compiler = Compiler::find().map_err(|e| vec![e.to_string()])?;
    compiler.include_dirs.push(modules);
    build(dir, &dir.join(OUTPUT_DIR), &compiler)
}

/// Where [`build_project`] puts the SPIR-V of a project.
pub fn output_dir(dir: &Path) -> PathBuf {
    dir.join(OUTPUT_DIR)
}
