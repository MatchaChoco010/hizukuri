#!/usr/bin/env bash
# Runs the checks of the repository: formatting, lints, and all tests.
# CI runs exactly this, inside the `ci` development shell:
#
#   nix develop .#ci --command tools/ci.sh
#
# usage: tools/ci.sh [lint|test|all]
#
#   lint  cargo fmt --check and cargo clippy
#   test  cargo test, then a check that the tests left no process running
#   all   both, in that order (the default)
#
# Environment:
#   HIZUKURI_TEST_DEVICES     comma separated list of Vulkan devices to test
#                             on: lavapipe (Mesa's software rasterizer) and
#                             gpu (a hardware device the Vulkan loader
#                             offers). Default: lavapipe, so that the checks
#                             do the same on every machine. A listed device
#                             that is missing is a failure.
#   HIZUKURI_TEST_REAL_CLIENTS  the real applications the tests use (see
#                             prototype/compositor/tests/common/mod.rs).
#                             A listed application that is missing is a
#                             failure.
#   HIZUKURI_TEST_SKIP_UDMABUF  set to 1 to leave out the tests that need
#                             /dev/udmabuf (dma-bufs without a GPU). Without
#                             it they fail when the device cannot be opened.
#   HIZUKURI_TEST_VALIDATION  set to 1 to run the tests with the Vulkan
#                             validation layer (it must be installed).
#   HIZUKURI_ICD_DIRS         colon separated directories searched for Vulkan
#                             driver manifests before the system's.
#   CARGO_BUILD_JOBS          parallel build jobs. Default here: 8.
#   CARGO_TARGET_DIR          where cargo builds. Default: target.
set -euo pipefail
cd "$(dirname "$0")/.."

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-8}"

lint() {
    echo "== cargo fmt --check"
    cargo fmt --all --check

    # With the profile of the tests: it builds the dependencies, and what
    # their build scripts compile, without optimization (Cargo.toml).
    echo "== cargo clippy"
    cargo clippy --profile test --workspace --all-targets -- -D warnings
}

# Every process the tests start inherits this variable, and so does whatever
# those processes start in turn. It is how this script recognizes what it
# left behind: by what the process carries, not by what it is called. The
# machine may be running other programs with the very same names
# (proto-compositor, wl-paste, terminals).
export HIZUKURI_TEST_RUN="ci-$$-$(date +%s%N)"

# Fills LEFTOVER with "pid command line" of the processes of this run that
# are still alive. Uses shell builtins only, so that the search itself
# starts no process that would carry the mark.
leftover() {
    local environ pid variable line
    LEFTOVER=()
    for environ in /proc/[0-9]*/environ; do
        pid="${environ#/proc/}"
        pid="${pid%/environ}"
        [[ "$pid" != "$$" && "$pid" != "$BASHPID" && -O "$environ" ]] || continue
        while IFS= read -r -d '' variable; do
            if [[ "$variable" == "HIZUKURI_TEST_RUN=$HIZUKURI_TEST_RUN" ]]; then
                line=""
                { IFS= read -r -d '' line < "/proc/$pid/cmdline"; } 2>/dev/null || true
                LEFTOVER+=("$pid ${line:-?}")
                break
            fi
        done 2>/dev/null < "$environ" || true
    done
}

# The path of the lavapipe ICD manifest, or nothing.
lavapipe_manifest() {
    local dir manifest
    local -a icd_dirs
    IFS=':' read -r -a icd_dirs <<< "${HIZUKURI_ICD_DIRS:-}"
    for dir in "${icd_dirs[@]}" /usr/share/vulkan/icd.d /etc/vulkan/icd.d \
        /run/opengl-driver/share/vulkan/icd.d; do
        for manifest in "$dir"/lvp_icd*.json; do
            if [ -f "$manifest" ] && [[ "$manifest" != *i686* ]]; then
                echo "$manifest"
                return
            fi
        done
    done
}

test_all() {
    # The gfx crates read this instead of the HIZUKURI_ variable.
    if [ "${HIZUKURI_TEST_VALIDATION:-}" = "1" ]; then
        export GFX_SPIKE_VALIDATION=1
    fi

    if [ -n "${HIZUKURI_TEST_SKIP_UDMABUF:-}" ]; then
        echo "== note: HIZUKURI_TEST_SKIP_UDMABUF is set; the tests that need /dev/udmabuf are left out"
    fi

    echo "== cargo test"
    cargo test --workspace

    # The tests of the script GPU API (proto-gfx-core, proto-gfx-lua) run
    # in-process, on one device per run: on lavapipe, unless gpu is listed.
    # When both are listed, the run above used the hardware device; run them
    # once more on lavapipe, like the compositor tests do for themselves.
    local devices="${HIZUKURI_TEST_DEVICES:-lavapipe}"
    if [[ ",$devices," == *",lavapipe,"* && ",$devices," == *",gpu,"* ]]; then
        local lavapipe
        lavapipe="$(lavapipe_manifest)"
        if [ -z "$lavapipe" ]; then
            echo "the lavapipe ICD manifest was not found" >&2
            exit 1
        fi
        echo "== cargo test (proto-gfx-core, proto-gfx-lua on lavapipe: $lavapipe)"
        VK_DRIVER_FILES="$lavapipe" cargo test -p proto-gfx-core -p proto-gfx-lua
    fi

    # Nothing is stopped here. A process that is still running is a defect
    # of the test that started it; it is reported, with its id.
    echo "== leftover processes"
    leftover
    if ((${#LEFTOVER[@]})); then
        printf '%s\n' "${LEFTOVER[@]}"
        echo "processes started by this test run are still running (listed above)" >&2
        exit 1
    fi
    echo "none"
}

case "${1:-all}" in
    lint) lint ;;
    test) test_all ;;
    all)
        lint
        test_all
        ;;
    *)
        echo "usage: tools/ci.sh [lint|test|all]" >&2
        exit 2
        ;;
esac
echo "== all checks passed"
