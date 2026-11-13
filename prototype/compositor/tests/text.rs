//! `hz.load_font` and `hz.render_text`: text as a texture, from the font
//! the session was started with.

mod common;

#[path = "common/square_font.rs"]
mod square_font;

use common::{Device, Session, script_project};
use proto_color::{AlphaMode, CompositeSpace, DecodeTransform, ImageDescription};
use serde_json::json;

device_tests!(text_becomes_a_texture, a_session_without_a_font_has_no_text,);

/// The text texture at the top left of the output, one texel per pixel,
/// multiplied by a color.
const SHADER: &str = r#"
struct Push {
    float4 color;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_text;

[shader("vertex")]
float4 vs_main(uint index : SV_VulkanVertexID) : SV_Position {
    float2 corner = float2(float((index << 1) & 2), float(index & 2));
    return float4(corner * 2.0 - 1.0, 0.0, 1.0);
}

[shader("fragment")]
float4 fs_main(float4 position : SV_Position) : SV_Target0 {
    uint width, height;
    u_text.GetDimensions(width, height);
    int2 texel = int2(position.xy);
    if (texel.x >= int(width) || texel.y >= int(height)) {
        return float4(0.0, 0.0, 0.0, 1.0);
    }
    // White with the coverage as alpha, premultiplied: times a color, over
    // black.
    float4 text = u_text.Load(int3(texel, 0));
    return float4(pc.color.rgb * text.a, 1.0);
}
"#;

const MAIN: &str = r##"
local color = hz.include("color")

local pipeline, font, text
local push = buffer.create(16)

function init()
    pipeline = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", shader = "shaders/text.slang", entry = "vs_main" }),
        fragment = gfx.create_shader({ stage = "fragment", shader = "shaders/text.slang", entry = "fs_main" }),
        targets = { { format = color.format() } },
    })
    font = hz.load_font()
    if font then
        text = hz.render_text(font, "AA", { size = 20 })
    end
    local r, g, b, a = hz.color("#ff8000")
    buffer.writef32(push, 0, r)
    buffer.writef32(push, 4, g)
    buffer.writef32(push, 8, b)
    buffer.writef32(push, 12, a)
end

-- Shows other text, in another size.
function show(words, size)
    text = font and hz.render_text(font, words, { size = size })
    hz.request_frame()
    return text ~= nil
end

function about()
    return {
        font = font,
        text = text,
        nothing = font and hz.render_text(font, "   ") == nil,
        coverage = font and hz.render_text(font, "A", { size = 20, format = "r8unorm" }),
    }
end

function frame(output, target)
    local composite = color.composite_for(output)
    gfx.begin_pass({ color = { { image = composite.image, load = "clear", clear = { 0, 0, 0, 1 } } } })
    if text then
        gfx.set_pipeline(pipeline)
        gfx.bind(0, text.texture)
        gfx.push(0, push)
        gfx.draw(3)
    end
    gfx.end_pass()
    color.present(composite, output, target)
end
"##;

fn project(name: &str, device: Device) -> std::path::PathBuf {
    let project = script_project(name, device, &[("main", MAIN)]);
    std::fs::create_dir_all(project.join("shaders")).unwrap();
    std::fs::write(project.join("shaders/text.slang"), SHADER).unwrap();
    project
}

fn text_becomes_a_texture(device: Device) {
    let project = project("text", device);
    // The font of the session: `A` is a square of 0.6 em, 0.1 em in, 0.8 em
    // wide.
    let font = project.join("square.ttf");
    std::fs::write(&font, square_font::square_font()).unwrap();
    let mut session = Session::start(
        "text",
        device,
        &[
            "--output-size",
            "64x32",
            "--script",
            project.to_str().unwrap(),
            "--font",
            font.to_str().unwrap(),
        ],
    );
    assert_eq!(session.script_state()["mode"], "running");
    let about = session.script("about", json!([]));
    assert_eq!(about["font"], 1);
    assert_eq!(about["nothing"], true);
    // Two squares of 12 pixels at 2 and at 18, in a line of 20 pixels.
    assert_eq!(about["text"]["format"], "rgba8unorm");
    assert_eq!(
        (&about["text"]["width"], &about["text"]["height"]),
        (&json!(30), &json!(20))
    );
    assert_eq!(about["coverage"]["format"], "r8unorm");
    assert_eq!(about["coverage"]["width"], 14);

    let (_, composite) = session.screenshot("text");
    let orange = DecodeTransform::new(
        &ImageDescription::SRGB_GAMMA22,
        AlphaMode::Straight,
        &CompositeSpace::default(),
    )
    .unwrap()
    .apply([1.0, 128.0 / 255.0, 0.0, 1.0]);
    let black = [0.0, 0.0, 0.0, 1.0];
    // Inside the first square, between the two, inside the second, past
    // them, and below the line.
    for (x, y, expected) in [
        (8, 14, orange),
        (16, 14, black),
        (24, 14, orange),
        (40, 14, black),
        (8, 26, black),
    ] {
        let actual = composite.pixel(x, y);
        for c in 0..4 {
            assert!(
                (actual[c] - expected[c]).abs() <= 2e-5 + expected[c].abs() * 2e-3,
                "({x}, {y}) is {actual:?}, expected {expected:?}"
            );
        }
    }
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// The font a session is given may be missing or broken. A project then
/// gets nil and draws without text.
fn a_session_without_a_font_has_no_text(device: Device) {
    let project = project("text-none", device);
    let font = project.join("broken.ttf");
    std::fs::write(&font, b"not a font").unwrap();
    let mut session = Session::start(
        "text-none",
        device,
        &[
            "--output-size",
            "64x32",
            "--script",
            project.to_str().unwrap(),
            "--font",
            font.to_str().unwrap(),
        ],
    );
    assert_eq!(session.script_state()["mode"], "running");
    let about = session.script("about", json!([]));
    assert!(about["font"].is_null(), "{about}");
    session.screenshot("no-text");
    session.assert_script_healthy();
    session.quit();
}

/// A picture of text in whatever font the session finds on the machine
/// (none is given to it). Not a test: ignored by default, and it asserts
/// nothing about the picture.
///
/// ```sh
/// cargo test -p proto-compositor --test text -- --ignored --nocapture a_picture_of_text
/// ```
#[test]
#[ignore = "a picture to look at, taken by hand"]
fn a_picture_of_text() {
    let device = Device::new(common::Gpu::Lavapipe);
    let project = project("text-picture", device);
    let mut session = Session::start(
        "text-picture",
        device,
        &[
            "--output-size",
            "640x64",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    if session.script("show", json!(["hizukuri  NO SIGNAL  workspace 3", 36])) == true {
        session.screenshot_png("text");
        println!(
            "written to {}",
            session.output_dir.join("text.png").display()
        );
    } else {
        println!("no font was found on this machine");
    }
    session.quit();
}
