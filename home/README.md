# FreeMDU Home

The FreeMDU home firmware provides a USB-UART bridge for Miele's optical diagnostic interface. It also features MQTT-based integration with Home Assistant.

## Getting started

To communicate with Miele devices via the diagnostic interface, a suitable optical communication adapter is required. The adapter consists of a microcontroller and an [OSRAM Multi TOPLED SFH 7250](https://ams-osram.com/products/leds/multi-color-leds/osram-multi-topled-sfh-7250) infrared emitter and phototransistor.

The firmware currently supports only the Espressif **ESP32-C3** and **ESP32-C6** microcontrollers. Other targets are not yet supported. The transceiver is connected to the microcontroller according to the schematic below:

<img src="adapter.svg" alt="Communication Adapter Schematic" align="right">

Because $R_P$ determines the phototransistor's sensitivity, an appropriate resistance must be selected. In most cases, a value of approximately $`47\,\text{k}\Omega`$ works well.

The standalone firmware uses two independent optical UARTs on a XIAO ESP32-C3:

The default optical polarity matches original FreeMDU: both TX and RX signals
are inverted. Each channel can be configured independently with
`OPTICAL_TX_INVERTED`, `OPTICAL_RX_INVERTED`, `OPTICAL2_TX_INVERTED` and
`OPTICAL2_RX_INVERTED` in `.cargo/local.toml` (the tracked defaults are
`"true"`). The TX GPIO starts at the corresponding UART idle level.

For the direct GPIO current-sink emitter wiring, set both
`OPTICAL_TX_INVERTED = "false"` and `OPTICAL2_TX_INVERTED = "false"` in the
ignored `.cargo/local.toml`; keep the RX inversion `"true"` with the shown
phototransistor/Schmitt receiver. The direct wiring is 3V3 -> 100 ohm -> IR
LED anode -> IR LED cathode -> GPIO TX. The TX pin then rests high (LED off)
and sinks current on transmitted low bits. For a board built with a low-side
MOSFET, remove each MOSFET and bridge its gate pad to its LED-side pad; leave
the GND pad unconnected. If the existing 100-ohm gate resistor is replaced
with 0 ohm, the 100-ohm LED resistor still limits current to roughly 16–18 mA
at 3.3 V. Configure each channel according to its actual wiring.

To test each LED and receiver without an appliance, enter `diag ir-test IR`
or `diag ir-test IR2` in the USB serial console (terminate with Ctrl-J). The
selected LED is on for about 91% of each UART frame for roughly 4.7 seconds.
Between `SEROPT ... ON` and `SEROPT ... OFF`, use a multimeter to check the
SFH 7250 phototransistor output / Schmitt input and the Schmitt output; the
firmware then clears the receiver before returning to normal polling. The
other optical channel continues to operate during the test. Disconnect the
appliance while running this raw IR test.

For a timing test without an oscilloscope, disconnect the appliance and run
`diag baud-sweep IR` or `diag baud-sweep IR2` on the USB serial console. The
selected port sends `55`, `11`, and `00` at 100, 150, 300, 600, 1200, 2400,
4800, 9600, and 19200 baud, stopping at the first incorrect echo or UART
error. Unsupported baud rates are skipped. Results appear as `SEROPT ... OK`,
`FAIL`, or `SKIP`; the port is returned to 2400-8E1 afterward. A failure at
the lowest supported rate points to the optical path or polarity; a failure
only at higher rates points to pulse timing or receiver recovery.

To check the actual protocol frame for intermittent errors at its normal
2400-8E1 rate, disconnect the appliance and run `diag burst-test IR` or
`diag burst-test IR2`. This sends `11 00 00 02` as one continuous UART frame
100 times, compares all four local echo bytes, and reports the trial and
one-based byte position on a failure. It stops after five failed trials and
prints `SEROPT ... BURST END ok=N fail=M`. This checks the local optical path;
it does not verify a reply from an appliance.

| Port | UART | TX | RX | GPIO TX/RX |
| --- | --- | --- | --- | --- |
| IR | UART1 | D3 | D4 | 5 / 6 |
| IR2 | UART0 | D6 | D7 | 21 / 20 |

Pin `10` drives the active-low status LED. The accelerometer defaults to D1/D2 (GPIO3/4); set `PIN_ACCEL_SDA` and `PIN_ACCEL_SCL` in your ignored `.cargo/local.toml` to match its actual wiring. Do not assign a GPIO to both an optical port and the accelerometer. On other boards, override all pin numbers in `.cargo/local.toml` using the board's GPIO map (XIAO ESP32-C6 uses different GPIO numbers for the same D labels).

Both optical UARTs poll independently. After each successful software-ID query,
ID 410 publishes to `washer/...` and ID 498 to `dryer/...` on either port;
availability and MQTT action routing follow that mapping. Unrecognized IDs
are logged and do not publish under either appliance's topics. The ID410 RAM
trace follows ID 410 on either port. USB bridge firmware, the standalone
diagnostic TCP/USB console, the autonomous key scan, and the remote optical
bridge still target UART1 (IR).

### Firmware modes

The firmware can be built in one of two modes depending on your use case. In **bridge mode**, the firmware simply forwards all data between the USB-UART connection and the infrared transceiver. This allows desktop tools, such as the [FreeMDU TUI](../tui), to communicate with the connected device for diagnostics or testing.

In **standalone mode**, the firmware independently polls two optical ports and publishes ID 410 and ID 498 under device-specific MQTT topics for Home Assistant. Put credentials and device-specific settings in the ignored `.cargo/local.toml` (copy [`.cargo/local.example.toml`](.cargo/local.example.toml)); tracked defaults are in [`.cargo/config.toml`](.cargo/config.toml).

### Flashing the firmware

1. Install the [`espflash`](https://github.com/esp-rs/espflash) tool:

```shell
cargo install espflash --locked
```

2. Use the following command to build and flash the firmware:

```shell
./cargo-local run --features esp32c3 --target riscv32imc-unknown-none-elf --release --bin standalone
```

Run this from `home/`. The configured runner flashes over USB and opens the serial monitor; press Ctrl-J to submit a `diag` command. For ESP32-C6 use `esp32c6` and `riscv32imac-unknown-none-elf`. The `bridge` binary uses IR/UART1 only.

## Usage

<img src="ha.svg" alt="Home Assistant Screenshot" align="right">

In bridge mode, connect the microcontroller to your desktop computer via USB. It appears as a USB CDC device and can be used like a standard serial port. In standalone mode, the firmware automatically connects to the configured Wi-Fi network and publishes MQTT data at regular intervals. It supports Home Assistant's [MQTT discovery](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery) feature, which automatically creates entities without manual configuration. The available entities may vary depending on the properties and actions supported by the connected Miele device.

### Receiving properties

Device properties are published to MQTT topics in the following format:

```
freemdu_home/<DEV>/<CHANNEL>/<PROP>/value
```

The `<DEV>` placeholder represents the ESP's hardware address, `<CHANNEL>` is `dryer` or `washer`, and `<PROP>` is the property ID. For example:

```
freemdu_home/b43a45abcdef/dryer/program_options/value
```

Older Home Assistant discovery entries from the single-port firmware may need removing once after this topic migration. Each channel publishes its own availability (`dryer/status` and `washer/status`), in addition to the gateway status.

### Triggering actions

Device actions are triggered by publishing values to MQTT topics with the following format:

```
freemdu_home/<DEV>/<CHANNEL>/<ACTION>/trigger
```

The `<DEV>` placeholder represents the ESP's hardware address, `<CHANNEL>` is `dryer` or `washer`, and `<ACTION>` is the action ID. For example:

```
freemdu_home/b43a45abcdef/washer/start_program/trigger
```

Some actions require parameters, in which case the published value is used as the argument. Actions without parameters ignore the published value. Due to technical limitations, actions requiring parameters are currently not displayed in Home Assistant, but can still be triggered via MQTT.

## Autonomous read-key scan (v3)

Firmware and `diag.py` must both be updated. The scan runs on the ESP without a
connected host. From `home/`:

```sh
./diag.py 10.0.42.155 scan-start 0x0000 0xffff --timeout-ms 40 --max-timeout-ms 500
./diag.py 10.0.42.155 scan-status
./diag.py 10.0.42.155 scan-status --watch 2
./diag.py 10.0.42.155 scan-pause
./diag.py 10.0.42.155 scan-resume
```

`find-read-key` is an alias for `scan-start`. Starting identical bounds and
initial/maximum timeouts is idempotent: it returns the existing job, or resumes
a paused job, retaining its effective timeout and progress. Different settings
require an explicit `scan-reset` first. Reset discards this job's results; normal
start, disconnect and reboot do not. A running job resumes automatically at boot;
a paused or finished job remains paused/finished. Do not change the appliance
while a job is active; every handshake checks its saved software ID.

Status includes state, software ID, current candidate, next sequential candidate,
tested count, effective/minimum/maximum RX timeout, error and increase counts,
and the confirmed key if found. Ctrl+C only detaches the watcher. Results remain
on the ESP and can be retrieved later. MQTT appliance polling and optical bridge
access pause while scanning; the network stack, OTA and status endpoint remain
available. Pause/reset take effect after the current candidate finishes.

Known candidates (including the reported ID1998 key `0x2b67`) are checked once
per timing configuration. Completed candidates and the known-key mask are
committed to a CRC-protected flash journal after every key. A torn write is
ignored; at most the current candidate is repeated after power loss. The old
`.freemdu-read-key-scan.json` on the computer is not imported or modified.

Timeouts are 40..2000 ms, in multiples of 5. The initial default remains 100 ms;
`--timeout-ms 40` explicitly selects the faster setting. The RX deadline starts
AFTER the request and echo have completed, so it excludes transmission time.
Transport errors, partial replies, late input and failed confirmation increase
the effective timeout by 5 ms and retry with a new session. The current candidate is retried; the cursor, completed known candidates, and
tested count are retained, including across reboot. Earlier silence observations
are not automatically rechecked. If a full scan finds no key, optionally use
`scan-reset` followed by `scan-start` with a higher initial timeout for a new pass. At `--max-timeout-ms`, another error pauses
the job for inspection. A software-ID change pauses immediately without trying
unlock on the new device. Full silence after a clean transmission is still not
proof of a wrong key; it requires two attempts but cannot distinguish a lost
reply from actual rejection. Silence alone does not increase the timeout.

## OTA partition-table migration

The original 4 MB layout ends `ota_1` at `0x3f0000`, leaving the last 64 KiB free.
The scanner can use this verified free tail after a normal application OTA,
without requiring an immediate partition-table change. `partitions.csv/bin` now
name it `keyscan` (custom type `0x40`, subtype `0x00`). Using a custom type avoids
the unknown-data-subtype panic in esp-idf-part 0.6.0 during `espflash save-image`.
The journal uses the first 60 KiB; the last 4 KiB at `0x3ff000` are reserved for a
backup of the boot partition-table sector.
All flash users share one serialized HAL instance; OTA and journal writes cannot
overlap within a flash operation.

To install the new table over Wi-Fi, first update to this firmware, pause any
running scan, then run:

```sh
./diag.py 10.0.42.155 scan-pause
./diag.py 10.0.42.155 partition-install
```

Alternatively, on a device without an active saved scan:

```sh
./ota-upload.py 10.0.42.155 --install-scan-partition
```

The latter performs normal application OTA, waits for reboot and requests the
migration. Repeating `partition-install` is a no-op once the target is installed.
Only the exact supplied original table (CRC checked) or the already-migrated
table is accepted. Existing app, OTA metadata, NVS and PHY offsets/sizes never
change. No arbitrary partition-table upload is exposed. The old 4 KiB sector is
backed up and verified before the primary sector is erased; the replacement is
read back and verified. Firmware-side I/O errors trigger a best-effort rollback.

**Keep ESP power stable during migration.** The standard bootloader has only one
partition table. Power loss while rewriting its sector can prevent booting and
requires USB recovery; the flash backup is not an automatic bootloader fallback.
The backup's 4096 bytes at `0x3ff000` can be restored to `0x8000` with a USB flash
tool. Both app slots remain untouched. Application OTA alone cannot repair a
non-booting partition table. The command changes metadata, not the compiled app,
and is intentionally separate from ordinary OTA updates.

## Validation

```sh
python3 -m unittest discover -s tests -v
cargo test --manifest-path scan-tests/Cargo.toml --target x86_64-unknown-linux-gnu
```

The host Rust target above applies to x86_64 Linux. Use your native Rust target
on other hosts (e.g. `aarch64-apple-darwin` on Apple Silicon). The scanner core is
hardware-independent and tested with a NOR flash model, including torn records,
ring wrap, reboot resume, adaptive timeouts, partition boundaries and migration.
The two independent read-key confirmations remain checksum-validated reads after
fresh inactivity resets; this functionality does not request full access or
write appliance memory. The shared registry is `../protocol/read_keys.csv`.

### ID498 EEPROM addressing

ID498 (T4223C, read key `0x2b2c`) uses byte-addressed EEPROM: incrementing
an `eeprom16` address by one shifts the response by one byte. Other IDs retain
the existing word-addressed dump behavior. Raw `eeprom16` addresses remain wire
addresses. Both dump CLIs accept byte offsets.

Start a NEW output file after this fix; do not resume an old ID498 EEPROM dump,
which contains overlapping blocks. Python uses 16-byte reads for ID498 so it
also works with older firmware; firmware `eeprom128` now uses the correct stride.

### EEPROM boundary probe

After updating firmware, `./diag.py HOST eeprom1 KEY ADDRESS` reads exactly one
byte at the raw protocol address. For ID498, compare addresses `0x00ff` and
`0x0100` with key `0x2b2c`. USB equivalent: `diag eeprom1 KEY ADDRESS`.

## Autonomous full-access key scan (HALT test)

Use only with an idle appliance that you can power-cycle. The full-access scan
uses the supplied read key, tries known full-access candidates first, and tests
HALT once per attempt. It performs no RAM/EEPROM writes. A HALT acknowledgement
stops the scan immediately and stores `full_key` with `halt_ack=1`; this is a
single acknowledgement, not an independently confirmed write test.

```sh
./diag.py HOST scan-reset
./diag.py HOST scan-full-start 0x2b2c 0x0000 0xffff --timeout-ms 100
./diag.py HOST scan-status --watch 10
```

`scan-reset` discards the previous scan result: retain the known read key first.
The usual pause/resume/status commands apply. Repeating start requires identical
mode, read key, range and initial timeout settings. Errors retain the candidate
and increase the timeout by 5 ms, pausing at the limit. The RX timeout excludes
HALT transmission and echo. A silent HALT is only counted negative after the
same software ID responds again; partial responses are retried as errors.

Before HALT the candidate is saved as a paused checkpoint. If the ESP restarts
there, or the appliance stops responding after HALT, status shows `pending_key`
and the scan stays paused. This is an uncertain result, not a confirmed key.
Power-cycle/check the appliance before explicitly resuming. No automatic MQTT
or bridge traffic is issued while a full-access hit or paused pending test is
held; explicit diagnostic commands remain available. If the acknowledgement was
lost but the device stays responsive, a candidate can still be missed.

Full scans use FKS4 journal records; existing FKS3 read-scan records remain
readable. Do not downgrade firmware with a full-access job stored: old firmware
cannot interpret that job. Reset the job first if a downgrade is necessary.

## Read with full diagnostic access

`mem16`, `eeprom1`, `eeprom16`, `dump-memory` and `dump-eeprom` accept
`--full-key KEY`. Every session unlocks read access first and full access second,
including renewed sessions inside 128-byte firmware reads. Only reads follow:
these commands do not send HALT or write appliance memory. Updated firmware is
required; distinct `full-*` wire commands prevent silent read-only fallback.
For ID498, `0x0f2f` has produced repeated HALT acknowledgements with read key
`0x2b2c`; additional readable ranges or write capability are not yet verified.

```sh
./diag.py HOST mem16 0x2b2c 0x0480 --full-key 0x0f2f
./diag.py HOST eeprom1 0x2b2c 0x0100 --full-key 0x0f2f
```

Use a fresh output filename for full-access dumps so previous read-only data
is not silently reused by dump resume. A found scan need not be reset to issue
these explicit diagnostics. Pause a running scan first.

## Experimental T4223C / ID498 monitoring

The firmware contains both the W307/ID410 profile and the ID498 profile.
Standalone mode uses two independent optical UARTs. Either machine can be
connected to either port: ID 410 uses `washer/...` MQTT topics, and ID 498 uses
`dryer/...`. The dryer entity IDs use `dryer_*`, preserving existing ID410
entity IDs.

ID498 publishes HA sensor entities every five seconds after initial detection
(which can take DEVICE_PUBLISH_INTERVAL, normally 60 seconds): selected program,
door, experimental run state, software ID, and raw program/door/run/flag values.
Every update reads three validated 16-byte blocks once and derives all entities
from that observation. Unknown selectors, markers and inconsistent door triplets
remain Unknown; raw values remain visible. Sensor entities are used deliberately
so unknown is not silently mapped to false. Communication failures use the
existing device availability mechanism. Monitoring uses read key 0x2b2c only,
never the full-access key, HALT, writes, or addresses at/above 0x0480.

Mappings from labelled captures:
- 0x00b6: 0x0f Ende (physical selector), 0x0e Koch/Bunt Schranktrocken+,
  0x08 Pflegeleicht Schranktrocken+, 0x04 20 min warm, 0x05 15 min kalt.
- 0x0265..0x0267: 00 00 00 closed; 01 01 01 open (repeated close/open/close).
- 0x0270: 0xaa running observed; 0x55 not running observed.
- 0x026a and 0x027e are raw candidate flags, not independently decoded.

No finished notification, remaining time, temperature, anti-crease or natural-end
claim is made. Manual completion in the fixtures means turning the selector to
Ende, not automatic completion. Read key 0x2b2c is confirmed; full key 0x0f2f has
repeated HALT ACKs, not a verified write test. EEPROM is byte-addressed for ID498;
the tested contiguous ranges are EEPROM 0x0000..0x00ff and memory 0x0000..0x047f.
Access beyond them can disrupt diagnostics. Fixtures 0..8 follow the sequence:
Ende, Koch/Bunt, Pflegeleicht, warm, cold, cold door-open, cold started, warm
started, warm manually ended.

For the 15-minute cold run, clear any stored HALT scan first (retain both keys):

```sh
./diag.py HOST scan-reset
./diag.py HOST capture-id498 t4223c-kalt-complete.jsonl --interval 5
```

Start capture before the program. JSONL records contain wall-clock timestamps,
acquisition duration and raw blocks at 0x00b0, 0x0260 and 0x0270; failed samples
are explicit errors. The output must be a new file; Ctrl+C flushes and stops.
Let the program end naturally without moving the selector; record at least two
more minutes with the door closed and note LEDs/drum movement. Capture uses the
existing authenticated diagnostic commands and also works without MQTT. It needs
the computer to stay connected; gaps are not reconstructed. USB `ID498 SNAP`
lines provide the same blocks during MQTT polling, with ESP uptime timestamps.
The current firmware intentionally has no TCP log service.
Full snapshots can still be collected separately up to inclusive address 0x047f.
ID410 polling and the existing ID410 trace remain unchanged.

### ID498 cold-run observations (2026-09-07)

Additional selector positions inferred from the sequential knob sweep: 09 =
Pflegeleicht Schranktrocken, 0d = Schranktrocken/Schonen, 0c = Buegelfeucht.
Existing entity identifiers and the ID410 profile are unchanged.

New HA text sensors: `dryer_motion` interprets 027e as off/pause (0), direction A
(1) or direction B (2). This is a suspected command, not physical motion feedback.
`dryer_phase` separates program active (0270=aa) from inactive states (55).
Motor commands with 55 are labelled possible anti-crease; they never imply a new
program start. Inactive without motor command cannot distinguish waiting from
completion. Unknown markers remain Unknown. No finished event or countdown is
emitted from this single run.

Raw sensors `dryer_post_run_raw` (0260), `dryer_motion_raw` (027e) and
`dryer_transition_raw` (027f) expose the supporting observations. The 0260 marker
returned to zero after briefly becoming one; it is not a latched completion flag.
In the uploaded cold run, aa changed to 55 about 13m12s after the first aa sample;
subsequent motor markers 1 and 2 occurred with 55. Physical direction and the
anti-crease interpretation remain unverified. Existing `dryer_run_state` refers
to the program marker, including its pauses, not drum movement.


## ID498 validation update (2026-09-08)

The annotated capture confirms: opening the door while running changes aa to 55;
closing it leaves 55 and the appliance waits for Start; pressing Start returns aa.
Turning the selector to Ende also changes aa to 55. Selecting cold again without
Start leaves 55. Consequently 55 is not a completion indication. Closed-door idle,
interrupted and post-run states are deliberately not distinguished without history.
An open door with 55 is shown as Door open / inactive; conflicting samples remain
Unknown. No inferred finished, paused-cycle or anti-crease event is emitted.

Additional annotated selector positions: 01 Koch/Bunt Schranktrocken/Schonen,
03 Koch/Bunt Buegelfeucht, 02 Mangelfeucht, 06 Glaetten, 07 Finish Wolle.
Koch/Bunt Schranktrocken WITHOUT plus was not captured unambiguously and remains
unmapped. Some reads took 6–12 seconds and overlapped selector changes; no mapping
is invented for the missing position. All three blocks are sequential, so an
individual snapshot may contain values from opposite sides of a transition.

Direction and anti-crease interpretations above remain research hypotheses.
The motion entity now reports Marker 0/1/2 only; its existing identifier is retained.
