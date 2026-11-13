//! The built-in configs of the OCIO library: what they list, and known
//! values through the CPU processor.

use proto_ocio::{Config, DisplayRequest, Dynamic, ReferenceSpace, UniformType};

const STUDIO: &str = "ocio://studio-config-latest";

fn config() -> Config {
    proto_ocio::quiet();
    Config::from_file(STUDIO).expect("the built-in studio config")
}

fn srgb_encode(l: f64) -> f64 {
    if l <= 0.0031308 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

#[test]
fn lists_what_a_builtin_config_has() {
    let config = config();
    println!("OCIO {}", proto_ocio::version());
    println!("built-in configs: {:?}", proto_ocio::builtin_configs());
    println!("config: {}", config.name());
    assert!(!proto_ocio::builtin_configs().is_empty());

    let spaces = config.color_spaces();
    println!("{} colour spaces", spaces.len());
    assert!(spaces.iter().any(|s| s == "ACEScg"), "{spaces:?}");
    assert!(spaces.iter().any(|s| s == "ACES2065-1"));
    assert_eq!(
        config.reference_space("ACEScg"),
        Some(ReferenceSpace::Scene)
    );
    assert_eq!(config.reference_space("no such space"), None);

    let displays = config.displays();
    println!("displays: {displays:?}");
    assert!(displays.iter().any(|d| d == "sRGB - Display"));
    for display in &displays {
        println!("  {display}: {:?}", config.views(display));
    }
    let views = config.views("sRGB - Display");
    assert!(views.iter().any(|v| v == "Un-tone-mapped"), "{views:?}");
    assert!(config.default_display().is_some());
    assert!(config.default_view("sRGB - Display").is_some());

    let roles = config.roles();
    println!("roles: {roles:?}");
    assert_eq!(config.role("scene_linear").as_deref(), Some("ACEScg"));
    assert_eq!(config.resolve("scene_linear").as_deref(), Some("ACEScg"));
    assert_eq!(
        config.role("aces_interchange").as_deref(),
        Some("ACES2065-1")
    );
    assert!(config.role("cie_xyz_d65_interchange").is_some());
    assert!(!config.cache_id().unwrap().is_empty());
}

#[test]
fn errors_arrive_as_values() {
    proto_ocio::quiet();
    let missing = Config::from_file("/nonexistent/config.ocio");
    assert!(missing.is_err());
    println!("{}", missing.err().unwrap());
    assert!(Config::from_builtin("no-such-config").is_err());

    let config = config();
    let e = config.processor("ACEScg", "no such space").err().unwrap();
    println!("{e}");
    assert!(!e.0.is_empty());
    assert!(
        config
            .display_processor(&DisplayRequest::new("ACEScg", "no display", "no view"))
            .is_err()
    );
    // A processor without the property refuses to set it.
    let processor = config.processor("ACEScg", "ACES2065-1").unwrap();
    assert!(
        processor
            .cpu(true)
            .unwrap()
            .set_dynamic(Dynamic::Exposure, 1.0)
            .is_err()
    );
}

#[test]
fn cpu_values_match_known_ones() {
    let config = config();

    // Linear Rec.709 to the sRGB display without tone mapping is the sRGB
    // encoding.
    let processor = config
        .display_processor(&DisplayRequest::new(
            "Linear Rec.709 (sRGB)",
            "sRGB - Display",
            "Un-tone-mapped",
        ))
        .unwrap();
    let cpu = processor.cpu(true).unwrap();
    for l in [0.0, 0.001, 0.0031308, 0.01, 0.18, 0.5, 1.0] {
        let out = cpu.apply([l as f32, l as f32, l as f32, 0.5]).unwrap();
        let want = srgb_encode(l);
        // OCIO's sRGB curve is the continuous one (a slope of about 12.9232
        // below the break), not the rounded 12.92 of IEC 61966-2-1: the two
        // differ by up to 1e-5 near the break.
        for c in &out[..3] {
            assert!(
                (f64::from(*c) - want).abs() < 2e-5,
                "{l}: {out:?} vs {want}"
            );
        }
        assert_eq!(out[3], 0.5);
    }

    // ACES2065-1 to ACEScg is the AP0-to-AP1 matrix: grey stays grey, and
    // the first row is the published one.
    let processor = config.processor("ACES2065-1", "ACEScg").unwrap();
    let m = processor.as_matrix().unwrap().expect("a matrix");
    println!("AP0 -> AP1: {m:?}");
    let want = [1.4514393161, -0.2365107469, -0.2149285693];
    for (a, b) in m[0].iter().zip(want) {
        assert!((a - b).abs() < 1e-5, "{m:?}");
    }
    for row in m {
        assert!((row.iter().sum::<f64>() - 1.0).abs() < 1e-5);
    }

    // A view with tone mapping is not a matrix.
    let views = config.views("sRGB - Display");
    let aces = views
        .iter()
        .find(|v| v.starts_with("ACES"))
        .expect("an ACES view");
    let tone_mapped = config
        .display_processor(&DisplayRequest::new("ACEScg", "sRGB - Display", aces))
        .unwrap();
    assert_eq!(tone_mapped.as_matrix().unwrap(), None);
    let out = tone_mapped
        .cpu(true)
        .unwrap()
        .apply([0.18, 0.18, 0.18, 1.0])
        .unwrap();
    println!("{aces}: 0.18 grey -> {out:?}");
    assert!(out[0] > 0.2 && out[0] < 0.6);
    // The view puts scene white below the display's white, and rolls off
    // what is brighter.
    let cpu = tone_mapped.cpu(true).unwrap();
    let mut previous = 0.0;
    for scene in [1.0f32, 2.0, 4.0, 16.0] {
        let out = cpu.apply([scene, scene, scene, 1.0]).unwrap();
        println!("{aces}: {scene} -> {out:?}");
        assert!(out[0] > previous && out[0] <= 1.0);
        previous = out[0];
    }
    assert!(cpu.apply([1.0, 1.0, 1.0, 1.0]).unwrap()[0] < 0.9);
}

/// A view can be run backwards: display values to the scene values that the
/// view shows as them. Display colours brought in this way come out of the
/// view as they were.
#[test]
fn a_view_runs_backwards() {
    let config = config();
    let view = "ACES 2.0 - SDR 100 nits (Rec.709)";
    let forward = config
        .display_processor(&DisplayRequest::new("ACEScg", "sRGB - Display", view))
        .unwrap()
        .cpu(true)
        .unwrap();
    let mut request = DisplayRequest::new("ACEScg", "sRGB - Display", view);
    request.inverse = true;
    let backward = config
        .display_processor(&request)
        .unwrap()
        .cpu(true)
        .unwrap();
    for display in [
        [1.0f32, 1.0, 1.0],
        [0.5, 0.5, 0.5],
        [0.9, 0.47, 0.16],
        [0.08, 0.78, 0.35],
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
    ] {
        let scene = backward
            .apply([display[0], display[1], display[2], 1.0])
            .unwrap();
        let again = forward.apply(scene).unwrap();
        println!(
            "display {display:?} <- scene {:?} -> display {:?}",
            &scene[..3],
            &again[..3]
        );
        for c in 0..3 {
            assert!(
                (again[c] - display[c]).abs() < 2e-3,
                "{display:?} -> {again:?}"
            );
        }
    }
}

#[test]
fn working_space_primaries_come_out_as_a_matrix() {
    let config = config();
    let xyz = config.role("cie_xyz_d65_interchange").unwrap();
    let processor = config.processor("ACEScg", &xyz).unwrap();
    let m = processor.as_matrix().unwrap().expect("a matrix");
    println!("ACEScg -> {xyz}: {m:?}");
    // White (1, 1, 1) lands on D65 with Y = 1.
    let white: Vec<f64> = m.iter().map(|row| row.iter().sum()).collect();
    let (x, y) = (
        white[0] / (white[0] + white[1] + white[2]),
        white[1] / (white[0] + white[1] + white[2]),
    );
    println!("white: {white:?} xy = ({x}, {y})");
    assert!((white[1] - 1.0).abs() < 1e-4);
    assert!((x - 0.3127).abs() < 2e-4 && (y - 0.3290).abs() < 2e-4);
}

#[test]
fn shader_text_textures_and_uniforms_come_out() {
    let config = config();
    let views = config.views("sRGB - Display");
    println!("{views:?}");

    // No tone mapping: text only.
    let plain = config
        .display_processor(&DisplayRequest::new(
            "ACEScg",
            "sRGB - Display",
            "Un-tone-mapped",
        ))
        .unwrap();
    let mut desc = plain.shader("OCIOMain", "ocio").unwrap();
    let text = desc.text();
    assert!(text.contains("float4 OCIOMain("), "{text}");
    assert!(desc.textures().unwrap().is_empty());
    assert!(desc.uniforms().unwrap().is_empty());
    assert!(!desc.cache_id().is_empty());

    // With dynamic controls: uniforms that follow the properties.
    let mut request = DisplayRequest::new("ACEScg", "sRGB - Display", "Un-tone-mapped");
    request.dynamic_exposure = true;
    request.dynamic_gamma = true;
    let dynamic = config.display_processor(&request).unwrap();
    let mut desc = dynamic.shader("OCIOMain", "ocio").unwrap();
    assert!(desc.has_dynamic(Dynamic::Exposure));
    assert!(desc.has_dynamic(Dynamic::Gamma));
    assert!(!desc.has_dynamic(Dynamic::Contrast));
    let uniforms = desc.uniforms().unwrap();
    println!("{uniforms:?}");
    assert_eq!(uniforms.len(), 2);
    assert!(uniforms.iter().all(|u| u.ty == UniformType::Float));
    desc.set_dynamic(Dynamic::Exposure, 1.5).unwrap();
    desc.set_dynamic(Dynamic::Gamma, 0.8).unwrap();
    let after = desc.uniforms().unwrap();
    println!("{after:?}");
    assert!(after.iter().any(|u| u.value == [1.5]));
    assert!(after.iter().any(|u| u.value == [0.8]));

    // The CPU follows the same property: +1 stop doubles linear light.
    let mut request = DisplayRequest::new("ACEScg", "sRGB - Display", "Un-tone-mapped");
    request.dynamic_exposure = true;
    let processor = config.display_processor(&request).unwrap();
    let mut cpu = processor.cpu(true).unwrap();
    let base = cpu.apply([0.1, 0.1, 0.1, 1.0]).unwrap();
    cpu.set_dynamic(Dynamic::Exposure, 1.0).unwrap();
    let brighter = cpu.apply([0.1, 0.1, 0.1, 1.0]).unwrap();
    assert!((f64::from(base[0]) - srgb_encode(0.1)).abs() < 1e-5);
    assert!((f64::from(brighter[0]) - srgb_encode(0.2)).abs() < 1e-5);

    // The scale is part of the processor.
    let mut request = DisplayRequest::new("ACEScg", "sRGB - Display", "Un-tone-mapped");
    request.scale = 2.0;
    let scaled = config.display_processor(&request).unwrap();
    let out = scaled
        .cpu(true)
        .unwrap()
        .apply([0.1, 0.1, 0.1, 1.0])
        .unwrap();
    assert!((f64::from(out[0]) - srgb_encode(0.2)).abs() < 1e-5);

    // Views with lookup tables: every texture has its values.
    for display in config.displays() {
        for view in config.views(&display) {
            let processor = config
                .display_processor(&DisplayRequest::new("ACEScg", &display, &view))
                .unwrap();
            let desc = processor.shader("OCIOMain", "ocio").unwrap();
            let textures = desc.textures().unwrap();
            for t in &textures {
                assert_eq!(
                    t.values.len(),
                    (t.width * t.height * t.depth * t.channels) as usize
                );
                assert!(desc.text().contains(&t.name));
                assert!(desc.text().contains(&t.sampler));
            }
            println!(
                "{display} / {view}: {} bytes, textures {:?}",
                desc.text().len(),
                textures
                    .iter()
                    .map(|t| format!(
                        "{} {}x{}x{} c{} {}",
                        t.name,
                        t.width,
                        t.height,
                        t.depth,
                        t.channels,
                        if t.linear { "linear" } else { "nearest" }
                    ))
                    .collect::<Vec<_>>()
            );
        }
    }
}
