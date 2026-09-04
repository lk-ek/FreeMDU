#!/usr/bin/env python3
"""List or extract automatic FreeMDU ID410 RAM snapshots from a serial/log capture."""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys

BEGIN = re.compile(r"ID410 SNAPSHOT BEGIN seq=(\d+).*state=([0-9a-fA-F]{2})->([0-9a-fA-F]{2}).*phase=([0-9a-fA-F]{2})->([0-9a-fA-F]{2})")
LINE = re.compile(r"ID410 SNAPSHOT seq=(\d+) ([0-9a-fA-F]{4}) \[([^]]+)\]")
END = re.compile(r"ID410 SNAPSHOT END seq=(\d+)")


def parse(path: Path):
    snapshots: dict[int, dict] = {}
    for raw in path.read_text(errors="replace").splitlines():
        if m := BEGIN.search(raw):
            seq = int(m.group(1))
            snapshots[seq] = {
                "state": (int(m.group(2), 16), int(m.group(3), 16)),
                "phase": (int(m.group(4), 16), int(m.group(5), 16)),
                "chunks": {},
                "complete": False,
            }
            continue
        if m := LINE.search(raw):
            seq = int(m.group(1))
            snap = snapshots.get(seq)
            if snap is None:
                continue
            offset = int(m.group(2), 16)
            tokens = [token.strip() for token in m.group(3).split(",")]
            try:
                data = bytes(int(token, 16) for token in tokens if token)
            except ValueError:
                continue
            if len(data) == 16:
                snap["chunks"][offset] = data
            continue
        if m := END.search(raw):
            seq = int(m.group(1))
            if seq in snapshots:
                snapshots[seq]["complete"] = True
    return snapshots


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("input", type=Path, help="captured FreeMDU log")
    ap.add_argument("output", type=Path, nargs="?", help="binary output")
    ap.add_argument("--snapshot", type=int, help="snapshot sequence; defaults to latest complete")
    ap.add_argument("--list", action="store_true", help="list snapshots without extracting")
    args = ap.parse_args()

    snapshots = parse(args.input)
    if not snapshots:
        print("error: no ID410 snapshots found", file=sys.stderr)
        return 1

    if args.list:
        for seq in sorted(snapshots):
            snap = snapshots[seq]
            old_state, state = snap["state"]
            old_phase, phase = snap["phase"]
            print(
                f"seq={seq} state={old_state:02x}->{state:02x} "
                f"phase={old_phase:02x}->{phase:02x} "
                f"blocks={len(snap['chunks'])}/64 complete={snap['complete']}"
            )
        return 0

    if args.output is None:
        ap.error("OUTPUT is required unless --list is used")

    if args.snapshot is not None:
        seq = args.snapshot
        if seq not in snapshots:
            print(f"error: snapshot {seq} not found", file=sys.stderr)
            return 1
    else:
        complete = [seq for seq, snap in snapshots.items() if snap["complete"]]
        if not complete:
            print("error: no complete snapshot found", file=sys.stderr)
            return 1
        seq = max(complete)

    snap = snapshots[seq]
    chunks = snap["chunks"]
    missing = [offset for offset in range(0, 0x400, 16) if offset not in chunks]
    if missing:
        print(
            "error: snapshot is incomplete; missing "
            + ", ".join(f"0x{x:04x}" for x in missing),
            file=sys.stderr,
        )
        return 1

    data = b"".join(chunks[offset] for offset in range(0, 0x400, 16))
    args.output.write_bytes(data)
    print(f"Wrote snapshot {seq}: {len(data)} bytes to {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
