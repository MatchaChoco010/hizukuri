//! Shared helpers for the integration tests.
#![allow(dead_code)]

use std::time::Duration;

use gfx_core::{Format, Gpu, GpuConfig, Handle, ImageDesc, ImageUsage};
use gfx_lua::{HostOptions, ScriptHost};

/// The time budget of calls that are expected to return. These tests are
/// not about how fast the machine is: a call under this budget only times
/// out when it is stuck. Calls that are meant to be interrupted get a short
/// budget of their own (`ScriptHost::call_in_frame_within` and friends).
pub const PATIENT: Duration = Duration::from_secs(300);

/// How much later than its budget an interrupted call may come back. The
/// interrupt is prompt (a fraction of a millisecond); the allowance is for a
/// machine that is busy with other things.
pub const STOPS_WITHIN: Duration = Duration::from_secs(30);

/// Host options with the [`PATIENT`] time budget.
pub fn patient() -> HostOptions {
    HostOptions {
        time_budget: Some(PATIENT),
        ..HostOptions::default()
    }
}

pub fn host() -> ScriptHost {
    host_with(patient())
}

pub fn host_with(options: HostOptions) -> ScriptHost {
    let gpu = Gpu::new(GpuConfig::for_tests()).expect("create gpu");
    ScriptHost::new(gpu, options).expect("create script host")
}

pub fn script(name: &str) -> String {
    let path = format!("{}/scripts/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// Loads a script from `scripts/` and runs its `init` inside a frame.
pub fn load_example(host: &ScriptHost, name: &str) {
    host.load(name, &script(name)).expect("load script");
    host.call_in_frame("init", ()).expect("init");
}

pub fn image(
    host: &ScriptHost,
    format: Format,
    width: u32,
    height: u32,
    usage: ImageUsage,
) -> Handle {
    host.gpu()
        .create_image(&ImageDesc {
            format,
            width,
            height,
            usage,
        })
        .expect("create image")
}

/// An image filled by the host, standing in for a window's contents.
pub fn texture(host: &ScriptHost, format: Format, width: u32, height: u32, data: &[u8]) -> Handle {
    let handle = image(
        host,
        format,
        width,
        height,
        ImageUsage::SAMPLED | ImageUsage::COPY_DST | ImageUsage::COPY_SRC,
    );
    let mut gpu = host.gpu();
    gpu.begin_frame().expect("begin frame");
    gpu.upload_image(handle, data).expect("upload");
    gpu.end_frame().expect("end frame");
    handle
}

pub fn target(host: &ScriptHost, format: Format, width: u32, height: u32) -> Handle {
    image(
        host,
        format,
        width,
        height,
        ImageUsage::COLOR_TARGET | ImageUsage::COPY_SRC | ImageUsage::SAMPLED,
    )
}

pub fn read(host: &ScriptHost, image: Handle) -> Vec<u8> {
    host.gpu().read_image(image).expect("read image")
}

pub fn px8(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * width + x) * 4) as usize;
    pixels[at..at + 4].try_into().unwrap()
}

pub fn px16f(pixels: &[u8], width: u32, x: u32, y: u32) -> [f32; 4] {
    let at = ((y * width + x) * 8) as usize;
    std::array::from_fn(|c| {
        half::f16::from_bits(u16::from_ne_bytes([
            pixels[at + 2 * c],
            pixels[at + 2 * c + 1],
        ]))
        .to_f32()
    })
}

pub fn f16_bytes(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|v| half::f16::from_f32(*v).to_bits().to_ne_bytes())
        .collect()
}

pub fn assert_close(actual: [f32; 4], expected: [f32; 4], tolerance: f32, what: &str) {
    for c in 0..4 {
        assert!(
            (actual[c] - expected[c]).abs() <= tolerance,
            "{what}: channel {c}: got {actual:?}, expected {expected:?} (tolerance {tolerance})"
        );
    }
}

/// How far an 8-bit UNORM channel written from a float may be from the
/// round-to-nearest code value. Vulkan converts a float `f` to unsigned
/// normalized fixed-point by taking either of the two code values closest to
/// `f × 255`: rounding to nearest is recommended, not required, and only a
/// product that is exactly an integer must come out unchanged. `0.5` (127.5)
/// may be written as 127 or 128.
pub const UNORM8_CONVERSION: u8 = 1;

pub fn close8(actual: [u8; 4], expected: [u8; 4], tolerance: u8) -> bool {
    (0..4).all(|c| actual[c].abs_diff(expected[c]) <= tolerance)
}

pub fn assert_close8(actual: [u8; 4], expected: [u8; 4], tolerance: u8, what: &str) {
    for c in 0..4 {
        assert!(
            actual[c].abs_diff(expected[c]) <= tolerance,
            "{what}: channel {c}: got {actual:?}, expected {expected:?} (tolerance {tolerance})"
        );
    }
}

/// With `GFX_SPIKE_VALIDATION=1`, fails if the validation layer reported
/// anything: the API's own checks must reject misuse before Vulkan sees it.
pub fn assert_no_validation_errors() {
    assert_eq!(
        gfx_core::validation_error_count(),
        0,
        "validation layer messages: {:#?}",
        gfx_core::validation_messages()
    );
}

pub fn device_line(host: &ScriptHost) -> String {
    let gpu = host.gpu();
    format!("{} / {}", gpu.device_name(), gpu.driver_name())
}
