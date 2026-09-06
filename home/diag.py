#!/usr/bin/env python3
"""Read-only FreeMDU Miele diagnostics and binary dumps over Wi-Fi."""

from __future__ import annotations

import argparse
import csv
from datetime import datetime
import json
from pathlib import Path
import re
import socket
import sys
import time

from local_config import load_config_value


CHUNK_SIZE = 0x10
DUMP_CHUNK_SIZE = 0x80
CONNECT_RETRIES = 20
CONNECT_RETRY_DELAY = 0.1
DUMP_RETRY_DELAY = 0.5
READ_KEY_SCAN_STATE = Path(".freemdu-read-key-scan.json")

# Read-access keys used by the device implementations currently shipped in
# FreeMDU.  The unknown-device probe only tries these known keys and performs
# read operations; it never scans arbitrary keys or sends write commands.
KEY_REGISTRY = Path(__file__).resolve().parents[1] / "protocol" / "read_keys.csv"
with KEY_REGISTRY.open(encoding="utf-8", newline="") as registry:
    READ_KEY_CANDIDATES = tuple(csv.DictReader(registry))
KNOWN_READ_KEYS = tuple(int(row["key"], 0) for row in READ_KEY_CANDIDATES)
SCAN_VERSION = 2
SCAN_METHOD = "echo-checked-two-silent-reads-v2"
SCAN_RETRIES = 3
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




def key_range(value: str) -> tuple[int, int]:
    """Parse an inclusive 16-bit key range such as 0x1000-0x1fff."""
    match = re.fullmatch(r"\s*([^:-]+)\s*[:-]\s*([^:-]+)\s*", value)
    if match is None:
        raise argparse.ArgumentTypeError("range must be START-END or START:END")
    try:
        start = int(match.group(1), 0)
        end = int(match.group(2), 0)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid key range: {value}") from exc
    if not 0 <= start <= 0xFFFF or not 0 <= end <= 0xFFFF:
        raise argparse.ArgumentTypeError("range values must fit in 16 bits")
    if start > end:
        raise argparse.ArgumentTypeError("range start must not be greater than end")
    return start, end


def merge_ranges(ranges: list[tuple[int, int]]) -> list[tuple[int, int]]:
    merged: list[tuple[int, int]] = []
    for start, end in sorted(ranges):
        if not merged or start > merged[-1][1] + 1:
            merged.append((start, end))
        else:
            merged[-1] = (merged[-1][0], max(merged[-1][1], end))
    return merged


def subtract_ranges(
    start: int, end: int, excluded: list[tuple[int, int]]
) -> list[tuple[int, int]]:
    """Return inclusive subranges not covered by excluded ranges."""
    pending = [(start, end)]
    for ex_start, ex_end in merge_ranges(excluded):
        next_pending: list[tuple[int, int]] = []
        for cur_start, cur_end in pending:
            if ex_end < cur_start or ex_start > cur_end:
                next_pending.append((cur_start, cur_end))
                continue
            if ex_start > cur_start:
                next_pending.append((cur_start, ex_start - 1))
            if ex_end < cur_end:
                next_pending.append((ex_end + 1, cur_end))
        pending = next_pending
    return pending


def load_read_key_scan_state(path: Path) -> dict:
    if not path.exists():
        return {"version": 2, "devices": {}, "profiles": {}}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise RuntimeError(f"cannot read scan state {path}: {exc}") from exc
    if not isinstance(data, dict) or data.get("version") not in (1, 2):
        raise RuntimeError(f"unsupported scan state format in {path}")
    if not isinstance(data.get("devices"), dict):
        raise RuntimeError(f"invalid scan state in {path}: missing devices map")
    if data["version"] == 1:
        # Preserve old evidence, but never reuse old negative ranges.
        return {"version": 2, "devices": data["devices"], "profiles": {},
                "legacy_v1": data}
    if not isinstance(data.get("profiles"), dict):
        raise RuntimeError(f"invalid scan state in {path}: missing profiles map")
    return data


def save_read_key_scan_state(path: Path, state: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(state, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    tmp.replace(path)


def state_device_key(software_id: int) -> str:
    return f"software_id:{software_id}"


def scan_profile(software_id: int, timeout_ms: int) -> str:
    return f"{software_id}:{SCAN_METHOD}:{timeout_ms}ms"


def get_tested_ranges(state: dict, profile: str) -> list[tuple[int, int]]:
    entry = state["profiles"].get(profile, {})
    ranges = entry.get("silent_ranges", [])
    result = []
    for item in ranges:
        if not (isinstance(item, list) and len(item) == 2
                and all(type(value) is int for value in item)
                and 0 <= item[0] <= item[1] <= 0xffff):
            raise RuntimeError("invalid silent range in scan state")
        result.append(tuple(item))
    return merge_ranges(result)


def record_silent_range(path: Path, state: dict, software_id: int,
                        timeout_ms: int, start: int, end: int) -> None:
    profile = scan_profile(software_id, timeout_ms)
    ranges = get_tested_ranges(state, profile) + [(start, end)]
    state["profiles"][profile] = {
        "software_id": software_id, "timeout_ms": timeout_ms,
        "method": SCAN_METHOD, "firmware_scan_version": SCAN_VERSION,
        "silent_ranges": [[a, b] for a, b in merge_ranges(ranges)],
        "updated_at": datetime.now().astimezone().isoformat(timespec="seconds"),
    }
    save_read_key_scan_state(path, state)


def record_found_key(path: Path, state: dict, software_id: int, key_value: int) -> None:
    entry = state["devices"].setdefault(state_device_key(software_id), {})
    entry.update(software_id=software_id, read_key=key_value,
                 confirmations=2, firmware_scan_version=SCAN_VERSION,
                 updated_at=datetime.now().astimezone().isoformat(timespec="seconds"))
    save_read_key_scan_state(path, state)


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


def request_reply(host: str, port: int, token: str, *parts: object) -> str:
    """Send one diagnostic request and return the unclassified reply line."""
    wire = ["FMDUDIAG1", token, *(str(part) for part in parts)]

    with connect_with_retry(host, port) as sock:
        sock.settimeout(30)
        sock.sendall((" ".join(wire) + "\n").encode("ascii"))

        reader = sock.makefile("rb", buffering=0)
        reply = reader.readline()
        if not reply:
            raise DiagnosticDisconnect("device disconnected without a diagnostic response")

    return reply.decode("utf-8", errors="replace").rstrip()


def request(host: str, port: int, token: str, *parts: object) -> str:
    text = request_reply(host, port, token, *parts)
    if not text.startswith("OK "):
        # The firmware can accept the TCP request but still fail the fresh
        # optical handshake/read because the 2400-baud link is temporarily
        # busy or noisy. Treat explicit timeout replies as transient so dump
        # commands retry the same block instead of aborting the whole file.
        if text.startswith("ERR ") and (
            "timeout" in text.lower() or text.startswith("ERR query_software_id")
        ):
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


def scan_read_keys(
    host: str, port: int, token: str, start: int, end: int,
    timeout_ms: int, chunk_size: int, state_path: Path,
    explicit_excludes: list[tuple[int, int]], recheck: bool = False,
) -> str:
    """Resume observations under identical settings; silence is not key proof."""
    if not (0 <= start <= end <= 0xffff and 100 <= timeout_ms <= 2000
            and 1 <= chunk_size <= 4096):
        raise RuntimeError("invalid scan range, timeout (100..2000 ms), or chunk size")
    software_id = parse_software_id(request_with_transient_retry(host, port, token, "id"))
    state = load_read_key_scan_state(state_path)
    profile = scan_profile(software_id, timeout_ms)
    observed = [] if recheck else get_tested_ranges(state, profile)

    def scan_chunk(first: int, last: int) -> str:
        for attempt in range(SCAN_RETRIES):
            try:
                reply = request_reply(host, port, token, "find-read-key",
                                      f"0x{first:04x}", f"0x{last:04x}", timeout_ms)
                if reply.startswith("ERR scan_inconclusive") or (
                    reply.startswith("ERR ") and "timeout" in reply.lower()
                ):
                    raise DiagnosticTransientError(reply)
            except (OSError, DiagnosticDisconnect, DiagnosticTransientError) as exc:
                if attempt + 1 == SCAN_RETRIES:
                    raise RuntimeError(f"scan incomplete; range not saved: {exc}") from exc
                print(f"retry {attempt + 1}/{SCAN_RETRIES}: {exc}", file=sys.stderr)
                time.sleep(DUMP_RETRY_DELAY)
                continue
            fields = dict(re.findall(r"([a-z_]+)=([^ ]+)", reply))
            if (fields.get("scan_version") != "2"
                    or fields.get("software_id") != str(software_id)):
                raise RuntimeError(f"scan version/device mismatch; update firmware: {reply}")
            if reply.startswith("OK read_key="):
                try:
                    key = int(fields["read_key"], 0)
                except (ValueError, KeyError) as exc:
                    raise RuntimeError(f"malformed key reply: {reply}") from exc
                if not first <= key <= last or fields.get("confirmed") != "2":
                    raise RuntimeError(f"unconfirmed/out-of-range key: {reply}")
                record_found_key(state_path, state, software_id, key)
                return reply
            if (reply.startswith("NO_RESPONSE ")
                    and fields.get("start") == f"0x{first:04x}"
                    and fields.get("end") == f"0x{last:04x}"
                    and fields.get("timeout_ms") == str(timeout_ms)):
                record_silent_range(state_path, state, software_id, timeout_ms, first, last)
                return reply
            raise RuntimeError(f"unexpected scan reply; range not saved: {reply}")
        raise AssertionError("unreachable")

    # Saved positives and known keys are always revalidated, even if a previous
    # run recorded silence. Explicit user exclusions are invocation-local only.
    saved = state["devices"].get(state_device_key(software_id), {}).get("read_key")
    candidates = list(dict.fromkeys(([saved] if type(saved) is int else []) + list(KNOWN_READ_KEYS)))
    for key in candidates:
        if start <= key <= end and subtract_ranges(key, key, explicit_excludes):
            print(f"checking known/saved candidate 0x{key:04x}", file=sys.stderr)
            reply = scan_chunk(key, key)
            if reply.startswith("OK "):
                return reply
            observed.append((key, key))

    pending = subtract_ranges(start, end, observed + explicit_excludes)
    for first, last in pending:
        for current in range(first, last + 1, chunk_size):
            chunk_end = min(last, current + chunk_size - 1)
            print(f"scan 0x{current:04x}..0x{chunk_end:04x} ({timeout_ms} ms)",
                  file=sys.stderr, flush=True)
            reply = scan_chunk(current, chunk_end)
            if reply.startswith("OK "):
                return reply
    return (f"NO_KEY_CONFIRMED software_id={software_id} "
            "silence_is_not_proof; use --recheck or a larger --timeout-ms to repeat")


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

def read_block(
    host: str, port: int, token: str, kind: str, key: int, address: int, size: int = CHUNK_SIZE
) -> bytes:
    if size not in (CHUNK_SIZE, DUMP_CHUNK_SIZE):
        raise RuntimeError(f"unsupported diagnostic block size: {size}")
    suffix = size
    command = f"eeprom{suffix}" if kind == "eeprom" else f"mem{suffix}"
    width = 4 if kind == "eeprom" else 8
    reply = request(
        host,
        port,
        token,
        command,
        f"0x{key:04x}",
        f"0x{address:0{width}x}",
    )

    expected = f"OK kind={kind} address=0x{address:0{width}x} data="
    if not reply.startswith(expected):
        raise RuntimeError(f"response kind/address mismatch: {reply}")
    marker = " data="
    if marker not in reply:
        raise RuntimeError(f"malformed response: {reply}")

    try:
        data = bytes.fromhex(reply.split(marker, 1)[1])
    except ValueError as exc:
        raise RuntimeError(f"invalid hex payload: {reply}") from exc

    if len(data) != size:
        raise RuntimeError(f"expected {size} bytes, got {len(data)}")
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
    if start % DUMP_CHUNK_SIZE:
        raise RuntimeError(f"start must be aligned to 0x{DUMP_CHUNK_SIZE:x}")
    if (end + 1) % CHUNK_SIZE:
        raise RuntimeError(f"end + 1 must be aligned to 0x{CHUNK_SIZE:x}")

    # ID498 is experimentally confirmed to use byte addresses.
    software_id = None
    if kind == "eeprom":
        software_id = parse_software_id(
            request_with_transient_retry(host, port, token, "id")
        )
    address_unit = 1 if software_id == 498 else 2

    # Older Miele controllers address EEPROM in 16-bit words while the read
    # length is still expressed in bytes. The dump CLI intentionally uses byte
    # offsets, so a contiguous block advances the protocol address by half the
    # byte count.
    if kind == "eeprom":
        if end > 0xFFFF * address_unit + address_unit - 1:
            raise RuntimeError("EEPROM byte end offset is out of range")

        def protocol_address(byte_offset: int) -> int:
            return byte_offset // address_unit
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

            remaining = end - byte_offset + 1
            block_size = DUMP_CHUNK_SIZE if remaining >= DUMP_CHUNK_SIZE else CHUNK_SIZE
            if kind == "eeprom" and software_id == 498:
                # Compatible with firmware whose eeprom128 still uses word strides.
                block_size = CHUNK_SIZE

            try:
                data = read_block(host, port, token, kind, key, address, block_size)
            except (OSError, DiagnosticDisconnect, DiagnosticTransientError) as exc:
                # Every block uses a fresh TCP connection. Keep the last fully
                # written block as the resume point and retry the same
                # address after connection failures or transient firmware/optical
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


def autonomous_start(host: str, port: int, token: str, start: int, end: int,
                     timeout_ms: int, maximum_ms: int) -> str:
    if not (0 <= start <= end <= 0xffff and 40 <= timeout_ms <= maximum_ms <= 2000
            and timeout_ms % 5 == 0 and maximum_ms % 5 == 0):
        raise RuntimeError("range invalid or timeouts not multiples of 5 within 40..2000 ms")
    reply = request(host, port, token, "scan-start", f"0x{start:04x}", f"0x{end:04x}",
                    timeout_ms, maximum_ms)
    fields = dict(re.findall(r"([a-z_]+)=([^ ]+)", reply))
    if fields.get("scan_version") != "3":
        raise RuntimeError("autonomous scanner requires v3 firmware")
    if fields.get("state") == "storage_error":
        raise RuntimeError("ESP scan storage unavailable; see USB log")
    return reply


def watch_scan(host: str, port: int, token: str, interval: float) -> None:
    if interval < 0.5:
        raise RuntimeError("watch interval must be at least 0.5 seconds")
    while True:
        try:
            reply = request(host, port, token, "scan-status")
            print(reply, flush=True)
            fields = dict(re.findall(r"([a-z_]+)=([^ ]+)", reply))
            if fields.get("scan_version") != "3":
                raise RuntimeError("autonomous scanner requires v3 firmware")
            if fields.get("state") in ("found", "done", "paused", "idle", "storage_error"):
                return
        except (OSError, DiagnosticDisconnect) as exc:
            print(f"status unavailable: {exc}; scan remains on ESP", file=sys.stderr, flush=True)
        time.sleep(interval)


def main() -> None:
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

    scan = sub.add_parser("scan-start", aliases=["find-read-key"],
                          help="start/resume autonomous ESP job, then disconnect")
    scan.add_argument("start", type=number16)
    scan.add_argument("end", type=number16)
    scan.add_argument("--timeout-ms", type=int, default=100,
                      help="initial RX-only timeout, 40..2000 ms in steps of 5")
    scan.add_argument("--max-timeout-ms", type=int, default=500,
                      help="automatic +5 ms limit; errors at the limit pause the job")
    status = sub.add_parser("scan-status")
    status.add_argument("--watch", type=float, nargs="?", const=2.0,
                        help="poll status every N seconds (default 2); Ctrl+C detaches")
    for command in ("scan-pause", "scan-resume", "scan-reset", "partition-install"):
        sub.add_parser(command)
    sub.add_parser("max-baud")

    mem = sub.add_parser("mem16")
    mem.add_argument("key", type=number16)
    mem.add_argument("address", type=number32)

    single = sub.add_parser("eeprom1", help="read one EEPROM byte at a raw protocol address")
    single.add_argument("key", type=number16)
    single.add_argument("address", type=number16)

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
        elif args.command in ("find-read-key", "scan-start"):
            print(autonomous_start(args.host, args.port, token, int(args.start, 0),
                                   int(args.end, 0), args.timeout_ms, args.max_timeout_ms))
        elif args.command == "scan-status":
            if args.watch is None:
                print(request(args.host, args.port, token, "scan-status"))
            else:
                watch_scan(args.host, args.port, token, args.watch)
        elif args.command in ("scan-pause", "scan-resume", "scan-reset", "partition-install"):
            print(request(args.host, args.port, token, args.command))
        elif args.command == "max-baud":
            print(request(args.host, args.port, token, "max-baud"))
        elif args.command == "mem16":
            data = read_block(
                args.host, args.port, token, "memory", int(args.key, 0), int(args.address, 0)
            )
            print(data.hex())
        elif args.command == "eeprom1":
            reply = request(args.host, args.port, token, "eeprom1",
                            args.key, args.address)
            match = re.fullmatch(r"OK kind=eeprom address=0x[0-9a-fA-F]{4} data=([0-9a-fA-F]{2})", reply.strip())
            if match is None:
                raise RuntimeError(reply)
            print(match.group(1).lower())
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


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("Detached. An autonomous scan continues on the ESP.", file=sys.stderr)
