#!/usr/bin/env python3
"""Checks the compositor as a DRM/KMS session in the verification VM.

Run it through the VM script, which starts and stops the VM, inside the Nix
development shell (the guest has the libraries of that shell):

    tools/vm/vm test tools/vm-check.py [--out DIR] [--only lavapipe,script] [--skip-build]

No GPU is needed: the guest renders on the CPU (lavapipe). One part needs the
VM started in another way and is not part of the default run:

    HIZUKURI_VM_CPU=host,-hypervisor tools/vm/vm test tools/vm-check.py --only real-hardware

With the VM already running:

    HIZUKURI_VM=tools/vm/vm tools/vm-check.py

It builds the binaries, sends them and the draw-only project
(`prototype/compositor/projects/draw-only`) to the guest, and in the part
`lavapipe` (the script draws on the CPU, frames are copied into dumb buffers)
starts the compositor on a guest VT, shows windows, injects input through
QMP, switches VTs, and ends the compositor in each of the supported ways.
What is on screen is read back through the VM's VNC socket and compared as
numbers.

The compositor draws with the script. Unless an item is about the script's
window management (the built-in project, which tiles the windows), it runs
the draw-only project: the built-in drawing, and the core's fallback places
the windows, so that windows stay where the items put them.

The part `script` runs the script layer (the script's drawing and window
management, on lavapipe) with real applications: tiling, faults in every
hook, editing and reloading a script, VT switching, the watchdog, frame
times, and the `hzk script` launcher.

The part `vkms` runs the compositor and its self test on the virtual KMS
driver over SSH alone, without a VT. `safety` covers the watchdog; `outputs`
two outputs and hotplug; `ime` Japanese input with a real input method
(fcitx5 with Anthy) into foot.

Every item prints PASS, FAIL or SKIP. Everything needed to see why an item
failed is kept under the output directory (default
`artifacts/vm-check/<time>/`):

    results.json, summary.txt        every item with its detail
    <path>/proto-compositor-<stage>.log  the compositor's log (the guest's tty-run.log)
    <path>/state-<stage>.json        the compositor's state, from the control socket
    <path>/screen-<stage>.png        the guest's screen
    <path>/client.log, foot.log      the clients' output
    <path>/script-*.log              the clients of the script layer item
    <path>/selftest*/                the reports of `proto-compositor kms-selftest`
    script/screen-*.png              the script layer: real applications tiled, four busy
                                     windows at 1920x1080
    script/frame-time-apps-1080p.json  frame times, with the fallback's placement and tiled
    ime/screen-candidates.png        the candidate window of fcitx5 by foot's cursor
    ime/fcitx5.log, fcitx5-requests.txt  fcitx5's log with WAYLAND_DEBUG, and the
                                     requests it made in the order of first use
    script/hzk-script.log            what `hzk script` printed
    kms-info.json                    the guest's DRM device
    kms-summary.txt                  `proto-compositor kms-info --summary` in the guest

The exit status is 0 when nothing failed.
"""

import argparse
import json
import os
import shlex
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
VM = os.environ.get("HIZUKURI_VM", os.path.join(ROOT, "tools", "vm", "vm"))

GUEST_BIN = "/home/hizukuri/bin"
# Where the guest's Mesa puts the manifest of its CPU driver.
LAVAPIPE_ICD = "/run/opengl-driver/share/vulkan/icd.d/lvp_icd.x86_64.json"
GUEST_DIR = "/home/hizukuri/hzk-check"
SOCKET = "hzk-vm"
# Unlike anything a client draws here, so "the background" is unambiguous.
BACKGROUND = (64, 64, 128)
BACKGROUND_ARG = "404080"
CLIENT_COLOR = (200, 100, 50)
CLIENT_COLOR_ARG = "C86432FF"
CLIENT_SIZE = (300, 200)
CLIENT_POSITION = (100, 100)
# 8 bit values go through the 16 bit float compositing buffer and back.
TOLERANCE = 2
KEY_A = 30
BTN_LEFT = 272

# Where the draw-only project goes in the guest.
DRAW_ONLY = "/home/hizukuri/hzk-draw-only"

PATHS = {
    "lavapipe": {
        "args": ["--vulkan-device", "cpu", "--kms-scanout", "cpu"],
        "selftest": ["--vulkan-device", "cpu", "--scanout", "cpu"],
        "scanout": "cpu-copy",
        "gpu": lambda r: r.get("kind") == "cpu"
        or "llvmpipe" in r.get("device", "").lower(),
    },
}

# What runs on any machine, in this order, when nothing is asked for.
DEFAULT_PARTS = ["lavapipe", "script", "vkms", "safety", "outputs", "ime"]


def drawing(args, builtin=False):
    """The compositor's arguments with the project it runs: the draw-only
    project, unless the arguments name one or `builtin` asks for the
    built-in project (which tiles the windows)."""
    args = list(args)
    if not builtin and "--script" not in args:
        args += ["--script", DRAW_ONLY]
    return args


class Failed(Exception):
    """An item failed; the message says how."""


class Skipped(Exception):
    """An item could not be checked here; the message says why."""


def vm(*args, check=True, timeout=120):
    """Runs a `vm` command; returns its stdout."""
    try:
        result = subprocess.run([VM, *args], capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        raise Failed(f"vm {' '.join(args)}: no answer within {timeout} s")
    if check and result.returncode != 0:
        raise Failed(
            f"vm {' '.join(args)} exited with {result.returncode}: "
            f"{(result.stderr or result.stdout).strip()[-600:]}"
        )
    return result.stdout


def guest(command, check=True, timeout=120):
    """Runs a shell command in the guest over SSH; returns its stdout."""
    return vm("ssh", command, check=check, timeout=timeout)


# What this check started in the guest and has not stopped yet: one process
# id per line. Guest processes are only ever signalled by an id recorded
# here or by the main process id of the unit `tty-run` started, never by
# name.
GUEST_PIDS = f"{GUEST_DIR}/pids"


def spawn(command, log):
    """Starts a command in the guest and leaves it running. Its output goes
    to `log` in the guest's working directory for this check. Returns its
    process id, which is also the id of its own session and process group
    (`setsid`)."""
    # `a && b &` would put the whole list in the background, in a subshell
    # that keeps the SSH session's output open until `b` ends. Only the
    # command itself may go to the background, with all three streams
    # redirected.
    out = guest(
        f"mkdir -p {GUEST_DIR} && cd {GUEST_DIR} || exit 1; "
        f"nohup setsid {command} > {shlex.quote(log)} 2>&1 < /dev/null & "
        f"echo $! | tee -a {GUEST_PIDS}",
        timeout=30,
    )
    try:
        return int(out.strip().splitlines()[-1])
    except (ValueError, IndexError):
        raise Failed(f"could not start `{command}` in the guest: {out!r}")


# Signals a spawned process and what it started (its process group), after
# checking that the id still is the session leader `spawn` made it: an id
# that was reused by something else is not.
_SIGNAL = (
    'if [ "$(ps -o sid= -p {pid} 2>/dev/null | tr -d " ")" = "{pid}" ]; then '
    "kill -{signal} -- -{pid} 2>/dev/null || kill -{signal} {pid} 2>/dev/null; fi; "
)


def stop_spawned(pid=None):
    """Stops one process `spawn` started, or with no argument all of them.
    Never fails."""
    if pid is not None:
        guest(_SIGNAL.format(pid=pid, signal="TERM") + "true", check=False)
        return
    guest(
        f"for pid in $(cat {GUEST_PIDS} 2>/dev/null); do "
        + _SIGNAL.format(pid="$pid", signal="TERM")
        + f"done; rm -f {GUEST_PIDS}; true",
        check=False,
    )


def alive(pid):
    """Whether a process `spawn` started is still running."""
    return bool(guest(f"kill -0 {pid} 2>/dev/null && echo yes || true", check=False).strip())


def main_pid():
    """The process id of what `tty-run` started on the VT, or None."""
    for line in vm("tty-status", check=False).splitlines():
        if line.startswith("MainPID="):
            pid = int(line.split("=")[1] or 0)
            return pid or None
    return None


def pixels(points):
    out = vm("pixels", *[f"{int(x)},{int(y)}" for x, y in points])
    values = []
    for line in out.splitlines():
        if ": " in line:
            values.append(tuple(int(v) for v in line.split(": ")[1].split()))
    if len(values) != len(points):
        raise Failed(f"`vm pixels` returned {len(values)} values for {len(points)} points: {out!r}")
    return values


def near(actual, expected, tolerance=TOLERANCE):
    return all(abs(a - e) <= tolerance for a, e in zip(actual, expected))


def wait_for(what, condition, seconds=10.0, interval=0.25):
    """Polls `condition` until it returns something true; fails otherwise."""
    deadline = time.monotonic() + seconds
    last_error = None
    while time.monotonic() < deadline:
        try:
            value = condition()
            if value:
                return value
        except Failed as error:
            last_error = error
        time.sleep(interval)
    raise Failed(f"timed out after {seconds:.0f} s waiting for {what}" +
                 (f" (last error: {last_error})" if last_error else ""))


class Run:
    """One scanout path: its items, its files."""

    def __init__(self, name, out, uid, results):
        self.name = name
        self.spec = PATHS[name]
        self.out = os.path.join(out, name)
        os.makedirs(self.out, exist_ok=True)
        self.runtime = f"/run/user/{uid}"
        self.control = f"{self.runtime}/{SOCKET}.control"
        self.results = results
        self.screen = None

    # -- bookkeeping --------------------------------------------------------

    def item(self, name, function):
        """Runs one item and records PASS, FAIL or SKIP. Returns whether it
        passed."""
        label = f"{self.name}/{name}"
        try:
            detail = function() or ""
            status = "PASS"
        except Skipped as reason:
            status, detail = "SKIP", str(reason)
        except Failed as error:
            status, detail = "FAIL", str(error)
            self.evidence(f"fail-{name}")
        except Exception as error:  # a bug in this script is a failure too
            status, detail = "FAIL", f"{type(error).__name__}: {error}"
            self.evidence(f"fail-{name}")
        print(f"{status} {label}: {detail}", flush=True)
        self.results.append({"item": label, "status": status, "detail": detail})
        return status == "PASS"

    def evidence(self, stage):
        """Saves the log, the state and the screen. Never fails."""
        for kind, action in (
            ("proto-compositor", lambda p: open(p + ".log", "w").write(vm("tty-log", check=False))),
            ("state", lambda p: open(p + ".json", "w").write(self.ctl_raw("state"))),
            ("screen", lambda p: vm("screendump", p + ".png", check=False)),
            ("unit", lambda p: open(p + ".txt", "w").write(vm("tty-status", check=False))),
        ):
            try:
                action(os.path.join(self.out, f"{kind}-{stage}"))
            except Exception:
                pass

    def fetch(self, guest_path, name):
        try:
            text = guest(f"cat {shlex.quote(guest_path)}", check=False)
            with open(os.path.join(self.out, name), "w") as file:
                file.write(text)
            return text
        except Exception:
            return ""

    # -- the compositor -------------------------------------------------------

    def ctl_raw(self, *args):
        command = " ".join(
            shlex.quote(a)
            for a in [f"{GUEST_BIN}/proto-ctl", "--socket", self.control, *args]
        )
        return guest(f"timeout 10 {command}", timeout=30)

    def ctl(self, *args):
        return json.loads(self.ctl_raw(*args))

    def state(self):
        return self.ctl("state")

    def unit_active(self):
        return "ActiveState=active" in vm("tty-status", check=False)

    def active_vt(self):
        for line in vm("tty-status", check=False).splitlines():
            if line.startswith("active VT:"):
                return int(line.split(":")[1])
        return None

    def start(self, *extra, builtin=False):
        vm(
            "tty-run", "--", f"{GUEST_BIN}/proto-compositor", "--backend", "kms",
            "--socket", SOCKET, "--background", BACKGROUND_ARG, "--cursor", "on",
            *drawing([*self.spec["args"], *extra], builtin),
        )
        try:
            wait_for("the control socket to answer", lambda: self.unit_active()
                     and "ok" in self.ctl_raw("ping"), seconds=20)
        except Failed as error:
            log = vm("tty-log", check=False).strip().splitlines()
            raise Failed(f"{error}; the log ends with: {' | '.join(log[-6:])}")

    def stop(self):
        """Ends whatever is on the VT and the clients. Never fails."""
        vm("tty-stop", check=False)
        stop_spawned()

    def window(self, state, **wanted):
        for window in state["windows"]:
            if all(window.get(key) == value for key, value in wanted.items()):
                return window
        return None

    def client_events(self):
        text = self.fetch(f"{GUEST_DIR}/client.log", "client.log")
        events = []
        for line in text.splitlines():
            try:
                events.append(json.loads(line))
            except ValueError:
                pass
        return events

    def move_pointer(self, x, y):
        # The kernel drops an absolute position that repeats the previous
        # one, so go somewhere else first.
        vm("move", str(x + 37), str(y + 23))
        time.sleep(0.15)
        vm("move", str(x), str(y))

    # -- items ----------------------------------------------------------------

    def check_start(self):
        self.start("--exit-after", "300")
        state = self.state()
        if state["backend"] != "kms":
            raise Failed(f"the backend is {state['backend']!r}")
        if not state["session_active"]:
            raise Failed("the session is not active")
        return f"DRM/KMS session on tty{self.active_vt()}, control socket {self.control}"

    def check_gpu(self):
        gpu = self.state()["gpu"]
        if not gpu:
            raise Failed("the compositor runs without drawing")
        description = f"{gpu['device']} ({gpu['driver']})"
        if not self.spec["gpu"](gpu):
            raise Failed(f"the script draws on {description}, not the device this part is about")
        errors = gpu.get("validation_errors")
        return description + (f", validation errors: {errors}" if errors is not None else "")

    def check_scanout_path(self):
        output = self.state()["backend_status"]["output"]
        if output["scanout"] != self.spec["scanout"]:
            raise Failed(f"scanout path is {output['scanout']!r}, expected {self.spec['scanout']!r}")
        return f"{output['scanout']} on {output['connector']}"

    def check_mode(self):
        output = self.state()["backend_status"]["output"]
        self.screen = tuple(int(v) for v in vm("screen-size").strip().split("x"))
        if tuple(output["mode"]) != self.screen:
            raise Failed(f"the backend set {output['mode']}, the VM shows {self.screen}")
        return f"{self.screen[0]}x{self.screen[1]} at {output['refresh_mhz'] / 1000:.3f} Hz"

    def check_background(self):
        width, height = self.screen
        points = [(0, 0), (width - 1, 0), (0, height - 1), (width - 1, height - 1),
                  (width // 2, height // 2)]

        def shown():
            values = pixels(points)
            return values if all(near(v, BACKGROUND) for v in values) else None

        try:
            values = wait_for("the background on screen", shown, seconds=5)
        except Failed:
            raise Failed(f"corners and center are {pixels(points)}, expected {BACKGROUND}")
        return f"corners and center are {values[0]}"

    def check_client_window(self):
        spawn(
            f"{GUEST_BIN}/proto-test-client --socket {self.runtime}/{SOCKET} "
            f"--title hzk-client --app-id hzk.client "
            f"--size {CLIENT_SIZE[0]}x{CLIENT_SIZE[1]} --color {CLIENT_COLOR_ARG} --duration 240",
            "client.log",
        )
        window = wait_for("the client's window",
                          lambda: self.window(self.state(), title="hzk-client", mapped=True))
        self.client_window = window["id"]
        (x, y), (w, h) = CLIENT_POSITION, CLIENT_SIZE
        self.ctl("move-window", str(window["id"]), str(x), str(y))
        inside = [(x, y), (x + w - 1, y + h - 1), (x + w // 2, y + h // 2)]
        outside = [(x - 1, y), (x + w, y + h - 1), (x + w // 2, y - 1), (x + w // 2, y + h)]

        def on_screen():
            values = pixels(inside + outside)
            good = all(near(v, CLIENT_COLOR) for v in values[:3]) and all(
                near(v, BACKGROUND) for v in values[3:])
            return values if good else None

        try:
            values = wait_for("the window's pixels", on_screen, seconds=5)
        except Failed:
            raise Failed(f"inside {pixels(inside)} (expected {CLIENT_COLOR}), "
                         f"outside {pixels(outside)} (expected {BACKGROUND})")
        return f"{w}x{h} at {x},{y}: inside {values[2]}, outside {values[3]}"

    def check_pointer(self):
        # Over the background first: the cursor is drawn at the pointer.
        px, py = 700, 500
        self.move_pointer(px, py)

        def arrived():
            position = self.state()["pointer"]["position"]
            return position if position and abs(position[0] - px) <= 2 and abs(
                position[1] - py) <= 2 else None

        try:
            position = wait_for("the pointer position", arrived, seconds=5)
        except Failed:
            raise Failed(f"the compositor has the pointer at {self.state()['pointer']['position']}, "
                         f"QMP moved it to ({px}, {py})")
        # The arrow is 12x19 with its tip at the pointer: black outline,
        # white inside. Where exactly its pixels fall depends on rounding,
        # so count them in a box around it.
        x, y = round(position[0]), round(position[1])
        box = [(bx, by) for by in range(y - 2, y + 22) for bx in range(x - 2, x + 15)]
        values = pixels(box)
        black = sum(1 for v in values if max(v) <= 16)
        white = sum(1 for v in values if min(v) >= 239)
        background = sum(1 for v in values if near(v, BACKGROUND))
        if black < 20 or white < 20 or background < 100:
            raise Failed(f"no cursor at the pointer: in a 17x24 box around ({x}, {y}) there are "
                         f"{black} black, {white} white and {background} background pixels")

        # Then onto the client: it gets enter and motion.
        cx, cy = CLIENT_POSITION[0] + 150, CLIENT_POSITION[1] + 100
        self.move_pointer(cx, cy)

        def entered():
            events = self.client_events()
            return any(e["event"] == "pointer_enter" for e in events) and [
                e for e in events if e["event"] == "pointer_motion"]

        motions = wait_for("pointer events at the client", entered, seconds=5)
        last = motions[-1]
        if abs(last["x"] - 150) > 2 or abs(last["y"] - 100) > 2:
            raise Failed(f"the client saw the pointer at ({last['x']}, {last['y']}), "
                         f"expected about (150, 100)")
        return (f"libinput to compositor ({position[0]:.0f}, {position[1]:.0f}), cursor drawn "
                f"({black} black and {white} white pixels), "
                f"client saw ({last['x']:.0f}, {last['y']:.0f})")

    def check_click(self):
        vm("click")

        def clicked():
            buttons = [e for e in self.client_events() if e["event"] == "pointer_button"]
            return buttons if len(buttons) >= 2 else None

        buttons = wait_for("the button events at the client", clicked, seconds=5)
        if buttons[0].get("button") != BTN_LEFT:
            raise Failed(f"the client saw button {buttons[0].get('button')}, expected {BTN_LEFT}")
        focus = self.state()["focus"]["window"]
        if focus != self.client_window:
            raise Failed(f"the focus is on window {focus}, not on the client's {self.client_window}")
        return "press and release reached the client, its window has the focus"

    def check_keyboard(self):
        vm("key", "a")

        def typed():
            keys = [e for e in self.client_events()
                    if e["event"] == "key" and e["keycode"] == KEY_A]
            return keys if len(keys) >= 2 else None

        keys = wait_for("the key events at the client", typed, seconds=5)
        if not (keys[0]["pressed"] and not keys[1]["pressed"]):
            raise Failed(f"unexpected key events: {keys}")
        return f"press and release of KEY_A reached the client (text {keys[0].get('text')!r})"

    def real_app(self, name, command, app_id):
        if not guest(f"command -v {name} || true").strip():
            raise Skipped(f"{name} is not installed in the guest")
        before = {w["id"] for w in self.state()["windows"]}
        spawn(
            f"env -u DISPLAY XDG_RUNTIME_DIR={self.runtime} WAYLAND_DISPLAY={SOCKET} {command}",
            f"{name}.log",
        )

        def appeared():
            for window in self.state()["windows"]:
                if window["id"] not in before and window["mapped"] and window["surfaces"] and \
                        window["surfaces"][0]["has_contents"]:
                    return window
            return None

        try:
            window = wait_for(f"a window of {name}", appeared, seconds=15)
        except Failed as error:
            log = self.fetch(f"{GUEST_DIR}/{name}.log", f"{name}.log").strip().splitlines()
            raise Failed(f"{error}; {name} said: {' | '.join(log[-5:])}")
        self.fetch(f"{GUEST_DIR}/{name}.log", f"{name}.log")
        x, y = 480, 300
        self.ctl("move-window", str(window["id"]), str(x), str(y))
        width, height = window["surfaces"][0]["buffer_size"]
        # The middle of a terminal that printed one short line is empty.
        point = (min(x + width // 2, self.screen[0] - 1), min(y + height // 2, self.screen[1] - 1))
        value = wait_for(f"pixels of {name}",
                         lambda: (lambda v: v if not near(v, BACKGROUND, 4) else None)(
                             pixels([point])[0]), seconds=5)
        vm("screendump", os.path.join(self.out, f"screen-{name}.png"), check=False)
        if app_id and window["app_id"] != app_id:
            raise Failed(f"the window's app_id is {window['app_id']!r}, expected {app_id!r}")
        self.ctl("close-window", str(window["id"]))
        return (f"window {window['id']} (app_id {window['app_id']!r}, {width}x{height}) is drawn, "
                f"pixel {point} is {value}")

    def check_vt_switch(self):
        home = self.active_vt()
        frames_before = self.state()["backend_status"]["output"]["frames_presented"]
        # The compositor handles the key itself and asks the seat to switch.
        vm("key", "ctrl-alt-f2")
        wait_for("the session to pause",
                 lambda: not self.state()["session_active"], seconds=8)
        if self.active_vt() != 2:
            raise Failed(f"the active VT is {self.active_vt()}, expected 2")
        time.sleep(1.0)
        if not self.unit_active():
            raise Failed("the compositor exited when the VT was switched away")
        windows = len(self.state()["windows"])
        self.evidence("vt-away")

        guest(f"sudo chvt {home}")
        wait_for("the session to resume", lambda: self.state()["session_active"], seconds=8)
        (x, y), (w, h) = CLIENT_POSITION, CLIENT_SIZE
        # Not the middle of the window: the pointer was left there, and the
        # cursor is drawn over it.
        points = [(x + 20, y + 20), (x + w - 20, y + h - 20)]
        try:
            wait_for("the client's window back on screen",
                     lambda: all(near(v, CLIENT_COLOR) for v in pixels(points)), seconds=8)
        except Failed:
            raise Failed(f"after coming back, {points} are {pixels(points)}, "
                         f"expected {CLIENT_COLOR}")
        state = self.state()
        frames_after = state["backend_status"]["output"]["frames_presented"]
        if frames_after <= frames_before:
            raise Failed(f"no frame was presented after coming back ({frames_before} before, "
                         f"{frames_after} after)")
        if len(state["windows"]) != windows:
            raise Failed("windows were lost across the VT switch")
        # Input still arrives.
        count = len([e for e in self.client_events() if e["event"] == "key"])
        vm("click")
        vm("key", "a")

        def typed():
            keys = [e for e in self.client_events() if e["event"] == "key"]
            return keys[count:] if len(keys) > count else None

        keys = wait_for("a key after the VT switch", typed, seconds=5)
        # Ctrl and Alt were held when the VT was left and released while
        # away. If they were still thought to be held, this would not be "a".
        text = keys[0].get("text")
        if text != "a":
            raise Failed(f"the key after the switch arrived as {text!r}, not 'a': "
                         f"modifiers are stuck")
        return (f"tty{home} to tty2 and back: paused, resumed, {windows} windows kept, "
                f"pixels and input are back")

    def check_frame_time(self):
        state = self.state()
        times = state["frame_time_ms"]
        flips = state["backend_status"]["output"]["flip_interval_ms"]
        with open(os.path.join(self.out, "frame-time.json"), "w") as file:
            json.dump({"frame_time_ms": times, "flip_interval_ms": flips,
                       "mode": state["backend_status"]["output"]["mode"]}, file, indent=2)
        if not times.get("total"):
            raise Failed("no frame was timed")
        return (f"draw {times['draw']['median']:.2f} ms + present "
                f"{times['present']['median']:.2f} ms (median of {times['window']} frames, "
                f"p99 total {times['total']['p99']:.2f} ms), flips every "
                f"{flips['median']:.2f} ms")

    def check_quit_key(self):
        self.evidence("before-quit")
        vm("key", "ctrl-alt-backspace")
        wait_for("the compositor to exit", lambda: not self.unit_active(), seconds=8)
        log = vm("tty-log", check=False)
        with open(os.path.join(self.out, "proto-compositor-quit-key.log"), "w") as file:
            file.write(log)
        if "quit requested" not in log:
            raise Failed("the compositor exited, but its log does not mention the quit request")
        return "Ctrl+Alt+Backspace ended the session"

    def check_exit_after(self):
        started = time.monotonic()
        vm("tty-run", "--wait", "--", f"{GUEST_BIN}/proto-compositor", "--backend", "kms",
           "--socket", SOCKET, *drawing([*self.spec["args"], "--exit-after", "3"]), timeout=60)
        elapsed = time.monotonic() - started
        log = vm("tty-log", check=False)
        with open(os.path.join(self.out, "proto-compositor-exit-after.log"), "w") as file:
            file.write(log)
        if "--exit-after elapsed" not in log:
            raise Failed("the log does not show the timer ending the run")
        if elapsed > 30:
            raise Failed(f"the run took {elapsed:.1f} s")
        return f"exit status 0 after {elapsed:.1f} s (with startup and VT switches)"

    def check_control_quit(self):
        self.start("--exit-after", "60")
        self.ctl("quit")
        wait_for("the compositor to exit", lambda: not self.unit_active(), seconds=8)
        return "`proto-ctl quit` over SSH ended the session"

    def check_sighup(self):
        """A hangup (the shell or terminal the compositor was started from
        went away) must end the run as cleanly as SIGTERM does."""
        self.start("--exit-after", "60")
        pid = main_pid()
        if pid is None:
            raise Failed("the unit has no main process")
        guest(f"kill -HUP {pid}")
        wait_for("the compositor to exit", lambda: not self.unit_active(), seconds=8)
        log = vm("tty-log", check=False)
        with open(os.path.join(self.out, "proto-compositor-sighup.log"), "w") as file:
            file.write(log)
        if "stopping signal=SIGHUP" not in log:
            raise Failed("the log does not show a clean stop on SIGHUP")
        left = guest(f"ls {self.runtime} | grep -x -e {SOCKET} -e {SOCKET}.control || true").split()
        if left:
            raise Failed(f"sockets were left behind: {left}")
        return "SIGHUP ended the session cleanly, no socket left"

    def check_apps_1080p(self):
        """Several real applications that redraw all the time, at 1920x1080:
        what a frame costs when there is something to composite."""
        self.start("--kms-mode", "1920x1080", "--exit-after", "120")
        output = self.state()["backend_status"]["output"]
        screen = tuple(int(v) for v in vm("screen-size").strip().split("x"))
        if tuple(output["mode"]) != (1920, 1080) or screen != (1920, 1080):
            raise Failed(f"the backend set {output['mode']}, the VM shows {screen}")
        corners = [(0, 0), (1919, 0), (0, 1079), (1919, 1079)]
        wait_for("the background at 1920x1080",
                 lambda: all(near(v, BACKGROUND) for v in pixels(corners)), seconds=5)

        busy = "sh -c 'while :; do seq 1 3000; done'"
        commands = [("foot", f"foot {busy}"), ("foot", f"foot {busy}"),
                    ("weston-terminal", "weston-terminal")]
        # vkcube on lavapipe presents through wl_shm, which is all the
        # compositor takes so far.
        commands.append(("vkcube", f"env VK_DRIVER_FILES={LAVAPIPE_ICD} vkcube --wsi wayland"))
        started = 0
        for index, (name, command) in enumerate(commands):
            if not guest(f"command -v {name} || true").strip():
                continue
            spawn(f"env -u DISPLAY XDG_RUNTIME_DIR={self.runtime} WAYLAND_DISPLAY={SOCKET} "
                  f"{command}", f"apps-{index}-{name}.log")
            started += 1
        if started < 2:
            raise Skipped("fewer than two of foot, weston-terminal and vkcube are installed")

        def all_mapped():
            windows = [w for w in self.state()["windows"]
                       if w["mapped"] and w["surfaces"] and w["surfaces"][0]["has_contents"]]
            return windows if len(windows) >= started else None

        windows = wait_for(f"{started} application windows", all_mapped, seconds=20)
        for index, window in enumerate(windows):
            self.ctl("move-window", str(window["id"]), str(40 + index * 460), str(40 + index * 120))
        time.sleep(8.0)
        state = self.state()
        vm("screendump", os.path.join(self.out, "screen-apps-1080p.png"), check=False)
        times = state["frame_time_ms"]
        output = state["backend_status"]["output"]
        with open(os.path.join(self.out, "frame-time-apps-1080p.json"), "w") as file:
            json.dump({"frame_time_ms": times, "flip_interval_ms": output["flip_interval_ms"],
                       "mode": output["mode"], "windows": len(windows)}, file, indent=2)
        # Something of every window is on screen: not the background.
        first = windows[0]["surfaces"][0]["buffer_size"]
        probe = (40 + first[0] // 2, 40 + first[1] // 2)
        if near(pixels([probe])[0], BACKGROUND, 4):
            raise Failed(f"pixel {probe} is the background: the first window is not on screen")
        if not times.get("total"):
            raise Failed("no frame was timed")
        self.ctl("quit")
        wait_for("the compositor to exit", lambda: not self.unit_active(), seconds=8)
        return (f"{len(windows)} busy windows at 1920x1080: draw {times['draw']['median']:.2f} ms "
                f"+ present {times['present']['median']:.2f} ms (median of {times['window']} "
                f"frames, p99 total {times['total']['p99']:.2f} ms), flips every "
                f"{output['flip_interval_ms']['median']:.2f} ms")

    def check_launcher(self):
        """The command that is typed after logging in on the console."""
        launcher = "hzk"
        control = f"{self.runtime}/hizukuri-0.control"
        ctl = f"timeout 10 {GUEST_BIN}/proto-ctl --socket {control}"
        vm("tty-run", "--", launcher, self.name, "--seconds", "60")

        def ready():
            state = json.loads(guest(f"{ctl} state", timeout=30))
            mapped = [w for w in state["windows"] if w["mapped"]]
            return state if len(mapped) >= 2 else None

        try:
            state = wait_for("the launcher's windows", ready, seconds=30)
        except Failed as error:
            log = guest("tail -n 5 ~/hzk.log", check=False).strip().splitlines()
            raise Failed(f"{error}; hzk.log ends with: {' | '.join(log)}")
        scanout = state["backend_status"]["output"]["scanout"]
        if scanout != self.spec["scanout"]:
            raise Failed(f"`hzk {self.name}` runs with scanout {scanout!r}")
        vm("screendump", os.path.join(self.out, "screen-launcher.png"), check=False)
        vm("key", "ctrl-alt-backspace")
        wait_for("the launcher to end", lambda: not self.unit_active(), seconds=10)
        log = vm("tty-log", check=False)
        with open(os.path.join(self.out, "launcher.log"), "w") as file:
            file.write(log)
        if "proto-compositor ended with status 0" not in log:
            raise Failed(f"the launcher did not report a clean end: {log.strip()[-200:]}")
        # The launcher's clients are known by what they were given, the
        # launcher's socket, not by their names.
        left = guest("grep -lzx 'WAYLAND_DISPLAY=hizukuri-0' /proc/[0-9]*/environ 2>/dev/null; "
                     "true").split()
        if left:
            raise Failed(f"the launcher left clients running: {left}")
        return (f"`hzk {self.name}` showed {len(state['windows'])} windows ({scanout}), "
                f"Ctrl+Alt+Backspace ended it, no client left")

    def selftest(self, label, *extra):
        report_dir = f"{GUEST_DIR}/{self.name}-{label}"
        guest(f"rm -rf {report_dir}")
        failure = None
        try:
            vm("tty-run", "--wait", "--", f"{GUEST_BIN}/proto-compositor", "kms-selftest",
               "--output", "Virtual-1", "--seconds", "6", "--report", report_dir,
               *self.spec["selftest"], *extra, timeout=90)
        except Failed as error:
            failure = error
        with open(os.path.join(self.out, f"proto-compositor-{label}.log"), "w") as file:
            file.write(vm("tty-log", check=False))
        local = os.path.join(self.out, label)
        vm("pull", report_dir + "/", local + "/", check=False)
        try:
            with open(os.path.join(local, "report.json")) as file:
                report = json.load(file)
        except (OSError, ValueError):
            raise Failed(f"no report was written ({failure or 'the command succeeded'})")
        failed = [c for c in report["checks"] if c["status"] == "fail"]
        if failed or failure:
            raise Failed("; ".join(f"{c['name']}: {c['detail']}" for c in failed) or str(failure))
        output = report["backend"]["output"]
        if output["scanout"] != self.spec["scanout"]:
            raise Failed(f"scanout path was {output['scanout']!r}")
        checks = {c["name"]: c for c in report["checks"]}
        times = report["frame_time_ms"]
        return (f"{output['mode'][0]}x{output['mode'][1]} {output['scanout']}: "
                f"{report['frames_presented']} frames, flips {checks['page_flip_timing']['detail']} "
                f"[{checks['page_flip_timing']['status']}], draw {times['draw']['median']:.2f} ms + "
                f"present {times['present']['median']:.2f} ms "
                f"[frame_time {checks['frame_time']['status']}]")

    def check_protocols(self):
        """The protocol layer on a real KMS session: a client that sends
        dmabufs, a buffer transform, a popup with a grab driven through
        libinput, and the selection."""
        self.start("--dmabuf", "cpu", "--exit-after", "120")
        screen = tuple(int(v) for v in vm("screen-size").strip().split("x"))
        notes = []

        # 1. A Vulkan client on the guest's default driver. Without a GPU
        # that is lavapipe, which presents through wl_shm: the window must be
        # shown.
        expected_kind = "shm"
        if guest("command -v vkcube || true").strip():
            cube_pid = spawn(
                f"env -u DISPLAY XDG_RUNTIME_DIR={self.runtime} WAYLAND_DISPLAY={SOCKET} "
                f"vkcube --wsi wayland", "protocols-vkcube.log")

            def cube():
                for window in self.state()["windows"]:
                    surfaces = window["surfaces"]
                    if window["mapped"] and surfaces and surfaces[0].get("buffer_kind"):
                        return window
                return None

            try:
                window = wait_for("vkcube's window", cube, seconds=15)
                surface = window["surfaces"][0]
                readable = "shown" if surface["has_contents"] else \
                    "not shown (the buffer cannot be mapped)"
                notes.append(f"vkcube sends {surface['buffer_kind']} buffers, {readable}")
                if surface["buffer_kind"] != expected_kind:
                    raise Failed(f"vkcube's buffers are {surface['buffer_kind']}, "
                                 f"not {expected_kind}")
                if expected_kind == "shm" and not surface["has_contents"]:
                    raise Failed("vkcube's wl_shm buffers are not shown")
                self.ctl("close-window", str(window["id"]))
            finally:
                stop_spawned(cube_pid)
        else:
            notes.append("vkcube is not installed")

        # 2. weston-terminal's context menu: an xdg_popup with a grab,
        # opened and dismissed with real pointer input.
        if guest("command -v weston-terminal || true").strip():
            before = {w["id"] for w in self.state()["windows"]}
            spawn(f"env -u DISPLAY XDG_RUNTIME_DIR={self.runtime} WAYLAND_DISPLAY={SOCKET} "
                  f"weston-terminal", "protocols-weston-terminal.log")

            def terminal():
                for window in self.state()["windows"]:
                    if window["id"] not in before and window["mapped"] and \
                            window["surfaces"] and window["surfaces"][0]["has_contents"]:
                        return window
                return None

            window = wait_for("weston-terminal's window", terminal, seconds=15)
            self.ctl("move-window", str(window["id"]), "100", "100")
            width, height = window["size"]
            x, y = 100 + width // 2, 100 + height // 2

            def popups():
                for candidate in self.state()["windows"]:
                    if candidate["id"] == window["id"]:
                        return [s for s in candidate["surfaces"] if s["role"] == "popup"]
                return []

            self.move_pointer(x, y)
            vm("click", "right")
            menu = wait_for("the menu", lambda: [p for p in popups() if p["has_contents"]],
                            seconds=8)
            focus = self.state()["focus"]
            if focus["keyboard_surface"] != menu[0]["id"]:
                raise Failed(f"the menu did not get the keyboard: {focus}")
            vm("screendump", os.path.join(self.out, "screen-menu.png"), check=False)
            # A press on the background dismisses it.
            self.move_pointer(screen[0] - 40, screen[1] - 40)
            vm("click")
            wait_for("the menu to go away", lambda: not popups(), seconds=8)
            notes.append(f"weston-terminal's menu ({menu[0]['size'][0]}x{menu[0]['size'][1]}) "
                         f"opened with the keyboard and was dismissed by a press outside")
            self.ctl("close-window", str(window["id"]))
        else:
            notes.append("weston-terminal is not installed")

        # 3. The selection, with wl-clipboard if the guest has it.
        if guest("command -v wl-copy && command -v wl-paste || true").count("\n") >= 2:
            env = f"env -u DISPLAY XDG_RUNTIME_DIR={self.runtime} WAYLAND_DISPLAY={SOCKET}"
            copy_pid = spawn(f"{env} wl-copy --foreground 'copied in the guest'",
                             "protocols-wl-copy.log")
            # Wait for wl-copy to own the selection by asking the
            # compositor, not by trying with wl-paste.
            wait_for("wl-copy to own the selection",
                     lambda: self.state().get("selection"), seconds=10)
            pasted = guest(f"{env} timeout 5 wl-paste --no-newline 2>/dev/null || true")
            stop_spawned(copy_pid)
            if pasted != "copied in the guest":
                raise Failed(f"wl-paste read {pasted!r}")
            notes.append("wl-copy to wl-paste works")
        else:
            notes.append("wl-clipboard is not installed")

        self.ctl("quit")
        wait_for("the compositor to exit", lambda: not self.unit_active(), seconds=8)
        return "; ".join(notes)

    def check_script_layer(self):
        """Drawing and window management by the script layer, on a real KMS
        session: the script's drawing on lavapipe with the CPU copy path, the
        tiling script, input to a window the script turned, and recovery
        from a script that fails. `prototype/compositor/tests/script.rs` checks
        the same things on a headless backend."""
        self.start("--exit-after", "240", builtin=True)
        screen = tuple(int(v) for v in vm("screen-size").strip().split("x"))
        notes = []
        state = self.state()
        script = state.get("script") or {}
        if script.get("mode") != "running" or state.get("window_manager") != "script":
            raise Failed(f"the script is not in charge: {script}, "
                         f"window manager {state.get('window_manager')!r}")
        if state["gpu"].get("kind") != "cpu":
            raise Failed(f"the script draws on {state['gpu']}, not on lavapipe")

        def script_call(function, *args):
            return self.ctl("script-call", function, *[json.dumps(a) for a in args])

        def healthy(what):
            script = self.state()["script"]
            if script["mode"] != "running" or script["failures"]:
                raise Failed(f"{what}: the script failed: mode {script['mode']}, "
                             f"{script['last_failures']}")

        # 1. Two windows, side by side with a gap of 8, as the tiling script
        # puts them. Their pixels are on the screen.
        colors = {"hzk-left": (200, 100, 50), "hzk-right": (60, 160, 220)}
        gap = 8
        width, height = (screen[0] - 3 * gap) // 2, screen[1] - 2 * gap
        expected = {"hzk-left": [gap, gap], "hzk-right": [2 * gap + width, gap]}
        for title, color in colors.items():
            spawn(
                f"{GUEST_BIN}/proto-test-client --socket {self.runtime}/{SOCKET} "
                f"--title {title} --size 300x200 --color {'%02X%02X%02XFF' % color} "
                f"--duration 200",
                f"script-{title}.log",
            )
            wait_for(f"{title}'s window",
                     lambda: self.window(self.state(), title=title, mapped=True), seconds=15)

        def tiled():
            state = self.state()
            windows = {t: self.window(state, title=t, mapped=True) for t in colors}
            return windows if all(
                w and w["position"] == expected[t] and w["size"] == [width, height]
                for t, w in windows.items()) else None

        try:
            windows = wait_for("the tiles", tiled, seconds=10)
        except Failed:
            found = [(w["title"], w["position"], w["size"]) for w in self.state()["windows"]]
            raise Failed(f"the windows are at {found}, expected {expected} with size "
                         f"{[width, height]}")
        centers = {t: (expected[t][0] + width // 2, gap + height // 2) for t in colors}
        between = (gap + width + gap // 2, screen[1] // 2)
        values = pixels([centers["hzk-left"], centers["hzk-right"], between])
        if not (near(values[0], colors["hzk-left"]) and near(values[1], colors["hzk-right"])
                and near(values[2], BACKGROUND)):
            raise Failed(f"on screen: left {values[0]}, right {values[1]}, between {values[2]}")
        notes.append(f"two tiles of {width}x{height} drawn by the script")

        # 2. The keys of the tiling script, through the guest's keyboard:
        # logo+H moves the focus left, logo+shift+L swaps the tiles.
        right, left = windows["hzk-right"]["id"], windows["hzk-left"]["id"]
        if self.state()["focus"]["window"] != right:
            raise Failed(f"the newest window does not have the focus: {self.state()['focus']}")
        vm("key", "logo-h")
        wait_for("the focus on the left tile",
                 lambda: self.state()["focus"]["window"] == left, seconds=5)
        vm("key", "logo-shift-l")
        wait_for("the tiles to swap", lambda: (
            self.window(self.state(), title="hzk-left")["position"] == expected["hzk-right"]
            and self.window(self.state(), title="hzk-right")["position"] == expected["hzk-left"]),
            seconds=5)
        values = pixels([centers["hzk-left"], centers["hzk-right"]])
        if not (near(values[0], colors["hzk-right"]) and near(values[1], colors["hzk-left"])):
            raise Failed(f"after the swap the screen shows {values}")
        vm("key", "logo-shift-h")
        wait_for("the tiles to swap back", tiled, seconds=5)
        healthy("after the key bindings")
        notes.append("logo+H and logo+shift+L/H moved the focus and swapped the tiles")

        # 3. The script turns the left window by 20 degrees around its
        # center and shrinks it. A point of the window, given on the
        # screen, reaches the client as that point of its surface.
        import math
        angle, scale = math.radians(20), 0.7
        a, b = scale * math.cos(angle), -scale * math.sin(angle)
        c, d = scale * math.sin(angle), scale * math.cos(angle)
        cx, cy = width / 2, height / 2
        tx = gap + cx - (a * cx + b * cy)
        ty = gap + cy - (c * cx + d * cy)
        script_call("set_transform", left, [a, b, c, d, tx, ty])

        def on_screen(x, y):
            return (a * x + b * y + tx, c * x + d * y + ty)

        probe = (cx + 100, cy - 60)
        sx, sy = on_screen(*probe)
        wait_for("the turned window's pixels",
                 lambda: near(pixels([(sx, sy)])[0], colors["hzk-left"]), seconds=5)
        # The corner of the tile the window no longer covers.
        corner = pixels([(gap + 3, gap + 3)])[0]
        if not near(corner, BACKGROUND):
            raise Failed(f"the corner of the tile shows {corner} after the window was turned")
        self.move_pointer(round(sx), round(sy))

        def seen():
            text = self.fetch(f"{GUEST_DIR}/script-hzk-left.log", "script-hzk-left.log")
            motions = []
            for line in text.splitlines():
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                if event.get("event") in ("pointer_enter", "pointer_motion"):
                    motions.append(event)
            return motions

        motions = wait_for("pointer events at the turned window", seen, seconds=5)
        last = motions[-1]
        # The absolute pointer of the VM has whole pixels on the screen;
        # the window is shown at 0.7 of its size, so allow three.
        if abs(last["x"] - probe[0]) > 3 or abs(last["y"] - probe[1]) > 3:
            raise Failed(f"the turned window saw the pointer at ({last['x']:.1f}, "
                         f"{last['y']:.1f}), expected about {probe}")
        vm("click")
        wait_for("the focus on the turned window",
                 lambda: self.state()["focus"]["window"] == left, seconds=5)
        script_call("set_transform", left, None)
        healthy("after the transform")
        notes.append(f"a turned window got the pointer at ({last['x']:.0f}, {last['y']:.0f}) "
                     f"for {probe}")

        # 4. A script that fails to draw. The project is replaced at run
        # time by one whose frame can be made to throw.
        project = f"{GUEST_DIR}/faulty-script"
        guest(f"mkdir -p {project} && cat > {project}/main.luau <<'LUAU'\n"
              "hz.include(\"session\")\n"
              "local usual = frame\n"
              "local broken = false\n"
              "function set_broken(value) broken = value end\n"
              "function frame(output, target)\n"
              "    if broken then error(\"deliberate failure in the VM\") end\n"
              "    usual(output, target)\n"
              "end\n"
              "LUAU")
        self.ctl("script-reload", "--path", project)
        wait_for("the tiles after the reload", tiled, seconds=10)
        healthy("after the reload")
        script_call("set_broken", True)

        def fallback():
            script = self.state()["script"]
            return script if script["mode"] == "fallback" else None

        script = wait_for("the built-in fallback to take over", fallback, seconds=5)
        # The fallback marks its frames with a bar along the top edge, and
        # still shows both windows.
        marker = (screen[0] // 2, 1)
        values = pixels([marker, centers["hzk-left"], centers["hzk-right"]])
        if not (values[0][0] >= 250 and values[0][1] <= 5
                and near(values[1], colors["hzk-left"])
                and near(values[2], colors["hzk-right"])):
            raise Failed(f"in the fallback the screen shows marker {values[0]}, "
                         f"left {values[1]}, right {values[2]}")
        # Clients are connected and get input.
        vm("key", "a")

        def typed():
            text = self.fetch(f"{GUEST_DIR}/script-hzk-left.log", "script-hzk-left.log")
            return '"event":"key"' in text.replace(" ", "")

        wait_for("a key at the focused client while the script is out", typed, seconds=5)
        notes.append(f"a failing frame ({script['last_failures'][-1]['kind']}) left the windows "
                     f"on screen, drawn by the fallback, with input working")

        # 5. The emergency keys: Ctrl+Alt+Shift+R reads the script again.
        script_call("set_broken", False)
        vm("key", "ctrl-alt-shift-r")
        wait_for("the script to be back in charge",
                 lambda: self.state()["script"]["mode"] == "running", seconds=10)
        wait_for("the marker to go away",
                 lambda: near(pixels([marker])[0], BACKGROUND), seconds=5)
        vm("key", "ctrl-alt-shift-f")
        wait_for("the fallback by its key",
                 lambda: self.state()["script"]["mode"] == "fallback", seconds=5)
        self.ctl("script-mode", "running")
        wait_for("the tiles at the end", tiled, seconds=10)
        notes.append("Ctrl+Alt+Shift+R reloaded the script, Ctrl+Alt+Shift+F left it")
        times = self.state()["frame_time_ms"]
        notes.append(f"frame time: draw {times['draw']['median']:.2f} ms + "
                     f"present {times['present']['median']:.2f} ms")

        self.ctl("quit")
        wait_for("the compositor to exit", lambda: not self.unit_active(), seconds=8)
        return "; ".join(notes)

    # -- the sequence ---------------------------------------------------------

    def check_input_protocols(self):
        """What a game and a clipboard manager ask of the session:
        relative motion from libinput and a pointer lock, the primary
        selection and data control with wl-clipboard."""
        self.start("--exit-after", "120")
        notes = []

        # 1. A game: it asks for relative motion and a lock. The Rust layout
        # focuses the new window; the lock starts once the pointer is on it.
        before = {w["id"] for w in self.state()["windows"]}
        spawn(f"{GUEST_BIN}/proto-test-client --socket {self.runtime}/{SOCKET} "
              f"--title hzk-game --size 300x200 --duration 100 "
              f"--relative-pointer --lock-pointer", "input-game.log")
        window = wait_for("the game's window", lambda: next(
            (w for w in self.state()["windows"]
             if w["id"] not in before and w["mapped"] and w["title"] == "hzk-game"), None),
            seconds=10)
        self.ctl("move-window", str(window["id"]), "100", "100")
        game_events = lambda: [json.loads(line) for line in
                               self.fetch(f"{GUEST_DIR}/input-game.log", "input-game.log")
                               .splitlines() if line.startswith("{")]
        self.move_pointer(250, 200)
        wait_for("the lock", lambda: any(e["event"] == "pointer_locked" for e in game_events()),
                 seconds=8)
        held_at = self.state()["pointer"]["position"]
        # Relative motion from the virtual mouse, through the kernel and
        # libinput. While locked, the pointer stays and no wl_pointer.motion
        # is sent; the motion arrives as relative motion, before and after
        # libinput's acceleration.
        for _ in range(3):
            vm("rel", "40", "0")
            time.sleep(0.1)

        def moved():
            events = game_events()
            locked = next(i for i, e in enumerate(events) if e["event"] == "pointer_locked")
            relative = [e for e in events[locked:] if e["event"] == "relative_motion"]
            return (relative, events[locked:]) if len(relative) >= 3 else None

        relative, after = wait_for("relative motion at the game", moved, seconds=8)
        if any(e["event"] == "pointer_motion" for e in after):
            raise Failed("wl_pointer.motion arrived while the pointer was locked")
        if self.state()["pointer"]["position"] != held_at:
            raise Failed(f"the locked pointer moved from {held_at} to "
                         f"{self.state()['pointer']['position']}")
        unaccelerated = sum(e["dx_unaccel"] for e in relative)
        accelerated = sum(e["dx"] for e in relative)
        notes.append(f"libinput to a locked game: {len(relative)} relative motions, "
                     f"dx {accelerated:.1f} after acceleration, {unaccelerated:.1f} before, "
                     f"no wl_pointer.motion, the pointer held at {held_at}")
        self.ctl("close-window", str(window["id"]))
        stop_spawned()

        # 2. wl-clipboard: with data control it reads and replaces the
        # selections without a window of its own.
        if guest("command -v wl-copy && command -v wl-paste || true").count("\n") >= 2:
            env = f"env -u DISPLAY XDG_RUNTIME_DIR={self.runtime} WAYLAND_DISPLAY={SOCKET}"
            windows = len(self.state()["windows"])
            for primary, text in ((False, "clipboard via data control"),
                                  (True, "primary via data control")):
                flag = "--primary " if primary else ""
                key = "primary_selection" if primary else "selection"
                copy_pid = spawn(f"{env} wl-copy {flag}--foreground {shlex.quote(text)}",
                                 "input-wl-copy.log")
                wait_for(f"wl-copy to own the {key}", lambda: self.state().get(key), seconds=10)
                if len(self.state()["windows"]) != windows:
                    raise Failed("wl-copy opened a window: it did not use data control")
                pasted = guest(f"{env} timeout 5 wl-paste {flag}--no-newline 2>/dev/null || true")
                stop_spawned(copy_pid)
                if pasted != text:
                    raise Failed(f"wl-paste {flag}read {pasted!r}, expected {text!r}")
                # The owner is gone, and so is the selection.
                wait_for(f"the {key} to go with wl-copy",
                         lambda: self.state().get(key) is None, seconds=10)
            notes.append("wl-copy and wl-paste read and replace the clipboard and the primary "
                         "selection without a window")
        else:
            notes.append("wl-clipboard is not installed")

        self.ctl("quit")
        wait_for("the compositor to exit", lambda: not self.unit_active(), seconds=8)
        return "; ".join(notes)

    def run(self):
        self.stop()
        if self.item("start", self.check_start):
            self.item("gpu", self.check_gpu)
            self.item("scanout-path", self.check_scanout_path)
            if self.item("mode", self.check_mode):
                self.item("background", self.check_background)
                if self.item("client-window", self.check_client_window):
                    self.item("pointer", self.check_pointer)
                    self.item("click-focus", self.check_click)
                    self.item("keyboard", self.check_keyboard)
                    self.item("vt-switch", self.check_vt_switch)
                self.item("real-app-foot", lambda: self.real_app(
                    "foot", "foot sh -c 'echo hizukuri; sleep 60'", "foot"))
                self.item("real-app-weston-terminal", lambda: self.real_app(
                    "weston-terminal", "weston-terminal", None))
            self.item("frame-time", self.check_frame_time)
            self.evidence("end")
            self.item("quit-key", self.check_quit_key)
        self.stop()
        self.item("exit-after", self.check_exit_after)
        self.stop()
        self.item("control-quit", self.check_control_quit)
        self.stop()
        self.item("sighup", self.check_sighup)
        self.stop()
        self.item("apps-1080p", self.check_apps_1080p)
        self.stop()
        self.item("launcher", self.check_launcher)
        self.stop()
        self.item("protocols", self.check_protocols)
        self.stop()
        self.item("input-protocols", self.check_input_protocols)
        self.stop()
        self.item("script-layer", self.check_script_layer)
        self.stop()
        self.item("selftest", lambda: self.selftest("selftest"))
        self.item("selftest-1080p", lambda: self.selftest("selftest-1080p", "--mode", "1920x1080"))
        self.stop()


def check_vkms(out, uid, results):
    """The compositor on vkms, over SSH only: no VT and no seat daemon
    (libseat's `noop` backend opens the devices directly). What vkms shows
    cannot be captured from outside, so this checks that the session runs and
    presents frames, and leaves the pixels to the self test's own comparison."""
    directory = os.path.join(out, "vkms")
    os.makedirs(directory, exist_ok=True)
    runtime = f"/run/user/{uid}"
    socket = "hzk-vkms"
    ctl = f"timeout 10 {GUEST_BIN}/proto-ctl --socket {runtime}/{socket}.control"
    common = f"export XDG_RUNTIME_DIR={runtime} LIBSEAT_BACKEND=noop; "
    found = {}

    def item(name, function):
        label = f"vkms/{name}"
        try:
            status, detail = "PASS", function() or ""
        except Skipped as reason:
            status, detail = "SKIP", str(reason)
        except Failed as error:
            status, detail = "FAIL", str(error)
        except Exception as error:
            status, detail = "FAIL", f"{type(error).__name__}: {error}"
        print(f"{status} {label}: {detail}", flush=True)
        results.append({"item": label, "status": status, "detail": detail})
        return status == "PASS"

    def keep(guest_path, name):
        with open(os.path.join(directory, name), "w") as file:
            file.write(guest(f"cat {guest_path}", check=False))

    def load():
        found["card"] = guest("vkms-load").strip()
        info = json.loads(guest(f"{GUEST_BIN}/proto-compositor kms-info --device {found['card']}"))
        # With --device the output is one device's description; without it,
        # a list of {device, snapshot}.
        snapshot = info[0]["snapshot"] if isinstance(info, list) else info
        names = [c["name"] for c in snapshot["connectors"] if c.get("connected")]
        if not names:
            raise Failed(f"no connected connector on {found['card']}")
        found["connector"] = names[0]
        return f"{found['card']}, connector {found['connector']}"

    def session():
        pids = [spawn(
            f"env XDG_RUNTIME_DIR={runtime} LIBSEAT_BACKEND=noop {GUEST_BIN}/proto-compositor "
            f"--backend kms --socket {socket} --kms-device {found['card']} "
            f"--vulkan-device cpu --kms-scanout cpu --background {BACKGROUND_ARG} "
            f"--script {DRAW_ONLY} --exit-after 30", "vkms-session.log")]
        try:
            wait_for("the control socket", lambda: "ok" in guest(f"{ctl} ping", check=False),
                     seconds=15)
            pids.append(spawn(
                f"{GUEST_BIN}/proto-test-client --socket {runtime}/{socket} "
                f"--title hzk-vkms --size 300x200 --color {CLIENT_COLOR_ARG} --duration 20",
                "vkms-client.log"))

            def mapped():
                state = json.loads(guest(f"{ctl} state"))
                return state if any(w["mapped"] for w in state["windows"]) else None

            state = wait_for("the client's window", mapped, seconds=10)
            status = state["backend_status"]
            if status["driver"] != "vkms":
                raise Failed(f"the DRM driver is {status['driver']!r}")
            output = status["output"]
            if output["frames_presented"] < 1:
                raise Failed("no frame was presented")
            with open(os.path.join(directory, "state.json"), "w") as file:
                json.dump(state, file, indent=2)
            guest(f"{ctl} quit")
            wait_for("the compositor to exit", lambda: not alive(pids[0]), seconds=8)
            return (f"session over SSH on {output['connector']} "
                    f"{output['mode'][0]}x{output['mode'][1]} ({output['scanout']}), "
                    f"{output['frames_presented']} frames presented, a client window mapped")
        finally:
            keep(f"{GUEST_DIR}/vkms-session.log", "proto-compositor-session.log")
            for pid in pids:
                stop_spawned(pid)

    def selftest():
        report_dir = f"{GUEST_DIR}/vkms-selftest"
        guest(f"rm -rf {report_dir}")
        log = guest(f"{common}{GUEST_BIN}/proto-compositor kms-selftest --device {found['card']} "
                    f"--output {found['connector']} --seconds 5 --report {report_dir} "
                    f"--mode 1920x1080 --vulkan-device cpu --scanout cpu 2>&1", check=False,
                    timeout=60)
        with open(os.path.join(directory, "proto-compositor-selftest.log"), "w") as file:
            file.write(log)
        vm("pull", report_dir + "/", os.path.join(directory, "selftest") + "/", check=False)
        try:
            with open(os.path.join(directory, "selftest", "report.json")) as file:
                report = json.load(file)
        except (OSError, ValueError):
            raise Failed(f"no report was written; the log ends with: {log.strip()[-300:]}")
        failed = [c for c in report["checks"] if c["status"] == "fail"]
        if failed:
            raise Failed("; ".join(f"{c['name']}: {c['detail']}" for c in failed))
        checks = {c["name"]: c for c in report["checks"]}
        times = report["frame_time_ms"]
        return (f"1920x1080 on vkms without a VT: {report['frames_presented']} frames, flips "
                f"{checks['page_flip_timing']['detail']} [{checks['page_flip_timing']['status']}], "
                f"draw {times['draw']['median']:.2f} ms + present "
                f"{times['present']['median']:.2f} ms")

    if item("load", load):
        item("session", session)
        item("selftest-1080p", selftest)


class Part:
    """A group of items that is not one of the two scanout paths."""

    def __init__(self, name, out, uid, results):
        self.name = name
        self.out = os.path.join(out, name)
        os.makedirs(self.out, exist_ok=True)
        self.runtime = f"/run/user/{uid}"
        self.control = f"{self.runtime}/{SOCKET}.control"
        self.results = results

    def record(self, name, status, detail):
        label = f"{self.name}/{name}"
        print(f"{status} {label}: {detail}", flush=True)
        self.results.append({"item": label, "status": status, "detail": detail})

    def item(self, name, function):
        try:
            status, detail = "PASS", function() or ""
        except Skipped as reason:
            status, detail = "SKIP", str(reason)
        except Failed as error:
            status, detail = "FAIL", str(error)
        except Exception as error:
            status, detail = "FAIL", f"{type(error).__name__}: {error}"
        if status == "FAIL":
            self.keep_log(f"fail-{name}")
        self.record(name, status, detail)
        return status == "PASS"

    def keep_log(self, stage):
        try:
            with open(os.path.join(self.out, f"proto-compositor-{stage}.log"), "w") as file:
                file.write(vm("tty-log", check=False))
        except Exception:
            pass

    def ctl(self, *args):
        command = " ".join(shlex.quote(a) for a in
                           [f"{GUEST_BIN}/proto-ctl", "--socket", self.control, *args])
        return json.loads(guest(f"timeout 10 {command}", timeout=30))

    def unit_active(self):
        return "ActiveState=active" in vm("tty-status", check=False)

    def start(self, *args, builtin=False):
        vm("tty-run", "--", f"{GUEST_BIN}/proto-compositor", "--backend", "kms", "--socket", SOCKET,
           "--background", BACKGROUND_ARG, "--vulkan-device", "cpu", *drawing(args, builtin))
        try:
            wait_for("the control socket to answer", lambda: self.unit_active()
                     and self.ctl("ping").get("ok"), seconds=20)
        except Failed as error:
            log = vm("tty-log", check=False).strip().splitlines()
            raise Failed(f"{error}; the log ends with: {' | '.join(log[-4:])}")

    def stop(self):
        vm("tty-stop", check=False)
        stop_spawned()

    def output(self):
        return self.ctl("state")["backend_status"].get("output")


def run_exit(*args, timeout=90):
    """Runs a command on a guest VT until it ends. Returns (exit status, log)."""
    result = subprocess.run([VM, "tty-run", "--wait", "--", *args], capture_output=True,
                            text=True, timeout=timeout)
    return result.returncode, vm("tty-log", check=False)


def check_outputs(out, uid, results):
    """Two outputs and hotplug. The VM has two outputs unless it was started
    with HIZUKURI_VM_OUTPUTS=1: the second one is connected and disconnected
    with `vm output 1 on|off`, and its screen is read with `--head 1`."""
    part = Part("outputs", out, uid, results)
    if subprocess.run([VM, "screen-size", "--head", "1"], capture_output=True).returncode != 0:
        part.record("configured", "FAIL",
                    "the VM has one output; start it without HIZUKURI_VM_OUTPUTS=1")
        return

    def second(points):
        out_text = vm("pixels", "--head", "1", *[f"{x},{y}" for x, y in points])
        return [tuple(int(v) for v in line.split(": ")[1].split())
                for line in out_text.splitlines() if ": " in line]

    corners = [(5, 5), (640, 400), (1270, 790)]

    def connect():
        vm("output", "1", "on")
        summary = wait_for("Virtual-2 in the summary", lambda: (lambda text: text if
                           "Virtual-2" in text else None)(
                               guest(f"{GUEST_BIN}/proto-compositor kms-info --summary")), seconds=8)
        with open(os.path.join(part.out, "kms-summary.txt"), "w") as file:
            file.write(summary)
        return " | ".join(line.strip() for line in summary.splitlines())

    def default_leaves_the_other():
        before = second(corners)
        part.start("--exit-after", "60")
        try:
            output = part.output()
            if output["connector"] != "Virtual-1":
                raise Failed(f"the default output is {output['connector']}")
            wait_for("the background on the first output",
                     lambda: all(near(v, BACKGROUND) for v in pixels(corners)), seconds=5)
            after = second(corners)
            if after != before:
                raise Failed(f"the second output changed: {before} to {after}")
            # Plugging the display that is not in use must not disturb anything.
            frames = output["frames_presented"]
            vm("output", "1", "off")
            time.sleep(1.5)
            vm("output", "1", "on")
            time.sleep(1.5)
            output = part.output()
            if not output or output["connector"] != "Virtual-1":
                raise Failed(f"after hotplugging the other display the output is {output}")
            if not all(near(v, BACKGROUND) for v in pixels(corners)):
                raise Failed("the first output lost its picture")
            return (f"Virtual-1 is driven, Virtual-2 keeps showing {before[0]}; unplugging and "
                    f"plugging Virtual-2 changes nothing ({frames} frames before)")
        finally:
            part.keep_log("default")
            part.stop()

    def named_and_hotplug():
        first_before = pixels(corners)
        part.start("--kms-output", "Virtual-2", "--exit-after", "90")
        try:
            output = part.output()
            if output["connector"] != "Virtual-2":
                raise Failed(f"asked for Virtual-2, got {output['connector']}")
            wait_for("the background on the second output",
                     lambda: all(near(v, BACKGROUND) for v in second(corners)), seconds=5)
            if pixels(corners) != first_before:
                raise Failed("the first output changed")
            spawn(f"{GUEST_BIN}/proto-test-client --socket {part.runtime}/{SOCKET} "
                  f"--title hzk-outputs --size {CLIENT_SIZE[0]}x{CLIENT_SIZE[1]} "
                  f"--color {CLIENT_COLOR_ARG} --duration 80", "outputs-client.log")
            window = wait_for("the client's window", lambda: next(
                (w for w in part.ctl("state")["windows"] if w["mapped"]), None))
            part.ctl("move-window", str(window["id"]), "100", "100")
            inside = [(120, 120), (380, 280)]
            wait_for("the window on the second output",
                     lambda: all(near(v, CLIENT_COLOR) for v in second(inside)), seconds=5)
            vm("screendump", "--head", "1", os.path.join(part.out, "screen-second-output.png"),
               check=False)

            vm("output", "1", "off")
            wait_for("the output to go away", lambda: part.output() is None, seconds=8)
            if not part.unit_active():
                raise Failed("the compositor exited when its display was unplugged")
            windows = len(part.ctl("state")["windows"])
            vm("output", "1", "on")
            wait_for("the output to come back", lambda: part.output() is not None, seconds=8)
            wait_for("the window back on the second output",
                     lambda: all(near(v, CLIENT_COLOR) for v in second(inside)), seconds=8)
            if len(part.ctl("state")["windows"]) != windows:
                raise Failed("windows were lost across the hotplug")
            return ("`--kms-output Virtual-2` drives the second output only; unplugged: the "
                    "output is removed and the compositor keeps running; plugged back: the "
                    "output and the window's pixels return")
        finally:
            part.keep_log("named")
            part.stop()

    def script_layer_hotplug():
        """The same unplug and plug with the script drawing and tiling."""
        part.start("--kms-output", "Virtual-2", "--exit-after", "90", builtin=True)
        try:
            colors = {"hzk-hot-a": (200, 100, 50), "hzk-hot-b": (60, 160, 220)}
            for title, color in colors.items():
                spawn(f"{GUEST_BIN}/proto-test-client --socket {part.runtime}/{SOCKET} "
                      f"--title {title} --size 300x200 --color "
                      f"{'%02X%02X%02XFF' % color} --duration 80", f"{title}.log")
                wait_for(f"the window of {title}", lambda: next(
                    (w for w in part.ctl("state")["windows"] if w["title"] == title
                     and w["mapped"]), None))

            def tiles():
                windows = part.ctl("state")["windows"]
                if len(windows) != 2 or any(w["position"][1] != 8 or w["size"][0] <= 300
                                            for w in windows):
                    return None
                return {w["title"]: (w["position"][0] + w["size"][0] // 2,
                                     w["position"][1] + w["size"][1] // 2) for w in windows}

            centers = wait_for("two tiles on the second output", tiles, seconds=10)
            points = [centers[title] for title in colors]

            def drawn():
                values = second(points)
                return values if all(near(v, c) for v, c in zip(values, colors.values())) \
                    else None

            wait_for("the tiles' pixels on the second output", drawn, seconds=8)
            vm("output", "1", "off")
            wait_for("the output to go away", lambda: part.output() is None, seconds=8)
            time.sleep(1.0)
            state = part.ctl("state")
            if not part.unit_active() or state["script"]["mode"] != "running":
                raise Failed(f"without its display: unit active {part.unit_active()}, "
                             f"script {state['script']['mode']}")
            vm("output", "1", "on")
            wait_for("the output to come back", lambda: part.output() is not None, seconds=8)
            wait_for("the tiles back after the hotplug", tiles, seconds=8)
            values = wait_for("the tiles' pixels back on the second output", drawn, seconds=8)
            vm("screendump", "--head", "1",
               os.path.join(part.out, "screen-script-after-hotplug.png"), check=False)
            script = part.ctl("state")["script"]
            if script["mode"] != "running" or script["failures"]:
                raise Failed(f"the script did not get through the hotplug: {script['mode']}, "
                             f"{script['last_failures']}")
            return (f"script drawing and tiling on Virtual-2: unplugged, the script "
                    f"stays loaded with no failure; plugged back, the two tiles are drawn "
                    f"again ({values})")
        finally:
            part.keep_log("script-hotplug")
            part.stop()

    part.stop()
    try:
        if part.item("connect", connect):
            part.item("default-leaves-the-other", default_leaves_the_other)
            part.item("named-and-hotplug", named_and_hotplug)
            part.item("script-layer-hotplug", script_layer_hotplug)
    finally:
        vm("output", "1", "off", check=False)


def check_safety(out, uid, results):
    """The watchdog. gdb keeps the main thread stopped while the watchdog
    thread runs: the process must abort and the console must come back. A
    stop of the whole process must not be mistaken for that."""
    part = Part("safety", out, uid, results)
    def watchdog():
        part.start("--watchdog", "5", "--exit-after", "60")
        pid = main_pid()
        if pid is None:
            raise Failed("the unit has no main process")
        answer = guest(f"freeze-main-loop {pid} watchdog 30", timeout=60).split()
        log = vm("tty-log", check=False)
        with open(os.path.join(part.out, "proto-compositor-watchdog.log"), "w") as file:
            file.write(log)
        if answer[0] != "gone":
            raise Failed("the process survived a main loop that was stuck for 30 s")
        if "the main loop has been stuck" not in log:
            raise Failed("the process ended, but not through the watchdog")
        took = int(answer[1]) / 1000
        if not 4.0 <= took <= 12.0:
            raise Failed(f"the watchdog (5 s) took {took:.1f} s")
        wait_for("the unit to end", lambda: not part.unit_active(), seconds=8)
        guest(f"rm -f {part.runtime}/{SOCKET} {part.runtime}/{SOCKET}.control "
              f"{part.runtime}/{SOCKET}.lock")
        return f"main thread frozen with gdb: aborted after {took:.1f} s (--watchdog 5)"

    def stopped_process():
        part.start("--watchdog", "5", "--exit-after", "60")
        try:
            pid = main_pid()
            if pid is None:
                raise Failed("the unit has no main process")
            guest(f"kill -STOP {pid}; sleep 9; kill -CONT {pid}", timeout=30)
            time.sleep(1.5)
            if not part.unit_active() or not part.ctl("ping").get("ok"):
                raise Failed("the watchdog ended a process that was stopped and continued")
            return "stopped for 9 s (--watchdog 5) and continued: still running and answering"
        finally:
            part.keep_log("stopped-process")
            part.stop()

    part.stop()
    part.item("watchdog", watchdog)
    part.stop()
    part.item("stopped-process", stopped_process)

FAULTY_PROJECT = """\
-- The built-in session with faults that can be switched on from outside.
hz.include("session")
local usual_frame, usual_key, usual_hit = frame, key, hit_test
local fault = nil
function set_fault(kind) fault = kind end
function frame(output, target)
    if fault == "throw" then error("deliberate failure in the VM") end
    if fault == "loop" then while true do end end
    if fault == "gpu" then gfx.draw(3) end
    if fault == "open-pass" then
        gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 1, 0, 1, 1 } } } })
        return
    end
    usual_frame(output, target)
end
function key(keycode, pressed, mods)
    if fault == "key-loop" then while true do end end
    if usual_key then return usual_key(keycode, pressed, mods) end
    return false
end
function hit_test(x, y, accepts)
    if fault == "hit-throw" then error("deliberate failure in hit_test") end
    return usual_hit(x, y, accepts)
end
"""


def check_script(out, uid, results):
    """The script layer (drawing and window management written in the
    scripting language) as a DRM/KMS session, beyond what
    `lavapipe/script-layer` covers: real applications, faults in every hook,
    editing the script while it runs, VT switching, the watchdog, and a
    frame times. Runs on lavapipe."""
    part = Part("script", out, uid, results)
    project = f"{GUEST_DIR}/script-project"
    env = f"env -u DISPLAY XDG_RUNTIME_DIR={part.runtime} WAYLAND_DISPLAY={SOCKET}"
    apps = ["foot", "weston-terminal", "alacritty"]
    app_pids = []

    def state():
        return part.ctl("state")

    def script_state():
        return state()["script"]

    def clients():
        """How many of the applications are still running, by the process
        ids they were started with."""
        return len(guest("".join(f"kill -0 {pid} 2>/dev/null && echo {pid}; "
                                 for pid in app_pids) + "true").split())

    def tiles(count, gap=8):
        width = (1280 - gap * (count + 1)) // count
        return [(gap + i * (width + gap), width) for i in range(count)]

    def open_apps():
        for index, app in enumerate(apps):
            if not guest(f"command -v {app} || true").strip():
                raise Skipped(f"{app} is not installed in the guest")
            app_pids.append(spawn(f"{env} {app}", f"script-{app}.log"))
            wait_for(f"the window of {app}", lambda: len(
                [w for w in state()["windows"] if w["mapped"] and w["surfaces"]
                 and w["surfaces"][0]["has_contents"]]) == index + 1, seconds=20)

    def tiled(gap=8):
        windows = sorted(state()["windows"], key=lambda w: w["position"][0])
        expected = tiles(len(windows), gap)
        # Terminals round their size down to whole character cells.
        good = all(w["position"][1] == gap and abs(w["position"][0] - x) <= 24
                   and width - 48 <= w["size"][0] <= width + 8
                   for w, (x, width) in zip(windows, expected))
        return windows if good and len(windows) == len(apps) else None

    def real_apps():
        part.start("--script", project, "--exit-after", "240")
        current = state()
        if current["window_manager"] != "script" or current["script"]["mode"] != "running":
            raise Failed(f"the script is not in charge: {current['script']}")
        open_apps()
        windows = wait_for("three tiles", tiled, seconds=10)
        # Something of every application is on the screen, and the gaps show
        # the background. The gap points stay clear of weston-terminal, whose
        # buffer carries a translucent shadow around the window.
        middles = [(w["position"][0] + w["size"][0] // 2, 400) for w in windows]
        clear = min(windows, key=lambda w: w["position"][0])
        below = clear["position"][0] + clear["size"][0] // 2
        gaps = [(3, 400), (1277, 400), (below, 3), (below, 797)]
        seen = []

        def on_screen():
            seen[:] = pixels(middles + gaps)
            return seen[:] if all(not near(v, BACKGROUND, 6) for v in seen[:3]) and all(
                near(v, BACKGROUND) for v in seen[3:]) else None

        try:
            values = wait_for("the applications on screen", on_screen, seconds=8)
        except Failed as error:
            raise Failed(f"{error}: middles {seen[:3]}, gaps {seen[3:]}")
        vm("screendump", os.path.join(part.out, "screen-real-apps.png"), check=False)
        # The tiling script's keys, with real applications.
        newest = state()["focus"]["window"]
        vm("key", "logo-h")
        wait_for("the focus to move left", lambda: state()["focus"]["window"] != newest,
                 seconds=5)
        order = [w["id"] for w in windows]
        vm("key", "logo-shift-l")
        wait_for("two tiles to swap", lambda: (lambda now: now and [w["id"] for w in now]
                                               != order)(tiled()), seconds=5)
        vm("key", "logo-shift-h")
        wait_for("the tiles to swap back", lambda: (lambda now: now and [w["id"] for w in now]
                                                    == order)(tiled()), seconds=5)
        if script_state()["failures"]:
            raise Failed(f"the script failed: {script_state()['last_failures']}")
        return (f"{', '.join(apps)} side by side in tiles at x "
                f"{[w['position'][0] for w in windows]} with widths "
                f"{[w['size'][0] for w in windows]}; middles {values[:3]}, gaps are the "
                f"background; logo+H moved the focus, logo+shift+L/H swapped two tiles")

    def faults():
        count = len(state()["windows"])
        windows = sorted(state()["windows"], key=lambda w: w["position"][0])
        middles = [(w["position"][0] + w["size"][0] // 2, 400) for w in windows]
        typed = f"{GUEST_DIR}/typed-in-fallback"
        guest(f"rm -f {typed}")
        notes = []

        def recover(fault):
            part.ctl("script-call", "set_fault", "null")
            part.ctl("script-reload")
            wait_for(f"the script back after {fault}",
                     lambda: script_state()["mode"] == "running", seconds=8)
            wait_for("the marker to go away",
                     lambda: pixels([(20, 1)])[0][0] < 200, seconds=5)

        def in_fallback(fault, kind):
            script = wait_for(f"the fallback after {fault}", lambda: (lambda sc: sc if
                              sc["mode"] == "fallback" else None)(script_state()), seconds=8)
            last = script["last_failures"][-1]
            if last["kind"] != kind:
                raise Failed(f"{fault}: recorded as {last['kind']!r}, expected {kind!r}")
            # The fallback's marker along the top, and the windows below it.
            marker = pixels([(20, 1)])[0]
            if not (marker[0] >= 250 and marker[1] <= 5):
                raise Failed(f"{fault}: no fallback marker on screen ({marker})")
            shown = pixels(middles)
            if any(near(v, BACKGROUND, 6) for v in shown):
                raise Failed(f"{fault}: a window is not drawn by the fallback ({shown})")
            if not part.unit_active() or clients() != len(apps) or \
                    len(state()["windows"]) != count:
                raise Failed(f"{fault}: {clients()} of {len(apps)} applications run, "
                             f"{len(state()['windows'])} of {count} windows are left")
            notes.append(f"{fault} ({kind})")

        for fault, kind in (("throw", "runtime"), ("gpu", "gfx"), ("open-pass", "runtime"),
                            ("loop", "timeout")):
            part.ctl("script-call", "set_fault", json.dumps(fault))
            in_fallback(f"frame: {fault}", kind)
            if fault == "loop":
                # The session is usable without the script: a click gives the
                # focus to the terminal on the left, and what is typed runs
                # in its shell.
                vm("move", str(middles[0][0]), "400")
                vm("click")
                vm("type", f"touch {typed}")
                vm("key", "ret")
                wait_for("the typed command to run in foot",
                         lambda: guest(f"ls {typed} 2>/dev/null || true").strip(), seconds=8)
                notes.append("typing into foot works in the fallback")
            recover(fault)

        # A hit test that throws: every pointer motion asks the script. After
        # eight failures in a row the core stops asking.
        part.ctl("script-call", "set_fault", json.dumps("hit-throw"))
        for index in range(12):
            vm("move", str(200 + index * 70), str(250 + (index % 3) * 120))
            time.sleep(0.1)
        in_fallback("hit_test: throw, 12 pointer motions", "runtime")
        vm("click")  # still lands on a window, through the built-in hit test
        recover("hit-throw")

        # A key handler that does not return: the key arrives after the
        # time budget, the script is taken out at once.
        part.ctl("script-call", "set_fault", json.dumps("key-loop"))
        vm("key", "a")
        in_fallback("key: endless loop", "timeout")
        recover("key-loop")
        if clients() != len(apps):
            raise Failed("an application went away")
        return ("each fault ends in the built-in fallback with all windows on screen and "
                "every application still connected, and a reload brings the script back: "
                + ", ".join(notes))

    def live_edit():
        guest(f"sed -i 's/^T.gap = 8$/T.gap = 40/' {project}/tile.luau")
        vm("key", "ctrl-alt-shift-r")
        windows = wait_for("the tiles with the new gap", lambda: tiled(40), seconds=10)
        if not near(pixels([(20, 400)])[0], BACKGROUND):
            raise Failed("the wider gap does not show the background")
        if clients() != len(apps):
            raise Failed("an application went away over the reload")
        # A syntax error in a module: the reload is refused before the
        # running script goes. The script stays in charge with the gap it
        # had, and the failure names the file.
        before = script_state()
        guest(f"echo 'this is not luau (' >> {project}/tile.luau")
        vm("key", "ctrl-alt-shift-r")
        broken = wait_for("the refused reload to be recorded", lambda: (lambda sc: sc if
                          sc["failures"] > before["failures"] else None)(script_state()),
                          seconds=8)
        last = broken["last_failures"][-1]
        if (last["what"], last["kind"]) != ("load", "syntax") or \
                "tile.luau" not in last["message"]:
            raise Failed(f"the syntax error was recorded as {last}")
        if broken["mode"] != "running" or broken["reloads"] != before["reloads"]:
            raise Failed(f"a module with a syntax error replaced the running script: mode "
                         f"{broken['mode']}, reloads {before['reloads']} to "
                         f"{broken['reloads']}")
        if not part.unit_active() or clients() != len(apps) or not tiled(40):
            raise Failed("the running script did not stay in charge over the syntax error")
        if pixels([(20, 1)])[0][0] >= 200:
            raise Failed("the fallback's marker is on screen after the syntax error")
        # Fixed: the reload goes through.
        guest(f"sed -i '$d' {project}/tile.luau && "
              f"sed -i 's/^T.gap = 40$/T.gap = 8/' {project}/tile.luau")
        vm("key", "ctrl-alt-shift-r")
        wait_for("the script after the fix", lambda: (lambda sc: sc["mode"] == "running" and
                 sc["reloads"] > before["reloads"])(script_state()), seconds=8)
        wait_for("the tiles with the old gap", lambda: tiled(8), seconds=10)
        return (f"edited tile.luau in the guest (gap 8 to 40), Ctrl+Alt+Shift+R: tiles at "
                f"{[w['position'][0] for w in windows]}, applications kept; a syntax error "
                f"in tile.luau was refused and the running script stayed (mode "
                f"{broken['mode']}, {last['message']}); fixed and reloaded")

    def vt_switch():
        points = [(3, 400), (200, 400), (640, 400), (1064, 400)]
        before = pixels(points)
        vm("key", "ctrl-alt-f2")
        wait_for("the session to pause", lambda: not state()["session_active"], seconds=8)
        guest("sudo chvt 7")
        wait_for("the session to resume", lambda: state()["session_active"], seconds=8)
        wait_for("the same pixels as before", lambda: pixels(points) == before, seconds=8)
        if script_state()["mode"] != "running" or clients() != len(apps):
            raise Failed(f"after the switch: {script_state()['mode']}, {clients()} applications")
        return "away with Ctrl+Alt+F2 and back: the script draws the same pixels again"

    def quit_key():
        vm("key", "ctrl-alt-backspace")
        wait_for("the compositor to exit", lambda: not part.unit_active(), seconds=8)
        part.keep_log("session")
        return "Ctrl+Alt+Backspace ended the session"

    def watchdog():
        part.start("--watchdog", "5", "--exit-after", "60", builtin=True)
        pid = main_pid()
        if pid is None:
            raise Failed("the unit has no main process")
        answer = guest(f"freeze-main-loop {pid} watchdog 30", timeout=60).split()
        log = vm("tty-log", check=False)
        if answer[0] != "gone" or "the main loop has been stuck" not in log:
            raise Failed(f"the watchdog did not end the stuck session ({answer})")
        wait_for("the unit to end", lambda: not part.unit_active(), seconds=8)
        guest(f"rm -f {part.runtime}/{SOCKET} {part.runtime}/{SOCKET}.control "
              f"{part.runtime}/{SOCKET}.lock")
        return f"main thread frozen: aborted after {int(answer[1]) / 1000:.1f} s (--watchdog 5)"

    def apps_1080p():
        busy = "sh -c 'while :; do seq 1 3000; done'"
        commands = [f"foot {busy}", f"foot {busy}", "weston-terminal",
                    f"env VK_DRIVER_FILES={LAVAPIPE_ICD} vkcube --wsi wayland"]
        measured = {}
        for label in ("script", "script-tiling"):
            part.stop()
            part.start("--kms-mode", "1920x1080", "--exit-after", "120",
                       builtin=label == "script-tiling")
            for index, command in enumerate(commands):
                spawn(f"{env} {command}", f"busy-{index}.log")
            windows = wait_for("four busy windows", lambda: (lambda ws: ws if len(ws) == 4
                               else None)([w for w in state()["windows"] if w["mapped"]
                                           and w["surfaces"]
                                           and w["surfaces"][0]["has_contents"]]), seconds=25)
            if label != "script-tiling":
                for index, window in enumerate(windows):
                    part.ctl("move-window", str(window["id"]), str(40 + index * 460),
                             str(40 + index * 120))
            time.sleep(8.0)
            current = state()
            times = current["frame_time_ms"]
            output = current["backend_status"]["output"]
            measured[label] = {"frame_time_ms": times,
                               "flip_interval_ms": output["flip_interval_ms"],
                               "script_timing": (current.get("script") or {}).get("timing")}
            if label == "script-tiling":
                vm("screendump", os.path.join(part.out, "screen-apps-1080p-tiling.png"),
                   check=False)
        part.stop()
        with open(os.path.join(part.out, "frame-time-apps-1080p.json"), "w") as file:
            json.dump(measured, file, indent=2)

        def line(label):
            times = measured[label]["frame_time_ms"]
            call = (measured[label]["script_timing"] or {}).get("frame")
            return (f"{label}: draw {times['draw']['median']:.2f} + present "
                    f"{times['present']['median']:.2f} = {times['total']['median']:.2f} ms, "
                    f"flips every {measured[label]['flip_interval_ms']['median']:.2f} ms"
                    + (f", the script's frame call {call['mean_us'] / 1000:.2f} ms"
                       if call else ""))

        return "4 busy windows at 1920x1080 on lavapipe; " + "; ".join(
            line(label) for label in measured)

    def launcher():
        """What a person does by hand: `hzk script` on a VT, edit a script in
        one of its terminals, load it again with the key."""
        control = f"{part.runtime}/hizukuri-0.control"
        edited = "/home/hizukuri/hzk-scripts"

        def hzk_state():
            text = guest(f"timeout 10 {GUEST_BIN}/proto-ctl --socket {control} state "
                         f"2>/dev/null || true", timeout=30)
            return json.loads(text) if text.strip() else None

        def hzk_tiles(gap):
            current = hzk_state()
            if not current or current["script"]["mode"] != "running":
                return None
            windows = sorted(current["windows"], key=lambda w: w["position"][0])
            if len(windows) != 3 or any(w["position"][1] != gap for w in windows) or \
                    windows[0]["position"][0] != gap:
                return None
            return windows

        guest(f"rm -rf {edited}.orig && mkdir -p {edited}.orig")
        vm("push", os.path.join(ROOT, "prototype", "compositor", "scripts") + "/",
           "hzk-scripts.orig/")
        vm("tty-run", "--", "hzk", "script", "--seconds", "90", "--reset-scripts")
        try:
            wait_for("three tiles from `hzk script`", lambda: hzk_tiles(8), seconds=40)
            # The newest window, a terminal, has the focus: type the edit there.
            vm("type", f"sed -i 's/T.gap = 8/T.gap = 30/' {edited}/tile.luau")
            vm("key", "ret")
            wait_for("the edit to reach the file", lambda: "T.gap = 30" in guest(
                f"grep '^T.gap' {edited}/tile.luau || true"), seconds=8)
            vm("key", "ctrl-alt-shift-r")
            windows = wait_for("the tiles with the edited gap", lambda: hzk_tiles(30),
                               seconds=10)
            # What the launcher started: the compositor and the applications
            # are its children.
            launcher_pid = main_pid()
            children = guest(f"ps -o pid= --ppid {launcher_pid}").split()
            if len(children) < 4:
                raise Failed(f"the launcher has {len(children)} children, expected the "
                             f"compositor and three applications")
            vm("key", "ctrl-alt-backspace")
            wait_for("the launcher to end", lambda: not part.unit_active(), seconds=10)
            log = vm("tty-log", check=False)
            if "proto-compositor ended with status 0" not in log:
                raise Failed(f"the launcher's log ends with: {log.strip().splitlines()[-1:]}")
            left = guest("".join(f"kill -0 {pid} 2>/dev/null && echo {pid}; "
                                 for pid in children) + "true").split()
            if left:
                raise Failed(f"processes of the launcher were left behind: {left}")
            return (f"`hzk script` on a VT opens three tiled applications; a script edited "
                    f"from one of them and Ctrl+Alt+Shift+R moves the tiles to x "
                    f"{[w['position'][0] for w in windows]}; Ctrl+Alt+Backspace ends it and "
                    f"the applications")
        finally:
            with open(os.path.join(part.out, "hzk-script.log"), "w") as file:
                file.write(vm("tty-log", check=False))
            part.stop()
            guest(f"rm -rf {edited} {edited}.bak; true", check=False)

    part.stop()
    # The project the session runs: the built-in scripts as files, with the
    # faults added to its entry. Editing these files is what `live-edit` does.
    guest(f"rm -rf {project} && mkdir -p {project}")
    vm("push", os.path.join(ROOT, "prototype", "compositor", "scripts") + "/", f"{project}/")
    guest(f"cat > {project}/main.luau <<'LUAU'\n{FAULTY_PROJECT}LUAU")
    try:
        if part.item("real-apps-tiled", real_apps):
            part.item("faults", faults)
            part.item("live-edit", live_edit)
            part.item("vt-switch", vt_switch)
            part.item("quit-key", quit_key)
    finally:
        part.stop()
        part.item("watchdog", watchdog)
    part.stop()
    part.item("apps-1080p", apps_1080p)
    part.stop()
    part.item("hzk-script-launcher", launcher)


# The input method of the `ime` part: fcitx5 with the Anthy engine, starting
# in Japanese (hiragana, romaji typing). Written to a config directory of the
# check's own.
FCITX5_PROFILE = """[Groups/0]
Name=Default
Default Layout=us
DefaultIM=anthy

[Groups/0/Items/0]
Name=keyboard-us
Layout=

[Groups/0/Items/1]
Name=anthy
Layout=

[GroupOrder]
0=Default
"""
FCITX5_CONFIG = """[Behavior]
ActiveByDefault=True
ShareInputState=All
ShowInputMethodInformation=False
"""
# Run by foot: every line typed into the terminal goes to a file of its own.
IME_READER = """#!/bin/sh
n=0
while IFS= read -r line; do
  n=$((n + 1))
  printf '%s' "$line" > "$1/ime-line-$n.txt"
done
"""


def protocol_order(log):
    """The Wayland requests a client made, in the order of their first use,
    from its WAYLAND_DEBUG output: `interface.request` per line."""
    import re
    seen = []
    # `interface#id.request(...)`, or `interface@id.request(...)` in older
    # versions of libwayland.
    pattern = re.compile(r" -> ([A-Za-z0-9_]+)[@#][0-9]+\.([A-Za-z0-9_]+)\(")
    for line in log.splitlines():
        match = pattern.search(line)
        if match:
            name = f"{match.group(1)}.{match.group(2)}"
            if name not in seen:
                seen.append(name)
    return seen


def check_ime(out, uid, results):
    """Japanese input with a real input method: fcitx5 (Anthy) and foot, in
    the script layer on the DRM/KMS session. Keys are typed through QMP;
    what the terminal's shell receives is written to files and compared.
    The candidate window is captured, and its place checked against the
    cursor rectangle the terminal reported."""
    part = Part("ime", out, uid, results)
    work = f"{GUEST_DIR}/ime"
    env = (f"env -u DISPLAY -u DBUS_SESSION_BUS_ADDRESS XDG_RUNTIME_DIR={part.runtime} "
           f"WAYLAND_DISPLAY={SOCKET}")
    fcitx5 = {"pid": None}

    def state():
        return part.ctl("state")

    def ime():
        return state()["input_method"]

    def line(number):
        return guest(f"cat {work}/ime-line-{number}.txt 2>/dev/null || true", check=False)

    def press(keys):
        """`vm type` for `type:TEXT`, `vm key` for anything else."""
        for key in keys:
            if key.startswith("type:"):
                vm("type", key[5:])
            else:
                vm("key", key)
            time.sleep(0.4)

    def typed_line(number, keys, expected, settle=1.0):
        """Types `keys`, then Return twice (the first one commits what is
        composed, the second ends the line); waits for the shell's line
        `number`."""
        press(keys)
        time.sleep(settle)
        press(["ret", "ret"])
        try:
            return wait_for(f"line {number} in the terminal",
                            lambda: line(number) or None, seconds=10)
        except Failed:
            vm("screendump", os.path.join(part.out, f"screen-line-{number}.png"), check=False)
            raise Failed(f"nothing arrived for {expected!r}")

    def setup():
        if not guest("command -v fcitx5 && command -v foot || true").strip():
            raise Failed("fcitx5 or foot is not installed in the guest")
        guest(f"rm -rf {work} && mkdir -p {work}/config/fcitx5 && "
              f"cat > {work}/config/fcitx5/profile <<'EOF'\n{FCITX5_PROFILE}EOF\n"
              f"cat > {work}/config/fcitx5/config <<'EOF'\n{FCITX5_CONFIG}EOF\n"
              f"cat > {work}/reader.sh <<'EOF'\n{IME_READER}EOF\n"
              f"chmod +x {work}/reader.sh")
        part.start("--exit-after", "300", builtin=True)
        fcitx5["pid"] = spawn(
            f"{env} XDG_CONFIG_HOME={work}/config WAYLAND_DEBUG=1 dbus-run-session -- "
            f"fcitx5 --verbose '*=3'", "ime-fcitx5.log")
        wait_for("fcitx5 to become the input method", lambda: ime()["bound"], seconds=30)
        spawn(f"{env} foot --app-id ime-foot sh {work}/reader.sh {work}", "ime-foot.log")
        wait_for("foot's window", lambda: any(
            w["app_id"] == "ime-foot" and w["mapped"] for w in state()["windows"]), seconds=20)
        status = wait_for("the input method active, holding the keyboard",
                          lambda: (lambda s: s if s["active"] and s["keyboard_grab"]
                                   else None)(ime()), seconds=20)
        vm("screendump", os.path.join(part.out, "screen-start.png"), check=False)
        return ("fcitx5 became the input method; foot got the focus and enabled its text "
                f"input; fcitx5 was activated and grabbed the keyboard ({status})")

    def kana():
        press(["type:aiueo"])
        time.sleep(1.0)
        vm("screendump", os.path.join(part.out, "screen-preedit.png"), check=False)
        text = typed_line(1, [], "あいうえお", settle=0)
        if text != "あいうえお":
            raise Failed(f"the terminal got {text!r}")
        return "aiueo, Return, Return: the shell read あいうえお"

    def conversion():
        text = typed_line(2, ["type:nihongo", "spc"], "日本語")
        if text != "日本語":
            raise Failed(f"the terminal got {text!r}")
        return "nihongo, Space, Return, Return: the shell read 日本語"

    def candidates():
        press(["type:kouho", "spc", "spc"])
        popup = wait_for("the candidate window", lambda: (lambda p: p if p and p["shown"]
                                                          else None)(ime()["popup"]), seconds=10)

        def drawn():
            current = state()
            for window in current["windows"]:
                for node in window["surfaces"]:
                    if node["role"] == "input-popup" and node["has_contents"]:
                        return current, window, node
            return None

        current, window, node = wait_for("the candidate window's contents", drawn, seconds=10)
        time.sleep(0.5)
        vm("screendump", os.path.join(part.out, "screen-candidates.png"), check=False)
        with open(os.path.join(part.out, "state-candidates.json"), "w") as file:
            json.dump(current, file, indent=1)
        popup = current["input_method"]["popup"]
        root = window["surfaces"][0]
        cx, cy, cw, ch = popup["cursor_rectangle"]
        px, py = popup["position"]
        width, height = node["size"]
        # The cursor rectangle is relative to foot's surface; the popup's
        # position to the window geometry, where the surface starts at the
        # root surface's offset.
        ox, oy = root["offset"]
        below = py == oy + cy + ch
        above = py + height == oy + cy
        if not (below or above) or width <= 0 or height <= 0:
            raise Failed(f"the popup at {popup['position']} ({width}x{height}) is not by the "
                         f"cursor {popup['cursor_rectangle']} (surface offset {root['offset']})")
        # Take the selected candidate. The window goes; Return ends the line.
        press(["ret"])
        wait_for("the candidate window to go",
                 lambda: not (ime()["popup"] or {}).get("shown"), seconds=10)
        press(["ret"])
        text = wait_for("line 3 in the terminal", lambda: line(3) or None, seconds=10)
        if text.isascii():
            raise Failed(f"the terminal got {text!r} for a candidate")
        return (f"kouho, Space, Space: a {width}x{height} candidate window "
                f"{'below' if below else 'above'} the cursor {popup['cursor_rectangle']}, "
                f"at {popup['position']} in the window; fcitx5 was told "
                f"{popup['sent_rectangle']} (relative to its popup); Return took {text!r} "
                f"and the window went away")

    def focus_moves():
        spawn(f"{GUEST_BIN}/proto-test-client --socket {part.runtime}/{SOCKET} "
              f"--title other --duration 120", "ime-client.log")
        wait_for("the other window to take the focus", lambda: (lambda s: any(
            w["title"] == "other" and w["id"] == s["focus"]["window"]
            for w in s["windows"]))(state()), seconds=15)
        wait_for("the input method to be deactivated", lambda: not ime()["active"],
                 seconds=10)
        vm("key", "logo-h")
        wait_for("the focus back on foot and the input method active again",
                 lambda: ime()["active"], seconds=10)
        return ("a window without a text input took the focus: deactivated; logo+H "
                "(the script's key, handled before the grab) moved the focus back: active again")

    def crash():
        # As a crash: no chance to let go of anything.
        guest(_SIGNAL.format(pid=fcitx5["pid"], signal="KILL") + "true", check=False)
        wait_for("the input method to be gone", lambda: not ime()["bound"], seconds=15)
        status = ime()
        if status["keyboard_grab"]:
            raise Failed(f"the grab outlived the input method: {status}")
        text = typed_line(4, ["type:abc"], "abc", settle=0.3)
        if text != "abc":
            raise Failed(f"the terminal got {text!r}")
        return "fcitx5 killed (SIGKILL): no grab is left, and abc, Return reached the shell directly"

    def protocols():
        log = guest(f"cat {GUEST_DIR}/ime-fcitx5.log 2>/dev/null || true", check=False)
        with open(os.path.join(part.out, "fcitx5.log"), "w") as file:
            file.write(log)
        order = protocol_order(log)
        with open(os.path.join(part.out, "fcitx5-requests.txt"), "w") as file:
            file.write("\n".join(order) + "\n")
        wanted = ["zwp_input_method_manager_v2.get_input_method",
                  "zwp_input_method_v2.grab_keyboard",
                  "zwp_virtual_keyboard_manager_v1.create_virtual_keyboard",
                  "zwp_input_method_v2.get_input_popup_surface"]
        missing = [name for name in wanted if name not in order]
        if missing:
            raise Failed(f"fcitx5 never made {missing}; it made {order}")
        return "fcitx5's requests, first uses in order: " + ", ".join(
            name for name in order if name.startswith("zwp_"))

    try:
        if part.item("setup", setup):
            part.item("kana", kana)
            part.item("conversion", conversion)
            part.item("candidates", candidates)
            part.item("focus", focus_moves)
            part.item("crash", crash)
        part.item("protocols", protocols)
    finally:
        for name in ("foot", "client"):
            try:
                with open(os.path.join(part.out, f"{name}.log"), "w") as file:
                    file.write(guest(f"cat {GUEST_DIR}/ime-{name}.log 2>/dev/null || true",
                                     check=False))
            except Exception:
                pass
        part.keep_log("end")
        part.stop()


def check_real_hardware(out, uid, results):
    """The guard against taking over a real display by accident. Needs a VM
    that looks like real hardware: start it with
    HIZUKURI_VM_CPU=host,-hypervisor (the guest then has no `hypervisor` CPU
    flag, which is what the compositor goes by)."""
    part = Part("real-hardware", out, uid, results)
    if guest("grep -c -w hypervisor /proc/cpuinfo || true").strip() != "0":
        part.record("configured", "FAIL", "the guest has the hypervisor CPU flag; start the VM "
                                          "with HIZUKURI_VM_CPU=host,-hypervisor")
        return
    binary = f"{GUEST_BIN}/proto-compositor"
    device, output = ["--kms-device", "/dev/dri/card0"], ["--kms-output", "Virtual-1"]
    allow = ["--allow-real-hardware"]
    words = {"allow": "--allow-real-hardware", "device": "--kms-device", "output": "--kms-output"}

    def refusals():
        cases = [
            ("nothing", [], ["allow", "device", "output"]),
            ("allow only", allow, ["device", "output"]),
            ("device and output", device + output, ["allow"]),
            ("allow and device", allow + device, ["output"]),
            ("allow and output", allow + output, ["device"]),
        ]
        for label, given, missing in cases:
            status, log = run_exit(binary, "--backend", "kms", "--exit-after", "3", *given)
            last = log.strip().splitlines()[-1] if log.strip() else ""
            if status == 0 or "this is real hardware" not in last:
                raise Failed(f"with {label}: exit status {status}, said {last[-200:]!r}")
            for key, word in words.items():
                if (key in missing) != (word in last):
                    raise Failed(f"with {label}: the message is wrong about {word}: "
                                 f"{last[-300:]!r}")
            if "session opened" in log or "KMS output" in log:
                raise Failed(f"with {label}: the display was touched before refusing")
        status, log = run_exit(binary, "--backend", "auto", "--exit-after", "3")
        if status == 0 or "this is real hardware" not in log:
            raise Failed(f"--backend auto on a bare console started (exit status {status})")
        return (f"{len(cases)} incomplete combinations and `--backend auto` are refused, each "
                f"naming exactly what is missing, before the display is touched")

    def accepted():
        status, log = run_exit(binary, "--backend", "kms", "--exit-after", "3",
                               "--vulkan-device", "cpu", *allow, *device, *output)
        with open(os.path.join(part.out, "proto-compositor-accepted.log"), "w") as file:
            file.write(log)
        if status != 0:
            raise Failed(f"exit status {status}: {log.strip()[-200:]}")
        if "taking over a display on real hardware" not in log or "KMS output" not in log:
            raise Failed("it ran, but the log does not show the takeover")
        return "with all three it runs, and says so in the log"

    def selftest():
        report = f"{GUEST_DIR}/real-hardware-selftest"
        base = [binary, "kms-selftest", "--seconds", "4", "--report", report,
                "--vulkan-device", "cpu", "--scanout", "cpu"]
        status, log = run_exit(*base, "--output", "Virtual-1", *allow)
        if status == 0 or "this is real hardware" not in log or "--kms-device" not in log:
            raise Failed(f"the self test without --device: exit status {status}")
        guest(f"rm -rf {report}")
        status, log = run_exit(*base, "--device", "/dev/dri/card0", "--output", "Virtual-1",
                               *allow)
        summary = guest(f"cat {report}/summary.txt", check=False)
        with open(os.path.join(part.out, "selftest-summary.txt"), "w") as file:
            file.write(summary)
        if status != 0 or "kms-selftest: PASSED" not in summary:
            raise Failed(f"the self test with everything named: exit status {status}, "
                         f"{summary.strip()[:120]!r}")
        return "refused without --device; with --device, --output and the permission it passes"

    part.stop()
    part.item("refusals", refusals)
    part.item("accepted", accepted)
    part.item("selftest", selftest)
    part.stop()


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--out", help="directory for results and evidence")
    parser.add_argument("--only", default=",".join(DEFAULT_PARTS),
                        help="comma separated parts to check: lavapipe, script, vkms, safety, "
                             "outputs, ime (the default: all of these), real-hardware (needs "
                             "HIZUKURI_VM_CPU=host,-hypervisor)")
    parser.add_argument("--skip-build", action="store_true",
                        help="send the binaries that are already built")
    args = parser.parse_args()
    names = [n for n in args.only.split(",") if n]
    for name in names:
        if name not in PATHS and name not in PARTS:
            parser.error(f"unknown path {name!r}")
    out = os.path.abspath(args.out or os.path.join(
        ROOT, "artifacts", "vm-check", time.strftime("%Y%m%d-%H%M%S")))
    os.makedirs(out, exist_ok=True)

    target = os.path.join(os.environ.get("CARGO_TARGET_DIR", os.path.join(ROOT, "target")),
                          "debug")
    if not args.skip_build:
        subprocess.run(
            ["cargo", "build", "-j", os.environ.get("CARGO_BUILD_JOBS", "8"),
             "-p", "proto-compositor", "-p", "proto-test-client"], cwd=ROOT, check=True)
    binaries = [os.path.join(target, name)
                for name in ("proto-compositor", "proto-ctl", "proto-test-client")]
    results = []
    try:
        guest(f"mkdir -p {GUEST_BIN} {GUEST_DIR} && rm -rf {DRAW_ONLY} && mkdir -p {DRAW_ONLY}")
        vm("push", *binaries, "bin/", timeout=300)
        vm("push", os.path.join(ROOT, "prototype", "compositor", "projects", "draw-only") + "/",
           f"{DRAW_ONLY}/")
        uid = int(guest("id -u").strip())
    except (Failed, ValueError) as error:
        print(f"FAIL setup: {error}")
        return 1
    try:
        versions = guest(f"uname -r; {GUEST_BIN}/proto-compositor --version; "
                         "cat /run/current-system/nixos-version; echo; "
                         "readlink /run/current-system; "
                         "vulkaninfo --summary 2>/dev/null | grep -E 'deviceName|driverInfo'")
    except Failed as error:
        # The guest's libraries are those of the development shell. A binary
        # that was linked against other ones does not find them.
        print(f"FAIL setup: the binaries do not start in the guest. Build them in the Nix "
              f"development shell (nix develop). {error}")
        return 1
    with open(os.path.join(out, "guest.txt"), "w") as file:
        file.write(versions)

    # The guest's DRM device as the planning code sees it. Read-only.
    try:
        info = guest(f"{GUEST_BIN}/proto-compositor kms-info --anonymize 2>/dev/null || "
                     f"sudo {GUEST_BIN}/proto-compositor kms-info --anonymize", check=False)
        with open(os.path.join(out, "kms-info.json"), "w") as file:
            file.write(info)
    except Failed:
        pass

    # The list a person reads to choose --kms-device and --kms-output.
    try:
        summary = guest(f"{GUEST_BIN}/proto-compositor kms-info --summary 2>&1 || "
                        f"sudo {GUEST_BIN}/proto-compositor kms-info --summary 2>&1", check=False)
        with open(os.path.join(out, "kms-summary.txt"), "w") as file:
            file.write(summary)
        lines = summary.splitlines()
        good = any(line.startswith("/dev/dri/card") and "virtio_gpu" in line for line in lines) \
            and any("Virtual-1" in line for line in lines)
        detail = " | ".join(line.strip() for line in lines[:4])
        status = "PASS" if good else "FAIL"
    except Failed as error:
        status, detail = "FAIL", str(error)
    print(f"{status} setup/kms-summary: {detail}", flush=True)
    results.append({"item": "setup/kms-summary", "status": status, "detail": detail})

    seconds = {}
    for name in names:
        began = time.monotonic()
        if name in PARTS:
            PARTS[name](out, uid, results)
        else:
            Run(name, out, uid, results).run()
        seconds[name] = round(time.monotonic() - began)
        print(f"TIME {name}: {seconds[name]} s", flush=True)

    counts = {s: sum(1 for r in results if r["status"] == s) for s in ("PASS", "FAIL", "SKIP")}
    summary = "\n".join(f"{r['status']} {r['item']}: {r['detail']}" for r in results)
    summary += "\n\n" + "\n".join(f"TIME {name}: {took} s" for name, took in seconds.items())
    verdict = (f"vm-check: {counts['PASS']} passed, {counts['FAIL']} failed, "
               f"{counts['SKIP']} skipped. Evidence: {out}")
    with open(os.path.join(out, "summary.txt"), "w") as file:
        file.write(summary + "\n\n" + verdict + "\n")
    with open(os.path.join(out, "results.json"), "w") as file:
        json.dump({"counts": counts, "items": results, "seconds": seconds,
                   "guest": versions}, file, indent=2)
    print(verdict)
    return 1 if counts["FAIL"] else 0


PARTS = {
    "script": check_script,
    "vkms": check_vkms,
    "safety": check_safety,
    "outputs": check_outputs,
    "ime": check_ime,
    "real-hardware": check_real_hardware,
}


if __name__ == "__main__":
    sys.exit(main())
