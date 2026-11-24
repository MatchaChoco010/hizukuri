//! `hz.window_table()`: the numbers to read every window with, kept by the
//! core in one storage buffer. A script passes on an index and never sees
//! the numbers.

mod common;

use common::{Device, Session, options, script_project};
use proto_color::{AlphaMode, CompositeSpace, DecodeTransform, ImageDescription};
use proto_compositor::control::Request;
use serde_json::json;

device_tests!(a_shader_reads_windows_through_the_cores_table,);

/// The output in four columns, one window each, read at the middle of the
/// window with the numbers of the core's table.
const SHADER: &str = r#"
import color;

struct Push {
    uint4 index;
    float4 gain;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_window0;
[[vk::binding(1, 0)]] Texture2D<float4> u_window1;
[[vk::binding(2, 0)]] Texture2D<float4> u_window2;
[[vk::binding(3, 0)]] Texture2D<float4> u_window3;
[[vk::binding(4, 0)]] SamplerState u_sampler;
[[vk::binding(5, 0)]] StructuredBuffer<WindowParams> u_table;

struct VertexOutput {
    float4 position : SV_Position;
    [[vk::location(0)]] float2 st : TEXCOORD0;
};

[shader("vertex")]
VertexOutput vs_main(uint index : SV_VulkanVertexID) {
    float2 corner = float2(float((index << 1) & 2), float(index & 2));
    VertexOutput o;
    o.st = corner;
    o.position = float4(corner * 2.0 - 1.0, 0.0, 1.0);
    return o;
}

[shader("fragment")]
float4 fs_main(VertexOutput i) : SV_Target0 {
    uint column = min(uint(i.st.x * 4.0), 3u);
    float2 middle = float2(0.5, 0.5);
    WindowParams params = u_table[pc.index[column]];
    float4 c;
    switch (column) {
    case 0: c = WindowTexture(u_window0, u_sampler, params).sample_level(middle, 0.0); break;
    case 1: c = WindowTexture(u_window1, u_sampler, params).sample_level(middle, 0.0); break;
    case 2: c = WindowTexture(u_window2, u_sampler, params).sample_level(middle, 0.0); break;
    default: c = WindowTexture(u_window3, u_sampler, params).sample_level(middle, 0.0); break;
    }
    return scale_light(c, pc.gain[column]);
}
"#;

const MAIN: &str = r#"
local color = hz.include("color")

local pipeline, sampler
local push = buffer.create(32)
local brightness = {}
local seen = {}

function init()
    pipeline = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", shader = "shaders/table.slang", entry = "vs_main" }),
        fragment = gfx.create_shader({ stage = "fragment", shader = "shaders/table.slang", entry = "fs_main" }),
        targets = { { format = color.format() } },
    })
    sampler = gfx.create_sampler({ mag = "linear", min = "linear" })
end

function set_brightness(index, value)
    brightness[index] = value
    hz.request_frame()
end

function indices()
    return seen
end

function outside()
    return hz.window_table()
end

function frame(output, target)
    local composite = color.composite_for(output)
    -- First the table, then the surfaces: they carry their index into it.
    local table_slot = hz.window_table()
    local windows = hz.windows()
    table.sort(windows, function(a, b)
        return (a.title or "") < (b.title or "")
    end)
    local surfaces = {}
    seen = {}
    for i = 1, 4 do
        local window = windows[i]
        local surface = window and hz.surfaces(window.id)[1]
        if not surface or not surface.texture or not surface.params then
            gfx.begin_pass({ color = { { image = composite.image, load = "clear", clear = { 0, 0, 0, 1 } } } })
            gfx.end_pass()
            color.present(composite, output, target)
            return
        end
        surfaces[i] = surface
        seen[i] = surface.params
        buffer.writeu32(push, (i - 1) * 4, surface.params)
        buffer.writef32(push, 16 + (i - 1) * 4, brightness[i] or 1)
    end
    gfx.begin_pass({ color = { { image = composite.image, load = "dont-care" } } })
    gfx.set_pipeline(pipeline)
    for i = 1, 4 do
        gfx.bind(i - 1, surfaces[i].texture)
    end
    gfx.bind(4, sampler)
    gfx.bind(5, table_slot)
    gfx.push(0, push)
    gfx.draw(3)
    gfx.end_pass()
    color.present(composite, output, target)
end
"#;

fn a_shader_reads_windows_through_the_cores_table(device: Device) {
    let project = script_project("window-table", device, &[("main", MAIN)]);
    std::fs::create_dir_all(project.join("shaders")).unwrap();
    std::fs::write(project.join("shaders/table.slang"), SHADER).unwrap();
    let mut session = Session::start(
        "window-table",
        device,
        &[
            "--output-size",
            "256x64",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    assert_eq!(session.script_state()["mode"], "running");

    let windows: [([u8; 4], Option<&str>, ImageDescription); 4] = [
        ([200, 100, 50, 255], None, ImageDescription::SRGB_GAMMA22),
        (
            [100, 60, 20, 128],
            Some("display-p3"),
            ImageDescription::DISPLAY_P3,
        ),
        (
            [140, 120, 90, 255],
            Some("bt2020-pq"),
            ImageDescription::BT2020_PQ,
        ),
        (
            [90, 180, 30, 255],
            Some("scrgb-linear"),
            ImageDescription::SCRGB_LINEAR,
        ),
    ];
    let mut clients = Vec::new();
    for (i, (color, preset, _)) in windows.iter().enumerate() {
        let title = format!("w{}", i + 1);
        let (client, _) = session.mapped_client(options(&title, 64, 48, *color));
        if let Some(preset) = preset {
            let state = session.state();
            let surface = common::window(&state, &title).unwrap()["surfaces"][0]["id"]
                .as_u64()
                .unwrap();
            session.send(Request::SetSurfaceColor {
                surface,
                image_description: (*preset).into(),
                alpha_mode: None,
            });
        }
        clients.push(client);
    }

    let space = CompositeSpace::default();
    let check = |session: &mut Session, name: &str, brightness: [f32; 4]| {
        let (_, composite) = session.screenshot(name);
        for (i, (color, _, description)) in windows.iter().enumerate() {
            let expected =
                DecodeTransform::new(description, AlphaMode::PremultipliedElectrical, &space)
                    .unwrap()
                    .apply(color.map(|v| v as f32 / 255.0));
            let gain = brightness[i];
            let expected = [
                expected[0] * gain,
                expected[1] * gain,
                expected[2] * gain,
                expected[3],
            ];
            let actual = composite.pixel(32 + 64 * i as u32, 32);
            for c in 0..4 {
                assert!(
                    (actual[c] - expected[c]).abs() <= 2e-5 + expected[c].abs() * 2e-3,
                    "{name}: window {} is {actual:?}, expected {expected:?}",
                    i + 1
                );
            }
        }
    };
    check(&mut session, "table", [1.0; 4]);
    // Four surfaces, four places in the table.
    let mut indices: Vec<u64> = session
        .script("indices", json!([]))
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    indices.sort();
    assert_eq!(indices, [0, 1, 2, 3]);

    // The brightness is not in the table: the shader scales what it reads.
    session.script("set_brightness", json!([1, 0.5]));
    session.script("set_brightness", json!([3, 2.0]));
    check(&mut session, "brightness", [0.5, 1.0, 2.0, 1.0]);

    // The table is made for a frame: outside `frame` there is none.
    let error = session
        .try_send(Request::ScriptCall {
            function: "outside".into(),
            args: vec![],
        })
        .unwrap_err();
    assert!(error.contains("for `frame`"), "{error}");
    session.assert_drawing_clean();
    drop(clients);
    session.quit();
}
