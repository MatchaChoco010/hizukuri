#!/usr/bin/env bash
# Runs scene scripts that capture screenshots of the verification compositor.
#
#   run-scene.sh OUT_DIR SCENE.sh [SCENE.sh ...]
#
# Builds the compositor in the Nix development shell and runs each scene
# there, with HZ_OUT set to OUT_DIR. A scene sources headless.sh (the
# variable HZ_LIB holds its path) and uses the hz_* functions. The fonts the
# applications in the scenes use are those of the `slides` shell.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"

out="${1:?usage: run-scene.sh OUT_DIR SCENE.sh [SCENE.sh ...]}"
shift
(($#)) || { echo "usage: run-scene.sh OUT_DIR SCENE.sh [SCENE.sh ...]" >&2; exit 2; }
mkdir -p "$out"
out="$(cd "$out" && pwd)"
scenes=()
for scene in "$@"; do
    scenes+=("$(cd "$(dirname "$scene")" && pwd)/$(basename "$scene")")
done

cd "$root"
fonts="$(nix develop .#slides --command printenv TYPST_FONT_PATHS)"
nix develop --command cargo build -j "${CARGO_BUILD_JOBS:-8}" -p hizukuri -p hizukuri-test-client

status=0
for scene in "${scenes[@]}"; do
    echo "== $scene"
    HZ_OUT="$out" HZ_LIB="$here/headless.sh" HZ_FONT_PATHS="$fonts" \
        nix develop --command bash "$scene" || { echo "scene failed: $scene" >&2; status=1; }
done
exit "$status"
