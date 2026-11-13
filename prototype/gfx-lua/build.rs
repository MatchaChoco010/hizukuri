//! Compiles the Slang shaders this crate carries to SPIR-V in `OUT_DIR`, the
//! way `proto-compositor build-shaders` builds a project's:
//!
//! - `scripts/shaders/`: the shaders of the example scripts (`scripts/` is
//!   their project; they name them as `shaders/<file>.slang`).
//! - `shaders/`: the shaders of `gfx-probe` and `gfx-bench`.
//!
//! The SPIR-V goes into the library (`shaders.rs`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use proto_slangc::Compiler;
use proto_slangc::project;

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed=shaders");
    println!("cargo:rerun-if-changed=scripts/shaders");
    println!(
        "cargo:rerun-if-env-changed={}",
        proto_slangc::SLANGC_VARIABLE
    );
    let compiler = Compiler::find().unwrap_or_else(|e| fail(&e.to_string()));

    let mut code = String::new();
    for (name, project_dir, output) in [
        ("EXAMPLES", manifest.join("scripts"), out.join("examples")),
        ("TOOLS", manifest.clone(), out.join("tools")),
    ] {
        let built = project::build(&project_dir, &output, &compiler)
            .unwrap_or_else(|errors| fail(&errors.join("\n")));
        for warning in &built.warnings {
            println!("cargo:warning={warning}");
        }
        writeln!(code, "pub const {name}: &[(&str, &str, &[u8])] = &[").unwrap();
        for (rel, entry) in built.all() {
            let file = project::spirv_file(&output, rel, entry);
            writeln!(
                code,
                "    ({:?}, {entry:?}, include_bytes!({:?})),",
                format!("shaders/{rel}"),
                path_text(&file)
            )
            .unwrap();
        }
        code.push_str("];\n");
    }
    std::fs::write(out.join("embedded_shaders.rs"), code).unwrap();
}

fn path_text(path: &Path) -> String {
    path.to_str().expect("a UTF-8 path").to_owned()
}

fn fail(message: &str) -> ! {
    eprintln!("error: cannot compile the shaders of proto-gfx-lua\n{message}");
    std::process::exit(1);
}
