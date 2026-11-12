//! A project's own Slang shaders: built by `build-shaders` (the test session
//! builds them before it starts), named by the script, read as SPIR-V by the
//! compositor.

mod common;

use common::{Device, Session, script_project};
use proto_compositor::control::Request;

device_tests!(
    a_project_draws_with_its_own_shaders,
    a_shader_without_spirv_is_an_error_value,
);

/// Fills the output with one colour: linear (0.25, 0.5, 0.75), encoded with
/// the core's `color` module.
const FILL: &str = r#"
import color;

[shader("vertex")]
float4 vs_main(uint index : SV_VulkanVertexID) : SV_Position {
    float2 corner = float2(float((index << 1) & 2), float(index & 2));
    return float4(corner * 2.0 - 1.0, 0.0, 1.0);
}

[shader("fragment")]
float4 fs_main() : SV_Target0 {
    return float4(tf_encode(float3(0.25, 0.5, 0.75), TF_SRGB, 1.0), 1.0);
}
"#;

const MAIN: &str = r#"
local vertex, fragment
local pipelines = {}

function init()
    vertex = gfx.create_shader({ stage = "vertex", shader = "shaders/fill.slang", entry = "vs_main" })
    fragment = gfx.create_shader({ stage = "fragment", shader = "shaders/fill.slang", entry = "fs_main" })
end

function frame(output, target)
    local pipeline = pipelines[output.format]
    if not pipeline then
        pipeline = gfx.create_render_pipeline({
            vertex = vertex,
            fragment = fragment,
            targets = { { format = output.format } },
        })
        pipelines[output.format] = pipeline
    end
    gfx.begin_pass({ color = { { image = target, load = "dont-care" } } })
    gfx.set_pipeline(pipeline)
    gfx.draw(3)
    gfx.end_pass()
end
"#;

fn fill_project(name: &str, device: Device) -> std::path::PathBuf {
    let project = script_project(name, device, &[("main", MAIN)]);
    std::fs::create_dir_all(project.join("shaders")).unwrap();
    std::fs::write(project.join("shaders/fill.slang"), FILL).unwrap();
    project
}

fn a_project_draws_with_its_own_shaders(device: Device) {
    let project = fill_project("project-shaders", device);
    let mut session = Session::start(
        "project-shaders",
        device,
        &[
            "--output-size",
            "64x48",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    // What the build wrote: one SPIR-V file per entry point.
    let output = proto_compositor::shaders::output_dir(&project);
    for entry in ["vs_main", "fs_main"] {
        assert!(
            output.join(format!("fill.slang.{entry}.spv")).is_file(),
            "{entry}"
        );
    }
    assert_eq!(session.script_state()["mode"], "running");
    let image = session.screenshot_png("fill");
    // sRGB of 0.25, 0.5 and 0.75, give or take 1 (UNORM rounding).
    let pixel = image.pixel(32, 24);
    for (c, expected) in [137u16, 188, 225].into_iter().enumerate() {
        assert!(pixel[c].abs_diff(expected) <= 1, "{pixel:?}");
    }
}

/// The compositor does not compile: a project whose shader has no SPIR-V
/// (the build tool was not run) gets an error value that says what to run.
fn a_shader_without_spirv_is_an_error_value(device: Device) {
    let project = fill_project("project-shaders-unbuilt", device);
    let mut session = Session::start(
        "project-shaders-unbuilt",
        device,
        &[
            "--output-size",
            "64x48",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    assert_eq!(session.script_state()["mode"], "running");
    std::fs::remove_dir_all(proto_compositor::shaders::output_dir(&project)).unwrap();
    let error = session
        .try_send(Request::ScriptReload { path: None })
        .unwrap_err();
    assert!(error.contains("shaders/fill.slang"), "{error}");
    assert!(error.contains("build-shaders"), "{error}");
}
