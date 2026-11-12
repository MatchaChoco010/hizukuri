//! Compiles the WGSL shaders in `shaders/` to SPIR-V with naga, so building
//! does not depend on an external shader compiler.

use std::path::{Path, PathBuf};
use std::{env, fs};

use naga::ShaderStage;
use naga::back::spv;
use naga::front::wgsl;
use naga::valid::{Capabilities, ValidationFlags, Validator};

/// Shared code prepended to every shader (WGSL has no include directive).
const COMMON: &str = "color.wgsl";

fn main() {
    let shader_dir = Path::new("shaders");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    println!("cargo:rerun-if-changed=shaders");
    println!("cargo:rerun-if-changed=build.rs");

    let common = read(&shader_dir.join(COMMON));
    for name in ["surface", "encode"] {
        let file = format!("{name}.wgsl");
        let source = format!("{common}\n{}", read(&shader_dir.join(&file)));
        let module = wgsl::parse_str(&source)
            .unwrap_or_else(|e| panic!("failed to parse {file}:\n{}", e.emit_to_string(&source)));
        let info = Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .unwrap_or_else(|e| {
                panic!("failed to validate {file}:\n{}", e.emit_to_string(&source))
            });

        // Keep Vulkan conventions: no Y flip, no depth remapping.
        let options = spv::Options {
            flags: spv::WriterFlags::empty(),
            ..spv::Options::default()
        };
        for (entry_point, stage, suffix) in [
            ("vs_main", ShaderStage::Vertex, "vert"),
            ("fs_main", ShaderStage::Fragment, "frag"),
        ] {
            let pipeline_options = spv::PipelineOptions {
                shader_stage: stage,
                entry_point: entry_point.to_string(),
            };
            let words = spv::write_vec(&module, &info, &options, Some(&pipeline_options))
                .unwrap_or_else(|e| panic!("failed to generate SPIR-V for {file}: {e}"));
            let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
            let out = out_dir.join(format!("{name}.{suffix}.spv"));
            fs::write(&out, bytes).unwrap_or_else(|e| panic!("failed to write {out:?}: {e}"));
        }
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {path:?}: {e}"))
}
