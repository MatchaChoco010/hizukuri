//! The SPIR-V of the shaders this crate carries, compiled from Slang by
//! `build.rs`: those of the example scripts (`scripts/shaders/`), and those
//! of `gfx-probe` and `gfx-bench` (`shaders/`).

use std::rc::Rc;

use crate::ShaderLoader;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_shaders.rs"));
}

fn find(table: &'static [(&str, &str, &[u8])], name: &str, entry: &str) -> Option<&'static [u8]> {
    table
        .iter()
        .find(|(n, e, _)| *n == name && *e == entry)
        .map(|(_, _, spirv)| *spirv)
}

/// The loader for the example scripts: `shaders/<file>.slang` of
/// `scripts/`.
pub fn examples() -> ShaderLoader {
    ShaderLoader(Rc::new(|name: &str, entry: &str| {
        find(embedded::EXAMPLES, name, entry)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| format!("the examples have no shader {name} with entry point `{entry}`"))
    }))
}

/// A shader of `gfx-probe` or `gfx-bench` (`shaders/<file>.slang`).
pub fn tool(name: &str, entry: &str) -> &'static [u8] {
    find(embedded::TOOLS, name, entry)
        .unwrap_or_else(|| panic!("no shader {name} with entry point `{entry}`"))
}
