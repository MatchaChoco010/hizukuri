//! Finding a Vulkan driver's ICD manifest, so that a test can point the
//! Vulkan loader at one driver (`VK_DRIVER_FILES`) in a child process.

use std::path::PathBuf;

/// Finds the manifest of a Vulkan driver whose file name starts with
/// `prefix` (`lvp` for lavapipe), for the architecture of this process. Set
/// `VK_DRIVER_FILES` to the returned path before the process starts.
///
/// The directories in `HIZUKURI_ICD_DIRS` (separated by colons) are
/// searched before the usual system locations, for environments that keep
/// manifests elsewhere (a Nix store, a Mesa build tree).
pub fn find_icd_manifest(prefix: &str) -> Option<PathBuf> {
    let extra = std::env::var_os("HIZUKURI_ICD_DIRS");
    let extra = extra
        .as_deref()
        .map(|dirs| std::env::split_paths(dirs).collect::<Vec<_>>())
        .unwrap_or_default();
    find_icd_manifest_in(prefix, &icd_search_dirs(&extra))
}

/// The directories to look for manifests in, in order: the given extra
/// ones, then the system locations.
pub fn icd_search_dirs(extra: &[PathBuf]) -> Vec<PathBuf> {
    const SYSTEM: [&str; 3] = [
        "/usr/share/vulkan/icd.d",
        "/usr/local/share/vulkan/icd.d",
        "/etc/vulkan/icd.d",
    ];
    extra
        .iter()
        .filter(|dir| !dir.as_os_str().is_empty())
        .cloned()
        .chain(SYSTEM.iter().map(PathBuf::from))
        .collect()
}

/// The first manifest for `prefix` in `dirs`, in order.
pub fn find_icd_manifest_in(prefix: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    // Multi-arch installs carry one manifest per architecture.
    const OTHER_ARCHES: [&str; 4] = ["i686", "i386", "armhf", "x86_64"];
    let foreign = |name: &str| {
        OTHER_ARCHES
            .iter()
            .filter(|arch| **arch != std::env::consts::ARCH)
            .any(|arch| name.contains(arch))
    };
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut candidates: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".json") && !foreign(n))
            })
            .collect();
        candidates.sort();
        if let Some(path) = candidates.into_iter().next() {
            return Some(path);
        }
    }
    None
}
