#!/usr/bin/env python3
"""Capture a read-only first-contact probe over the ESP32-C3 USB serial console."""

from __future__ import annotations

import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import re
import select
import sys
import termios
import time


CHUNK_SIZE = 16
RAM_SIZE = 0x400


def safe_label(value: str) -> str:
    value = value.strip().lower()
    value = re.sub(r"[^a-z0-9._-]+", "-", value)
    return value.strip("-._") or "step"


def configure_serial(fd: int, baud: int) -> None:
    speed_name = f"B{baud}"
    if not hasattr(termios, speed_name):
        raise RuntimeError(f"unsupported baud rate: {baud}")
    speed = getattr(termios, speed_name)

    attrs = termios.tcgetattr(fd)
    attrs[0] = 0
    attrs[1] = 0
    attrs[2] = termios.CLOCAL | termios.CREAD | termios.CS8
    attrs[3] = 0
    attrs[4] = speed
    attrs[5] = speed
    attrs[6][termios.VMIN] = 0
    attrs[6][termios.VTIME] = 0
    termios.tcsetattr(fd, termios.TCSANOW, attrs)
    termios.tcflush(fd, termios.TCIOFLUSH)


class SerialLines:
    def __init__(self, fd: int, transcript):
        self.fd = fd
        self.transcript = transcript
        self.buffer = bytearray()

    def send(self, command: str) -> None:
        wire = (command.rstrip() + "\n").encode("ascii")
        total = 0
        while total < len(wire):
            written = os.write(self.fd, wire[total:])
            total += written
        self.transcript.write(f">>> {command.rstrip()}\n")
        self.transcript.flush()

    def readline(self, deadline: float) -> str:
        while True:
            nl = self.buffer.find(b"\n")
            if nl >= 0:
                raw = bytes(self.buffer[: nl + 1])
                del self.buffer[: nl + 1]
                text = raw.decode("utf-8", errors="replace").rstrip("\r\n")
                self.transcript.write(text + "\n")
                self.transcript.flush()
                print(text)
                return text

            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError("timeout waiting for USB serial response")
            ready, _, _ = select.select([self.fd], [], [], min(remaining, 1.0))
            if not ready:
                continue
            try:
                data = os.read(self.fd, 4096)
            except BlockingIOError:
                continue
            if data:
                self.buffer.extend(data)


def collect_dump(lines: SerialLines, kind: str, deadline: float) -> tuple[bytes, list[int]]:
    chunks: dict[int, bytes] = {}
    saw_begin = False

    while True:
        line = lines.readline(deadline)
        if line.startswith(f"SERDUMP BEGIN {kind} "):
            saw_begin = True
            continue
        if line.startswith(f"SERDUMP ERROR {kind} "):
            raise RuntimeError(line)
        if line == f"SERDUMP END {kind}":
            if not saw_begin:
                raise RuntimeError(f"saw end of {kind} dump without begin")
            break
        prefix = f"SERDUMP {kind} "
        if not line.startswith(prefix):
            continue
        fields = line.split()
        if len(fields) != 4:
            continue
        try:
            offset = int(fields[2], 16)
            data = bytes.fromhex(fields[3])
        except ValueError:
            continue
        if len(data) == CHUNK_SIZE:
            chunks[offset] = data

    if not chunks:
        raise RuntimeError(f"no {kind} dump blocks received")
    start = min(chunks)
    end = max(chunks) + CHUNK_SIZE
    missing = [offset for offset in range(start, end, CHUNK_SIZE) if offset not in chunks]
    blob = bytearray(end - start)
    for offset, data in chunks.items():
        blob[offset - start : offset - start + CHUNK_SIZE] = data
    return bytes(blob), missing


def collect_initial_probe(lines: SerialLines, deadline: float):
    software_ids: list[int] = []
    read_key: int | None = None
    ram: bytes | None = None
    eeprom: bytes | None = None
    ram_missing: list[int] = []
    eeprom_missing: list[int] = []

    while True:
        line = lines.readline(deadline)
        match = re.search(r"SERPROBE ID sample=\d+ OK software_id=(\d+)", line)
        if match:
            software_ids.append(int(match.group(1)))
        match = re.search(r"SERPROBE READ_KEY key=0x([0-9a-fA-F]+) result=ok", line)
        if match:
            read_key = int(match.group(1), 16)
        if line.startswith("SERDUMP BEGIN memory "):
            # The begin line has already been consumed. Reconstruct the same
            # state machine inline so ordinary firmware logs may remain mixed in.
            chunks: dict[int, bytes] = {}
            while True:
                row = lines.readline(deadline)
                if row.startswith("SERDUMP ERROR memory "):
                    raise RuntimeError(row)
                if row == "SERDUMP END memory":
                    break
                if row.startswith("SERDUMP memory "):
                    fields = row.split()
                    if len(fields) == 4:
                        try:
                            offset = int(fields[2], 16)
                            data = bytes.fromhex(fields[3])
                        except ValueError:
                            continue
                        if len(data) == CHUNK_SIZE:
                            chunks[offset] = data
            ram, ram_missing = assemble_chunks(chunks, RAM_SIZE)
        elif line.startswith("SERDUMP BEGIN eeprom "):
            chunks = {}
            while True:
                row = lines.readline(deadline)
                if row.startswith("SERDUMP ERROR eeprom "):
                    # EEPROM is optional for first contact. Preserve the RAM
                    # capture and continue until SERPROBE END.
                    break
                if row == "SERDUMP END eeprom":
                    break
                if row.startswith("SERDUMP eeprom "):
                    fields = row.split()
                    if len(fields) == 4:
                        try:
                            offset = int(fields[2], 16)
                            data = bytes.fromhex(fields[3])
                        except ValueError:
                            continue
                        if len(data) == CHUNK_SIZE:
                            chunks[offset] = data
            if chunks:
                eeprom, eeprom_missing = assemble_chunks(chunks, RAM_SIZE)
        if line.startswith("SERPROBE END"):
            break

    return software_ids, read_key, ram, ram_missing, eeprom, eeprom_missing


def assemble_chunks(chunks: dict[int, bytes], expected_size: int) -> tuple[bytes, list[int]]:
    missing = [offset for offset in range(0, expected_size, CHUNK_SIZE) if offset not in chunks]
    blob = bytearray(expected_size)
    for offset, data in chunks.items():
        if 0 <= offset <= expected_size - CHUNK_SIZE:
            blob[offset : offset + CHUNK_SIZE] = data
    return bytes(blob), missing


def collect_memory_after_command(lines: SerialLines, deadline: float) -> tuple[bytes, list[int]]:
    chunks: dict[int, bytes] = {}
    saw_begin = False
    while True:
        line = lines.readline(deadline)
        if line.startswith("SERDUMP BEGIN memory "):
            saw_begin = True
            continue
        if line.startswith("SERDUMP ERROR memory "):
            raise RuntimeError(line)
        if line == "SERDUMP END memory":
            if not saw_begin:
                raise RuntimeError("memory dump ended without begin")
            break
        if line.startswith("SERDUMP memory "):
            fields = line.split()
            if len(fields) == 4:
                try:
                    offset = int(fields[2], 16)
                    data = bytes.fromhex(fields[3])
                except ValueError:
                    continue
                if len(data) == CHUNK_SIZE:
                    chunks[offset] = data
    return assemble_chunks(chunks, RAM_SIZE)


def diff_bytes(old: bytes, new: bytes) -> list[tuple[int, int, int]]:
    return [(i, a, b) for i, (a, b) in enumerate(zip(old, new)) if a != b]


def write_diff(path: Path, changes: list[tuple[int, int, int]]) -> None:
    with path.open("w", encoding="utf-8") as handle:
        for offset, old, new in changes:
            handle.write(f"0x{offset:04x} {old:02x}->{new:02x}\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("device", help="USB serial device, e.g. /dev/cu.usbmodemXXXX")
    parser.add_argument("output_dir", type=Path)
    parser.add_argument("--baud", type=int, default=115200)
    parser.add_argument("--timeout", type=float, default=600.0)
    parser.add_argument("--no-interactive", action="store_true")
    args = parser.parse_args()

    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    run_dir = args.output_dir / f"serial-probe-{stamp}"
    run_dir.mkdir(parents=True, exist_ok=False)
    transcript_path = run_dir / "serial.log"

    fd = os.open(args.device, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        configure_serial(fd, args.baud)
        with transcript_path.open("w", encoding="utf-8") as transcript:
            lines = SerialLines(fd, transcript)
            # Give USB Serial/JTAG a moment after opening, then clear stale input.
            time.sleep(0.2)
            termios.tcflush(fd, termios.TCIFLUSH)
            lines.send("diag probe")
            deadline = time.monotonic() + args.timeout
            ids, key, ram, ram_missing, eeprom, eeprom_missing = collect_initial_probe(
                lines, deadline
            )

            manifest: dict[str, object] = {
                "created": datetime.now().astimezone().isoformat(),
                "device": args.device,
                "software_id_samples": ids,
                "read_key": None if key is None else f"0x{key:04x}",
                "ram_missing_blocks": [f"0x{x:04x}" for x in ram_missing],
                "eeprom_missing_blocks": [f"0x{x:04x}" for x in eeprom_missing],
                "captures": [],
            }

            if ram is not None:
                baseline_path = run_dir / "ram-00-baseline.bin"
                baseline_path.write_bytes(ram)
                manifest["captures"] = [
                    {"index": 0, "label": "baseline", "file": baseline_path.name}
                ]
            else:
                baseline_path = None
            if eeprom is not None:
                (run_dir / "eeprom-baseline.bin").write_bytes(eeprom)
                manifest["eeprom"] = "eeprom-baseline.bin"

            (run_dir / "probe.json").write_text(
                json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
            )

            if key is None or baseline_path is None:
                print(f"probe incomplete; inspect {transcript_path}", file=sys.stderr)
                return 2
            if ram_missing:
                print(
                    f"warning: baseline RAM is missing {len(ram_missing)} block(s); "
                    "inspect probe.json",
                    file=sys.stderr,
                )

            print(
                f"baseline captured: software ID {ids[-1] if ids else '?'}; "
                f"read key 0x{key:04x}; files in {run_dir}",
                file=sys.stderr,
            )

            if args.no_interactive:
                return 0

            baseline = baseline_path.read_bytes()
            previous = baseline
            captures = manifest["captures"]
            assert isinstance(captures, list)
            index = 1
            print(
                "\nChange exactly ONE thing on the dryer, then type a label and press Enter.\n"
                "Good first labels: program-change, door-open, door-closed, started-30s, stopped.\n"
                "Type 'done' when finished.\n",
                file=sys.stderr,
            )
            while True:
                try:
                    raw_label = input(f"probe step {index} label [done]: ").strip()
                except EOFError:
                    raw_label = "done"
                if raw_label.lower() in {"done", "quit", "q"}:
                    break
                label = safe_label(raw_label or f"step-{index:02d}")
                lines.send(f"diag dump-memory 0x{key:04x} 0x0000 0x03ff")
                current, missing = collect_memory_after_command(
                    lines, time.monotonic() + args.timeout
                )
                path = run_dir / f"ram-{index:02d}-{label}.bin"
                path.write_bytes(current)
                prev_changes = diff_bytes(previous, current)
                base_changes = diff_bytes(baseline, current)
                prev_diff = run_dir / f"diff-{index:02d}-{label}-from-previous.txt"
                base_diff = run_dir / f"diff-{index:02d}-{label}-from-baseline.txt"
                write_diff(prev_diff, prev_changes)
                write_diff(base_diff, base_changes)
                print(
                    f"{label}: {len(prev_changes)} changed byte(s) from previous; "
                    f"{len(base_changes)} from baseline; missing blocks={len(missing)}",
                    file=sys.stderr,
                )
                if prev_changes:
                    print(
                        "changes: "
                        + ", ".join(
                            f"0x{o:04x}:{a:02x}->{b:02x}"
                            for o, a, b in prev_changes[:24]
                        ),
                        file=sys.stderr,
                    )
                captures.append(
                    {
                        "index": index,
                        "label": raw_label or label,
                        "file": path.name,
                        "missing_blocks": [f"0x{x:04x}" for x in missing],
                        "changed_from_previous": len(prev_changes),
                        "changed_from_baseline": len(base_changes),
                        "diff_from_previous": prev_diff.name,
                        "diff_from_baseline": base_diff.name,
                    }
                )
                (run_dir / "probe.json").write_text(
                    json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
                )
                previous = current
                index += 1

            print(f"probe complete -> {run_dir}", file=sys.stderr)
            return 0
    finally:
        os.close(fd)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(1)
