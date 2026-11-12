#!/usr/bin/env bash
# Runs a real wl_shm application (weston-terminal) inside a headless
# proto-compositor, clicks and types into it through the control socket, and
# captures the screen. This is a look-at-it check, not an automated test:
# open the PNGs it writes.
#
# Nothing touches a session that is running where this is started: the
# compositor and the application get their own XDG_RUNTIME_DIR,
# WAYLAND_DISPLAY is set explicitly and DISPLAY is removed. Everything
# started here is stopped before the script returns.
#
# usage: tools/real-app-check.sh OUT_DIR [VULKAN_DEVICE] [VK_DRIVER_FILES]
#   tools/real-app-check.sh artifacts/real-app
#   tools/real-app-check.sh artifacts/real-app-lavapipe cpu "$HIZUKURI_ICD_LAVAPIPE"
set -u
cd "$(dirname "$0")/.."
OUT="${1:?usage: tools/real-app-check.sh OUT_DIR [VULKAN_DEVICE] [VK_DRIVER_FILES]}"
DEVICE="${2:-auto}"
ICD="${3:-}"
BIN="${CARGO_TARGET_DIR:-target}/debug"

command -v weston-terminal > /dev/null || { echo "weston-terminal is not installed" >&2; exit 1; }
cargo build -j "${CARGO_BUILD_JOBS:-8}" -p proto-compositor || exit 1

# Unix socket paths are short, so the runtime directory must be too.
RT="${XDG_RUNTIME_DIR:-/tmp}/hzk-app-$$"
rm -rf "$RT"; mkdir -m 700 "$RT"
mkdir -p "$OUT"
ctl() { timeout 15 "$BIN/proto-ctl" --socket "$RT/hizukuri-app.control" "$@"; }
EXTRA=(); [ -n "$ICD" ] && EXTRA+=(--vulkan-icd "$ICD")

# A stand-in for the shell: prints known text, then echoes what is typed.
cat > "$RT/fake-shell.sh" <<'SHELL'
#!/bin/sh
printf 'proto-compositor headless: weston-terminal\r\n'
printf '\033[31mred \033[32mgreen \033[34mblue \033[0mdefault\r\n'
printf 'typed: '
exec cat
SHELL
chmod +x "$RT/fake-shell.sh"

# Nothing started here can find a session that is already running: not by
# the name of its Wayland socket, not through an inherited socket, an X
# server or the session bus, and not through any other variable that names
# a socket in that session's runtime directory.
ISOLATED=(env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET -u DISPLAY -u XAUTHORITY
    -u DBUS_SESSION_BUS_ADDRESS)
if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    while IFS='=' read -r -d '' name value; do
        if [[ "$value" == "$XDG_RUNTIME_DIR" || "$value" == "$XDG_RUNTIME_DIR"/* ||
            "$value" == *"=$XDG_RUNTIME_DIR"/* ]]; then
            ISOLATED+=(-u "$name")
        fi
    done < <(env -0)
fi
ISOLATED+=(XDG_RUNTIME_DIR="$RT")
SERVER=""
APP=""
# Whatever happens, stop what this script started, by the ids it got when it
# started them. Never by name: other programs of the same names may be
# running.
cleanup() {
    local pid
    for pid in $APP $SERVER; do
        kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT

"${ISOLATED[@]}" timeout -k 2 60 "$BIN/proto-compositor" \
    --backend headless --socket hizukuri-app --output-size 1024x640 --exit-after 45 \
    --vulkan-device "$DEVICE" "${EXTRA[@]}" > "$OUT/proto-compositor.log" 2>&1 &
SERVER=$!
for _ in $(seq 50); do [ -S "$RT/hizukuri-app.control" ] && break; sleep 0.1; done
if ! ctl ping > /dev/null; then
    echo "the compositor did not start:" >&2
    cat "$OUT/proto-compositor.log" >&2
    wait $SERVER
    rm -rf "$RT"
    exit 1
fi

"${ISOLATED[@]}" WAYLAND_DISPLAY=hizukuri-app timeout -k 2 30 \
    weston-terminal --shell="$RT/fake-shell.sh" > "$OUT/weston-terminal.log" 2>&1 &
APP=$!
sleep 2
ctl state > "$OUT/state-before.json"
ctl screenshot "$OUT/terminal-1.png" > /dev/null

# Click into the middle of the window, then type "hi a"
# (evdev codes: h=35, i=23, space=57, a=30).
read -r CX CY < <(python3 - "$OUT/state-before.json" <<'PY'
import json, sys
window = json.load(open(sys.argv[1]))["windows"][0]
print(window["position"][0] + window["size"][0] // 2, window["position"][1] + window["size"][1] // 2)
PY
)
ctl pointer-motion "$CX" "$CY" > /dev/null
ctl pointer-button 272 press > /dev/null
ctl pointer-button 272 release > /dev/null
for key in 35 23 57 30; do
    ctl key "$key" press > /dev/null
    ctl key "$key" release > /dev/null
done
sleep 1
ctl screenshot "$OUT/terminal-2.png" --composite "$OUT/terminal-2.exr" > /dev/null
ctl state > "$OUT/state-after.json"
python3 - "$OUT/state-after.json" <<'PY'
import json, sys
state = json.load(open(sys.argv[1]))
print("gpu:", state["gpu"]["device"])
print("focus:", state["focus"])
for window in state["windows"]:
    print("window", window["id"], repr(window["title"]), "position", window["position"], "size", window["size"])
    for surface in window["surfaces"]:
        print("   surface", surface["id"], surface["role"], "offset", surface["offset"], "size", surface["size"])
PY

ctl quit > /dev/null
wait $SERVER; echo "proto-compositor exit status: $?"
wait $APP; echo "weston-terminal exit status: $?"
rm -f "$RT/fake-shell.sh"
if rmdir "$RT"; then echo "runtime directory removed"; else echo "left in $RT:"; ls -la "$RT"; fi
# Both were waited for above; a process that could not be reaped is still
# there under the id this script knows it by.
for pid in $SERVER $APP; do
    if kill -0 "$pid" 2>/dev/null; then
        echo "process $pid, started by this script, is still running" >&2
        exit 1
    fi
done
echo "screenshots: $OUT/terminal-1.png (before input), $OUT/terminal-2.png (after typing 'hi a')"
