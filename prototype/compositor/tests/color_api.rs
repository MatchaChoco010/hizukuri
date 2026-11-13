//! What a project that draws with its own shaders gets from the core for
//! color: reading windows as values of the working space (`WindowTexture` of
//! the Slang module `color`, with the numbers from the built-in `color`
//! module), copying a window converted, and handing the result to an output.

mod common;

use common::{Device, Session, options, script_project};
use proto_color::{AlphaMode, CompositeSpace, DecodeTransform, ImageDescription};
use proto_compositor::control::Request;
use serde_json::json;

device_tests!(
    a_projects_own_shader_reads_windows_in_the_working_space,
    colors_and_images_come_in_the_working_space,
    the_built_in_drawing_takes_a_brightness_per_window,
    a_window_is_filtered_as_stored_and_its_copy_as_light,
    a_frame_that_does_not_present_is_noticed,
);

/// Left: the window read at its very middle, where the four quadrants meet.
/// Right: the smallest mip level of a converted copy of the window.
const FILTER_SHADER: &str = r#"
import color;

struct Push {
    WindowParams window;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_window;
[[vk::binding(1, 0)]] SamplerState u_sampler;
[[vk::binding(2, 0)]] Texture2D<float4> u_copy;

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
    if (i.st.x < 0.5) {
        return WindowTexture(u_window, u_sampler, pc.window).sample_level(float2(0.5, 0.5), 0.0);
    }
    return u_copy.Load(int3(0, 0, 6));
}
"#;

const FILTER_MAIN: &str = r#"
local color = hz.include("color")

local pipeline, sampler, copy, copy_slot

function init()
    pipeline = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", shader = "shaders/filter.slang", entry = "vs_main" }),
        fragment = gfx.create_shader({ stage = "fragment", shader = "shaders/filter.slang", entry = "fs_main" }),
        targets = { { format = color.format() } },
    })
    sampler = gfx.create_sampler({ mag = "linear", min = "linear" })
    -- The size of the window's buffer, with every mip level.
    copy = gfx.create_image({
        format = color.format(),
        width = 64,
        height = 64,
        mip_levels = 7,
        usage = { "color_target", "sampled", "copy_src", "copy_dst" },
    })
    copy_slot = gfx.create_texture_slot(copy)
end

function frame(output, target)
    local composite = color.composite_for(output)
    local window = hz.windows()[1]
    local surface = window and hz.surfaces(window.id)[1]
    if not surface or not surface.texture then
        gfx.begin_pass({ color = { { image = composite.image, load = "clear", clear = { 0, 0, 0, 1 } } } })
        gfx.end_pass()
        color.present(composite, output, target)
        return
    end
    -- Converted texel by texel, then scaled down as light.
    color.blit_window(surface, copy, { filter = "nearest", mip = 0 })
    gfx.generate_mipmaps(copy)

    gfx.begin_pass({ color = { { image = composite.image, load = "dont-care" } } })
    gfx.set_pipeline(pipeline)
    gfx.bind(0, surface.texture)
    gfx.bind(1, sampler)
    gfx.bind(2, copy_slot)
    gfx.push(0, color.window_params(surface))
    gfx.draw(3)
    gfx.end_pass()
    color.present(composite, output, target)
end
"#;

/// A window's texture holds what the client stored, so a filtered read
/// mixes stored values and converts the mix. A copy made with
/// `color.blit_window` holds light, and its mip levels are means of light.
/// Between black and white the two differ, and for PQ by orders of
/// magnitude: what is to be blurred or scaled down goes through a copy.
fn a_window_is_filtered_as_stored_and_its_copy_as_light(device: Device) {
    let project = script_project("color-api-filter", device, &[("main", FILTER_MAIN)]);
    std::fs::create_dir_all(project.join("shaders")).unwrap();
    std::fs::write(project.join("shaders/filter.slang"), FILTER_SHADER).unwrap();
    let mut session = Session::start(
        "color-api-filter",
        device,
        &[
            "--output-size",
            "128x64",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    let (white, black) = ([255u8, 255, 255, 255], [0u8, 0, 0, 255]);
    let mut client_options = options("checks", 64, 64, white);
    client_options.pattern = proto_test_client::Pattern::Quadrants([white, black, black, white]);
    let (client, _) = session.mapped_client(client_options);
    let surface = common::window(&session.state(), "checks").unwrap()["surfaces"][0]["id"]
        .as_u64()
        .unwrap();

    let space = CompositeSpace::default();
    for (name, preset, description, tolerance) in [
        ("gamma22", None, ImageDescription::SRGB_GAMMA22, 0.01),
        // PQ is steep: the weights of the filter, which have 8 bits, show.
        ("pq", Some("bt2020-pq"), ImageDescription::BT2020_PQ, 0.08),
    ] {
        if let Some(preset) = preset {
            session.send(Request::SetSurfaceColor {
                surface,
                image_description: preset.into(),
                alpha_mode: None,
            });
        }
        let transform =
            DecodeTransform::new(&description, AlphaMode::PremultipliedElectrical, &space).unwrap();
        // The mix of the stored values, converted.
        let stored = transform.apply([0.5, 0.5, 0.5, 1.0]);
        // The mean of the light.
        let (w, b) = (
            transform.apply([1.0, 1.0, 1.0, 1.0]),
            transform.apply([0.0, 0.0, 0.0, 1.0]),
        );
        let light: [f32; 4] = std::array::from_fn(|c| (w[c] + b[c]) / 2.0);
        let (_, composite) = session.screenshot(name);
        for (what, x, expected, tolerance) in [
            ("the window, filtered", 32, stored, tolerance),
            ("the copy, scaled down", 96, light, 2e-3),
        ] {
            let actual = composite.pixel(x, 32);
            for c in 0..4 {
                assert!(
                    (actual[c] - expected[c]).abs() <= 2e-5 + expected[c].abs() * tolerance,
                    "{name}: {what} is {actual:?}, expected {expected:?}"
                );
            }
        }
        println!("{name}: filtered as stored {stored:?}, as light {light:?}");
        // They are far apart.
        assert!(light[1] > stored[1] * 2.0, "{name}: {stored:?} {light:?}");
    }
    session.assert_script_healthy();
    session.assert_drawing_clean();
    drop(client);
    session.quit();
}

/// The built-in renderer draws a window brighter or darker when asked:
/// an SDR window above the reference white, for an HDR output.
fn the_built_in_drawing_takes_a_brightness_per_window(device: Device) {
    let mut session = Session::start(
        "color-api-brightness",
        device,
        &[
            "--output-size",
            "256x192",
            "--output-image-description",
            "bt2020-pq",
            "--output-format",
            "rgba16",
        ],
    );
    let color = [200u8, 100, 50, 255];
    let (mut client, id) = session.mapped_client(options("bright", 100, 60, color));
    session.send(Request::MoveWindow {
        window: id,
        x: 40,
        y: 30,
    });
    session.wait(&mut [&mut client], "moved", |s| {
        common::window(s, "bright").unwrap()["position"] == json!([40, 30])
    });
    let space = CompositeSpace::default();
    let reference = DecodeTransform::new(
        &ImageDescription::SRGB_GAMMA22,
        AlphaMode::PremultipliedElectrical,
        &space,
    )
    .unwrap()
    .apply(color.map(|v| v as f32 / 255.0));
    let encode = proto_color::EncodeTransform::new(&space, &ImageDescription::BT2020_PQ).unwrap();
    for (name, factor) in [
        ("as-is", None),
        ("brighter", Some(2.5)),
        ("darker", Some(0.25)),
    ] {
        session.script("set_brightness", json!([id, factor]));
        let (encoded, composite) = session.screenshot(name);
        let gain = factor.unwrap_or(1.0) as f32;
        let expected = [
            reference[0] * gain,
            reference[1] * gain,
            reference[2] * gain,
            1.0,
        ];
        let actual = composite.pixel(90, 60);
        for c in 0..4 {
            assert!(
                (actual[c] - expected[c]).abs() <= 2e-5 + expected[c].abs() * 2e-3,
                "{name}: the compositing buffer holds {actual:?}, expected {expected:?}"
            );
        }
        // What reaches the PQ output is that much light.
        let code = encode.apply([expected[0], expected[1], expected[2]]);
        let pixel = encoded.pixel(90, 60);
        for c in 0..3 {
            let wanted = code[c] * 65535.0;
            assert!(
                (pixel[c] as f32 - wanted).abs() <= 64.0,
                "{name}: the output holds {pixel:?}, expected {code:?}"
            );
        }
    }
    session.assert_script_healthy();
    session.assert_drawing_clean();
    session.quit();
}

/// The output in six columns: four textures read at their texel (1, 0), a
/// color from the push data, and the second mip level of a texture.
const IMAGES_SHADER: &str = r#"
struct Push {
    float4 color;
};
[[vk::push_constant]] ConstantBuffer<Push> pc;

[[vk::binding(0, 0)]] Texture2D<float4> u_image0;
[[vk::binding(1, 0)]] Texture2D<float4> u_image1;
[[vk::binding(2, 0)]] Texture2D<float4> u_image2;
[[vk::binding(3, 0)]] Texture2D<float4> u_image3;
[[vk::binding(4, 0)]] Texture2D<float4> u_image4;

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
    int3 texel = int3(1, 0, 0);
    switch (uint(i.st.x * 6.0)) {
    case 0: return u_image0.Load(texel);
    case 1: return u_image1.Load(texel);
    case 2: return u_image2.Load(texel);
    case 3: return u_image3.Load(texel);
    case 4: return pc.color;
    default: return u_image4.Load(int3(0, 0, 1));
    }
}
"#;

const IMAGES_MAIN: &str = r##"
local color = hz.include("color")

local pipeline
local images = {}
local push = buffer.create(16)

function init()
    pipeline = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", shader = "shaders/images.slang", entry = "vs_main" }),
        fragment = gfx.create_shader({ stage = "fragment", shader = "shaders/images.slang", entry = "fs_main" }),
        targets = { { format = color.format() } },
    })
    images[1] = hz.load_image("textures/color.png")
    images[2] = hz.load_image("textures/color.png", { data = true })
    images[3] = hz.load_image("textures/light.hdr")
    images[4] = hz.load_image("textures/lut.bin", { raw = { width = 2, height = 1, channels = 4 } })
    images[5] = hz.load_image("textures/color.png", { mipmaps = true })
    local r, g, b, a = hz.color("#20406080")
    buffer.writef32(push, 0, r)
    buffer.writef32(push, 4, g)
    buffer.writef32(push, 8, b)
    buffer.writef32(push, 12, a)
end

function loaded()
    return images
end

function session_color()
    local working, generation = hz.working_space()
    return {
        working = working,
        generation = generation,
        format = hz.color_format(),
        luminance = { hz.working_luminance() },
    }
end

function colors()
    return {
        { hz.color("#1e1e2e") },
        { hz.color(0.5, 0.25, 1.0) },
        { hz.color(0.5, 0.25, 1.0, 0.5) },
        { hz.color(0.5, 0.25, 1.0, 0.5, { premultiply = false }) },
        { hz.color(0.5, 0.25, 1.0, { space = "display-p3" }) },
        { hz.color("#ff8000", { space = "srgb-piecewise" }) },
        { hz.color(0.5, 0.25, 1.0, { space = "acescg" }) },
        { hz.color(0.5, 0.25, 1.0, { space = { primaries = "adobe-rgb", transfer = 1.8 } }) },
    }
end

function try(what)
    if what == "outside" then
        hz.load_image("../outside.png")
    elseif what == "missing" then
        hz.load_image("textures/none.png")
    elseif what == "color" then
        hz.color("1e1e2e")
    elseif what == "space" then
        hz.color("#1e1e2e", { space = "no-such-space" })
    end
end

function frame(output, target)
    local composite = color.composite_for(output)
    gfx.begin_pass({ color = { { image = composite.image, load = "dont-care" } } })
    gfx.set_pipeline(pipeline)
    for i = 1, 5 do
        gfx.bind(i - 1, images[i].texture)
    end
    gfx.push(0, push)
    gfx.draw(3)
    gfx.end_pass()
    color.present(composite, output, target)
end
"##;

fn colors_and_images_come_in_the_working_space(device: Device) {
    use proto_color::{Primaries, TransferFunction};

    let project = script_project("color-api-images", device, &[("main", IMAGES_MAIN)]);
    std::fs::create_dir_all(project.join("shaders")).unwrap();
    std::fs::write(project.join("shaders/images.slang"), IMAGES_SHADER).unwrap();
    let textures = project.join("textures");
    std::fs::create_dir_all(&textures).unwrap();

    // A PNG without color information, with straight alpha.
    let png_pixels: [[u8; 4]; 2] = [[10, 20, 30, 255], [200, 100, 50, 128]];
    {
        let file = std::fs::File::create(textures.join("color.png")).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(png_pixels.as_flattened()).unwrap();
    }
    // A Radiance picture: flat RGBE pixels.
    let rgbe: [[u8; 4]; 2] = [[1, 2, 3, 128], [128, 64, 32, 130]];
    {
        let mut hdr = b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 2\n".to_vec();
        hdr.extend_from_slice(rgbe.as_flattened());
        std::fs::write(textures.join("light.hdr"), hdr).unwrap();
    }
    // Raw floats.
    let raw: [[f32; 4]; 2] = [[0.0, 1.0, 2.0, 3.0], [0.125, -3.5, 1000.25, 0.75]];
    {
        let bytes: Vec<u8> = raw
            .as_flattened()
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        std::fs::write(textures.join("lut.bin"), bytes).unwrap();
    }

    let mut session = Session::start(
        "color-api-images",
        device,
        &[
            "--output-size",
            "384x64",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    assert_eq!(session.script_state()["mode"], "running");

    let space = CompositeSpace::default();
    let decode = |description: ImageDescription, rgba: [f32; 4]| {
        DecodeTransform::new(&description, AlphaMode::Straight, &space)
            .unwrap()
            .apply(rgba)
    };
    let srgb = ImageDescription::SRGB_GAMMA22;

    // What a session starts with: ACEScg, and the most exact float format
    // the device can draw to, blend in and filter.
    let session_color = session.script("session_color", json!([]));
    assert_eq!(session_color["working"], "acescg");
    assert_eq!(session_color["generation"], 0);
    // How bright a color of ACEScg is: the weights of its primaries.
    for (c, weight) in [0.2722, 0.6741, 0.0537].into_iter().enumerate() {
        let actual = session_color["luminance"][c].as_f64().unwrap();
        assert!((actual - weight).abs() < 1e-4, "{session_color}");
    }
    let format = session_color["format"].as_str().unwrap().to_owned();
    assert!(
        format == "rgba32float" || format == "rgba16float",
        "{format}"
    );
    if device.gpu == common::Gpu::Lavapipe {
        assert_eq!(format, "rgba32float");
    }

    // Its frames go out through the core's conversion.
    session.screenshot_png("first");
    assert_eq!(session.script_state()["frames_without_present"], 0);

    // What the script was told about the images.
    let loaded = session.script("loaded", json!([]));
    let expect = [
        ("rgba16float", "srgb-gamma22"),
        ("rgba8unorm", "data"),
        ("rgba16float", "scrgb-linear"),
        ("rgba32float", "data"),
    ];
    for (i, (format, meaning)) in expect.iter().enumerate() {
        let image = &loaded[i];
        assert_eq!(image["format"], *format, "{image}");
        assert_eq!(image["space"], *meaning, "{image}");
        assert_eq!((&image["width"], &image["height"]), (&json!(2), &json!(1)));
        assert_eq!(image["mip_levels"], 1);
    }
    assert_eq!(loaded[4]["mip_levels"], 2);

    let (_, composite) = session.screenshot("images");
    let linear_srgb = ImageDescription::new(Primaries::SRGB, TransferFunction::Linear);
    let hdr = [128.5 / 64.0, 64.5 / 64.0, 32.5 / 64.0, 1.0];
    // The smaller mip level is the mean of the two texels as a shader reads
    // them: of linear light, premultiplied.
    let texels = png_pixels.map(|p| decode(srgb, p.map(|v| v as f32 / 255.0)));
    let mean: [f32; 4] = std::array::from_fn(|c| (texels[0][c] + texels[1][c]) / 2.0);
    let expected: [(&str, [f32; 4]); 6] = [
        (
            "a color PNG",
            decode(srgb, png_pixels[1].map(|v| v as f32 / 255.0)),
        ),
        ("a data PNG", png_pixels[1].map(|v| v as f32 / 255.0)),
        ("a Radiance picture", decode(linear_srgb, hdr)),
        ("raw floats", raw[1]),
        (
            "a color in push data",
            decode(srgb, [0x20, 0x40, 0x60, 0x80].map(|v| v as f32 / 255.0)),
        ),
        ("the second mip level of a color PNG", mean),
    ];
    for (i, (what, expected)) in expected.iter().enumerate() {
        let actual = composite.pixel(32 + 64 * i as u32, 32);
        for c in 0..4 {
            assert!(
                (actual[c] - expected[c]).abs() <= 2e-5 + expected[c].abs() * 2e-3,
                "{what}: {actual:?}, expected {expected:?}"
            );
        }
    }

    // Colors, against the CPU reference.
    let colors = session.script("colors", json!([]));
    let straight = |rgba: [f32; 4]| {
        let [r, g, b, _] = decode(srgb, [rgba[0], rgba[1], rgba[2], 1.0]);
        [r, g, b, rgba[3]]
    };
    let expected: [[f32; 4]; 8] = [
        decode(srgb, [0x1e, 0x1e, 0x2e, 0xff].map(|v| v as f32 / 255.0)),
        decode(srgb, [0.5, 0.25, 1.0, 1.0]),
        decode(srgb, [0.5, 0.25, 1.0, 0.5]),
        straight([0.5, 0.25, 1.0, 0.5]),
        decode(ImageDescription::DISPLAY_P3, [0.5, 0.25, 1.0, 1.0]),
        decode(
            ImageDescription::SRGB_PIECEWISE,
            [1.0, 128.0 / 255.0, 0.0, 1.0],
        ),
        decode(
            ImageDescription::new(Primaries::ACES_AP1, TransferFunction::Linear),
            [0.5, 0.25, 1.0, 1.0],
        ),
        decode(
            ImageDescription::new(Primaries::ADOBE_RGB, TransferFunction::Power(1.8)),
            [0.5, 0.25, 1.0, 1.0],
        ),
    ];
    for (i, expected) in expected.iter().enumerate() {
        for c in 0..4 {
            let actual = colors[i][c].as_f64().unwrap() as f32;
            assert!(
                (actual - expected[c]).abs() <= 1e-6,
                "color {i}: {} expected {expected:?}",
                colors[i]
            );
        }
    }

    // What is refused comes back as an error the script can catch.
    for (what, message) in [
        ("outside", "inside the project"),
        ("missing", "none.png"),
        ("color", "is not a color"),
        ("space", "unknown color space"),
    ] {
        let error = session
            .try_send(Request::ScriptCall {
                function: "try".into(),
                args: vec![json!(what)],
            })
            .unwrap_err();
        assert!(error.contains(message), "{what}: {error}");
    }
    session.assert_drawing_clean();
    session.quit();
}

/// The output in four columns, one window each, read at the middle of the
/// window. The top half reads the windows themselves; the bottom half reads
/// converted copies (`color.blit_window`).
const OWN_SHADER: &str = r#"
import color;

struct Scene {
    WindowParams windows[4];
};

[[vk::binding(0, 0)]] Texture2D<float4> u_window0;
[[vk::binding(1, 0)]] Texture2D<float4> u_window1;
[[vk::binding(2, 0)]] Texture2D<float4> u_window2;
[[vk::binding(3, 0)]] Texture2D<float4> u_window3;
[[vk::binding(4, 0)]] SamplerState u_sampler;
[[vk::binding(5, 0)]] ConstantBuffer<Scene> scene;
[[vk::binding(6, 0)]] Texture2D<float4> u_copies[4];

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
    if (i.st.y >= 0.5) {
        return u_copies[column].SampleLevel(u_sampler, middle, 0.0);
    }
    switch (column) {
    case 0:
        return WindowTexture(u_window0, u_sampler, scene.windows[0]).sample_level(middle, 0.0);
    case 1:
        return WindowTexture(u_window1, u_sampler, scene.windows[1]).sample(middle);
    case 2:
        return WindowTexture(u_window2, u_sampler, scene.windows[2]).load(int2(3, 3));
    default:
        return window_to_working(u_window3.SampleLevel(u_sampler, middle, 0.0), scene.windows[3]);
    }
}
"#;

const MAIN: &str = r#"
local color = hz.include("color")

local pipeline, sampler, scene_buffer, scene_slot
local scene = buffer.create(4 * color.WINDOW_PARAMS_SIZE)
local copies, copy_slots = {}, nil
local brightness = {}

function init()
    pipeline = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", shader = "shaders/own.slang", entry = "vs_main" }),
        fragment = gfx.create_shader({ stage = "fragment", shader = "shaders/own.slang", entry = "fs_main" }),
        targets = { { format = color.format() } },
    })
    sampler = gfx.create_sampler({ mag = "linear", min = "linear" })
    scene_buffer = gfx.create_buffer({ size = buffer.len(scene), usage = { "uniform" } })
    scene_slot = gfx.create_uniform_slot(scene_buffer, 0, buffer.len(scene))
    for i = 1, 4 do
        copies[i] = gfx.create_image({
            format = color.format(),
            width = 8,
            height = 8,
            usage = { "color_target", "sampled" },
        })
    end
    copy_slots = gfx.create_texture_slots(copies)
end

function set_brightness(index, value)
    brightness[index] = value
    hz.request_frame()
end

function frame(output, target)
    local composite = color.composite_for(output)
    -- The windows in the order of their titles ("w1" .. "w4").
    local windows = hz.windows()
    table.sort(windows, function(a, b)
        return (a.title or "") < (b.title or "")
    end)
    local surfaces = {}
    for i = 1, 4 do
        local window = windows[i]
        local surface = window and hz.surfaces(window.id)[1]
        if not surface or not surface.texture then
            gfx.begin_pass({ color = { { image = composite.image, load = "clear", clear = { 0, 0, 0, 1 } } } })
            gfx.end_pass()
            color.present(composite, output, target)
            return
        end
        surfaces[i] = surface
        local options = { brightness = brightness[i] }
        color.write_window_params(scene, (i - 1) * color.WINDOW_PARAMS_SIZE, surface, options)
        color.blit_window(surface, copies[i], options)
    end
    gfx.write_buffer(scene_buffer, 0, scene)

    gfx.begin_pass({ color = { { image = composite.image, load = "dont-care" } } })
    gfx.set_pipeline(pipeline)
    for i = 1, 4 do
        gfx.bind(i - 1, surfaces[i].texture)
    end
    gfx.bind(4, sampler)
    gfx.bind(5, scene_slot)
    gfx.bind(6, copy_slots[1])
    gfx.draw(3)
    gfx.end_pass()
    color.present(composite, output, target)
end
"#;

fn a_projects_own_shader_reads_windows_in_the_working_space(device: Device) {
    let project = script_project("color-api-read", device, &[("main", MAIN)]);
    std::fs::create_dir_all(project.join("shaders")).unwrap();
    std::fs::write(project.join("shaders/own.slang"), OWN_SHADER).unwrap();
    let mut session = Session::start(
        "color-api-read",
        device,
        &[
            "--output-size",
            "256x128",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    assert_eq!(session.script_state()["mode"], "running");

    // What each window stores, and what its values mean. The alpha of the
    // second is not 1: its stored color is premultiplied.
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
            let x = 32 + 64 * i as u32;
            for (what, y) in [("read", 32), ("copy", 96)] {
                let actual = composite.pixel(x, y);
                for c in 0..4 {
                    assert!(
                        (actual[c] - expected[c]).abs() <= 2e-5 + expected[c].abs() * 2e-3,
                        "{name}: window {} ({what}) is {actual:?}, expected {expected:?}",
                        i + 1
                    );
                }
            }
        }
    };
    check(&mut session, "read", [1.0; 4]);

    // The brightness of a window is the script's to choose.
    session.script("set_brightness", json!([1, 0.5]));
    session.script("set_brightness", json!([3, 2.0]));
    check(&mut session, "brightness", [0.5, 1.0, 2.0, 1.0]);

    session.assert_script_healthy();
    session.assert_drawing_clean();
    drop(clients);
    session.quit();
}

const PARAMS_COST_MAIN: &str = r##"
local color = hz.include("color")

function frame(output, target)
    local composite = color.composite_for(output)
    gfx.begin_pass({ color = { { image = composite.image, load = "clear", clear = { 0, 0, 0, 1 } } } })
    gfx.end_pass()
    color.present(composite, output, target)
end

-- Microseconds per call of `write_window_params` and of `hz.color`.
function measure(calls)
    local window = hz.windows()[1]
    local surface = hz.surfaces(window.id)[1]
    local push = buffer.create(color.WINDOW_PARAMS_SIZE)
    local options = { brightness = 1.5 }
    local started = hz.time()
    for _ = 1, calls do
        color.write_window_params(push, 0, surface, options)
    end
    local params = (hz.time() - started) / calls * 1e6
    started = hz.time()
    for _ = 1, calls do
        hz.color("#1e1e2e")
    end
    local colors = (hz.time() - started) / calls * 1e6
    started = hz.time()
    for _ = 1, calls do
        hz.surfaces(window.id)
    end
    local surfaces = (hz.time() - started) / calls * 1e6
    return { params = params, colors = colors, surfaces = surfaces }
end
"##;

/// What it costs a script to fill in the numbers for one window
/// (`color.write_window_params`), next to other calls of a frame.
///
/// Not a test; ignored by default.
#[test]
#[ignore = "a measurement, run by hand"]
fn window_params_cost() {
    let device = Device::new(common::Gpu::Lavapipe);
    let project = script_project(
        "color-api-params-cost",
        device,
        &[("main", PARAMS_COST_MAIN)],
    );
    let mut session = Session::start(
        "color-api-params-cost",
        device,
        &[
            "--output-size",
            "64x64",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    let (client, _) = session.mapped_client(options("w", 32, 32, [200, 100, 50, 255]));
    session.screenshot("first");
    let cost = session.script("measure", json!([20000]));
    println!(
        "write_window_params: {:.2} us, hz.color: {:.2} us, hz.surfaces: {:.2} us per call",
        cost["params"].as_f64().unwrap(),
        cost["colors"].as_f64().unwrap(),
        cost["surfaces"].as_f64().unwrap(),
    );
    drop(client);
    session.quit();
}

/// What the format of the compositing buffer costs: the time to draw a
/// frame of the built-in renderer with RGBA16F and with RGBA32F.
///
/// Not a test: it asserts nothing about the numbers and is ignored by
/// default. Run it on a quiet machine:
///
/// ```sh
/// cargo test -p proto-compositor --test color_api -- --ignored --nocapture color_format_cost
/// ```
#[test]
#[ignore = "a measurement, run by hand"]
fn color_format_cost() {
    const FRAMES: usize = 200;
    println!("| device | output | format | draw median (ms) | draw p99 (ms) |");
    println!("| --- | --- | --- | ---: | ---: |");
    for gpu in [common::Gpu::Hardware, common::Gpu::Lavapipe] {
        let device = Device::new(gpu);
        if !device.enabled() {
            continue;
        }
        for size in ["1920x1080", "3840x2160"] {
            for format in ["rgba16float", "rgba32float"] {
                let mut session = Session::start(
                    "color-format-cost",
                    device,
                    &[
                        "--output-size",
                        size,
                        "--script-wait-gpu",
                        "--color-format",
                        format,
                    ],
                );
                let mut clients = Vec::new();
                for index in 0..8u8 {
                    let title = format!("w{index}");
                    let color = [60 + index * 20, 200 - index * 15, 90, 255];
                    let (client, id) = session.mapped_client(options(&title, 600, 400, color));
                    session.send(Request::MoveWindow {
                        window: id,
                        x: 20 + (index % 4) as i32 * 450,
                        y: 20 + (index / 4) as i32 * 420,
                    });
                    clients.push(client);
                }
                session.screenshot("first");
                for frame in 0..FRAMES {
                    let level = 40 + (frame % 200) as u8;
                    session.recolor(&mut clients[0], "w0", [level, 255 - level, 90, 255]);
                }
                let state = session.state();
                let draw = &state["frame_time_ms"]["draw"];
                println!(
                    "| {} | {size} | {format} | {:.3} | {:.3} |",
                    device.gpu_name(),
                    draw["median"].as_f64().unwrap_or(f64::NAN),
                    draw["p99"].as_f64().unwrap_or(f64::NAN),
                );
                drop(clients);
                session.quit();
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

/// A project that writes straight into the output's image.
const UNPRESENTED_SHADER: &str = r#"
[shader("vertex")]
float4 vs_main(uint index : SV_VulkanVertexID) : SV_Position {
    float2 corner = float2(float((index << 1) & 2), float(index & 2));
    return float4(corner * 2.0 - 1.0, 0.0, 1.0);
}

[shader("fragment")]
float4 fs_main() : SV_Target0 {
    return float4(0.25, 0.5, 0.75, 1.0);
}
"#;

const UNPRESENTED_MAIN: &str = r#"
local pipelines = {}
local vertex, fragment

function init()
    vertex = gfx.create_shader({ stage = "vertex", shader = "shaders/plain.slang", entry = "vs_main" })
    fragment = gfx.create_shader({ stage = "fragment", shader = "shaders/plain.slang", entry = "fs_main" })
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

/// The way out is the core's (`hz.present`, which `color.present` calls). A
/// script that draws into the output's image itself is not stopped: its
/// values go to the output as they are. The core counts such frames (and
/// says so once in its log).
fn a_frame_that_does_not_present_is_noticed(device: Device) {
    let project = script_project(
        "color-api-unpresented",
        device,
        &[("main", UNPRESENTED_MAIN)],
    );
    std::fs::create_dir_all(project.join("shaders")).unwrap();
    std::fs::write(project.join("shaders/plain.slang"), UNPRESENTED_SHADER).unwrap();
    let mut session = Session::start(
        "color-api-unpresented",
        device,
        &[
            "--output-size",
            "64x48",
            "--script",
            project.to_str().unwrap(),
        ],
    );
    let image = session.screenshot_png("plain");
    // The values the shader wrote, not converted: 0.25, 0.5, 0.75 of 255.
    let pixel = image.pixel(32, 24);
    for (c, expected) in [64u16, 128, 191].into_iter().enumerate() {
        assert!(pixel[c].abs_diff(expected) <= 1, "{pixel:?}");
    }
    let state = session.script_state();
    assert_eq!(state["mode"], "running");
    assert!(
        state["frames_without_present"].as_u64().unwrap() >= 1,
        "{state}"
    );
    let log = std::fs::read_to_string(session.output_dir.join("proto-compositor.log")).unwrap();
    assert_eq!(
        log.matches("returned without presenting").count(),
        1,
        "{log}"
    );
    session.quit();
}
