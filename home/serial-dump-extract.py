#!/usr/bin/env python3
"""Extract a FreeMDU SERDUMP from a captured serial monitor log."""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys


LINE = re.compile(
    r"SERDUMP\s+(memory|eeprom)\s+([0-9a-fA-F]{8})\s+([0-9a-fA-F]{32})"
)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("input", type=Path, help="captured serial log")
    parser.add_argument("output", type=Path, help="binary output")
    parser.add_argument("--kind", choices=("memory", "eeprom"), required=True)
    args = parser.parse_args()

    chunks: dict[int, bytes] = {}

    for raw_line in args.input.read_text(errors="replace").splitlines():
        match = LINE.search(raw_line)
        if not match or match.group(1) != args.kind:
            continue
        offset = int(match.group(2), 16)
        chunks[offset] = bytes.fromhex(match.group(3))

    if not chunks:
        print(f"error: no SERDUMP {args.kind} lines found", file=sys.stderr)
        return 1

    offsets = sorted(chunks)
    expected = offsets[0]
    result = bytearray()

    for offset in offsets:
        if offset != expected:
            print(
                f"error: gap: expected 0x{expected:08x}, got 0x{offset:08x}",
                file=sys.stderr,
            )
            return 1
        result.extend(chunks[offset])
        expected += len(chunks[offset])

    args.output.write_bytes(result)
    print(
        f"Wrote {len(result)} bytes from "
        f"0x{offsets[0]:08x}..0x{expected - 1:08x} to {args.output}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
