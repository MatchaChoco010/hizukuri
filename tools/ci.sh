#!/usr/bin/env bash
# Runs the checks of the repository: formatting, lints, and all tests.
# CI runs exactly this, inside the `ci` development shell:
#
#   nix develop .#ci --command tools/ci.sh
#
# usage: tools/ci.sh [lint|test|all]
#
#   lint  cargo fmt --check and cargo clippy
#   test  cargo test (every test binary, also after a failure), then a
#         check that the tests left no process running. The tests run
#         through tools/ci-watch.sh: tests that get stuck are reported, with
#         the backtrace of every thread, and stopped.
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
#   HIZUKURI_TEST_STALL_SECONDS  seconds without output after which the
#                             tests are taken to be stuck. Default: 300.
#                             0 turns the watch off.
#   HIZUKURI_TEST_CORE_DIR    the directory the kernel writes cores into;
#                             the backtraces of processes that crashed are
#                             printed from them (see tools/ci-watch.sh).
#   HIZUKURI_TEST_INSPECT_AS_ROOT  set to 1 to read the kernel stacks of
#                             stuck tests with `sudo -n`.
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

# cargo test with the arguments given, every test binary also after one of
# them failed: a run that fails shows everything that fails, not only the
# first.
#
# The tests are built first, and then run through tools/ci-watch.sh, which
# takes a run that writes nothing for some minutes to be stuck. A build may
# well be silent that long; tests are not.
run_tests() {
    cargo test "$@" --no-run || return 1
    # cargo colors its output for a terminal, and would stop doing so when
    # its output is passed on by the watch.
    if [ -t 1 ] && [ -z "${CARGO_TERM_COLOR:-}" ]; then
        CARGO_TERM_COLOR=always tools/ci-watch.sh cargo test "$@" --no-fail-fast
    else
        tools/ci-watch.sh cargo test "$@" --no-fail-fast
    fi
}

# The drivers keep the shaders they compile in a cache on disk, by default
# in the home of whoever runs the tests. The tests, and every process they
# start, use a cache of their own instead, empty at the start of the run and
# removed at its end: a run never writes into the person's cache, and it
# compiles what a fresh CI runner compiles (a warm cache would let the
# drivers skip most of their shader compiler). One cache for the whole run,
# so that a shader the run has compiled once is not compiled again.
# .cargo/config.toml turns the caches off for a cargo test started without
# this script; the variables here take their place.
SHADER_CACHE=""
use_own_shader_cache() {
    SHADER_CACHE="$(mktemp -d "${TMPDIR:-/tmp}/hizukuri-shader-cache.XXXXXX")"
    trap 'rm -rf "$SHADER_CACHE"' EXIT
    mkdir "$SHADER_CACHE/mesa" "$SHADER_CACHE/nvidia"
    # Mesa (lavapipe, RADV, ANV and the other Mesa drivers).
    export MESA_SHADER_CACHE_DISABLE=false
    export MESA_SHADER_CACHE_DIR="$SHADER_CACHE/mesa"
    # The NVIDIA driver.
    export __GL_SHADER_DISK_CACHE=1
    export __GL_SHADER_DISK_CACHE_PATH="$SHADER_CACHE/nvidia"
}

test_all() {
    # The gfx crates read this instead of the HIZUKURI_ variable.
    if [ "${HIZUKURI_TEST_VALIDATION:-}" = "1" ]; then
        export GFX_SPIKE_VALIDATION=1
    fi

    if [ -n "${HIZUKURI_TEST_SKIP_UDMABUF:-}" ]; then
        echo "== note: HIZUKURI_TEST_SKIP_UDMABUF is set; the tests that need /dev/udmabuf are left out"
    fi

    use_own_shader_cache

    local failed=0
    local devices="${HIZUKURI_TEST_DEVICES:-lavapipe}"

    # These crates create their Vulkan devices in the test process, one per
    # test, on parallel test threads. On a hardware device they run one test
    # at a time: the NVIDIA driver can deadlock when one thread destroys its
    # device while another thread that used the driver exits (the thread's
    # exit runs the driver's thread-local destructors), and the test harness
    # then waits for that thread forever.
    local in_process=(proto-gfx-core proto-gfx-lua proto-slangc)

    if [[ ",$devices," == *",gpu,"* ]]; then
        local exclude=() packages=() crate
        for crate in "${in_process[@]}"; do
            exclude+=(--exclude "$crate")
            packages+=(-p "$crate")
        done
        echo "== cargo test"
        run_tests --workspace "${exclude[@]}" || failed=1
        echo "== cargo test (${in_process[*]} on the hardware device, one test at a time)"
        RUST_TEST_THREADS=1 run_tests "${packages[@]}" || failed=1
    else
        echo "== cargo test"
        run_tests --workspace || failed=1
    fi

    # The tests of the script GPU API (proto-gfx-core, proto-gfx-lua) run
    # in-process, on one device per run: on lavapipe, unless gpu is listed.
    # When both are listed, the runs above used the hardware device; run them
    # once more on lavapipe, like the compositor tests do for themselves.
    if [[ ",$devices," == *",lavapipe,"* && ",$devices," == *",gpu,"* ]]; then
        local lavapipe
        lavapipe="$(lavapipe_manifest)"
        if [ -z "$lavapipe" ]; then
            echo "the lavapipe ICD manifest was not found" >&2
            exit 1
        fi
        echo "== cargo test (proto-gfx-core, proto-gfx-lua on lavapipe: $lavapipe)"
        VK_DRIVER_FILES="$lavapipe" run_tests -p proto-gfx-core -p proto-gfx-lua || failed=1
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

    if ((failed)); then
        echo "tests failed (see the cargo test output above)" >&2
        exit 1
    fi
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
