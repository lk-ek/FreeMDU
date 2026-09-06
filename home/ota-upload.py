#!/usr/bin/env python3
"""Build and upload FreeMDU standalone firmware via the local FMDU1 OTA protocol."""

from __future__ import annotations

import argparse
import binascii
import socket
import subprocess
import sys
import time
from pathlib import Path

from local_config import load_config_value


TARGET = "riscv32imc-unknown-none-elf"
BIN = "standalone"
DEFAULT_PORT = 3232


def run(cmd: list[str]) -> None:
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, check=True)


def recv_line(sock: socket.socket, limit: int = 1024) -> bytes:
    data = bytearray()
    while len(data) < limit:
        chunk = sock.recv(1)
        if not chunk:
            raise RuntimeError("connection closed while waiting for response")
        data += chunk
        if chunk == b"\n":
            return bytes(data)
    raise RuntimeError("response line too long")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Build and upload FreeMDU standalone firmware over OTA."
    )
    parser.add_argument("host", help="FreeMDU device IP address or hostname")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument("--token", help="override OTA_TOKEN from .cargo/local.toml")
    parser.add_argument(
        "--no-build",
        action="store_true",
        help="reuse the existing release ELF instead of running cargo build",
    )
    parser.add_argument(
        "--image",
        type=Path,
        default=Path("target") / TARGET / "release" / "standalone-ota.bin",
        help="path for the generated OTA application image",
    )
    parser.add_argument("--timeout", type=float, default=30.0)
    parser.add_argument("--install-scan-partition", action="store_true",
                        help="after firmware reboot, install the additive scan partition table")
    parser.add_argument("--diag-port", type=int, default=3234)
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
    if any(c.isspace() for c in token):
        parser.error("OTA token must not contain whitespace")

    elf = Path("target") / TARGET / "release" / BIN

    if not args.no_build:
        run([
            "./cargo-local", "build",
            "--features", "esp32c3",
            "--target", TARGET,
            "--release",
            "--bin", BIN,
        ])

    if not elf.is_file():
        print(f"error: ELF not found: {elf}", file=sys.stderr)
        return 2

    args.image.parent.mkdir(parents=True, exist_ok=True)
    run([
        "espflash", "save-image",
        "--chip", "esp32c3",
        str(elf),
        str(args.image),
    ])

    firmware = args.image.read_bytes()
    if not firmware:
        print("error: generated firmware image is empty", file=sys.stderr)
        return 2

    crc = binascii.crc32(firmware) & 0xFFFFFFFF
    size = len(firmware)

    print(f"Image: {args.image}")
    print(f"Size:  {size:,} bytes")
    print(f"CRC32: {crc:08x}")
    print(f"Connecting to {args.host}:{args.port} ...", flush=True)

    with socket.create_connection((args.host, args.port), timeout=args.timeout) as sock:
        sock.settimeout(args.timeout)

        header = f"FMDU1 {size} {crc:08x} {token}\n".encode("ascii")
        sock.sendall(header)

        reply = recv_line(sock).decode("utf-8", errors="replace").strip()
        print(f"Device: {reply}")
        if reply != "READY":
            raise RuntimeError(f"device rejected OTA request: {reply}")

        sent = 0
        view = memoryview(firmware)
        next_report = 10

        while sent < size:
            end = min(sent + 16 * 1024, size)
            sock.sendall(view[sent:end])
            sent = end

            percent = sent * 100 // size
            if percent >= next_report or sent == size:
                print(f"Upload: {percent:3d}% ({sent:,}/{size:,})", flush=True)
                while next_report <= percent:
                    next_report += 10

        reply = recv_line(sock).decode("utf-8", errors="replace").strip()
        print(f"Device: {reply}")

        if not reply.startswith("OK "):
            raise RuntimeError(f"OTA failed: {reply}")

    print("OTA upload accepted and verified; device should reboot now.")
    if args.install_scan_partition:
        from diag import request
        print("Installing the additive scan partition after reboot. Keep ESP power stable.", flush=True)
        time.sleep(3)
        for attempt in range(30):
            try:
                status = request(args.host, args.diag_port, token, "scan-status")
                if "scan_version=3" not in status:
                    raise RuntimeError("partition migration requires the new firmware")
                # Do not stop an existing persistent job implicitly.
                if "state=running" in status:
                    raise RuntimeError("pause the running scan, then use diag.py partition-install")
                print(request(args.host, args.diag_port, token, "partition-install"))
                break
            except OSError:
                if attempt == 29:
                    raise
                time.sleep(2)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, subprocess.CalledProcessError, RuntimeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(1)
