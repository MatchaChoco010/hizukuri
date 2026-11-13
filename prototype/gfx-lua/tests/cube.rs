//! The 3D example script (`scripts/cube.luau`): a textured cube above a
//! floor, with a depth attachment and a mipmapped texture, checked by
//! reading pixels back.
//!
//! Its shaders are Slang; the test compiles them with `slangc` (the Nix dev
//! shell provides it) and hands the SPIR-V to the script through the host's
//! shader loader, the way a project's shaders reach it.
//!
//! With `GFX_EXAMPLE_SHOTS=<directory>`, the picture is also written there
//! as `cube.png`.
#![cfg(feature = "luau")]

mod common;

use std::path::Path;
use std::rc::Rc;

use common::*;
use proto_gfx_core::Format;
use proto_gfx_lua::{HostOptions, ShaderLoader};
use proto_slangc::{Compiler, Input};

/// Compiles `scripts/<name>` for the entry point a script asks for. The
/// stage is the one of the entry point's `[shader("...")]` attribute.
fn slang_loader() -> ShaderLoader {
    ShaderLoader(Rc::new(|name: &str, entry: &str| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join(name);
        let compiler = Compiler::find().map_err(|e| e.to_string())?;
        let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "entry-points-{}-{entry}.json",
            name.replace('/', "-")
        ));
        let stage = compiler
            .entry_points(&path, &scratch)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|(found, _)| found == entry)
            .map(|(_, stage)| stage)
            .ok_or_else(|| format!("no shader {name} with entry point `{entry}`"))?;
        compiler
            .compile(Input::File(&path), stage, entry)
            .map(|compiled| compiled.spirv)
            .map_err(|e| format!("{name}: {e}"))
    }))
}

fn write_png(directory: &str, name: &str, width: u32, height: u32, pixels: &[u8]) {
    std::fs::create_dir_all(directory).unwrap();
    let file = std::fs::File::create(Path::new(directory).join(name)).unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(pixels).unwrap();
}

#[test]
fn cube() {
    const WIDTH: u32 = 480;
    const HEIGHT: u32 = 360;
    let host = host_with(HostOptions {
        shader_loader: Some(slang_loader()),
        ..patient()
    });
    eprintln!("cube on {}", device_line(&host));
    load_example(&host, "cube.luau");
    let output = target(&host, Format::Rgba8Unorm, WIDTH, HEIGHT);
    host.call_in_frame("frame", (output.to_bits(), 0.0))
        .unwrap();
    let pixels = read(&host, output);
    if let Ok(directory) = std::env::var("GFX_EXAMPLE_SHOTS") {
        write_png(&directory, "cube.png", WIDTH, HEIGHT, &pixels);
    }

    let clear = [13, 18, 31, 255];
    let is_clear = |p: [u8; 4]| close8(p, clear, 2);
    // The sky above the scene is the clear colour.
    assert!(is_clear(px8(&pixels, WIDTH, 10, 10)), "top left corner");
    assert!(
        is_clear(px8(&pixels, WIDTH, WIDTH - 10, 10)),
        "top right corner"
    );
    // The cube is in the middle, tinted orange: more red than blue. The
    // floor, drawn after it, did not replace it (the depth test).
    let middle = px8(&pixels, WIDTH, WIDTH / 2, HEIGHT / 2 - 20);
    assert!(
        !is_clear(middle) && middle[0] > middle[2].saturating_add(20),
        "the cube: {middle:?}"
    );
    // The floor is grey (its tint is white): the channels are close.
    let floor = px8(&pixels, WIDTH, WIDTH / 2, HEIGHT - 12);
    assert!(
        !is_clear(floor) && floor[0].abs_diff(floor[1]) < 60,
        "the floor: {floor:?}"
    );
    // A second frame, turned: the same depth image is used again.
    host.call_in_frame("frame", (output.to_bits(), 0.5))
        .unwrap();
    let turned = read(&host, output);
    assert_ne!(pixels, turned);
    assert_no_validation_errors();
}

/// The example with shadows (`scripts/shadow.luau`): a shadow map read with
/// a comparison sampler, a multisampled scene pass resolved by the pass,
/// and instances in a storage buffer.
#[test]
fn shadow() {
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 400;
    let host = host_with(HostOptions {
        shader_loader: Some(slang_loader()),
        ..patient()
    });
    load_example(&host, "shadow.luau");
    let output = target(&host, Format::Rgba8Unorm, WIDTH, HEIGHT);
    host.call_in_frame("frame", output.to_bits()).unwrap();
    let pixels = read(&host, output);
    if let Ok(directory) = std::env::var("GFX_EXAMPLE_SHOTS") {
        write_png(&directory, "shadow.png", WIDTH, HEIGHT, &pixels);
    }

    // The mean of the red channel around the pixel that shows a point of
    // the floor.
    let floor = |x: f32, z: f32| -> f32 {
        let (px, py): (f32, f32) = host
            .call_function("project", (x, 0.0, z, WIDTH, HEIGHT), None)
            .unwrap();
        let (px, py) = (px.round() as i64, py.round() as i64);
        let mut sum = 0.0;
        let mut count = 0.0;
        for dy in -6..=6 {
            for dx in -6..=6 {
                let (x, y) = (px + dx, py + dy);
                assert!(
                    (0..i64::from(WIDTH)).contains(&x) && (0..i64::from(HEIGHT)).contains(&y),
                    "the point is outside the picture"
                );
                sum += f32::from(px8(&pixels, WIDTH, x as u32, y as u32)[0]);
                count += 1.0;
            }
        }
        sum / count
    };
    // The light travels towards -x and +z: the large cube at the origin
    // shadows the floor on that side of it, and not on the other.
    let shadowed = floor(-0.7, 0.75);
    let lit = floor(1.5, 1.5);
    assert!(
        lit > shadowed * 1.6 && shadowed > 20.0,
        "lit {lit}, shadowed {shadowed}"
    );
    // A second frame uses the same images again.
    host.call_in_frame("frame", output.to_bits()).unwrap();
    assert_eq!(pixels, read(&host, output));
    assert_no_validation_errors();
}
