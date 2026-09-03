#!/usr/bin/env python3
"""Stream FreeMDU logs over Wi-Fi, similar to `esphome logs`."""

from __future__ import annotations

import argparse
import socket
import sys
import time

from local_secrets import load_env_secret


def stream(host: str, port: int, token: str, timeout: float) -> None:
    with socket.create_connection((host, port), timeout=timeout) as sock:
        sock.settimeout(None)
        sock.sendall(f"FMDULOG1 {token}\n".encode("ascii"))

        reader = sock.makefile("rb", buffering=0)
        hello = reader.readline()
        if not hello:
            raise RuntimeError("connection closed during authentication")

        hello_text = hello.decode("utf-8", errors="replace").rstrip()
        if not hello_text.startswith("OK "):
            raise RuntimeError(f"device rejected log connection: {hello_text}")

        print(hello_text, file=sys.stderr, flush=True)

        while True:
            line = reader.readline()
            if not line:
                raise RuntimeError("device disconnected")
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()


def main() -> int:
    parser = argparse.ArgumentParser(description="Stream FreeMDU live logs over Wi-Fi")
    parser.add_argument("host", help="FreeMDU IP address or hostname")
    parser.add_argument("--port", type=int, default=3233)
    parser.add_argument("--token", help="override OTA_TOKEN from .cargo/secrets.toml")
    parser.add_argument("--reconnect", action="store_true", help="reconnect after OTA/reboots")
    parser.add_argument("--retry-delay", type=float, default=1.0)
    parser.add_argument("--timeout", type=float, default=10.0)
    args = parser.parse_args()

    try:
        token = args.token or load_env_secret("OTA_TOKEN")
    except RuntimeError as exc:
        parser.error(str(exc))

    while True:
        try:
            stream(args.host, args.port, token, args.timeout)
        except (OSError, RuntimeError) as exc:
            if not args.reconnect:
                print(f"error: {exc}", file=sys.stderr)
                return 1
            print(f"logs disconnected: {exc}; retrying...", file=sys.stderr, flush=True)
            time.sleep(max(args.retry_delay, 0.1))


if __name__ == "__main__":
    raise SystemExit(main())
