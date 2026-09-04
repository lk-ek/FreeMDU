#!/usr/bin/env python3
"""Read-only FreeMDU Miele diagnostics and binary dumps over Wi-Fi."""

from __future__ import annotations

import argparse
from pathlib import Path
import socket
import sys
import time

from local_config import load_config_value


CHUNK_SIZE = 0x10
CONNECT_RETRIES = 20
CONNECT_RETRY_DELAY = 0.1


def number16(value: str) -> str:
    try:
        parsed = int(value, 0)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid number: {value}") from exc
    if not 0 <= parsed <= 0xFFFF:
        raise argparse.ArgumentTypeError("value must fit in 16 bits")
    return value


def number32(value: str) -> str:
    try:
        parsed = int(value, 0)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid number: {value}") from exc
    if not 0 <= parsed <= 0xFFFF_FFFF:
        raise argparse.ArgumentTypeError("value must fit in 32 bits")
    return value


def connect_with_retry(host: str, port: int) -> socket.socket:
    last_error: OSError | None = None

    for attempt in range(CONNECT_RETRIES):
        try:
            return socket.create_connection((host, port), timeout=10)
        except ConnectionRefusedError as exc:
            last_error = exc
            if attempt + 1 == CONNECT_RETRIES:
                break
            time.sleep(CONNECT_RETRY_DELAY)

    assert last_error is not None
    raise last_error


def request(host: str, port: int, token: str, *parts: str) -> str:
    wire = ["FMDUDIAG1", token, *parts]

    with connect_with_retry(host, port) as sock:
        sock.settimeout(None)
        sock.sendall((" ".join(wire) + "\n").encode("ascii"))

        reader = sock.makefile("rb", buffering=0)
        reply = reader.readline()
        if not reply:
            raise RuntimeError("device disconnected without a diagnostic response")

    text = reply.decode("utf-8", errors="replace").rstrip()
    if not text.startswith("OK "):
        raise RuntimeError(text)
    return text


def read_block(host: str, port: int, token: str, kind: str, key: int, address: int) -> bytes:
    command = "eeprom16" if kind == "eeprom" else "mem16"
    width = 4 if kind == "eeprom" else 8
    reply = request(
        host,
        port,
        token,
        command,
        f"0x{key:04x}",
        f"0x{address:0{width}x}",
    )

    marker = " data="
    if marker not in reply:
        raise RuntimeError(f"malformed response: {reply}")

    try:
        data = bytes.fromhex(reply.split(marker, 1)[1])
    except ValueError as exc:
        raise RuntimeError(f"invalid hex payload: {reply}") from exc

    if len(data) != CHUNK_SIZE:
        raise RuntimeError(f"expected {CHUNK_SIZE} bytes, got {len(data)}")
    return data


def dump_range(
    host: str,
    port: int,
    token: str,
    kind: str,
    key: int,
    start: int,
    end: int,
    output: Path,
) -> None:
    if start > end:
        raise RuntimeError("start must not be greater than end")
    if start % CHUNK_SIZE:
        raise RuntimeError(f"start must be aligned to 0x{CHUNK_SIZE:x}")
    if (end + 1) % CHUNK_SIZE:
        raise RuntimeError(f"end + 1 must be aligned to 0x{CHUNK_SIZE:x}")

    # Older Miele controllers address EEPROM in 16-bit words while the read
    # length is still expressed in bytes. The dump CLI intentionally uses byte
    # offsets, so a contiguous 16-byte block advances the protocol address by
    # 8 words rather than 16.
    if kind == "eeprom":
        if end > 0x1FFFF:
            raise RuntimeError("EEPROM byte end offset is out of range")

        def protocol_address(byte_offset: int) -> int:
            return byte_offset // 2
    else:
        def protocol_address(byte_offset: int) -> int:
            return byte_offset

    total = end - start + 1
    completed = 0

    with output.open("wb") as handle:
        for byte_offset in range(start, end + 1, CHUNK_SIZE):
            address = protocol_address(byte_offset)
            print(
                f"\r{kind}: byte 0x{byte_offset:08x} "
                f"(protocol 0x{address:04x})  "
                f"{completed}/{total} bytes ({completed * 100 // total:3d}%)",
                end="",
                file=sys.stderr,
                flush=True,
            )
            data = read_block(host, port, token, kind, key, address)
            handle.write(data)
            handle.flush()
            completed += len(data)

    print(
        f"\r{kind}: done {completed} bytes -> {output}",
        file=sys.stderr,
    )


parser = argparse.ArgumentParser(description="FreeMDU read-only diagnostic client")
parser.add_argument("host")
parser.add_argument("--port", type=int, default=3234)
parser.add_argument("--token", help="override OTA_TOKEN from .cargo/local.toml")

sub = parser.add_subparsers(dest="command", required=True)
sub.add_parser("id")

scan = sub.add_parser("find-read-key")
scan.add_argument("start", type=number16)
scan.add_argument("end", type=number16)

mem = sub.add_parser("mem16")
mem.add_argument("key", type=number16)
mem.add_argument("address", type=number32)

eeprom = sub.add_parser("eeprom16")
eeprom.add_argument("key", type=number16)
eeprom.add_argument("address", type=number16)

dump_mem = sub.add_parser("dump-memory")
dump_mem.add_argument("output", type=Path)
dump_mem.add_argument("--key", type=number16, default="0x0000")
dump_mem.add_argument("--start", type=number32, required=True)
dump_mem.add_argument("--end", type=number32, required=True)

dump_eeprom = sub.add_parser(
    "dump-eeprom",
    help="dump a contiguous EEPROM byte range (old devices use word addresses on wire)",
)
dump_eeprom.add_argument("output", type=Path)
dump_eeprom.add_argument("--key", type=number16, default="0x0000")
dump_eeprom.add_argument("--start", type=number32, default="0x0000", help="byte offset")
dump_eeprom.add_argument("--end", type=number32, required=True, help="inclusive byte offset")

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

try:
    if args.command == "id":
        print(request(args.host, args.port, token, "id"))
    elif args.command == "find-read-key":
        print(request(args.host, args.port, token, "find-read-key", args.start, args.end))
    elif args.command == "mem16":
        data = read_block(
            args.host, args.port, token, "memory", int(args.key, 0), int(args.address, 0)
        )
        print(data.hex())
    elif args.command == "eeprom16":
        address = int(args.address, 0)
        if address > 0xFFF0:
            parser.error("eeprom16 address must be <= 0xfff0")
        data = read_block(
            args.host, args.port, token, "eeprom", int(args.key, 0), address
        )
        print(data.hex())
    elif args.command == "dump-memory":
        dump_range(
            args.host, args.port, token, "memory", int(args.key, 0),
            int(args.start, 0), int(args.end, 0), args.output,
        )
    elif args.command == "dump-eeprom":
        dump_range(
            args.host, args.port, token, "eeprom", int(args.key, 0),
            int(args.start, 0), int(args.end, 0), args.output,
        )
except (OSError, RuntimeError) as exc:
    print(f"error: {exc}", file=sys.stderr)
    raise SystemExit(1)
