//! Compiles the compositor's Slang shaders to SPIR-V in `OUT_DIR`, where the
//! code includes them. The built compositor carries the SPIR-V and needs no
//! Slang compiler to use it.
//!
//! - [`CORE_SHADERS`]: the shaders of the core (the fallback, the KMS self
//!   test).
//! - `scripts/shaders/`: the shaders of the built-in project, built the same
//!   way `proto-compositor build-shaders` builds any project's.
//!
//! It also embeds the core's modules (`shaders/`, [`CORE_MODULES`]), which
//! `build-shaders` gives to `slangc` for every project.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use proto_slangc::{Compiler, Input, Stage};

use proto_slangc::project as project_shaders;

/// The core's own shaders: `(file in shaders/, stage, entry point)`. Each
/// becomes `OUT_DIR/<file stem>.<entry>.spv`.
const CORE_SHADERS: &[(&str, Stage, &str)] = &[
    ("fallback.slang", Stage::Vertex, "vs"),
    ("fallback.slang", Stage::Fragment, "fs"),
    ("selftest.slang", Stage::Vertex, "vs"),
    ("selftest.slang", Stage::Fragment, "fs"),
];

/// The modules of `shaders/` that a project's shaders can `import`.
const CORE_MODULES: &[&str] = &["color.slang"];

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let shaders = manifest.join("shaders");
    let builtin = manifest.join("scripts");
    // Directories: any change beneath them runs this again.
    println!("cargo:rerun-if-changed=shaders");
    println!("cargo:rerun-if-changed=scripts/shaders");
    println!(
        "cargo:rerun-if-env-changed={}",
        proto_slangc::SLANGC_VARIABLE
    );

    let mut compiler = match Compiler::find() {
        Ok(compiler) => compiler,
        Err(e) => fail(&e.to_string()),
    };
    compiler.include_dirs.push(shaders.clone());
    // The compositor compiles OCIO's shaders at run time. When it finds no
    // slangc where it looks first, it tries the one it was built with.
    println!(
        "cargo:rustc-env=HIZUKURI_BUILD_SLANGC={}",
        compiler.slangc.display()
    );

    for (file, stage, entry) in CORE_SHADERS {
        let compiled = compiler
            .compile(Input::File(&shaders.join(file)), *stage, entry)
            .unwrap_or_else(|e| fail(&format!("{e}")));
        warn(compiled.warnings.iter().map(|w| w.to_string()));
        let stem = Path::new(file).file_stem().unwrap().to_str().unwrap();
        std::fs::write(out.join(format!("{stem}.{entry}.spv")), compiled.spirv).unwrap();
    }

    // The vertex stage of the OCIO pass: `OUT_DIR/ocio.vs.spv`.
    let compiled = compiler
        .compile(
            Input::File(&shaders.join("ocio.slang")),
            Stage::Vertex,
            "vs",
        )
        .unwrap_or_else(|e| fail(&format!("{e}")));
    warn(compiled.warnings.iter().map(|w| w.to_string()));
    std::fs::write(out.join("ocio.vs.spv"), compiled.spirv).unwrap();

    let output = out.join("builtin-shaders");
    let built = project_shaders::build(&builtin, &output, &compiler)
        .unwrap_or_else(|errors| fail(&errors.join("\n")));
    warn(built.warnings.iter().cloned());

    let mut code = String::from(
        "/// The built-in project's shaders: `(name, entry point, SPIR-V)`.\n\
         pub const BUILTIN_SHADERS: &[(&str, &str, &[u8])] = &[\n",
    );
    for (rel, entry) in built.all() {
        let file = project_shaders::spirv_file(&output, rel, entry);
        writeln!(
            code,
            "    ({:?}, {entry:?}, include_bytes!({:?})),",
            format!("shaders/{rel}"),
            path_text(&file)
        )
        .unwrap();
    }
    code.push_str(
        "];\n\n/// The core's modules: `(file name, source)`.\n\
         pub const CORE_MODULES: &[(&str, &str)] = &[\n",
    );
    for name in CORE_MODULES {
        writeln!(
            code,
            "    ({name:?}, include_str!({:?})),",
            path_text(&shaders.join(name))
        )
        .unwrap();
    }
    code.push_str("];\n");
    std::fs::write(out.join("embedded_shaders.rs"), code).unwrap();
}

fn path_text(path: &Path) -> String {
    path.to_str().expect("a UTF-8 path").to_owned()
}

fn warn(warnings: impl Iterator<Item = String>) {
    for warning in warnings {
        println!("cargo:warning={warning}");
    }
}

fn fail(message: &str) -> ! {
    eprintln!("error: cannot compile the shaders of proto-compositor\n{message}");
    std::process::exit(1);
}
