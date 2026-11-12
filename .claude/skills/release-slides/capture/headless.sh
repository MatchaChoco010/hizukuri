# Helpers for capturing screenshots of the verification compositor without a
# display. Source this from a scene script; run the scene with run-scene.sh,
# which puts it in the Nix development shell.
#
#   hz_start [HIZUKURI ARGS...]   start a headless compositor (CPU rendering)
#   hz_client [ARGS...]           start hizukuri-test-client in it
#   hz_app CMD [ARGS...]          start a real application in it
#   hz_wait_windows N             wait until N windows exist
#   hz_settle                     wait until no frame is owed
#   hz_ctl ARGS...                hizukuri-ctl against this compositor
#   hz_window_id PATTERN          id of the first window whose title or app id
#                                 contains PATTERN
#   hz_shot NAME                  write $HZ_OUT/NAME.png
#   hz_stop                       stop everything that was started here
#
# Nothing started here can reach a session that is already running on the
# machine: the compositor and its clients get their own runtime directory,
# home and font configuration, and every variable that names a display or a
# socket of the surrounding session is removed. Processes are stopped by the
# ids they were started with, never by name. The runtime directory is removed
# at the end.

set -u

: "${HZ_OUT:?set HZ_OUT to the directory the screenshots go to}"
: "${CARGO_TARGET_DIR:?run the scene through run-scene.sh (it enters the Nix development shell)}"
: "${HIZUKURI_ICD_LAVAPIPE:?run the scene through run-scene.sh (it enters the Nix development shell)}"

HZ_BIN="$CARGO_TARGET_DIR/debug"
# Unix socket paths are short, so the runtime directory must be too.
HZ_RT="${XDG_RUNTIME_DIR:-/tmp}/hzs-$$"
HZ_SOCKET="hz"
HZ_SERVER=""
HZ_CLIENTS=()

_hz_isolated=(env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET -u DISPLAY -u XAUTHORITY
    -u DBUS_SESSION_BUS_ADDRESS)
if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    while IFS='=' read -r -d '' _hz_name _hz_value; do
        if [[ "$_hz_value" == "$XDG_RUNTIME_DIR" || "$_hz_value" == "$XDG_RUNTIME_DIR"/* ||
            "$_hz_value" == *"=$XDG_RUNTIME_DIR"/* ]]; then
            _hz_isolated+=(-u "$_hz_name")
        fi
    done < <(env -0)
fi

hz_stop() {
    local pid
    for pid in "${HZ_CLIENTS[@]}" $HZ_SERVER; do
        kill "$pid" 2>/dev/null || true
    done
    for pid in "${HZ_CLIENTS[@]}" $HZ_SERVER; do
        wait "$pid" 2>/dev/null || true
    done
    HZ_CLIENTS=()
    HZ_SERVER=""
    rm -rf "$HZ_RT"
}
trap hz_stop EXIT

# The applications draw their text with the fonts the slides are set in, on
# every machine: fontconfig is pointed at those directories and nowhere else.
_hz_write_fontconfig() {
    local dir
    {
        echo '<?xml version="1.0"?>'
        echo '<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">'
        echo '<fontconfig>'
        IFS=':' read -r -a _hz_font_dirs <<<"${HZ_FONT_PATHS:-}"
        for dir in "${_hz_font_dirs[@]}"; do
            [ -n "$dir" ] && echo "  <dir>$dir</dir>"
        done
        echo "  <cachedir>$HZ_RT/home/.cache/fontconfig</cachedir>"
        echo '  <alias><family>monospace</family><prefer><family>JetBrains Mono</family><family>Noto Sans Mono CJK JP</family></prefer></alias>'
        echo '  <alias><family>sans-serif</family><prefer><family>Noto Sans CJK JP</family></prefer></alias>'
        echo '  <alias><family>serif</family><prefer><family>Noto Sans CJK JP</family></prefer></alias>'
        echo '</fontconfig>'
    } >"$HZ_RT/fonts.conf"
}

hz_start() {
    rm -rf "$HZ_RT"
    mkdir -m 700 "$HZ_RT"
    mkdir -p "$HZ_RT/home/.config" "$HZ_RT/home/.cache" "$HZ_OUT"
    _hz_write_fontconfig
    "${_hz_isolated[@]}" XDG_RUNTIME_DIR="$HZ_RT" "$HZ_BIN/hizukuri" \
        --backend headless --socket "$HZ_SOCKET" --exit-after "${HZ_SECONDS:-180}" \
        --vulkan-device cpu --vulkan-icd "$HIZUKURI_ICD_LAVAPIPE" \
        "$@" >"$HZ_RT/hizukuri.log" 2>&1 &
    HZ_SERVER=$!
    local _
    for _ in $(seq 100); do
        [ -S "$HZ_RT/$HZ_SOCKET.control" ] && hz_ctl ping >/dev/null 2>&1 && return 0
        kill -0 "$HZ_SERVER" 2>/dev/null || break
        sleep 0.1
    done
    echo "headless.sh: the compositor did not start:" >&2
    cat "$HZ_RT/hizukuri.log" >&2
    return 1
}

hz_ctl() {
    timeout 30 "$HZ_BIN/hizukuri-ctl" --socket "$HZ_RT/$HZ_SOCKET.control" "$@"
}

hz_client() {
    "$HZ_BIN/hizukuri-test-client" --socket "$HZ_RT/$HZ_SOCKET" "$@" >/dev/null 2>&1 &
    HZ_CLIENTS+=($!)
}

hz_app() {
    "${_hz_isolated[@]}" XDG_RUNTIME_DIR="$HZ_RT" WAYLAND_DISPLAY="$HZ_SOCKET" \
        HOME="$HZ_RT/home" XDG_CONFIG_HOME="$HZ_RT/home/.config" \
        XDG_CACHE_HOME="$HZ_RT/home/.cache" FONTCONFIG_FILE="$HZ_RT/fonts.conf" \
        "$@" >/dev/null 2>&1 &
    HZ_CLIENTS+=($!)
}

hz_wait_windows() {
    local want="$1" _ count
    for _ in $(seq 150); do
        count="$(hz_ctl state | python3 -c 'import json, sys; print(len(json.load(sys.stdin)["windows"]))')"
        [ "$count" -ge "$want" ] && return 0
        sleep 0.1
    done
    echo "headless.sh: expected $want windows, have $count" >&2
    return 1
}

hz_settle() {
    local _
    sleep "${1:-0.5}"
    for _ in $(seq 100); do
        hz_ctl state | python3 -c 'import json, sys; sys.exit(1 if json.load(sys.stdin)["redraw_pending"] else 0)' && return 0
        sleep 0.1
    done
    return 0
}

hz_window_id() {
    hz_ctl state | python3 -c '
import json, sys
pattern = sys.argv[1]
for window in json.load(sys.stdin)["windows"]:
    if pattern in (window.get("title") or "") or pattern in (window.get("app_id") or ""):
        print(window["id"])
        break
else:
    sys.exit("no window matches " + pattern)' "$1"
}

hz_shot() {
    hz_ctl screenshot "$HZ_OUT/$1.png" >/dev/null && echo "wrote $HZ_OUT/$1.png"
}

# Writes an executable file from standard input and prints its path. For a
# stand-in for the shell of a terminal: a real shell would put the name of
# the user and of the machine into the picture.
#   shell="$(hz_script demo-shell <<'SH'
#   #!/bin/sh
#   printf 'known text\r\n'; exec cat
#   SH
#   )"
hz_script() {
    cat >"$HZ_RT/$1"
    chmod +x "$HZ_RT/$1"
    echo "$HZ_RT/$1"
}
