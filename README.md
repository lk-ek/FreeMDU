# FreeMDU

<p align="center"><img src="demo.gif" alt="FreeMDU demo"></p>

FreeMDU provides tools for Miele's optical diagnostic interface: a [protocol library](protocol/README.md), a [terminal UI](tui/README.md), and [ESP firmware](home/README.md) for a USB bridge or MQTT/Home Assistant monitoring. It is an independent alternative to the proprietary Miele Diagnostic Utility. Background on the interface is in the [original research post](https://medusalix.github.io/posts/miele-interface).

> [!CAUTION]
> Diagnostic actions can affect an appliance. This project is experimental; use write or full-access functions only when you understand the device and their effects.

## Supported devices

The appliance reports a *software ID*. FreeMDU uses that ID to select a protocol implementation; the implementation supplies its device kind, properties and actions. A software ID identifies firmware, not necessarily a unique model. Confirmed combinations:

| Software ID | Device | Board | Microcontroller | Interface | Support |
| --- | --- | --- | --- | --- | --- |
| 218 | W 985 | EDPW 213 | Mitsubishi M37451MC | Check inlet (PC) | Protocol profile |
| 324 | W 980 | EDPW 213 | Mitsubishi M37451MC | Check inlet (PC) | Protocol profile |
| 360 | Bare board | EDPW 223-A | Mitsubishi M38078MC-065FP | Check inlet (PC) | Protocol profile |
| 419 | Bare board | EDPW 206 | Mitsubishi M37451MC-804FP | Check inlet (PC) | Protocol profile |
| 469 | W 487 S | EDPW 228-A | Mitsubishi M38078MF | Check inlet (PC) | Protocol profile |
| 517 | G 7804 | EGPL 061 | Mitsubishi M38079EFFP | (PC) DOS | Protocol profile |
| 605 | G 651 I PLUS-3 | EGPL 542-C | Mitsubishi M38027M8 | Salt (PC) | Protocol profile |
| 629 | W 2446 | EDPL 126-B | Mitsubishi M38079MF-308FP | Check inlet (PC) | Protocol profile |
| 2088 | W 3241 | EDPL 162-B | Mitsubishi M38079EFFP | Check inlet (PC) | Protocol profile |
| 2895 | W 3164 | EDPL 151-B | Mitsubishi M38079MF-322FP | Check inlet (PC) | Protocol profile |
| 410 | W 307 | — | — | Optical diagnostic port | Experimental read-only monitoring |
| 498 | T 4223 C | — | — | Optical diagnostic port | Experimental read-only monitoring |

Similar models may use a compatible software ID. Query the ID before assuming compatibility.

## Choose a mode

- **Diagnostics:** flash the ESP [bridge firmware](home/README.md#firmware-modes), connect the optical adapter and run the [TUI](tui/README.md).
- **Home Assistant:** flash the [standalone firmware](home/README.md#build-and-flash). Its two IR ports poll independently and publish MQTT discovery and values.
- **Custom tools:** use the [protocol crate](protocol/README.md) with a supported transport.

The standalone firmware maps a connected protocol profile's `DeviceKind::WashingMachine` to `washer/...` and `DeviceKind::TumbleDryer` to `dryer/...`, independently of the port. The MQTT mapping currently has one channel per kind. Other device kinds are supported by the protocol/TUI but have no standalone MQTT channel. Device-specific experimental features remain limited to their respective IDs. See [Home](home/README.md#device-detection-and-mqtt) for routing, pinout, configuration and limitations.

Past receiver tests, EEPROM investigations and protocol observations are in the [historical notes](home/HISTORY.md).

## License and attribution

This project is independent of and not affiliated with Miele & Cie. KG. Product names are used only to identify compatible appliances.

Licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT). Contributions submitted for inclusion are dual licensed under the same terms unless explicitly stated otherwise.
