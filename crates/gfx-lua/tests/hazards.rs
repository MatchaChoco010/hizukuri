//! Things the API's checks cannot prevent, tried in a child process so that a
//! hang or a crash stays there.
//!
//! The GPU experiments always run on the software rasterizer, whatever device
//! the rest of the test suite uses: a shader that never returns would make a
//! real GPU reset under whatever else is using it.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The manifest of the lavapipe driver: in the directories of
/// `HIZUKURI_ICD_DIRS` (separated by colons), then in the usual system
/// locations. The tests fail when there is none.
fn lavapipe_manifest() -> std::path::PathBuf {
    let extra = std::env::var_os("HIZUKURI_ICD_DIRS").unwrap_or_default();
    let dirs: Vec<std::path::PathBuf> = std::env::split_paths(&extra)
        .filter(|dir| !dir.as_os_str().is_empty())
        .chain(
            [
                "/usr/share/vulkan/icd.d",
                "/usr/local/share/vulkan/icd.d",
                "/etc/vulkan/icd.d",
            ]
            .map(std::path::PathBuf::from),
        )
        .collect();
    let native = format!("lvp_icd.{}.json", std::env::consts::ARCH);
    dirs.iter()
        .flat_map(|dir| [dir.join(&native), dir.join("lvp_icd.json")])
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("the lavapipe ICD manifest was not found in {dirs:?}"))
}

struct Outcome {
    stdout: String,
    stderr: String,
    /// `None` if the child had to be killed.
    status: Option<std::process::ExitStatus>,
    elapsed: Duration,
}

fn run_probe(args: &[&str], deadline: Duration) -> Outcome {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gfx-probe"))
        .args(args)
        .env("VK_DRIVER_FILES", lavapipe_manifest())
        .env_remove("GFX_SPIKE_DEVICE")
        .env_remove("GFX_SPIKE_VALIDATION")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gfx-probe");
    let start = Instant::now();
    let status = loop {
        match child.try_wait().expect("wait") {
            Some(status) => break Some(status),
            None if start.elapsed() > deadline => {
                child.kill().expect("kill");
                child.wait().expect("reap");
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let elapsed = start.elapsed();
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .ok();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .ok();
    Outcome {
        stdout,
        stderr,
        status,
        elapsed,
    }
}

/// A fragment shader that never terminates: the frame is valid, so nothing is
/// rejected; the host notices only because the GPU does not come back.
#[test]
fn shader_that_never_returns() {
    let outcome = run_probe(&["shader-hang"], Duration::from_secs(600));
    eprintln!(
        "shader-hang: status {:?} after {:?}\n{}",
        outcome.status, outcome.elapsed, outcome.stdout
    );
    let out = &outcome.stdout;
    assert!(out.contains("NORMAL_FRAME_OK"), "{out}\n{}", outcome.stderr);
    assert!(out.contains("HANG_SUBMITTED"), "{out}");
    // The API reports the timeout instead of blocking forever...
    assert!(out.contains("TIMEOUT_REPORTED"), "{out}");
    // ...and treats the device as lost from then on.
    assert!(out.contains("AFTER_TIMEOUT Err(DeviceLost)"), "{out}");
    // Whether the same process can carry on with a new device, and whether it
    // can exit by itself, is recorded here; on lavapipe both hold.
    assert!(out.contains("SECOND_DEVICE_OK"), "{out}");
    assert!(out.contains("DROPPED"), "{out}");
    // Whatever happened, killing the process is always available: this test
    // would have done so after the deadline.
    assert!(outcome.status.is_some(), "the child had to be killed");

    // A fresh process is unaffected.
    let again = run_probe(&["deep-script", "parens", "10"], Duration::from_secs(30));
    assert!(again.stdout.contains("LOADED"), "{}", again.stdout);
}

/// The shader compiler (naga) on inputs that nest or chain very deeply.
/// Called directly, without the large stack and the size limit that
/// `Gpu::create_shader` adds, to record what the compiler itself does.
#[test]
fn shader_compiler_on_hostile_sources() {
    let mut report = Vec::new();
    for (kind, depth) in [
        ("parens", 100_000),
        ("blocks", 100_000),
        ("ifs", 100_000),
        ("sum", 500),
        ("sum", 50_000),
    ] {
        let depth_text = depth.to_string();
        let outcome = run_probe(
            &["deep-shader", kind, &depth_text],
            Duration::from_secs(120),
        );
        let verdict = match outcome.status {
            None => "did not finish in 120 s; killed".to_owned(),
            Some(status) if status.success() => outcome
                .stdout
                .lines()
                .find(|l| l.starts_with("COMPILED") || l.starts_with("REJECTED"))
                .unwrap_or("no verdict")
                .chars()
                .take(90)
                .collect(),
            Some(status) => format!(
                "process died: {status} ({})",
                outcome
                    .stderr
                    .lines()
                    .find(|l| l.contains("overflow") || l.contains("panicked"))
                    .unwrap_or("")
                    .trim()
            ),
        };
        report.push(format!(
            "{kind} x {depth}: {verdict} [{:?}]",
            outcome.elapsed
        ));
    }
    eprintln!("naga on hostile sources:\n  {}", report.join("\n  "));
    // The nesting cases are handled by naga's own limits.
    assert!(report[0].contains("REJECTED"), "{}", report[0]);
    assert!(report[1].contains("REJECTED"), "{}", report[1]);
    assert!(report[2].contains("REJECTED"), "{}", report[2]);
    assert!(report[3].contains("COMPILED"), "{}", report[3]);
    // The long chain overflows the stack of the default thread and takes the
    // process with it: shader sources from scripts are not safe to compile
    // on the calling thread.
    assert!(report[4].contains("process died"), "{}", report[4]);
}

/// The script compiler on the same kind of input.
#[test]
fn script_compiler_on_hostile_sources() {
    for kind in ["parens", "tables", "calls"] {
        let outcome = run_probe(&["deep-script", kind, "1000000"], Duration::from_secs(60));
        eprintln!(
            "deep-script {kind}: {:?} {}",
            outcome.status,
            outcome.stdout.lines().nth(1).unwrap_or("")
        );
        assert!(
            outcome.status.is_some_and(|s| s.success()),
            "{kind}: {:?}\n{}",
            outcome.status,
            outcome.stderr
        );
        assert!(outcome.stdout.contains("REJECTED"), "{}", outcome.stdout);
    }
}
