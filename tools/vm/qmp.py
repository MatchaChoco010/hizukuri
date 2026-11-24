#!/usr/bin/env python3
"""Minimal QMP client for the hizukuri verification VM.

Only the Python standard library is used. Called through `tools/vm/vm`.
"""

import argparse
import json
import os
import socket
import sys
import time

# QEMU reports absolute axes in the range [0, 0x7fff].
ABS_MAX = 0x7FFF

# Delay between a press and its release, and between consecutive keys. libinput
# drops nothing at this speed, and it stays well below key-repeat delays.
KEY_DELAY = 0.03

ALIASES = {
    "enter": "ret",
    "return": "ret",
    "escape": "esc",
    "space": "spc",
    "super": "meta_l",
    "win": "meta_l",
    "logo": "meta_l",
    "control": "ctrl",
    "del": "delete",
    "bksp": "backspace",
}

# ASCII character -> (qcode, needs_shift), for a US layout.
CHAR_KEYS = {
    " ": ("spc", False),
    "\n": ("ret", False),
    "\t": ("tab", False),
    "-": ("minus", False),
    "_": ("minus", True),
    "=": ("equal", False),
    "+": ("equal", True),
    "[": ("bracket_left", False),
    "{": ("bracket_left", True),
    "]": ("bracket_right", False),
    "}": ("bracket_right", True),
    "\\": ("backslash", False),
    "|": ("backslash", True),
    ";": ("semicolon", False),
    ":": ("semicolon", True),
    "'": ("apostrophe", False),
    '"': ("apostrophe", True),
    "`": ("grave_accent", False),
    "~": ("grave_accent", True),
    ",": ("comma", False),
    "<": ("comma", True),
    ".": ("dot", False),
    ">": ("dot", True),
    "/": ("slash", False),
    "?": ("slash", True),
    "!": ("1", True),
    "@": ("2", True),
    "#": ("3", True),
    "$": ("4", True),
    "%": ("5", True),
    "^": ("6", True),
    "&": ("7", True),
    "*": ("8", True),
    "(": ("9", True),
    ")": ("0", True),
}


class QmpError(Exception):
    pass


class Qmp:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(30)
        self.sock.connect(path)
        self.file = self.sock.makefile("rw", encoding="utf-8", newline="\n")
        greeting = self._read()
        if "QMP" not in greeting:
            raise QmpError(f"unexpected greeting: {greeting}")
        self.command("qmp_capabilities")

    def _read(self):
        line = self.file.readline()
        if not line:
            raise QmpError("QMP connection closed")
        return json.loads(line)

    def command(self, name, arguments=None):
        request = {"execute": name}
        if arguments is not None:
            request["arguments"] = arguments
        return self.raw(request)

    def raw(self, request):
        self.file.write(json.dumps(request) + "\n")
        self.file.flush()
        while True:
            reply = self._read()
            if "event" in reply:
                continue  # asynchronous events are not interesting here
            if "error" in reply:
                raise QmpError(f"{request.get('execute')}: {reply['error']}")
            return reply.get("return")

    def send_events(self, events):
        self.command("input-send-event", {"events": events})


def key_event(qcode, down):
    return {
        "type": "key",
        "data": {"down": down, "key": {"type": "qcode", "data": qcode}},
    }


def btn_event(button, down):
    return {"type": "btn", "data": {"down": down, "button": button}}


def normalize_key(name):
    lowered = name.lower()
    return ALIASES.get(lowered, lowered)


def press_combo(qmp, combo):
    """Press a combo such as `ctrl-alt-f2`: modifiers go down first, up last."""
    if combo == "-":
        keys = ["minus"]
    else:
        keys = [normalize_key(k) for k in combo.split("-")]
    for key in keys:
        qmp.send_events([key_event(key, True)])
        time.sleep(KEY_DELAY)
    for key in reversed(keys):
        qmp.send_events([key_event(key, False)])
        time.sleep(KEY_DELAY)


def cmd_key(qmp, args):
    for combo in args.keys:
        press_combo(qmp, combo)


def cmd_type(qmp, args):
    for char in args.text:
        if char in CHAR_KEYS:
            qcode, shift = CHAR_KEYS[char]
        elif char.isascii() and char.isalnum():
            qcode, shift = char.lower(), char.isupper()
        else:
            raise QmpError(f"cannot type character {char!r}")
        press_combo(qmp, f"shift-{qcode}" if shift else qcode)


def screen_size(qmp, args):
    if args.screen:
        width, height = args.screen.lower().split("x")
        return int(width), int(height)
    return (
        int(os.environ.get("HIZUKURI_VM_XRES", "1280")),
        int(os.environ.get("HIZUKURI_VM_YRES", "800")),
    )


def cmd_move(qmp, args):
    width, height = screen_size(qmp, args)
    x = round(args.x * ABS_MAX / max(width - 1, 1))
    y = round(args.y * ABS_MAX / max(height - 1, 1))
    qmp.send_events(
        [
            {"type": "abs", "data": {"axis": "x", "value": x}},
            {"type": "abs", "data": {"axis": "y", "value": y}},
        ]
    )


def cmd_rel(qmp, args):
    events = []
    if args.dx:
        events.append({"type": "rel", "data": {"axis": "x", "value": args.dx}})
    if args.dy:
        events.append({"type": "rel", "data": {"axis": "y", "value": args.dy}})
    if events:
        qmp.send_events(events)


def cmd_click(qmp, args):
    qmp.send_events([btn_event(args.button, True)])
    time.sleep(KEY_DELAY)
    qmp.send_events([btn_event(args.button, False)])


def cmd_button(qmp, args):
    qmp.send_events([btn_event(args.button, args.state == "down")])


def cmd_scroll(qmp, args):
    button = "wheel-up" if args.direction == "up" else "wheel-down"
    for _ in range(args.count):
        qmp.send_events([btn_event(button, True)])
        qmp.send_events([btn_event(button, False)])
        time.sleep(KEY_DELAY)


def cmd_screendump(qmp, args):
    arguments = {"filename": os.path.abspath(args.file), "format": "png"}
    if args.head is not None:
        # A head other than 0 needs the device id of the display device.
        arguments["device"] = args.device
        arguments["head"] = args.head
    qmp.command("screendump", arguments)


def cmd_raw(qmp, args):
    print(json.dumps(qmp.raw(json.loads(args.json)), indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("key")
    p.add_argument("keys", nargs="+", help="qcodes or combos like ctrl-alt-f2")
    p.set_defaults(func=cmd_key)

    p = sub.add_parser("type")
    p.add_argument("text")
    p.set_defaults(func=cmd_type)

    p = sub.add_parser("move")
    p.add_argument("x", type=int)
    p.add_argument("y", type=int)
    p.add_argument("--screen", help="guest mode as WxH (default: the configured size)")
    p.set_defaults(func=cmd_move)

    p = sub.add_parser("rel")
    p.add_argument("dx", type=int)
    p.add_argument("dy", type=int)
    p.set_defaults(func=cmd_rel)

    p = sub.add_parser("click")
    p.add_argument("button", nargs="?", default="left")
    p.set_defaults(func=cmd_click)

    p = sub.add_parser("button")
    p.add_argument("button")
    p.add_argument("state", choices=["down", "up"])
    p.set_defaults(func=cmd_button)

    p = sub.add_parser("scroll")
    p.add_argument("direction", choices=["up", "down"])
    p.add_argument("count", nargs="?", type=int, default=1)
    p.set_defaults(func=cmd_scroll)

    p = sub.add_parser("screendump")
    p.add_argument("file")
    p.add_argument("--head", type=int)
    p.add_argument("--device", default="video0")
    p.set_defaults(func=cmd_screendump)

    p = sub.add_parser("raw")
    p.add_argument("json")
    p.set_defaults(func=cmd_raw)

    args = parser.parse_args()
    try:
        qmp = Qmp(args.socket)
        args.func(qmp, args)
    except (QmpError, OSError, ValueError) as error:
        print(f"qmp: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
