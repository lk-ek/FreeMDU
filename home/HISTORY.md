# Historical development notes

These notes record earlier migrations and research observations. Use [Home](README.md) for the current setup.

## Receiver experiments

With a 10–15 kΩ receiver pulldown, both ports passed repeated local 2400-baud bursts and a single-byte sweep through 4800 baud; 9600 baud was intermittent. These tests did not validate appliance replies.

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

## ID498 monitoring observations

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
