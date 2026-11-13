#!/usr/bin/env python3
"""Runs inside the guest. An X11 client for checking X11 applications through
xwayland-satellite: it shows windows filled with one color each and prints
what it receives, one JSON object per line on stdout.

    DISPLAY=:N x11-test-client.py [--title T] [--class C] [--size WxH]
                                  [--color RRGGBB] [--commands FIFO]

The window is filled by the X server (its background pixel), so what is on
screen does not depend on fonts or a toolkit. Commands are read from the
named pipe given with --commands, one per line:

    popup X Y W H RRGGBB     an override-redirect window (a menu, a tooltip)
                             at X, Y relative to the main window
    close-popup
    dialog W H RRGGBB        a window that is transient for the main window
    close-dialog
    fullscreen on|off        _NET_WM_STATE_FULLSCREEN, asked of the window
                             manager as EWMH says
    resize W H               the main window asks for another size
    copy TEXT                own CLIPBOARD and offer TEXT as UTF8_STRING
    paste                    read CLIPBOARD as UTF8_STRING
    select TEXT, paste-primary   the same with PRIMARY (what is selected,
                             pasted with the middle button)
    quit

Events: ready, configure, map, unmap, focus_in, focus_out, key, button,
motion (first one per window only), enter, leave, popup, dialog, owned,
lost, sent, pasted, paste_failed, error. The selection events name the
selection (`clipboard` or `primary`). Coordinates of key and button
events are relative to the window they happened in.

Needs python-xlib.
"""

import argparse
import json
import os
import select
import sys

from Xlib import X, Xatom, display, error
from Xlib.protocol import event as xevent

SELECTIONS = {"clipboard": "CLIPBOARD", "primary": "PRIMARY"}


def emit(event, **fields):
    print(json.dumps({"event": event, **fields}), flush=True)


class Client:
    def __init__(self, args):
        self.display = display.Display()
        self.screen = self.display.screen()
        self.root = self.screen.root
        self.atoms = {}
        width, height = (int(v) for v in args.size.split("x"))
        self.window = self.create(width, height, int(args.color, 16))
        self.names = {self.window.id: "main"}
        self.window.set_wm_name(args.title)
        self.window.change_property(self.atom("_NET_WM_NAME"), self.atom("UTF8_STRING"), 8,
                                    args.title.encode())
        self.window.set_wm_class(args.wm_class, args.wm_class)
        self.window.set_wm_protocols([self.atom("WM_DELETE_WINDOW")])
        self.window.map()
        self.popup = None
        self.dialog = None
        # What this client offers, by selection (clipboard, primary).
        self.offered = {}
        # An unmapped window that receives what is pasted.
        self.paste_target = self.root.create_window(0, 0, 1, 1, 0, X.CopyFromParent)
        self.moved = set()
        self.display.flush()

    def atom(self, name):
        if name not in self.atoms:
            self.atoms[name] = self.display.intern_atom(name)
        return self.atoms[name]

    def create(self, width, height, color, override_redirect=False):
        return self.root.create_window(
            0, 0, width, height, 0, self.screen.root_depth, X.InputOutput, X.CopyFromParent,
            background_pixel=color, override_redirect=override_redirect,
            event_mask=(X.ExposureMask | X.StructureNotifyMask | X.FocusChangeMask
                        | X.KeyPressMask | X.KeyReleaseMask | X.ButtonPressMask
                        | X.ButtonReleaseMask | X.PointerMotionMask | X.EnterWindowMask
                        | X.LeaveWindowMask | X.PropertyChangeMask))

    # -- commands ---------------------------------------------------------------

    def command(self, line):
        words = line.split()
        if not words:
            return True
        name, rest = words[0], words[1:]
        if name == "popup":
            x, y, width, height = (int(v) for v in rest[:4])
            self.close("popup")
            origin = self.window.translate_coords(self.root, 0, 0)
            # translate_coords gives the root's origin in the window's
            # coordinates; the window's position on the root is its negation.
            root_x, root_y = -origin.x + x, -origin.y + y
            self.popup = self.create(width, height, int(rest[4], 16), override_redirect=True)
            self.names[self.popup.id] = "popup"
            self.popup.configure(x=root_x, y=root_y)
            self.popup.map()
            emit("popup", x=root_x, y=root_y, width=width, height=height)
        elif name == "close-popup":
            self.close("popup")
        elif name == "dialog":
            width, height = (int(v) for v in rest[:2])
            self.close("dialog")
            self.dialog = self.create(width, height, int(rest[2], 16))
            self.names[self.dialog.id] = "dialog"
            self.dialog.set_wm_name("x11-test-client dialog")
            self.dialog.set_wm_transient_for(self.window)
            self.dialog.change_property(self.atom("_NET_WM_WINDOW_TYPE"), Xatom.ATOM, 32,
                                        [self.atom("_NET_WM_WINDOW_TYPE_DIALOG")])
            self.dialog.map()
            emit("dialog", width=width, height=height)
        elif name == "close-dialog":
            self.close("dialog")
        elif name == "fullscreen":
            # _NET_WM_STATE: 1 adds, 0 removes.
            action = 1 if rest[:1] == ["on"] else 0
            self.root.send_event(
                xevent.ClientMessage(
                    window=self.window, client_type=self.atom("_NET_WM_STATE"),
                    data=(32, [action, self.atom("_NET_WM_STATE_FULLSCREEN"), 0, 1, 0])),
                event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask)
        elif name == "resize":
            self.window.configure(width=int(rest[0]), height=int(rest[1]))
        elif name in ("copy", "select"):
            which = "clipboard" if name == "copy" else "primary"
            self.offered[which] = line.split(" ", 1)[1].encode() if len(words) > 1 else b""
            atom = self.atom(SELECTIONS[which])
            self.window.set_selection_owner(atom, X.CurrentTime)
            owner = self.display.get_selection_owner(atom)
            emit("owned" if owner == self.window else "error", selection=which,
                 owner=getattr(owner, "id", 0))
        elif name in ("paste", "paste-primary"):
            which = "clipboard" if name == "paste" else "primary"
            self.paste_target.convert_selection(self.atom(SELECTIONS[which]),
                                                self.atom("UTF8_STRING"),
                                                self.atom("HZK_PASTE"), X.CurrentTime)
        elif name == "quit":
            return False
        else:
            emit("error", what=f"unknown command {name!r}")
        self.display.flush()
        return True

    def close(self, which):
        window = getattr(self, which)
        if window is not None:
            window.destroy()
            setattr(self, which, None)

    # -- events -----------------------------------------------------------------

    def name_of(self, window):
        return self.names.get(getattr(window, "id", window), "other")

    def handle(self, event):
        kind = event.type
        if kind == X.ConfigureNotify:
            emit("configure", window=self.name_of(event.window), x=event.x, y=event.y,
                 width=event.width, height=event.height)
        elif kind == X.MapNotify:
            emit("map", window=self.name_of(event.window))
        elif kind == X.UnmapNotify:
            emit("unmap", window=self.name_of(event.window))
        elif kind == X.FocusIn:
            emit("focus_in", window=self.name_of(event.window))
        elif kind == X.FocusOut:
            emit("focus_out", window=self.name_of(event.window))
        elif kind in (X.KeyPress, X.KeyRelease):
            keysym = self.display.keycode_to_keysym(event.detail, 0)
            emit("key", window=self.name_of(event.window), keycode=event.detail,
                 pressed=kind == X.KeyPress, keysym=self.display.lookup_string(keysym) or
                 str(keysym))
        elif kind in (X.ButtonPress, X.ButtonRelease):
            emit("button", window=self.name_of(event.window), button=event.detail,
                 pressed=kind == X.ButtonPress, x=event.event_x, y=event.event_y,
                 root_x=event.root_x, root_y=event.root_y)
        elif kind == X.MotionNotify:
            name = self.name_of(event.window)
            if name not in self.moved:
                self.moved.add(name)
                emit("motion", window=name, x=event.event_x, y=event.event_y,
                     root_x=event.root_x, root_y=event.root_y)
        elif kind == X.EnterNotify:
            emit("enter", window=self.name_of(event.window), x=event.event_x, y=event.event_y)
        elif kind == X.LeaveNotify:
            emit("leave", window=self.name_of(event.window))
        elif kind == X.ClientMessage:
            if event.client_type == self.atom("WM_PROTOCOLS"):
                emit("close_requested", window=self.name_of(event.window))
        elif kind == X.SelectionRequest:
            self.send_selection(event)
        elif kind == X.SelectionClear:
            which = self.selection_name(event.atom)
            self.offered.pop(which, None)
            emit("lost", selection=which)
        elif kind == X.SelectionNotify:
            which = self.selection_name(event.selection)
            if event.property == X.NONE:
                emit("paste_failed", selection=which)
            else:
                data = self.paste_target.get_full_property(event.property, X.AnyPropertyType)
                text = data.value.decode() if data and isinstance(data.value, bytes) else (
                    bytes(data.value).decode() if data else "")
                emit("pasted", selection=which, text=text)

    def selection_name(self, atom):
        for which, name in SELECTIONS.items():
            if atom == self.atom(name):
                return which
        return "other"

    def send_selection(self, request):
        prop = request.property if request.property != X.NONE else request.target
        which = self.selection_name(request.selection)
        text = self.offered.get(which)
        if text is not None and request.target in (
                self.atom("UTF8_STRING"), Xatom.STRING, self.atom("TEXT"),
                self.atom("text/plain;charset=utf-8"), self.atom("text/plain")):
            request.requestor.change_property(prop, request.target, 8, text)
            emit("sent", selection=which, target=self.display.get_atom_name(request.target))
        elif text is not None and request.target == self.atom("TARGETS"):
            targets = [self.atom("TARGETS"), self.atom("UTF8_STRING"), Xatom.STRING,
                       self.atom("text/plain;charset=utf-8")]
            request.requestor.change_property(prop, Xatom.ATOM, 32, targets)
        else:
            prop = X.NONE
        request.requestor.send_event(xevent.SelectionNotify(
            time=request.time, requestor=request.requestor, selection=request.selection,
            target=request.target, property=prop))
        self.display.flush()


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--title", default="x11-test-client")
    parser.add_argument("--class", dest="wm_class", default="hizukuri.x11-test-client")
    parser.add_argument("--size", default="300x200")
    parser.add_argument("--color", default="c86432")
    parser.add_argument("--commands", help="a named pipe to read commands from")
    args = parser.parse_args()

    try:
        client = Client(args)
    except (error.DisplayError, error.ConnectionClosedError) as failure:
        emit("error", what=f"cannot connect: {failure}")
        return 1
    commands = None
    if args.commands:
        # Opened for writing too, so that a writer closing it is not an end
        # of file.
        commands = os.open(args.commands, os.O_RDWR | os.O_NONBLOCK)
    emit("ready", window=client.window.id)
    pending = b""
    x_fd = client.display.fileno()
    while True:
        try:
            while client.display.pending_events():
                client.handle(client.display.next_event())
        except error.ConnectionClosedError:
            emit("error", what="the X server closed the connection")
            return 1
        watched = [x_fd] + ([commands] if commands is not None else [])
        # What arrived on the X connection is read by pending_events above.
        readable, _, _ = select.select(watched, [], [], 1.0)
        if commands is not None and commands in readable:
            pending += os.read(commands, 4096)
            while b"\n" in pending:
                line, pending = pending.split(b"\n", 1)
                if not client.command(line.decode().strip()):
                    return 0


if __name__ == "__main__":
    sys.exit(main())
