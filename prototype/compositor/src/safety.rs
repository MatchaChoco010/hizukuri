//! Safety nets for running on a display that a person depends on.
//!
//! - [`virtualization`]: tells a virtual machine from real hardware, so the
//!   KMS backend can refuse to take over a real display unless that was
//!   asked for explicitly.
//! - [`Watchdog`]: ends the process when the main loop stops turning, so a
//!   hung compositor does not keep the display hostage. When the process
//!   dies the kernel and logind give the display back to the console.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// What the firmware and CPU say about where this is running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachineFacts {
    /// `/sys/class/dmi/id/sys_vendor`.
    pub sys_vendor: String,
    /// `/sys/class/dmi/id/product_name`.
    pub product_name: String,
    /// `/sys/hypervisor/type`, when the file exists.
    pub hypervisor_type: Option<String>,
    /// The `hypervisor` CPU flag is set (`/proc/cpuinfo`).
    pub hypervisor_flag: bool,
}

impl MachineFacts {
    /// Reads the facts of the running machine.
    pub fn read() -> Self {
        let read = |path: &str| {
            std::fs::read_to_string(Path::new(path))
                .map(|text| text.trim().to_string())
                .ok()
        };
        let cpuinfo = read("/proc/cpuinfo").unwrap_or_default();
        Self {
            sys_vendor: read("/sys/class/dmi/id/sys_vendor").unwrap_or_default(),
            product_name: read("/sys/class/dmi/id/product_name").unwrap_or_default(),
            hypervisor_type: read("/sys/hypervisor/type"),
            hypervisor_flag: cpuinfo
                .lines()
                .filter(|line| line.starts_with("flags"))
                .any(|line| line.split_whitespace().any(|flag| flag == "hypervisor")),
        }
    }
}

/// Names the virtualization this runs under, or `None` on real hardware.
///
/// The CPU's `hypervisor` flag is the decisive sign: a guest has it, a host
/// does not (also not a host that runs virtual machines itself). The DMI
/// strings only give it a name.
pub fn virtualization(facts: &MachineFacts) -> Option<String> {
    if !facts.hypervisor_flag && facts.hypervisor_type.is_none() {
        return None;
    }
    let text = format!("{} {}", facts.sys_vendor, facts.product_name).to_lowercase();
    let name = [
        ("qemu", "qemu"),
        ("kvm", "kvm"),
        ("vmware", "vmware"),
        ("virtualbox", "virtualbox"),
        ("innotek", "virtualbox"),
        ("microsoft corporation virtual", "hyper-v"),
        ("xen", "xen"),
        ("bochs", "bochs"),
        ("parallels", "parallels"),
    ]
    .iter()
    .find(|(needle, _)| text.contains(needle))
    .map(|(_, name)| name.to_string())
    .or_else(|| facts.hypervisor_type.clone())
    .unwrap_or_else(|| "unknown hypervisor".to_string());
    Some(name)
}

/// What was said on the command line about taking over a display.
#[derive(Debug, Clone, Copy, Default)]
pub struct TakeoverRequest<'a> {
    /// `--allow-real-hardware`.
    pub allow_real_hardware: bool,
    /// `--kms-device`: the DRM device, named explicitly.
    pub device: Option<&'a std::path::Path>,
    /// `--kms-output`: the one connector to drive, named explicitly.
    pub output: Option<&'a str>,
}

/// Decides whether a KMS session may start.
///
/// In a virtual machine there is nothing to protect: any request is fine.
/// On real hardware, taking over a display must be asked for, and nothing
/// may be left to a default: which GPU and which display are named on the
/// command line. A machine can have several GPUs (one of them may belong
/// to something else) and several displays, and "the first one" is not a
/// decision anyone made.
pub fn check_takeover(
    virtualization: Option<&str>,
    request: &TakeoverRequest<'_>,
) -> Result<(), String> {
    if virtualization.is_some() {
        return Ok(());
    }
    let mut missing = Vec::new();
    if !request.allow_real_hardware {
        missing.push("--allow-real-hardware (the KMS backend takes over the display)");
    }
    if request.device.is_none() {
        missing.push("the DRM device, e.g. --kms-device /dev/dri/card1");
    }
    if request.output.is_none() {
        missing.push("the one display to use, e.g. --kms-output DP-1");
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "this is real hardware, not a virtual machine. To run on it, say all of this on \
         purpose: {}. `proto-compositor kms-info --summary` lists the devices and displays without \
         touching them",
        missing.join("; ")
    ))
}

/// Ends the process when it is not fed for too long.
///
/// A thread checks the time since the last [`Watchdog::feed`]. The main
/// loop feeds it from a timer; if the loop hangs (a driver call that never
/// returns, a deadlock), the thread aborts the process.
///
/// A stop of the whole process is not a stuck main loop: SIGSTOP, a debugger,
/// a paused virtual machine. The watchdog thread is stopped along with the
/// rest and, once things continue, may well run before the main loop does.
/// It notices that it overslept itself and then gives the main loop a fresh
/// period instead of aborting a healthy process.
pub struct Watchdog {
    start: Instant,
    last_fed_ms: Arc<AtomicU64>,
}

/// How often the watchdog thread looks.
const WATCHDOG_INTERVAL_MS: u64 = 250;
/// A wake-up this much later than planned means the thread itself did not
/// run: the process was stopped, not just its main loop.
const WATCHDOG_OVERSLEEP_MS: u64 = 2_000;

/// Whether the time between two wake-ups of the watchdog thread shows that
/// the thread itself was not running.
fn watchdog_overslept(previous_wake_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(previous_wake_ms) > WATCHDOG_INTERVAL_MS + WATCHDOG_OVERSLEEP_MS
}

impl Watchdog {
    /// Starts the watchdog thread. `timeout` is how long the main loop may
    /// be silent.
    pub fn start(timeout: Duration) -> Self {
        let start = Instant::now();
        let last_fed_ms = Arc::new(AtomicU64::new(0));
        let shared = last_fed_ms.clone();
        let spawned = std::thread::Builder::new()
            .name("watchdog".to_string())
            .spawn(move || {
                let mut previous_wake = 0;
                loop {
                    std::thread::sleep(Duration::from_millis(WATCHDOG_INTERVAL_MS));
                    let now = start.elapsed().as_millis() as u64;
                    if watchdog_overslept(previous_wake, now) {
                        // The whole process stood still. Count the main
                        // loop's silence from here.
                        shared.store(now, Ordering::Relaxed);
                    }
                    previous_wake = now;
                    let silent = now.saturating_sub(shared.load(Ordering::Relaxed));
                    if silent > timeout.as_millis() as u64 {
                        // Not a panic and no destructors: the main thread is
                        // stuck and cannot be relied on. The kernel closes
                        // the devices.
                        eprintln!(
                            "proto-compositor: the main loop has been stuck for {silent} ms; aborting \
                             so that the display is released"
                        );
                        std::process::abort();
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!("could not start the watchdog thread: {error}");
        }
        Self { start, last_fed_ms }
    }

    /// Tells the watchdog the main loop is alive.
    pub fn feed(&self) {
        self.last_fed_ms
            .store(self.start.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_process_is_told_from_a_stuck_main_loop() {
        // Regular wake-ups, also somewhat late ones: the thread was running.
        assert!(!watchdog_overslept(1_000, 1_250));
        assert!(!watchdog_overslept(1_000, 2_500));
        // Far too late: the thread did not run, so nothing else did either.
        assert!(watchdog_overslept(1_000, 4_000));
        assert!(watchdog_overslept(1_000, 111_000));
    }

    #[test]
    fn a_virtual_machine_needs_no_permission() {
        assert_eq!(
            check_takeover(Some("qemu"), &TakeoverRequest::default()),
            Ok(())
        );
    }

    #[test]
    fn real_hardware_needs_everything_said_explicitly() {
        let device = std::path::Path::new("/dev/dri/card1");
        let full = TakeoverRequest {
            allow_real_hardware: true,
            device: Some(device),
            output: Some("DP-1"),
        };
        assert_eq!(check_takeover(None, &full), Ok(()));

        // Each one is required, and the message names what is missing.
        let cases = [
            (
                TakeoverRequest {
                    allow_real_hardware: false,
                    ..full
                },
                "--allow-real-hardware",
            ),
            (
                TakeoverRequest {
                    device: None,
                    ..full
                },
                "--kms-device",
            ),
            (
                TakeoverRequest {
                    output: None,
                    ..full
                },
                "--kms-output",
            ),
        ];
        for (request, flag) in cases {
            let error = check_takeover(None, &request).unwrap_err();
            assert!(error.contains(flag), "{error}");
            assert!(error.contains("real hardware"), "{error}");
            let others = ["--allow-real-hardware", "--kms-device", "--kms-output"];
            for other in others.iter().filter(|o| **o != flag) {
                assert!(!error.contains(&format!("{other} ")), "{error}");
            }
        }
        let error = check_takeover(None, &TakeoverRequest::default()).unwrap_err();
        for flag in ["--allow-real-hardware", "--kms-device", "--kms-output"] {
            assert!(error.contains(flag), "{error}");
        }
    }

    fn facts(vendor: &str, product: &str, flag: bool) -> MachineFacts {
        MachineFacts {
            sys_vendor: vendor.to_string(),
            product_name: product.to_string(),
            hypervisor_type: None,
            hypervisor_flag: flag,
        }
    }

    #[test]
    fn a_qemu_guest_is_recognized() {
        let guest = facts("QEMU", "Standard PC (Q35 + ICH9, 2009)", true);
        assert_eq!(virtualization(&guest).as_deref(), Some("qemu"));
    }

    #[test]
    fn real_hardware_is_not_a_virtual_machine() {
        // A desktop board. It may well run virtual machines itself; that
        // does not make it one.
        let host = facts("Some Vendor", "System Product Name", false);
        assert_eq!(virtualization(&host), None);
        // DMI strings alone do not decide: without the CPU flag this is
        // hardware (or something lying about its name).
        let odd = facts("QEMU", "Standard PC", false);
        assert_eq!(virtualization(&odd), None);
    }

    #[test]
    fn unknown_hypervisors_still_count() {
        let guest = facts("Some Cloud", "Instance", true);
        assert_eq!(
            virtualization(&guest).as_deref(),
            Some("unknown hypervisor")
        );
        let xen = MachineFacts {
            hypervisor_type: Some("xen".to_string()),
            ..facts("", "", false)
        };
        assert_eq!(virtualization(&xen).as_deref(), Some("xen"));
        assert_eq!(
            virtualization(&facts("innotek GmbH", "VirtualBox", true)).as_deref(),
            Some("virtualbox")
        );
    }

    #[test]
    fn reading_the_running_machine_does_not_fail() {
        // Whatever this machine is, reading must work and be consistent.
        let facts = MachineFacts::read();
        let _ = virtualization(&facts);
    }

    #[test]
    fn a_fed_watchdog_stays_quiet() {
        let watchdog = Watchdog::start(Duration::from_secs(3600));
        watchdog.feed();
        std::thread::sleep(Duration::from_millis(300));
        watchdog.feed();
    }
}
