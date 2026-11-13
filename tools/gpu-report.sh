#!/usr/bin/env bash
# Writes down what the GPUs of this machine, their kernel drivers and the
# Vulkan driver the loader offers look like. Reads only: nothing is loaded,
# installed or changed, and no device is opened for writing.
#
# usage: tools/gpu-report.sh OUT_DIR
#
#   OUT_DIR/hardware.txt        kernel, PCI display devices, DRM nodes
#   OUT_DIR/vulkaninfo.txt      the full output of vulkaninfo
#   OUT_DIR/vulkan-summary.md   the devices, and whether each extension the
#                               compositor depends on is offered
#
# The Vulkan part describes what `vulkaninfo` on PATH sees with the current
# environment. To describe one driver, point VK_DRIVER_FILES at its manifest
# before calling this. The exit status is 0 when the report was written,
# whatever it says.
set -uo pipefail

if [ $# -ne 1 ]; then
    echo "usage: tools/gpu-report.sh OUT_DIR" >&2
    exit 2
fi
out="$1"
mkdir -p "$out"

# Extensions the compositor needs (the first) or uses for dma-bufs and for
# finding the DRM node of a Vulkan device.
extensions=(
    VK_EXT_descriptor_heap
    VK_KHR_external_memory_fd
    VK_EXT_external_memory_dma_buf
    VK_EXT_image_drm_format_modifier
    VK_EXT_physical_device_drm
    VK_KHR_external_semaphore_fd
    VK_EXT_queue_family_foreign
)

section() {
    printf '\n== %s\n' "$1"
}

{
    section "kernel"
    uname -srvm

    section "PCI display devices"
    if command -v lspci > /dev/null; then
        lspci -nn | grep -iE 'vga|3d|display' || echo "none"
    else
        echo "lspci is not installed"
    fi

    section "DRM nodes"
    ls -l /dev/dri/ 2> /dev/null || echo "no /dev/dri"

    section "DRM cards"
    found=false
    for card in /sys/class/drm/card*; do
        [ -e "$card" ] || continue
        case "$card" in *-*) continue ;; esac # connectors, not cards
        found=true
        driver="none"
        if [ -e "$card/device/driver" ]; then
            driver="$(basename "$(readlink -f "$card/device/driver")")"
        fi
        vendor="$(cat "$card/device/vendor" 2> /dev/null || echo "-")"
        device="$(cat "$card/device/device" 2> /dev/null || echo "-")"
        echo "$(basename "$card"): driver $driver, vendor $vendor, device $device"
        for connector in "$card"/card*-*; do
            [ -e "$connector/status" ] || continue
            echo "    $(basename "$connector"): $(cat "$connector/status")"
        done
    done
    "$found" || echo "none"

    section "loaded GPU modules"
    if [ -r /proc/modules ]; then
        grep -E '^(amdgpu|radeon|nvidia[a-z_]*|nouveau|i915|xe|vkms|virtio_gpu|udmabuf) ' /proc/modules \
            | cut -d' ' -f1 | sort || true
    fi

    section "NVIDIA kernel driver"
    cat /proc/driver/nvidia/version 2> /dev/null || echo "not loaded"
} > "$out/hardware.txt" 2>&1

{
    echo "### Vulkan"
    echo
    echo "\`VK_DRIVER_FILES\`: \`${VK_DRIVER_FILES:-not set}\`"
    echo
    if ! command -v vulkaninfo > /dev/null; then
        echo "vulkaninfo is not on PATH."
    elif ! vulkaninfo > "$out/vulkaninfo.txt" 2> "$out/vulkaninfo.stderr"; then
        echo "vulkaninfo failed:"
        echo
        echo '```'
        tail -n 20 "$out/vulkaninfo.stderr"
        echo '```'
    else
        echo '```'
        # The lines of `vulkaninfo --summary` that name each device.
        vulkaninfo --summary 2> /dev/null \
            | grep -E '^(GPU[0-9]+:|[[:space:]]+(apiVersion|deviceName|deviceType|driverName|driverInfo|conformanceVersion)[[:space:]])'
        echo '```'
        echo
        devices="$(grep -c '^GPU[0-9]*:$' "$out/vulkaninfo.txt" || true)"
        echo "Devices: $devices. Each number below is how many of them offer the extension."
        echo
        echo "| extension | devices |"
        echo "| --- | --- |"
        for extension in "${extensions[@]}"; do
            count="$(grep -cE "^[[:space:]]+${extension}[[:space:]]+: extension revision" "$out/vulkaninfo.txt" || true)"
            echo "| \`$extension\` | $count |"
        done
    fi
} > "$out/vulkan-summary.md" 2>&1

echo "wrote $out/hardware.txt and $out/vulkan-summary.md"
