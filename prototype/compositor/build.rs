//! Compiles the compositor's Slang shaders (`shaders/`) to SPIR-V in
//! `OUT_DIR`, where the code includes them with `include_bytes!`. The built
//! compositor carries the SPIR-V and needs no Slang compiler to use it.

use std::path::{Path, PathBuf};

use proto_slangc::{Compiler, Input, Stage};

/// `(file in shaders/, stage, entry point)`. Each one becomes
/// `OUT_DIR/<file stem>.<entry>.spv`.
const SHADERS: &[(&str, Stage, &str)] = &[
    ("fallback.slang", Stage::Vertex, "vs"),
    ("fallback.slang", Stage::Fragment, "fs"),
];

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let shaders = manifest.join("shaders");
    // A directory: any change beneath it runs this again.
    println!("cargo:rerun-if-changed=shaders");
    println!(
        "cargo:rerun-if-env-changed={}",
        proto_slangc::SLANGC_VARIABLE
    );

    let mut compiler = match Compiler::find() {
        Ok(compiler) => compiler,
        Err(e) => fail(&e.to_string()),
    };
    compiler.include_dirs.push(shaders.clone());
    for (file, stage, entry) in SHADERS {
        let path = shaders.join(file);
        let compiled = compiler
            .compile(Input::File(&path), *stage, entry)
            .unwrap_or_else(|e| fail(&format!("{} ({entry}):\n{e}", path.display())));
        for warning in &compiled.warnings {
            println!("cargo:warning={warning}");
        }
        let stem = Path::new(file).file_stem().unwrap().to_str().unwrap();
        std::fs::write(out.join(format!("{stem}.{entry}.spv")), compiled.spirv).unwrap();
    }
}

fn fail(message: &str) -> ! {
    eprintln!("error: cannot compile the shaders of proto-compositor\n{message}");
    std::process::exit(1);
}
