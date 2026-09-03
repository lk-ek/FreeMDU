#!/usr/bin/env python3
"""Read-only FreeMDU Miele diagnostics over Wi-Fi."""

import argparse
import socket
import sys

from local_config import load_config_value


def number(value: str) -> str:
    try:
        parsed = int(value, 0)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid number: {value}") from exc

    if not 0 <= parsed <= 0xFFFF:
        raise argparse.ArgumentTypeError("value must fit in 16 bits")

    return value


parser = argparse.ArgumentParser(description="FreeMDU read-only diagnostic client")
parser.add_argument("host")
parser.add_argument("--port", type=int, default=3234)
parser.add_argument("--token", help="override OTA_TOKEN from .cargo/local.toml")

sub = parser.add_subparsers(dest="command", required=True)
sub.add_parser("id")

scan = sub.add_parser("find-read-key")
scan.add_argument("start", type=number)
scan.add_argument("end", type=number)

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

parts = ["FMDUDIAG1", token, args.command]
if args.command == "find-read-key":
    parts += [args.start, args.end]

with socket.create_connection((args.host, args.port), timeout=10) as sock:
    # A key scan may take a while, so disable the read timeout after connect.
    sock.settimeout(None)
    sock.sendall((" ".join(parts) + "\n").encode("ascii"))

    # FMDUDIAG1 is deliberately request/response: exactly one newline-
    # terminated reply is returned for each connection. Do not wait for TCP
    # EOF; an orderly FIN may be delayed by the embedded TCP stack.
    reader = sock.makefile("rb", buffering=0)
    reply = reader.readline()
    if not reply:
        raise RuntimeError("device disconnected without a diagnostic response")

    sys.stdout.buffer.write(reply)
    sys.stdout.buffer.flush()
