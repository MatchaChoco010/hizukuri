//! The KMS parts of the `proto-compositor` binary: starting the backend with its
//! safety nets, `kms-info` and `kms-selftest`.

use std::path::{Path, PathBuf};

use calloop::signals::Signals;
use calloop::{EventLoop, LoopHandle, LoopSignal};
use clap::{Args, ValueEnum};
use proto_backend::kms::KmsConfig;
use proto_backend::kms::device::KmsDevice;
use proto_backend::kms::drm_device::DrmDevice;
use proto_backend::kms::model::DeviceSnapshot;
use proto_backend::kms::plan::{ModeRequest, OutputRequest, ScanoutPreference};
use proto_backend::kms::session::{self, OpenedSession};
use proto_backend::{Backend, BackendHost};
use proto_color::{CompositeSpace, ImageDescription};
use proto_compositor::safety::{TakeoverRequest, Watchdog};
use proto_compositor::selftest::{PatternRenderer, SelfTest};
use proto_render::{DeviceSelector, RenderError, Renderer};
use serde_json::json;

pub use proto_backend::kms::session::RealKmsBackend;

use crate::{
    check_real_hardware, create_renderer, desktop_session_present, gpu_config,
    insert_common_sources,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScanoutArg {
    /// dma-buf when the renderer runs on the display's GPU, CPU copy
    /// otherwise.
    Auto,
    /// The renderer's images are scanned out directly.
    Dmabuf,
    /// Frames are read back and copied into dumb buffers.
    Cpu,
}

impl From<ScanoutArg> for ScanoutPreference {
    fn from(arg: ScanoutArg) -> Self {
        match arg {
            ScanoutArg::Auto => ScanoutPreference::Auto,
            ScanoutArg::Dmabuf => ScanoutPreference::Dmabuf,
            ScanoutArg::Cpu => ScanoutPreference::CpuCopy,
        }
    }
}

/// Options of the KMS backend.
#[derive(Debug, Args)]
pub struct KmsArgs {
    /// KMS: DRM device node. Defaults to the seat's primary GPU.
    #[arg(long)]
    pub kms_device: Option<PathBuf>,

    /// KMS: the one connector to drive, e.g. `DP-1`. Other displays are
    /// not touched. Defaults to the first connected one.
    #[arg(long)]
    pub kms_output: Option<String>,

    /// KMS: mode as WIDTHxHEIGHT or WIDTHxHEIGHT@HZ. Defaults to the
    /// display's preferred mode.
    #[arg(long, value_parser = parse_mode)]
    pub kms_mode: Option<ModeRequest>,

    /// KMS: how frames get to the display.
    #[arg(long, value_enum, default_value = "auto")]
    pub kms_scanout: ScanoutArg,

    /// KMS: allow taking over a display on real hardware. Without this the
    /// KMS backend only starts inside a virtual machine.
    #[arg(long)]
    pub allow_real_hardware: bool,

    /// KMS: abort when the main loop is stuck for this many seconds, so
    /// the display is released. 0 disables the watchdog.
    #[arg(long, default_value_t = 15.0)]
    pub watchdog: f64,
}

/// Parses `1920x1080` or `1920x1080@60` (the rate may have decimals).
pub fn parse_mode(text: &str) -> Result<ModeRequest, String> {
    let (size, rate) = match text.split_once('@') {
        Some((size, rate)) => (size, Some(rate)),
        None => (text, None),
    };
    let (width, height) = crate::parse_size(size)?;
    let refresh_mhz = match rate {
        Some(rate) => Some(
            rate.parse::<f64>()
                .ok()
                .filter(|hz| *hz > 0.0)
                .map(|hz| (hz * 1000.0).round() as u32)
                .ok_or_else(|| format!("invalid refresh rate {rate:?}"))?,
        ),
        None => None,
    };
    Ok(ModeRequest {
        width,
        height,
        refresh_mhz,
    })
}

/// What the caller wants from the Rust renderer.
pub struct RendererRequest {
    pub selector: DeviceSelector,
    pub validation: bool,
}

/// A started KMS backend with what belongs to it.
pub struct Started {
    pub backend: RealKmsBackend,
    pub drawing: crate::Drawing,
    pub watchdog: Option<Watchdog>,
}

/// Creates the Rust renderer (`--renderer rust`) for a KMS device. With
/// `auto` that is the GPU the device belongs to; when Vulkan has no such
/// device (a software rasterizer only), any device will do and frames are
/// copied on the CPU. Returns the renderer and whether it runs on the KMS
/// device's GPU.
pub fn pick_renderer(
    request: &RendererRequest,
    device_number: Option<(u32, u32)>,
) -> Result<(Renderer, bool), String> {
    let renderer = match (&request.selector, device_number) {
        (DeviceSelector::Auto, Some((major, minor))) => {
            match create_renderer(DeviceSelector::DrmNode { major, minor }, request.validation) {
                Ok(renderer) => renderer,
                Err(RenderError::NoSuitableDevice { available, .. }) => {
                    tracing::warn!(
                        ?available,
                        "no Vulkan device belongs to the display's GPU; using another one"
                    );
                    create_renderer(DeviceSelector::Auto, request.validation)
                        .map_err(|e| format!("failed to create the renderer: {e}"))?
                }
                Err(error) => return Err(format!("failed to create the renderer: {error}")),
            }
        }
        (selector, _) => create_renderer(selector.clone(), request.validation)
            .map_err(|e| format!("failed to create the renderer: {e}"))?,
    };
    let on_kms_device =
        device_number.is_some() && renderer.device_info().drm_primary == device_number;
    Ok((renderer, on_kms_device))
}

fn start_watchdog(seconds: f64) -> Option<Watchdog> {
    (seconds > 0.0).then(|| Watchdog::start(std::time::Duration::from_secs_f64(seconds)))
}

fn open_session(request: &TakeoverRequest<'_>) -> Result<(OpenedSession, Option<String>), String> {
    let virtual_machine = check_real_hardware(request)?;
    if desktop_session_present() {
        tracing::warn!(
            "WAYLAND_DISPLAY or DISPLAY is set: starting a KMS session from inside a desktop \
             session, as asked with --backend kms"
        );
    }
    let opened = session::open(request.device).map_err(|e| e.to_string())?;
    Ok((opened, virtual_machine))
}

/// Starts the KMS backend for the compositor.
pub fn start<D: BackendHost + 'static>(
    handle: &LoopHandle<'static, D>,
    args: &KmsArgs,
    make_drawing: &mut crate::MakeDrawing<'_>,
    scale: f64,
    image_description: ImageDescription,
    composite_space: CompositeSpace,
) -> Result<Started, String> {
    let (opened, _) = open_session(&TakeoverRequest {
        allow_real_hardware: args.allow_real_hardware,
        device: args.kms_device.as_deref(),
        output: args.kms_output.as_deref(),
    })?;
    let (mut drawing, on_kms_device) = make_drawing(opened.device_number())?;
    let config = KmsConfig {
        output: OutputRequest {
            connector: args.kms_output.clone(),
            mode: args.kms_mode,
        },
        scanout: args.kms_scanout.into(),
        scale,
        image_description,
        composite_space,
        ..Default::default()
    };
    let backend = opened
        .start(handle, config, drawing.targets(), on_kms_device)
        .map_err(|e| format!("failed to start the KMS backend: {e}"))?;
    Ok(Started {
        backend,
        drawing,
        watchdog: start_watchdog(args.watchdog),
    })
}

// ---------------------------------------------------------------------------
// kms-info
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct InfoArgs {
    /// DRM device node. Without it, every `/dev/dri/card*` is listed.
    #[arg(long)]
    pub device: Option<PathBuf>,

    /// Remove what identifies the displays from EDIDs (manufacturer,
    /// product, serial number, date, name), so the output can be shared or
    /// kept as a test fixture.
    #[arg(long)]
    pub anonymize: bool,

    /// Write to this file instead of standard output.
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Print a short list for people instead of JSON: each device with its
    /// driver and whether it is the seat's primary GPU, and each connected
    /// display with its preferred mode. For choosing `--kms-device` and
    /// `--kms-output`.
    #[arg(long, conflicts_with_all = ["anonymize"])]
    pub summary: bool,
}

/// What sysfs says about a DRM device node, for the summary.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CardFacts {
    /// The firmware's display device: what udev calls the primary GPU of
    /// the seat, and what hizukuri uses when no device is named.
    boot_vga: bool,
    /// The PCI address, if it is a PCI device.
    pci: Option<String>,
}

fn card_facts(path: &Path) -> CardFacts {
    let Some(name) = path.file_name() else {
        return CardFacts::default();
    };
    let device = Path::new("/sys/class/drm").join(name).join("device");
    CardFacts {
        boot_vga: std::fs::read_to_string(device.join("boot_vga"))
            .is_ok_and(|text| text.trim() == "1"),
        pci: std::fs::read_link(&device).ok().and_then(|target| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        }),
    }
}

/// The lines of `kms-info --summary` for one device.
fn summarize(path: &Path, facts: &CardFacts, snapshot: &DeviceSnapshot) -> String {
    let mut text = format!(
        "{}  driver {}{}{}\n",
        path.display(),
        snapshot.driver.name,
        facts
            .pci
            .as_ref()
            .map(|pci| format!("  {pci}"))
            .unwrap_or_default(),
        if facts.boot_vga {
            "  [the seat's primary GPU]"
        } else {
            ""
        },
    );
    let mut any = false;
    for connector in snapshot.connectors.iter().filter(|c| c.connected) {
        any = true;
        let mode = proto_backend::kms::plan::choose_mode(connector, None)
            .map(|mode| {
                format!(
                    "{}x{} at {:.3} Hz",
                    mode.hdisplay,
                    mode.vdisplay,
                    mode.refresh_mhz() as f64 / 1000.0
                )
            })
            .unwrap_or_else(|_| "no modes".to_string());
        let in_use = proto_backend::kms::model::property(&connector.properties, "CRTC_ID")
            .is_some_and(|p| p.value != 0);
        text.push_str(&format!(
            "    {:<12} {mode}{}\n",
            connector.name,
            if in_use { "  (in use)" } else { "" }
        ));
    }
    if !any {
        text.push_str("    no display connected\n");
    }
    text
}

fn card_nodes() -> Vec<PathBuf> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("card"))
        })
        .collect();
    nodes.sort();
    nodes
}

/// `proto-compositor kms-info`: prints what a DRM device reports. Read-only: the
/// device is opened without write access and nothing is committed.
pub fn info(args: InfoArgs) -> Result<(), String> {
    if args.summary {
        let nodes = match &args.device {
            Some(path) => vec![path.clone()],
            None => card_nodes(),
        };
        if nodes.is_empty() {
            return Err("no /dev/dri/card* device found".to_string());
        }
        let mut text = String::new();
        for path in nodes {
            let mut device = DrmDevice::open_for_inspection(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let snapshot = device
                .snapshot()
                .map_err(|e| format!("{}: {e}", path.display()))?;
            text.push_str(&summarize(&path, &card_facts(&path), &snapshot));
        }
        return match &args.output {
            Some(path) => {
                std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
            }
            None => {
                print!("{text}");
                Ok(())
            }
        };
    }
    let snapshot_of = |path: &Path| -> Result<serde_json::Value, String> {
        let mut device =
            DrmDevice::open_for_inspection(path).map_err(|e| format!("{}: {e}", path.display()))?;
        device.set_anonymize(args.anonymize);
        let snapshot = device
            .snapshot()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::to_value(snapshot).map_err(|e| e.to_string())
    };
    let value = match &args.device {
        // One device: the snapshot itself, which is the fixture format.
        Some(path) => snapshot_of(path)?,
        None => {
            let nodes = card_nodes();
            if nodes.is_empty() {
                return Err("no /dev/dri/card* device found".to_string());
            }
            let mut devices = Vec::new();
            for path in nodes {
                devices.push(json!({
                    "device": path,
                    "snapshot": snapshot_of(&path)?,
                }));
            }
            serde_json::Value::Array(devices)
        }
    };
    let mut text = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    text.push('\n');
    match &args.output {
        Some(path) => std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display())),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// kms-selftest
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct SelftestArgs {
    /// The one connector to test, e.g. `DP-1`. Other displays are not
    /// touched.
    #[arg(long)]
    pub output: String,

    /// How long to run, in seconds. The test always ends by itself.
    #[arg(long)]
    pub seconds: f64,

    /// Directory for the report (`report.json`, `device.json`,
    /// `summary.txt`). Created if needed.
    #[arg(long)]
    pub report: PathBuf,

    /// DRM device node. Defaults to the seat's primary GPU.
    #[arg(long)]
    pub device: Option<PathBuf>,

    /// Mode as WIDTHxHEIGHT or WIDTHxHEIGHT@HZ. Defaults to the display's
    /// preferred mode.
    #[arg(long, value_parser = parse_mode)]
    pub mode: Option<ModeRequest>,

    /// How frames get to the display.
    #[arg(long, value_enum, default_value = "auto")]
    pub scanout: ScanoutArg,

    /// Allow running on real hardware. Without this the test only runs
    /// inside a virtual machine.
    #[arg(long)]
    pub allow_real_hardware: bool,

    /// Vulkan device: auto (the display's GPU), gpu, cpu, or part of a
    /// device or driver name.
    #[arg(long, default_value = "auto")]
    pub vulkan_device: String,

    /// Vulkan driver manifest(s), as a `VK_DRIVER_FILES` value.
    #[arg(long)]
    pub vulkan_icd: Option<String>,

    /// Enable the Vulkan validation layer (must be installed).
    #[arg(long)]
    pub vulkan_validation: bool,

    /// Run without drawing the pattern: black frames, timing only.
    #[arg(long)]
    pub no_renderer: bool,

    /// Abort when the main loop is stuck for this many seconds. 0 disables
    /// the watchdog.
    #[arg(long, default_value_t = 15.0)]
    pub watchdog: f64,
}

struct SelftestApp {
    backend: RealKmsBackend,
    renderer: Option<PatternRenderer>,
    test: SelfTest,
    signal: LoopSignal,
}

impl BackendHost for SelftestApp {
    fn backend_wake(&mut self) {
        self.test.process(&mut self.backend, self.renderer.as_mut());
        if self.test.quit_requested() {
            self.signal.stop();
        }
    }
}

/// `proto-compositor kms-selftest`: see [`proto_compositor::selftest`].
pub fn selftest(args: SelftestArgs, signals: Signals) -> Result<(), String> {
    if !(args.seconds > 0.0 && args.seconds <= 600.0) {
        return Err("--seconds must be between 0 and 600".to_string());
    }
    std::fs::create_dir_all(&args.report).map_err(|e| format!("{}: {e}", args.report.display()))?;
    let write = |name: &str, text: String| {
        let path = args.report.join(name);
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
    };

    let (mut opened, virtual_machine) = open_session(&TakeoverRequest {
        allow_real_hardware: args.allow_real_hardware,
        device: args.device.as_deref(),
        output: Some(&args.output),
    })?;
    let device_path = opened.device_path().to_path_buf();
    let device_number = opened.device_number();
    let seat = opened.seat();

    // What the device looks like, for whoever reads the report later. It
    // is also a ready-made test fixture.
    let snapshot = opened.with_device(|device| {
        device.set_anonymize(true);
        let snapshot = device.snapshot();
        device.set_anonymize(false);
        snapshot
    });
    match snapshot {
        Ok(snapshot) => write(
            "device.json",
            serde_json::to_string_pretty(&snapshot).map_err(|e| e.to_string())?,
        )?,
        Err(error) => tracing::warn!("could not read the device resources: {error}"),
    }

    // The pattern is drawn on the display's GPU when Vulkan has it (`auto`),
    // else on any device, and frames are then copied on the CPU.
    let mut renderer = match args.no_renderer {
        true => None,
        false => Some(PatternRenderer::new(gpu_config(
            &args.vulkan_device,
            args.vulkan_validation,
            device_number,
        ))?),
    };
    let on_kms_device = renderer
        .as_ref()
        .is_some_and(|r| device_number.is_some() && r.device().drm_primary == device_number);
    let renderer_info = renderer.as_ref().map(|r| {
        let device = r.device();
        json!({
            "device": device.name,
            "kind": if device.is_cpu { "cpu" } else { "gpu" },
            "driver": device.driver,
            "driver_info": serde_json::Value::Null,
            "drm_primary": device.drm_primary,
            "on_kms_device": on_kms_device,
            "scanout_modifiers": proto_render::TargetProvider::scanout_modifiers(r),
        })
    });

    let mut event_loop: EventLoop<SelftestApp> =
        EventLoop::try_new().map_err(|e| format!("failed to create the event loop: {e}"))?;
    let handle = event_loop.handle();
    let config = KmsConfig {
        output: OutputRequest {
            connector: Some(args.output.clone()),
            mode: args.mode,
        },
        scanout: args.scanout.into(),
        ..Default::default()
    };
    let backend = opened
        .start(
            &handle,
            config,
            renderer.as_mut().and_then(PatternRenderer::targets),
            on_kms_device,
        )
        .map_err(|e| format!("failed to start the KMS backend: {e}"))?;
    insert_common_sources(
        &handle,
        signals,
        Some(args.seconds),
        start_watchdog(args.watchdog),
        |app: &mut SelftestApp| app.signal.stop(),
    )?;

    let mut app = SelftestApp {
        backend,
        renderer,
        test: SelfTest::new(),
        signal: event_loop.get_signal(),
    };
    tracing::info!(output = %args.output, seconds = args.seconds, "self test running");
    let started = std::time::SystemTime::now();
    // Pick up the output the backend announced at startup.
    app.backend_wake();
    let loop_result = event_loop.run(None, &mut app, |_| {});
    // The validation counters must be read before the renderer is used for
    // the shutdown.
    let validation_errors = (app.renderer.is_some() && args.vulkan_validation)
        .then(proto_gfx_core::validation_error_count);

    let context = json!({
        "started_at_unix": started
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "virtualization": virtual_machine,
        "seat": seat,
        "device": device_path,
        "device_number": device_number,
        "renderer": renderer_info,
        "vulkan_validation_errors": validation_errors,
        "requested": {
            "output": args.output,
            "mode": args.mode.map(|m| json!({
                "width": m.width,
                "height": m.height,
                "refresh_mhz": m.refresh_mhz,
            })),
            "scanout": format!("{:?}", args.scanout).to_lowercase(),
            "seconds": args.seconds,
        },
    });
    let report = app.test.report(&app.backend, context);
    // Release the display before anything else can go wrong.
    Backend::shutdown(
        &mut app.backend,
        app.renderer.as_mut().and_then(PatternRenderer::targets),
    );

    write(
        "report.json",
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?,
    )?;
    let mut summary = format!(
        "proto-compositor kms-selftest: {}\noutput {} for {} s, {} frames presented\n\n",
        if report["passed"] == true {
            "PASSED"
        } else {
            "FAILED"
        },
        args.output,
        args.seconds,
        report["frames_presented"],
    );
    for check in report["checks"].as_array().into_iter().flatten() {
        summary.push_str(&format!(
            "{:<16} {:<18} {}\n",
            check["status"].as_str().unwrap_or("?"),
            check["name"].as_str().unwrap_or("?"),
            check["detail"].as_str().unwrap_or(""),
        ));
    }
    print!("{summary}");
    write("summary.txt", summary)?;
    tracing::info!(report = %args.report.display(), "self test finished");

    loop_result.map_err(|e| format!("event loop error: {e}"))?;
    if report["passed"] == true {
        Ok(())
    } else {
        Err(format!(
            "the self test failed; see {}",
            args.report.join("summary.txt").display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_names_the_primary_gpu_and_the_displays() {
        use proto_backend::kms::mock::SnapshotBuilder;
        let snapshot = SnapshotBuilder::new("example")
            .crtcs(2)
            .connector(
                "DP-1",
                true,
                &[(3840, 2160, 144, true), (1920, 1080, 60, false)],
            )
            .connector("DP-2", false, &[])
            .connector("HDMI-A-1", true, &[(1920, 1080, 60, true)])
            .build();
        let facts = CardFacts {
            boot_vga: true,
            pci: Some("PCI-ADDRESS".to_string()),
        };
        let text = summarize(Path::new("/dev/dri/card1"), &facts, &snapshot);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "/dev/dri/card1  driver example  PCI-ADDRESS  [the seat's primary GPU]"
        );
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[1].contains("DP-1") && lines[1].contains("3840x2160 at 144.000 Hz"));
        assert!(lines[2].contains("HDMI-A-1") && lines[2].contains("1920x1080 at 60.000 Hz"));

        let empty = SnapshotBuilder::new("example")
            .crtcs(1)
            .connector("DP-3", false, &[])
            .build();
        let text = summarize(Path::new("/dev/dri/card0"), &CardFacts::default(), &empty);
        assert_eq!(
            text,
            "/dev/dri/card0  driver example\n    no display connected\n"
        );
    }

    #[test]
    fn modes_parse_with_and_without_a_rate() {
        assert_eq!(
            parse_mode("1920x1080"),
            Ok(ModeRequest {
                width: 1920,
                height: 1080,
                refresh_mhz: None
            })
        );
        assert_eq!(
            parse_mode("3840x2160@59.94"),
            Ok(ModeRequest {
                width: 3840,
                height: 2160,
                refresh_mhz: Some(59_940)
            })
        );
        assert!(parse_mode("1920").is_err());
        assert!(parse_mode("1920x1080@fast").is_err());
        assert!(parse_mode("1920x1080@0").is_err());
    }

    #[test]
    fn scanout_arguments_map_to_preferences() {
        assert_eq!(
            ScanoutPreference::from(ScanoutArg::Auto),
            ScanoutPreference::Auto
        );
        assert_eq!(
            ScanoutPreference::from(ScanoutArg::Dmabuf),
            ScanoutPreference::Dmabuf
        );
        assert_eq!(
            ScanoutPreference::from(ScanoutArg::Cpu),
            ScanoutPreference::CpuCopy
        );
    }
}
