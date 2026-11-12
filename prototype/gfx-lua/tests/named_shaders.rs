//! Shaders a script names (`gfx.create_shader({ shader = ..., entry = ... })`):
//! the host's loader gives their SPIR-V, which goes through the GPU API's
//! SPIR-V entry point.

mod common;

use std::rc::Rc;

use common::{host_with, patient};
use proto_gfx_core::{NagaCompiler, ShaderCompiler, ShaderLanguage, ShaderSource, ShaderStage};
use proto_gfx_lua::{HostOptions, ShaderLoader};

/// SPIR-V for the loader, written here by naga. naga's compiler writes
/// SPIR-V 1.3 and the API takes 1.4 and later; for a shader that uses no
/// resource, the module is the same in 1.4, so only the version changes.
fn spirv(stage: ShaderStage, source: &str) -> Vec<u8> {
    let mut words = NagaCompiler
        .compile(&ShaderSource {
            stage,
            language: ShaderLanguage::Wgsl,
            source,
            entry: "main",
        })
        .unwrap()
        .spirv;
    words[1] = 0x0001_0400;
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

const FRAGMENT: &str = "@fragment fn main() -> @location(0) vec4<f32> { return vec4(1.0); }";

fn with_loader() -> HostOptions {
    HostOptions {
        shader_loader: Some(ShaderLoader(Rc::new(|name: &str, entry: &str| {
            match (name, entry) {
                ("shaders/plain.slang", "main") => Ok(spirv(ShaderStage::Fragment, FRAGMENT)),
                _ => Err(format!("no shader {name} with entry point `{entry}`")),
            }
        }))),
        ..patient()
    }
}

#[test]
fn a_named_shader_is_created_from_its_spirv() {
    let host = host_with(with_loader());
    host.load(
        "named",
        r#"
        local shader = gfx.create_shader({ stage = "fragment", shader = "shaders/plain.slang", entry = "main" })
        assert(type(shader) == "number")
        local ok, err = pcall(gfx.create_shader, { stage = "fragment", shader = "shaders/other.slang", entry = "main" })
        assert(not ok and string.find(tostring(err), "no shader shaders/other.slang", 1, true), tostring(err))
        -- The module is fragment; asking for a vertex shader is refused by the
        -- SPIR-V checks.
        ok, err = pcall(gfx.create_shader, { stage = "vertex", shader = "shaders/plain.slang", entry = "main" })
        assert(not ok and string.find(tostring(err), "vertex", 1, true), tostring(err))
        ok, err = pcall(gfx.create_shader, { stage = "fragment", shader = "shaders/plain.slang", source = "x" })
        assert(not ok and string.find(tostring(err), "not both", 1, true), tostring(err))
        "#,
    )
    .unwrap();
}

#[test]
fn a_host_without_a_loader_refuses_names() {
    let host = host_with(patient());
    host.load(
        "unnamed",
        r#"
        local ok, err = pcall(gfx.create_shader, { stage = "fragment", shader = "shaders/plain.slang" })
        assert(not ok and string.find(tostring(err), "does not load shaders by name", 1, true), tostring(err))
        "#,
    )
    .unwrap();
}
