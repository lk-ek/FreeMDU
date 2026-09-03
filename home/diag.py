#!/usr/bin/env python3
"""Read-only FreeMDU Miele diagnostics over Wi-Fi."""

import argparse
import socket
import sys


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
parser.add_argument("--token", required=True)

sub = parser.add_subparsers(dest="command", required=True)
sub.add_parser("id")

scan = sub.add_parser("find-read-key")
scan.add_argument("start", type=number)
scan.add_argument("end", type=number)

args = parser.parse_args()

if not args.token:
    parser.error("--token must not be empty")

parts = ["FMDUDIAG1", args.token, args.command]
if args.command == "find-read-key":
    parts += [args.start, args.end]

with socket.create_connection((args.host, args.port), timeout=10) as sock:
    sock.settimeout(None)
    sock.sendall((" ".join(parts) + "\n").encode("ascii"))

    while True:
        chunk = sock.recv(4096)
        if not chunk:
            break
        sys.stdout.buffer.write(chunk)
        sys.stdout.buffer.flush()
