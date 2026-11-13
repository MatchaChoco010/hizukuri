//! Shaders a script names (`gfx.create_shader({ shader = ..., entry = ... })`):
//! the host's loader gives their SPIR-V, which goes through the GPU API's
//! SPIR-V checks.

mod common;

use std::rc::Rc;

use common::{host_with, patient};
use proto_gfx_lua::{HostOptions, ShaderLoader};

fn with_loader() -> HostOptions {
    HostOptions {
        shader_loader: Some(ShaderLoader(Rc::new(|name: &str, entry: &str| {
            match (name, entry) {
                // The fragment shader of the triangle example.
                ("shaders/plain.slang", entry) => {
                    (proto_gfx_lua::shaders::examples().0)("shaders/triangle.slang", entry)
                }
                // Bytes that are not a module, as from a damaged file.
                ("shaders/broken.slang", "main") => Ok(b"#version 450\nvoid main() {}\n".to_vec()),
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
        local shader = gfx.create_shader({ stage = "fragment", shader = "shaders/plain.slang", entry = "fs" })
        assert(type(shader) == "number")
        local ok, err = pcall(gfx.create_shader, { stage = "fragment", shader = "shaders/other.slang", entry = "main" })
        assert(not ok and string.find(tostring(err), "no shader shaders/other.slang", 1, true), tostring(err))
        -- The module is fragment; asking for a vertex shader is refused by the
        -- SPIR-V checks.
        ok, err = pcall(gfx.create_shader, { stage = "vertex", shader = "shaders/plain.slang", entry = "fs" })
        assert(not ok and string.find(tostring(err), "vertex", 1, true), tostring(err))
        ok, err = pcall(gfx.create_shader, { stage = "fragment", shader = "shaders/broken.slang", entry = "main" })
        assert(not ok and string.find(tostring(err), "ShaderCompile", 1, true), tostring(err))
        ok, err = pcall(gfx.create_shader, { stage = "fragment", shader = "shaders/plain.slang", source = "x" })
        assert(not ok and string.find(tostring(err), "not accepted", 1, true), tostring(err))
        "#,
    )
    .unwrap();
}

#[test]
fn a_host_without_a_loader_refuses_names() {
    let gpu = proto_gfx_core::Gpu::new(proto_gfx_core::GpuConfig::for_tests()).unwrap();
    let host = proto_gfx_lua::ScriptHost::new(gpu, patient()).unwrap();
    host.load(
        "unnamed",
        r#"
        local ok, err = pcall(gfx.create_shader, { stage = "fragment", shader = "shaders/plain.slang" })
        assert(not ok and string.find(tostring(err), "does not load shaders by name", 1, true), tostring(err))
        "#,
    )
    .unwrap();
}
