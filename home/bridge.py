#!/usr/bin/env python3
"""Expose the FreeMDU Wi-Fi optical bridge as a local pseudo-serial port."""

from __future__ import annotations

import argparse
import os
import pty
import select
import socket
import sys
import termios
import tty

from local_config import load_config_value


def authenticate(sock: socket.socket, token: str) -> None:
    sock.sendall(f"FMDUBRIDGE1 {token}\n".encode("ascii"))
    reader = sock.makefile("rb", buffering=0)
    reply = reader.readline()
    if not reply:
        raise RuntimeError("bridge closed during authentication")

    text = reply.decode("utf-8", errors="replace").rstrip()
    if not text.startswith("OK "):
        raise RuntimeError(text)

    print(text, file=sys.stderr, flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Expose FreeMDU's authenticated Wi-Fi optical bridge as a PTY"
    )
    parser.add_argument("host")
    parser.add_argument("--port", type=int, default=3235)
    parser.add_argument("--token", help="override OTA_TOKEN from .cargo/local.toml")
    args = parser.parse_args()

    try:
        token = args.token or load_config_value("OTA_TOKEN")
    except RuntimeError as exc:
        parser.error(str(exc))

    if not token or token == "change-me":
        parser.error(
            "OTA_TOKEN is unset/default; define it in .cargo/local.toml "
            "or pass --token"
        )

    master_fd, slave_fd = pty.openpty()

    # Present a transparent 8-bit PTY. The actual 2400-8E1 UART settings live
    # on the ESP and are intentionally not controlled by the host application.
    tty.setraw(master_fd)
    attrs = termios.tcgetattr(slave_fd)
    attrs[0] = 0
    attrs[1] = 0
    attrs[3] = 0
    termios.tcsetattr(slave_fd, termios.TCSANOW, attrs)

    slave_name = os.ttyname(slave_fd)

    with socket.create_connection((args.host, args.port), timeout=10) as sock:
        authenticate(sock, token)
        sock.settimeout(None)

        print(f"PTY: {slave_name}", file=sys.stderr, flush=True)
        print(
            "Keep this process running and point the FreeMDU TUI/protocol tool "
            "at the PTY above.",
            file=sys.stderr,
            flush=True,
        )

        while True:
            readable, _, _ = select.select([master_fd, sock], [], [])

            if master_fd in readable:
                data = os.read(master_fd, 4096)
                if not data:
                    break
                sock.sendall(data)

            if sock in readable:
                data = sock.recv(4096)
                if not data:
                    break
                os.write(master_fd, data)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
