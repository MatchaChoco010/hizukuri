#!/usr/bin/env python3
"""Runs inside the guest as root. Creates a virtual touchscreen with uinput
and plays touches on it, so that a compositor can be tested with input that
QMP cannot produce (several fingers, touches that stay down while they move).

    sudo uinput-touchscreen.py [--go FILE] STEP...

Steps, in order:

    down:SLOT:X:Y    a finger touches; X and Y are fractions of the screen
    move:SLOT:X:Y    the finger moves there
    up:SLOT          the finger is lifted
    frame            the events since the last frame happened together
    wait:SECONDS     a pause

The device speaks the kernel's multitouch protocol B (slots and tracking
ids) and says it is a direct touch device, which makes udev call it a
touchscreen. It stays for 1.5 s before the first step so that the
compositor has time to notice it, or, with --go, until FILE exists. It goes
away 0.5 s after the last step. Needs the Python libevdev module.
"""

import os
import sys
import time

import libevdev
from libevdev import EV_ABS, EV_KEY, EV_SYN, InputAbsInfo, InputEvent

RANGE = 4096  # device units on each axis; 16 per mm makes a 256 x 256 mm screen
SLOTS = 10


def create_touchscreen():
    device = libevdev.Device()
    device.name = "hizukuri test touchscreen"
    device.id = {"bustype": 0x18, "vendor": 0x1234, "product": 0x5679, "version": 1}
    axis = InputAbsInfo(minimum=0, maximum=RANGE - 1, resolution=16)
    device.enable(EV_ABS.ABS_X, axis)
    device.enable(EV_ABS.ABS_Y, axis)
    device.enable(EV_ABS.ABS_MT_SLOT, InputAbsInfo(minimum=0, maximum=SLOTS - 1))
    device.enable(EV_ABS.ABS_MT_TRACKING_ID, InputAbsInfo(minimum=0, maximum=65535))
    device.enable(EV_ABS.ABS_MT_POSITION_X, axis)
    device.enable(EV_ABS.ABS_MT_POSITION_Y, axis)
    device.enable(EV_KEY.BTN_TOUCH)
    device.enable(libevdev.INPUT_PROP_DIRECT)
    return device.create_uinput_device()


def units(fraction):
    return max(0, min(RANGE - 1, round(float(fraction) * RANGE)))


class Player:
    def __init__(self, screen):
        self.screen = screen
        self.events = []
        self.down = set()
        self.next_tracking_id = 1

    def slot(self, slot):
        self.events.append(InputEvent(EV_ABS.ABS_MT_SLOT, slot))

    def position(self, slot, x, y):
        self.events.append(InputEvent(EV_ABS.ABS_MT_POSITION_X, units(x)))
        self.events.append(InputEvent(EV_ABS.ABS_MT_POSITION_Y, units(y)))
        # The single touch axes follow the first finger, as on real screens.
        if slot == min(self.down):
            self.events.append(InputEvent(EV_ABS.ABS_X, units(x)))
            self.events.append(InputEvent(EV_ABS.ABS_Y, units(y)))

    def step(self, text):
        kind, *args = text.split(":")
        if kind == "down":
            slot, x, y = int(args[0]), args[1], args[2]
            if not self.down:
                self.events.append(InputEvent(EV_KEY.BTN_TOUCH, 1))
            self.down.add(slot)
            self.slot(slot)
            self.events.append(InputEvent(EV_ABS.ABS_MT_TRACKING_ID, self.next_tracking_id))
            self.next_tracking_id += 1
            self.position(slot, x, y)
        elif kind == "move":
            slot, x, y = int(args[0]), args[1], args[2]
            self.slot(slot)
            self.position(slot, x, y)
        elif kind == "up":
            slot = int(args[0])
            self.slot(slot)
            self.events.append(InputEvent(EV_ABS.ABS_MT_TRACKING_ID, -1))
            self.down.discard(slot)
            if not self.down:
                self.events.append(InputEvent(EV_KEY.BTN_TOUCH, 0))
        elif kind == "frame":
            self.events.append(InputEvent(EV_SYN.SYN_REPORT, 0))
            self.screen.send_events(self.events)
            self.events = []
            # Real screens report at about 100 Hz.
            time.sleep(0.01)
        elif kind == "wait":
            time.sleep(float(args[0]))
        else:
            raise SystemExit(f"unknown step {text!r}")


def main():
    args = sys.argv[1:]
    go = None
    if args[:1] == ["--go"]:
        go, args = args[1], args[2:]
    screen = create_touchscreen()
    print(f"created {screen.devnode}", flush=True)
    if go:
        deadline = time.monotonic() + 60
        while not os.path.exists(go):
            if time.monotonic() > deadline:
                raise SystemExit(f"{go} did not appear within 60 s")
            time.sleep(0.05)
    else:
        time.sleep(1.5)
    player = Player(screen)
    for text in args:
        player.step(text)
    print("played", flush=True)
    time.sleep(0.5)


if __name__ == "__main__":
    main()
