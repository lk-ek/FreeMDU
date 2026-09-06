# FreeMDU Home

The FreeMDU home firmware provides a USB-UART bridge for Miele's optical diagnostic interface. It also features MQTT-based integration with Home Assistant.

## Getting started

To communicate with Miele devices via the diagnostic interface, a suitable optical communication adapter is required. The adapter consists of a microcontroller and an [OSRAM Multi TOPLED SFH 7250](https://ams-osram.com/products/leds/multi-color-leds/osram-multi-topled-sfh-7250) infrared emitter and phototransistor.

The firmware currently supports only the Espressif **ESP32-C3** and **ESP32-C6** microcontrollers. Other targets are not yet supported. The transceiver is connected to the microcontroller according to the schematic below:

<img src="adapter.svg" alt="Communication Adapter Schematic" align="right">

Because $R_P$ determines the phototransistor's sensitivity, an appropriate resistance must be selected. In most cases, a value of approximately $`47\,\text{k}\Omega`$ works well.

By default, the firmware uses the `UART1` peripheral for infrared communication, with pin `0` as RX and `1` as TX. Pin `10` can be connected to an active-low status LED. All pin assignments can be modified in the [`.cargo/config.toml`](.cargo/config.toml) file.

### Firmware modes

The firmware can be built in one of two modes depending on your use case. In **bridge mode**, the firmware simply forwards all data between the USB-UART connection and the infrared transceiver. This allows desktop tools, such as the [FreeMDU TUI](../tui), to communicate with the connected device for diagnostics or testing.

In **standalone mode**, the firmware connects to a Wi-Fi network and periodically publishes operational properties and actions from the connected Miele device via MQTT. This mode is intended for integration into home automation systems such as Home Assistant. No desktop connection is required, but the Wi-Fi and MQTT configuration must be specified in the [`.cargo/config.toml`](.cargo/config.toml) file before flashing the firmware.

### Flashing the firmware

1. Install the [`espflash`](https://github.com/esp-rs/espflash) tool:

```shell
cargo install espflash --locked
```

2. Use the following command to build and flash the firmware:

```shell
cargo run --features esp32c6 --target riscv32imac-unknown-none-elf --release --bin <MODE>
```

Replace `<MODE>` with the desired firmware mode (`bridge` or `standalone`). For the ESP32-C3, substitute `esp32c3` and `riscv32imc-unknown-none-elf`.

## Usage

<img src="ha.svg" alt="Home Assistant Screenshot" align="right">

In bridge mode, connect the microcontroller to your desktop computer via USB. It appears as a USB CDC device and can be used like a standard serial port. In standalone mode, the firmware automatically connects to the configured Wi-Fi network and publishes MQTT data at regular intervals. It supports Home Assistant's [MQTT discovery](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery) feature, which automatically creates entities without manual configuration. The available entities may vary depending on the properties and actions supported by the connected Miele device.

### Receiving properties

Device properties are published to MQTT topics in the following format:

```
freemdu_home/<DEV>/<PROP>/value
```

The `<DEV>` placeholder represents the device's hardware address and `<PROP>` is the property ID. For example:

```
freemdu_home/b43a45abcdef/program_options/value
```

### Triggering actions

Device actions are triggered by publishing values to MQTT topics with the following format:

```
freemdu_home/<DEV>/<ACTION>/trigger
```

The `<DEV>` placeholder represents the device's hardware address and `<ACTION>` is the action ID. For example:

```
freemdu_home/b43a45abcdef/start_program/trigger
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
the effective timeout by 5 ms and retry with a new session. Previous silence
observations are rechecked at the higher timeout rather than permanently
excluding a potentially correct key. At `--max-timeout-ms`, another error pauses
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
