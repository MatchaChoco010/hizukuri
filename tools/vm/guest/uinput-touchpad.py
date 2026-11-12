#!/usr/bin/env python3
"""Runs inside the guest as root. Creates a virtual touchpad with uinput and
plays gestures on it, so that a compositor can be tested with input that QMP
cannot produce (multi-finger gestures, finger scrolling).

    sudo uinput-touchpad.py [swipe] [scroll] [pinch]

The device stays for 1.5 s before the first gesture so that the compositor has
time to notice it. Needs the Python libevdev module.
"""

import sys
import time

import libevdev
from libevdev import EV_ABS, EV_KEY, EV_SYN, InputAbsInfo, InputEvent

WIDTH, HEIGHT = 1200, 800  # device units; 12 units per mm gives a 100 x 67 mm pad
STEP_SECONDS = 0.012
FINGER_KEYS = {
    1: EV_KEY.BTN_TOOL_FINGER,
    2: EV_KEY.BTN_TOOL_DOUBLETAP,
    3: EV_KEY.BTN_TOOL_TRIPLETAP,
    4: EV_KEY.BTN_TOOL_QUADTAP,
}


def create_touchpad():
    device = libevdev.Device()
    device.name = "hizukuri test touchpad"
    device.id = {"bustype": 0x18, "vendor": 0x1234, "product": 0x5678, "version": 1}
    x = InputAbsInfo(minimum=0, maximum=WIDTH, resolution=12)
    y = InputAbsInfo(minimum=0, maximum=HEIGHT, resolution=12)
    device.enable(EV_ABS.ABS_X, x)
    device.enable(EV_ABS.ABS_Y, y)
    device.enable(EV_ABS.ABS_MT_SLOT, InputAbsInfo(minimum=0, maximum=4))
    device.enable(EV_ABS.ABS_MT_TRACKING_ID, InputAbsInfo(minimum=0, maximum=65535))
    device.enable(EV_ABS.ABS_MT_POSITION_X, x)
    device.enable(EV_ABS.ABS_MT_POSITION_Y, y)
    for key in (EV_KEY.BTN_LEFT, EV_KEY.BTN_TOUCH, *FINGER_KEYS.values()):
        device.enable(key)
    device.enable(libevdev.INPUT_PROP_POINTER)
    device.enable(libevdev.INPUT_PROP_BUTTONPAD)
    return device.create_uinput_device()


def play(pad, starts, deltas, steps=30):
    """Puts one finger at each start point, moves finger i by deltas[i] in total."""
    count = len(starts)
    tracking_base = play.next_tracking_id
    play.next_tracking_id += count

    def frame(positions, first):
        events = []
        for slot, (px, py) in enumerate(positions):
            events.append(InputEvent(EV_ABS.ABS_MT_SLOT, slot))
            if first:
                events.append(InputEvent(EV_ABS.ABS_MT_TRACKING_ID, tracking_base + slot))
            events.append(InputEvent(EV_ABS.ABS_MT_POSITION_X, int(px)))
            events.append(InputEvent(EV_ABS.ABS_MT_POSITION_Y, int(py)))
        events.append(InputEvent(EV_ABS.ABS_X, int(positions[0][0])))
        events.append(InputEvent(EV_ABS.ABS_Y, int(positions[0][1])))
        if first:
            events.append(InputEvent(EV_KEY.BTN_TOUCH, 1))
            events.append(InputEvent(FINGER_KEYS[count], 1))
        events.append(InputEvent(EV_SYN.SYN_REPORT, 0))
        pad.send_events(events)

    frame(starts, True)
    for step in range(1, steps + 1):
        time.sleep(STEP_SECONDS)
        fraction = step / steps
        frame(
            [(sx + dx * fraction, sy + dy * fraction) for (sx, sy), (dx, dy) in zip(starts, deltas)],
            False,
        )
    time.sleep(STEP_SECONDS)
    events = []
    for slot in range(count):
        events.append(InputEvent(EV_ABS.ABS_MT_SLOT, slot))
        events.append(InputEvent(EV_ABS.ABS_MT_TRACKING_ID, -1))
    events.append(InputEvent(EV_KEY.BTN_TOUCH, 0))
    events.append(InputEvent(FINGER_KEYS[count], 0))
    events.append(InputEvent(EV_SYN.SYN_REPORT, 0))
    pad.send_events(events)
    time.sleep(0.3)


play.next_tracking_id = 1

GESTURES = {
    # Three fingers moving right together: a swipe.
    "swipe": ([(300, 400), (500, 380), (700, 400)], [(350, 0)] * 3),
    # Two fingers moving down together: finger scrolling.
    "scroll": ([(500, 250), (650, 250)], [(0, 300)] * 2),
    # Two fingers moving apart: a pinch.
    "pinch": ([(550, 400), (650, 400)], [(-250, 0), (250, 0)]),
}


def main():
    names = sys.argv[1:] or ["swipe", "scroll", "pinch"]
    for name in names:
        if name not in GESTURES:
            raise SystemExit(f"unknown gesture {name!r}; choose from {', '.join(GESTURES)}")
    pad = create_touchpad()
    print(f"created {pad.devnode}", flush=True)
    time.sleep(1.5)
    for name in names:
        starts, deltas = GESTURES[name]
        play(pad, starts, deltas)
        print(f"played {name}", flush=True)
    time.sleep(0.5)


if __name__ == "__main__":
    main()
