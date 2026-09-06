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

## Reliable read-key scan (scanner v2)

Update the ESP firmware and `diag.py` together. The client rejects old scanner
replies rather than saving unverified results. No full-access key is requested
and no appliance memory is written by this scan.

```sh
python diag.py DEVICE_HOST find-read-key 0x0000 0xffff
python diag.py DEVICE_HOST find-read-key 0x0000 0xffff --timeout-ms 300
python diag.py DEVICE_HOST find-read-key 0x0000 0xffff --recheck
python -m unittest discover -s tests -v
```

The scanner forwards UART and echo errors, drains stale input and allows at
least 3.2 seconds without transmission after broken transactions. Each candidate
needs two clean handshakes followed by silence before recording `NO_RESPONSE`.
Partial responses, failed echoes and late bytes remain inconclusive. A hit is
confirmed twice after separate inactivity resets, using a one-byte read and a
16-byte checksum-validated RAM read at address zero. RAM values need not match
between confirmations. This assumes a device supporting reads at address zero;
a different memory map requires adapting the probe, not excluding every key.

The v2 state file records silent ranges under the software ID, scanner revision,
method and timeout. Silence is an observation, not proof of an incorrect key.
Old v1 negative ranges are preserved as legacy evidence but are not reused.
Changing the timeout or passing `--recheck` repeats previous observations.
`--exclude` only skips ranges for the current invocation and is not persisted as
evidence. Known and saved candidates inside the requested range are checked
first, including during resume. Run only one scan client per state file.

This conservative scan is slower: two probes plus quiet checks take roughly
8.4 hours for all 65536 keys at the default 100 ms timeout, including initial
recovery for 64-key chunks but excluding additional retries and device delays.
Use bounded ranges to limit the time MQTT polling is suspended. An interrupted
chunk is repeated; completed chunks resume from the state file. After three
inconclusive chunk attempts the client stops without recording that chunk.

`../protocol/read_keys.csv` is the shared source for Python and the generated
Rust registry. Each row records a key, reported software IDs and provenance.
The additional `0x2b67` candidate is reported for ID1998 in upstream issue #27;
it is not a confirmed T4223C key. When changing scanner semantics, bump the
firmware scan revision and the Python method/profile version together so old
observations cannot silently exclude candidates.
