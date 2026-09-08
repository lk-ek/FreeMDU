# FreeMDU optical adapter: XIAO ESP32-C3 + SFH7250

Files in this project:

- `freemdu_optical_adapter.kicad_sch` — KiCad 10-era schematic.
- `freemdu_optical_adapter_legacy.sch` — import fallback if a particular KiCad build rejects the hand-generated modern file.
- `symbols/FreeMDU_Optics.kicad_sym` — local symbols for SFH7250 and XIAO ESP32-C3.
- `footprints/FreeMDU_Optics.pretty/SFH7250_Multi_TOPLED.kicad_mod` — SFH7250 footprint based on the ams OSRAM recommended copper land pattern.
- `sym-lib-table`, `fp-lib-table` — project-local library registration.

## Pin assignment

FreeMDU upstream defaults to GPIO0 = RX and GPIO1 = TX. Those GPIOs are **not exposed on the XIAO ESP32-C3 header**, so this schematic deliberately uses:

- RX: XIAO `D4` / ESP32-C3 `GPIO6`
- TX: XIAO `D5` / ESP32-C3 `GPIO7`

Configure FreeMDU accordingly.

## Optical circuit

- SFH7250 pin 1 = IR LED anode
- pin 2 = IR LED cathode
- pin 3 = phototransistor collector
- pin 4 = phototransistor emitter
- `R2 = 47 kΩ` pull-up on RX, matching the FreeMDU upstream recommendation.
- `R1 = 100 Ω` series resistor for conservative direct GPIO drive. At 3.3 V and a typical SFH7250 forward voltage around 1.6 V, this is roughly 17 mA before GPIO output resistance. If field testing shows insufficient optical margin, use a transistor/MOSFET driver rather than simply lowering R1 aggressively.

## Footprint geometry

The footprint follows the datasheet's 4-pad recommended copper geometry:
approximately 0.9 × 1.5 mm pads on centers ±0.85 mm X and ±1.50 mm Y.
Pin-1 marking is at the lower-left corner in the datasheet top view.

Datasheet:
https://look.ams-osram.com/m/4b1e110b8b52f90d/original/SFH-7250.pdf

References:
- ams OSRAM SFH7250 datasheet v1.6, 2023-08-09
- FreeMDU Home README
- Seeed Studio XIAO ESP32-C3 pin map


## RX signal conditioning

The SFH7250 phototransistor emitter node (`IR_RX_RAW`) is buffered by a TI SN74LVC1G17 non-inverting Schmitt-trigger at 3.3 V before reaching `IR_RX_D4`. The existing 47 kΩ pull-down remains the optical sensitivity/load resistor.
