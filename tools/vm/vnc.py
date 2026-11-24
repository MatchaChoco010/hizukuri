#!/usr/bin/env python3
"""Grab the guest framebuffer from QEMU's VNC server on a Unix socket.

QMP `screendump` cannot capture GL scanouts, so the frame is read over RFB
instead. Only the Python standard library is used. Called through
`tools/vm/vm screendump` and `tools/vm/vm pixels`.
"""

import argparse
import socket
import struct
import sys
import zlib


class VncError(Exception):
    pass


def read_exact(sock, count):
    chunks = []
    while count > 0:
        chunk = sock.recv(min(count, 1 << 20))
        if not chunk:
            raise VncError("VNC connection closed")
        chunks.append(chunk)
        count -= len(chunk)
    return b"".join(chunks)


def grab(path, timeout=10.0):
    """Return (width, height, rgb_bytes) of the current guest frame."""
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(timeout)
    sock.connect(path)
    try:
        version = read_exact(sock, 12)
        if not version.startswith(b"RFB 003."):
            raise VncError(f"unexpected RFB version: {version!r}")
        sock.sendall(b"RFB 003.008\n")

        count = read_exact(sock, 1)[0]
        if count == 0:
            length = struct.unpack(">I", read_exact(sock, 4))[0]
            raise VncError(read_exact(sock, length).decode(errors="replace"))
        types = read_exact(sock, count)
        if 1 not in types:
            raise VncError(f"the VNC server requires authentication (types {list(types)})")
        sock.sendall(b"\x01")  # security type: None
        if struct.unpack(">I", read_exact(sock, 4))[0] != 0:
            raise VncError("VNC security handshake failed")

        sock.sendall(b"\x01")  # ClientInit: shared, do not disconnect others
        width, height = struct.unpack(">HH", read_exact(sock, 4))
        read_exact(sock, 16)  # server pixel format; replaced below
        name_length = struct.unpack(">I", read_exact(sock, 4))[0]
        read_exact(sock, name_length)

        # SetPixelFormat: 32 bpp, depth 24, little endian, true colour,
        # 8 bits per channel, red at bit 16, green at 8, blue at 0 (BGRX bytes).
        pixel_format = struct.pack(">BBBBHHHBBBxxx", 32, 24, 0, 1, 255, 255, 255, 16, 8, 0)
        sock.sendall(struct.pack(">Bxxx", 0) + pixel_format)
        # SetEncodings: raw only.
        sock.sendall(struct.pack(">BxHi", 2, 1, 0))
        # FramebufferUpdateRequest: not incremental, whole screen.
        sock.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, width, height))

        frame = bytearray(width * height * 4)
        remaining = width * height
        while remaining > 0:
            message = read_exact(sock, 1)[0]
            if message == 0:  # FramebufferUpdate
                rects = struct.unpack(">xH", read_exact(sock, 3))[0]
                for _ in range(rects):
                    x, y, w, h, encoding = struct.unpack(">HHHHi", read_exact(sock, 12))
                    if encoding != 0:
                        raise VncError(f"unexpected encoding {encoding}")
                    data = read_exact(sock, w * h * 4)
                    for row in range(h):
                        start = ((y + row) * width + x) * 4
                        frame[start : start + w * 4] = data[row * w * 4 : (row + 1) * w * 4]
                    remaining -= w * h
            elif message == 1:  # SetColourMapEntries
                colours = struct.unpack(">xHH", read_exact(sock, 5))[1]
                read_exact(sock, colours * 6)
            elif message == 2:  # Bell
                pass
            elif message == 3:  # ServerCutText
                length = struct.unpack(">xxxI", read_exact(sock, 7))[0]
                read_exact(sock, length)
            else:
                raise VncError(f"unexpected server message {message}")
    finally:
        sock.close()

    rgb = bytearray(width * height * 3)
    rgb[0::3] = frame[2::4]
    rgb[1::3] = frame[1::4]
    rgb[2::3] = frame[0::4]
    return width, height, bytes(rgb)


def resize(path, width, height, timeout=10.0):
    """Asks the server for a desktop of the given size (RFB SetDesktopSize).

    QEMU passes the request to the display device as "the user wants this
    size". For virtio-gpu that connects the output with that size as its
    preferred mode; 0x0 disconnects it. The guest sees a hotplug event.
    Returns the status the server answers with (0 accepted, 4 forwarded)."""
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(timeout)
    sock.connect(path)
    try:
        version = read_exact(sock, 12)
        if not version.startswith(b"RFB 003."):
            raise VncError(f"unexpected RFB version: {version!r}")
        sock.sendall(b"RFB 003.008\n")
        count = read_exact(sock, 1)[0]
        types = read_exact(sock, count)
        if 1 not in types:
            raise VncError("the VNC server requires authentication")
        sock.sendall(b"\x01")
        if struct.unpack(">I", read_exact(sock, 4))[0] != 0:
            raise VncError("VNC security handshake failed")
        sock.sendall(b"\x01")
        current = struct.unpack(">HH", read_exact(sock, 4))
        read_exact(sock, 16)
        read_exact(sock, struct.unpack(">I", read_exact(sock, 4))[0])

        # Raw, plus the ExtendedDesktopSize pseudo-encoding (-308), without
        # which the server ignores SetDesktopSize.
        sock.sendall(struct.pack(">BxHii", 2, 2, 0, -308))
        # SetDesktopSize: one screen covering the whole desktop.
        sock.sendall(struct.pack(">BxHHBx", 251, width, height, 1)
                     + struct.pack(">IHHHHI", 0, 0, 0, width, height, 0))
        # The answer is a FramebufferUpdate with an ExtendedDesktopSize
        # rectangle whose x is 1 (reply to this client) and y the status.
        sock.sendall(struct.pack(">BBHHHH", 3, 1, 0, 0, max(current[0], 1), max(current[1], 1)))
        while True:
            message = read_exact(sock, 1)[0]
            if message != 0:
                raise VncError(f"unexpected server message {message}")
            rects = struct.unpack(">xH", read_exact(sock, 3))[0]
            for _ in range(rects):
                x, y, w, h, encoding = struct.unpack(">HHHHi", read_exact(sock, 12))
                if encoding == 0:
                    read_exact(sock, w * h * bytes_per_pixel(sock))
                elif encoding == -308:
                    screens = read_exact(sock, 4)[0]
                    read_exact(sock, screens * 16)
                    if x == 1:
                        return y
                else:
                    raise VncError(f"unexpected encoding {encoding}")
    finally:
        sock.close()


def bytes_per_pixel(_sock):
    # QEMU's default pixel format for a new client is 32 bits per pixel; the
    # resize exchange does not change it.
    return 4


def write_png(path, width, height, rgb):
    def chunk(kind, payload):
        body = kind + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body))

    stride = width * 3
    raw = bytearray()
    for row in range(height):
        raw.append(0)  # filter type: None
        raw += rgb[row * stride : (row + 1) * stride]
    with open(path, "wb") as file:
        file.write(b"\x89PNG\r\n\x1a\n")
        file.write(chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)))
        file.write(chunk(b"IDAT", zlib.compress(bytes(raw), 6)))
        file.write(chunk(b"IEND", b""))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("shot", help="write the frame as a PNG file")
    p.add_argument("file")

    p = sub.add_parser("pixels", help="print the RGB value at each X,Y")
    p.add_argument("points", nargs="+", help="X,Y")

    sub.add_parser("size", help="print WIDTHxHEIGHT")

    p = sub.add_parser("resize", help="ask for a desktop size (connects an output; 0x0 disconnects)")
    p.add_argument("size", help="WIDTHxHEIGHT")

    args = parser.parse_args()
    try:
        if args.command == "resize":
            width, height = (int(value) for value in args.size.lower().split("x"))
            status = resize(args.socket, width, height)
            # QEMU answers 4, "request forwarded": it hands the size to the
            # display device and does not wait for the guest.
            if status not in (0, 4):
                raise VncError(f"the server refused the size (status {status})")
            return 0
        width, height, rgb = grab(args.socket)
        if args.command == "shot":
            write_png(args.file, width, height, rgb)
        elif args.command == "size":
            print(f"{width}x{height}")
        else:
            for point in args.points:
                x, y = (int(value) for value in point.split(","))
                if not (0 <= x < width and 0 <= y < height):
                    raise VncError(f"{point} is outside the {width}x{height} frame")
                offset = (y * width + x) * 3
                r, g, b = rgb[offset : offset + 3]
                print(f"{x},{y}: {r} {g} {b}")
    except (VncError, OSError, ValueError) as error:
        print(f"vnc: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
