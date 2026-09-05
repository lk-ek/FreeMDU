#!/usr/bin/env python3
"""Read-only FreeMDU Miele diagnostics and binary dumps over Wi-Fi."""

from __future__ import annotations

import argparse
from datetime import datetime
import json
from pathlib import Path
import re
import socket
import sys
import time

from local_config import load_config_value


CHUNK_SIZE = 0x10
CONNECT_RETRIES = 20
CONNECT_RETRY_DELAY = 0.1
DUMP_RETRY_DELAY = 0.5

# Read-access keys used by the device implementations currently shipped in
# FreeMDU.  The unknown-device probe only tries these known keys and performs
# read operations; it never scans arbitrary keys or sends write commands.
KNOWN_READ_KEYS = (0x43EA, 0x1234, 0x8542, 0xB4EE)
PROBE_ID_ATTEMPTS = 5
PROBE_RAM_START = 0x0000
PROBE_RAM_END = 0x03FF
PROBE_EEPROM_END = 0x03FF


class DiagnosticDisconnect(RuntimeError):
    """The diagnostic TCP connection closed before a reply arrived."""


class DiagnosticTransientError(RuntimeError):
    """The device returned a transient diagnostic error that is safe to retry."""


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
            raise DiagnosticDisconnect("device disconnected without a diagnostic response")

    text = reply.decode("utf-8", errors="replace").rstrip()
    if not text.startswith("OK "):
        # The firmware can accept the TCP request but still fail the fresh
        # optical handshake/read because the 2400-baud link is temporarily
        # busy or noisy. Treat explicit timeout replies as transient so dump
        # commands retry the same block instead of aborting the whole file.
        if text.startswith("ERR ") and "timeout" in text.lower():
            raise DiagnosticTransientError(text)
        raise RuntimeError(text)
    return text



def request_with_transient_retry(
    host: str, port: int, token: str, *parts: str, attempts: int = 20
) -> str:
    """Retry a diagnostic request when the optical link is temporarily noisy."""
    last_error: BaseException | None = None
    for attempt in range(attempts):
        try:
            return request(host, port, token, *parts)
        except (OSError, DiagnosticDisconnect, DiagnosticTransientError) as exc:
            last_error = exc
            if attempt + 1 == attempts:
                break
            time.sleep(DUMP_RETRY_DELAY)
    assert last_error is not None
    raise RuntimeError(f"transient diagnostic failure after {attempts} attempts: {last_error}")


def parse_software_id(reply: str) -> int:
    match = re.search(r"\bsoftware_id=(\d+)\b", reply)
    if match is None:
        raise RuntimeError(f"malformed software-id response: {reply}")
    return int(match.group(1), 10)


def safe_label(value: str) -> str:
    value = value.strip().lower()
    value = re.sub(r"[^a-z0-9._-]+", "-", value)
    return value.strip("-._") or "step"


def diff_dumps(before: Path, after: Path) -> list[tuple[int, int, int]]:
    old = before.read_bytes()
    new = after.read_bytes()
    if len(old) != len(new):
        raise RuntimeError(
            f"cannot diff dumps with different sizes: {before}={len(old)}, {after}={len(new)}"
        )
    return [(offset, a, b) for offset, (a, b) in enumerate(zip(old, new)) if a != b]


def write_diff(path: Path, changes: list[tuple[int, int, int]]) -> None:
    with path.open("w", encoding="utf-8") as handle:
        for offset, old, new in changes:
            handle.write(f"0x{offset:04x} {old:02x}->{new:02x}\n")


def probe_unknown_device(
    host: str,
    port: int,
    token: str,
    output_root: Path,
    eeprom_end: int,
    no_interactive: bool,
) -> None:
    """Collect a read-only first-contact data set from an unknown Miele device."""
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    run_dir = output_root / f"probe-{stamp}"
    run_dir.mkdir(parents=True, exist_ok=False)

    manifest: dict[str, object] = {
        "created": datetime.now().astimezone().isoformat(),
        "host": host,
        "software_id_samples": [],
        "known_read_keys_tried": [f"0x{key:04x}" for key in KNOWN_READ_KEYS],
        "read_key": None,
        "captures": [],
    }
    manifest_path = run_dir / "probe.json"

    def save_manifest() -> None:
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")

    print(f"probe: output -> {run_dir}", file=sys.stderr)
    print("probe: querying software ID repeatedly...", file=sys.stderr)
    ids: list[int] = []
    for _ in range(PROBE_ID_ATTEMPTS):
        reply = request_with_transient_retry(host, port, token, "id")
        software_id = parse_software_id(reply)
        ids.append(software_id)
        print(f"probe: software ID {software_id} (0x{software_id:04x})", file=sys.stderr)
        time.sleep(0.1)

    manifest["software_id_samples"] = ids
    save_manifest()
    if len(set(ids)) != 1:
        raise RuntimeError(f"software ID was not stable across probes: {ids}")

    print("probe: trying read keys already known to FreeMDU...", file=sys.stderr)
    selected_key: int | None = None
    key_results: dict[str, str] = {}
    for key in KNOWN_READ_KEYS:
        try:
            data = read_block_with_transient_retry(host, port, token, "memory", key, PROBE_RAM_START)
        except (OSError, RuntimeError) as exc:
            key_results[f"0x{key:04x}"] = f"failed: {exc}"
            print(f"probe: key 0x{key:04x}: no", file=sys.stderr)
            continue
        key_results[f"0x{key:04x}"] = f"ok: {data.hex()}"
        selected_key = key
        print(f"probe: key 0x{key:04x}: OK", file=sys.stderr)
        break

    manifest["read_key_results"] = key_results
    manifest["read_key"] = None if selected_key is None else f"0x{selected_key:04x}"
    save_manifest()
    if selected_key is None:
        raise RuntimeError(
            "none of FreeMDU's known read keys worked; probe stopped without brute-force scanning"
        )

    baseline = run_dir / "ram-00-baseline.bin"
    print("probe: capturing 1 KiB RAM baseline...", file=sys.stderr)
    dump_range(
        host, port, token, "memory", selected_key,
        PROBE_RAM_START, PROBE_RAM_END, baseline,
    )

    if eeprom_end >= 0:
        if (eeprom_end + 1) % CHUNK_SIZE:
            raise RuntimeError("probe EEPROM end + 1 must be 16-byte aligned")
        eeprom = run_dir / "eeprom-baseline.bin"
        print(
            f"probe: capturing EEPROM bytes 0x0000..0x{eeprom_end:04x}...",
            file=sys.stderr,
        )
        try:
            dump_range(
                host, port, token, "eeprom", selected_key, 0, eeprom_end, eeprom
            )
            manifest["eeprom"] = eeprom.name
        except RuntimeError as exc:
            # EEPROM access is useful but not required for first-contact RAM
            # reverse engineering. Record the failure and keep going.
            manifest["eeprom_error"] = str(exc)
            print(f"probe: EEPROM read failed, continuing: {exc}", file=sys.stderr)

    captures: list[dict[str, object]] = manifest["captures"]  # type: ignore[assignment]
    captures.append({"index": 0, "label": "baseline", "file": baseline.name})
    save_manifest()

    if no_interactive:
        print("probe: baseline capture complete", file=sys.stderr)
        return

    print(
        "\nInteractive capture ready. Change exactly ONE thing on the appliance, then type a label\n"
        "and press Enter (examples: program-cottons, door-open, running-30s).\n"
        "Type 'done' when finished. Each step captures another 1 KiB RAM image.\n",
        file=sys.stderr,
    )

    previous = baseline
    baseline_path = baseline
    index = 1
    while True:
        try:
            raw_label = input(f"probe step {index} label [done]: ").strip()
        except EOFError:
            raw_label = "done"
        if raw_label.lower() in {"done", "quit", "q"}:
            break

        label = safe_label(raw_label or f"step-{index:02d}")
        capture = run_dir / f"ram-{index:02d}-{label}.bin"
        dump_range(
            host, port, token, "memory", selected_key,
            PROBE_RAM_START, PROBE_RAM_END, capture,
        )

        from_previous = diff_dumps(previous, capture)
        from_baseline = diff_dumps(baseline_path, capture)
        previous_diff = run_dir / f"diff-{index:02d}-{label}-from-previous.txt"
        baseline_diff = run_dir / f"diff-{index:02d}-{label}-from-baseline.txt"
        write_diff(previous_diff, from_previous)
        write_diff(baseline_diff, from_baseline)

        print(
            f"probe: {label}: {len(from_previous)} byte(s) changed from previous, "
            f"{len(from_baseline)} from baseline",
            file=sys.stderr,
        )
        if from_previous:
            preview = ", ".join(
                f"0x{offset:04x}:{old:02x}->{new:02x}"
                for offset, old, new in from_previous[:24]
            )
            print(f"probe: changes: {preview}", file=sys.stderr)

        captures.append(
            {
                "index": index,
                "label": raw_label or label,
                "file": capture.name,
                "changed_from_previous": len(from_previous),
                "changed_from_baseline": len(from_baseline),
                "diff_from_previous": previous_diff.name,
                "diff_from_baseline": baseline_diff.name,
            }
        )
        save_manifest()
        previous = capture
        index += 1

    print(f"probe: complete -> {run_dir}", file=sys.stderr)

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



def read_block_with_transient_retry(
    host: str, port: int, token: str, kind: str, key: int, address: int, attempts: int = 8
) -> bytes:
    last_error: BaseException | None = None
    for attempt in range(attempts):
        try:
            return read_block(host, port, token, kind, key, address)
        except (OSError, DiagnosticDisconnect, DiagnosticTransientError) as exc:
            last_error = exc
            if attempt + 1 == attempts:
                break
            time.sleep(DUMP_RETRY_DELAY)
    assert last_error is not None
    raise RuntimeError(f"transient diagnostic failure after {attempts} attempts: {last_error}")

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

    # Resume an interrupted dump from the first complete block that is not yet
    # present in the output file. A block is flushed only after a successful
    # response, so the file size is also the committed byte count.
    completed = output.stat().st_size if output.exists() else 0
    if completed > total:
        raise RuntimeError(
            f"existing output is larger than requested dump: {completed} > {total} bytes"
        )
    if completed % CHUNK_SIZE:
        raise RuntimeError(
            f"existing output size {completed} is not aligned to 0x{CHUNK_SIZE:x}; "
            "refusing to append to a partial block"
        )

    if completed:
        print(
            f"{kind}: resuming {output} at {completed}/{total} bytes "
            f"(byte 0x{start + completed:08x})",
            file=sys.stderr,
        )

    with output.open("ab") as handle:
        byte_offset = start + completed
        while byte_offset <= end:
            address = protocol_address(byte_offset)
            print(
                f"\r{kind}: byte 0x{byte_offset:08x} "
                f"(protocol 0x{address:04x})  "
                f"{completed}/{total} bytes ({completed * 100 // total:3d}%)",
                end="",
                file=sys.stderr,
                flush=True,
            )

            try:
                data = read_block(host, port, token, kind, key, address)
            except (OSError, DiagnosticDisconnect, DiagnosticTransientError) as exc:
                # Every block uses a fresh TCP connection. Keep the last fully
                # written block as the resume point and retry the same address
                # after connection failures or transient firmware/optical
                # timeouts. Ctrl-C can still abort; rerunning the same command
                # resumes from disk.
                print(
                    f"\n{kind}: transient failure at byte 0x{byte_offset:08x}: {exc}; "
                    f"retrying in {DUMP_RETRY_DELAY:.1f}s",
                    file=sys.stderr,
                )
                time.sleep(DUMP_RETRY_DELAY)
                continue

            handle.write(data)
            handle.flush()
            completed += len(data)
            byte_offset += len(data)

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

probe = sub.add_parser(
    "probe-unknown",
    help="read-only first-contact probe for an unsupported Miele appliance",
)
probe.add_argument("output_dir", type=Path)
probe.add_argument(
    "--eeprom-end",
    type=number32,
    default="0x03ff",
    help="inclusive EEPROM byte offset for baseline capture",
)
probe.add_argument("--skip-eeprom", action="store_true")
probe.add_argument(
    "--no-interactive", action="store_true",
    help="stop after ID/key/RAM/EEPROM baseline instead of prompting for state captures",
)

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
    elif args.command == "probe-unknown":
        probe_unknown_device(
            args.host, args.port, token, args.output_dir,
            -1 if args.skip_eeprom else int(args.eeprom_end, 0),
            args.no_interactive,
        )
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
