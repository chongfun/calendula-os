# X4 Pro hardware baseline and platform start

Implementation continuation, Wave 2B–7C progress, and unrun hardware checks
are recorded in the [agent handoff](2026-11-16-x4-pro-handoff.md). Read it
before resuming code or hardware work.

Status: 2026-11-16, issue [#391](https://github.com/chongfun/calendula-os/issues/391)
(**in progress; not yet feature-complete**).
The purchased unit has completed both the `#339` S3 runtime/EPD-probe proof
(`fw-s3-proof.bin`, 3/3 boots) and two Wave 7C peripheral runs
(`fw-s3-peripherals.bin`, 3/3 boots in `wave7c-peripherals-boots.log`)
non-destructively on `app1 @ 0x7F0000`: Run 1 (`467,328` bytes) rendered the
CalendulaOS `Home` screen cleanly on glass (flipped 180°) and restored CrossPoint
(`wave7c-crosspoint-restore-boot.log`); Run 2 fixed the 180° double-inversion in
`uc8279_x4::fill_gate_row_from_framebuffer`, flashed the updated
`fw-s3-peripherals.bin` (`467,264` bytes), verified 3/3 clean boots on serial,
and left `fw-s3-peripherals.bin` active in `app1 @ 0x7F0000` (now in deep sleep
after idle timeout). Both `wave7c-preflight-verify.log` (Run 1 and Run 2) and
Run 1's `wave7c-restore-verify.log` matched `crosspoint-app1-0x7f0000.bin` over
`5,378,048` bytes. In the working tree, the foundational slices (#336–#339,
#343, and #391 Waves 7A–7C) have landed: `fw-c3` boundary + post-`fw-common`
descriptor/ELF parity check, `fw-common` shared runtime/pad contract,
MCU-neutral `hal-ext` drivers (`Cw2017`, `Bm8563`, `Gt911`,
`EpdBus::{rst_mut, wait_uc_idle_high}`), chip-aware `proto::ota`, sans-IO
`display::epd::uc8279_x4` (forward-order `fill_gate_row_from_framebuffer`) +
`resolve_x4_controller`, and Wave 7C serial-verified `fw-s3` boot/flush/
frontlight-init/RTC/fuel-gauge/touch-probe bring-up. Visual confirmation of the
post-fix upright render on glass, interactive & physical checks (LEDC frontlight
user control/ramp, interactive `Gt911` touch/swipe/Home-key & physical
`GPIO0`/`GPIO7`/`GPIO3` buttons, `GPIO3` deep-sleep wake + pad-hold release),
unimplemented S3 subsystems (1-bit SDMMC via `esp_hal::sdmmc`, S3 Wi-Fi/OTA),
full `#391` hardware qualification, physical C3 X4/X3 smoke tests, and
CrossPoint restoration from Run 2 remain open. This report is not a hardware
sign-off for full Calendula reader firmware.

## Observed on this unit

The device was connected at `/dev/cu.usbmodem2101`. `espflash board-info`
and esptool identified ESP32-S3 revision 0.2, a 40 MHz crystal, 16 MiB flash,
and embedded 8 MiB PSRAM. Secure boot and flash encryption were disabled.
CrossPoint reported 8,388,608 bytes total PSRAM, 8,288,400 free; these are
CrossPoint heap figures, not a Calendula memory budget.

A USB reset with the serial port held open captured the live probe:

```text
Device: xteink_x4_pro
SDMMC card mounted
SDK RTC found
FrontlightMgr begin: attached gpio=8 warm=9 ok=1
bus probe VER=00 0F 68 01 00 FLG=13 -> UltraChip
promoted SSD1677 -> UC8279 800x480 (LUT_VER=68)
MTP[0x000..0x02F]:
A5 A5 1F 20 25 25 3C 00 97 02 02 03 20 02 58 00
00 00 00 00 00 00 00 0F 00 00 68 01 00 00 00 00
00 00 00 00 00 00 00 00 00 00 05 0A 0F 14 50 5F 64 7F
```

CrossPoint selected the **UC8279 X4-family** driver. Its boot logged one
`8279x4_PON` wait of 40 ms and `8279x4_DRF` waits of 1,321, 480 and 480 ms.
Those are individual logged waits, not measured page-turn latency or a visual
assessment. The NVS value also said UltraChip, but the live bus is the evidence.
The `SDK IMU not found` line is not evidence of a failed X4 Pro peripheral:
the issue's board profile does not specify an IMU.

SD mounting, RTC detection and frontlight attachment are firmware-reported
observations. They do not verify filesystem durability, clock accuracy, light
color, touch orientation, physical keys, pad hold levels or standby current.
No Calendula firmware was flashed and no raw SD writes were performed.

## Recovery material

The owner's original stock backup is preserved in the sibling CrossPoint
checkout as `x4-pro-flash-backup-20261115.bin`, 16,777,216 bytes, SHA-256:

```text
5ad993ad14b8b6f252542137f14baded2cb05300df659f9a0fbf9c866c8bc7c1
```

New captures and a backup of the installed CrossPoint image live locally under
`target/bench/x4-pro-2026-11-16/`, excluded from Git. Full flash dumps may
contain device configuration and credentials; retain them locally. The stock
backup was size/hash checked, not restored or compared to the current device.

The first CrossPoint backup was read with:

```sh
esptool --chip esp32s3 --port /dev/cu.usbmodem2101 --baud 921600 \
  read-flash --no-progress 0 0x1000000 \
  target/bench/x4-pro-2026-11-16/crosspoint-full-flash.bin
```

It completed, but a later full-flash verification after application reboots
reported a digest mismatch. Comparing the later stable capture found differences
only in the NVS sector at `0xB000`. A second read at 921600 baud failed with
"Serial data stream stopped" and produced no backup. Retrying at 460800 with
`--after no-reset-stub`, then verifying with `--before no-reset` while the
application remained stopped, succeeded over all 16,777,216 bytes. The final
verified file is `crosspoint-stable-flash.bin`, SHA-256:

```text
ee9076e21f6370873d1cdbb667f370a26fd239f4ebf6efc2cc351153671e918c
```

The verify command reset the device back into CrossPoint afterward. Subsequent
non-destructive `app1 @ 0x7F0000` proof flashing and CrossPoint restore
verified both `preflight-verify.log` and `restore-verify.log` against
`crosspoint-app1-0x7f0000.bin`, and confirmed CrossPoint booting cleanly in
`crosspoint-restore-boot.log`. The read/verify logs are `stable-backup-read.log`
and `stable-backup-check.log`.

The captured partition table has NVS at `0x9000`, OTA data at `0xE000`
(`0xE000` `seq=3, state=2`; `0xF000` `seq=4, state=2`, selecting `app1`),
app0 at `0x10000` (size `0x7E0000`), app1 at `0x7F0000` (same size),
SPIFFS at `0xFD0000`, and coredump at `0xFE4000`. Do not assume Calendula's
C3 partition layout or application identity applies to these slots.

For non-destructive S3 proof testing on `app1 @ 0x7F0000` (preserving the
bootloader, partition table, `otadata`, NVS, `app0`, SPIFFS, and coredump),
artifacts in `target/bench/x4-pro-2026-11-16/` record both the baseline
CrossPoint slot and the S3 proof builds:

- `crosspoint-app1-0x7f0000.bin`: **5,378,048 bytes**, SHA-256
  `bbcfb288ee0a0ba6acf665d9ed9efb86565f188754fc6125162027f06825d034` (exact
  slice of `crosspoint-stable-flash.bin` starting at `0x7F0000`).
- `fw-s3-wave4-serial-only.bin`: **129,104 bytes**, SHA-256
  `da616a3019c9af9eb4ffebbb1318034b76ec07e385ec6a6480908bd385a124b4` (earlier
  Wave 4 serial-only build).
- `fw-s3-proof.bin`: **135,536 bytes**, SHA-256
  `b413adadec394100c4276674ccfd28b87fb097b846bde6ebebe9429f37338415` (hardware-verified
  post-Wave-5D runtime + `epd_probe` image; `esptool image-info`: ESP-IDF
  image version 1, `Segments: 6`, entry `0x4037872c`, `Chip ID: 9 (ESP32-S3)`,
  `16MB` `80m` `DIO`, `Checksum: 0x2d (valid)`, `Validation hash:
  53f2bfaa23705b15ffba5aaf37f8598a150f082a8f674a209844e560574872a2 (valid)`,
  `Project name: CalendulaOS X4 Pro proof`, `App version: 0.9.0`, `ELF file
  SHA256: 5f2a8124f998f3a518f513d3b2516c64ea999b1b8a973363203059a1c0de17f5`).
- `fw-s3-peripherals.bin`: **467,264 bytes**, SHA-256
  `1dfeec02545d7f90232ff6ce85131dd61ef2d9a8a2ffe5075a5eb31bc200b41f` (Wave 7C
  Run 2 peripheral build with forward-order `fill_gate_row_from_framebuffer`,
  **serial boot-verified across 3 runs on `app1 @ 0x7F0000` and currently still
  resident on hardware in deep sleep**; `esptool image-info`: ESP-IDF image
  version 1, `Segments: 6`, entry `0x403788c8`, `Chip ID: 9 (ESP32-S3)`, `16MB`
  `80m` `DIO`, `Checksum: 0x66 (valid)`, `Validation hash:
  3411cc12d388a928b01edc2201b03436b08c5d00c7786d8342fe4f3ff64b863d (valid)`,
  `Project name: CalendulaOS X4 Pro proof`, `App version: 0.9.0`, `ELF file
  SHA256: 7b9ba0cb50ba7b5995fd570d64dc6120415881219a17f56ba381e720b294d9e6`).

Wave 7C non-destructive `app1 @ 0x7F0000` peripheral runs & Run 1 CrossPoint
restore (executed on `/dev/cu.usbmodem2101`, after the Wave 6 `#339` proof
below):

- **Run 1 (`467,328` bytes, prior to orientation fix) & CrossPoint restore**:
  `wave7c-restore-write.log` restored `5,378,048` bytes (`3,627,465`
  compressed) in `46.0` s (`934.8 kbit/s`); `wave7c-restore-verify.log` matched
  all `5,378,048` bytes; and `wave7c-crosspoint-restore-boot.log` confirmed
  CrossPoint booting cleanly (`Device: xteink_x4_pro`, `SDMMC card mounted`,
  `RTC clock read: 2026-11-17 07:57`, `promoted SSD1677 -> UC8279 800x480
  (LUT_VER=68)`). Visually, the user observed the CalendulaOS display rendered
  cleanly on Run 1, flipped 180° — traced to double-inversion in
  `fill_gate_row_from_framebuffer` on top of `Framebuffer::bytes()`.
- **Run 2 (`467,264` bytes, post-fix upright scan) pre-flight, flash & 3-boot
  serial capture (`wave7c-preflight-verify.log`, `wave7c-flash.log`,
  `wave7c-peripherals-boots.log`)**: pre-flight `verify-flash` matched
  `0x521000` (`5,378,048`) bytes at `0x007f0000`; `wave7c-flash.log` wrote
  `467,264` bytes (`208,516` compressed) at `0x007f0000` in `3.3` s
  (`1131.4 kbit/s`, `Hash of data verified.`); across three consecutive
  `rst:0x15 (USB_UART_CHIP_RESET),boot:0xb (SPI_FAST_FLASH_BOOT)` boots with 0
  panics/errors, `fw-s3` logged:
  - `s3-proof: epd_probe verdict=uc81xx-confirmed variant=uc8279-x4 ver=00 0F 68 01 00 flg=13 lut_ver=68 mtp_valid=true` (`mtp_prefix=A5 A5 1F 20 25 25 3C 00 97 02 02 03 20 02 58 00`)
  - `s3-frontlight: init ok cool_gpio=8 warm_gpio=9 freq_hz=10000 bits=10 duty=(0, 0)`
  - `s3-display: busy pre-reset high=Some(true)`, `uc8279-x4 init done busy_high=Some(true) t_ms=92`, `PON busy_ms=39`, `DRF mode=Full busy_ms=1321`, `flush ok view=Home kind=Boot mode=Full t_ms=1454`
  - `s3-rtc: bm8563 init=Ok(0)` with live times `2026-11-17 08:11:32` / `08:11:42` / `08:11:52` (weekday `2`) across Boots 1–3
  - `s3-gauge: cw2017 ready=Ok(true) soc=Some(100)` with `mv=Some(4364)` / `Some(4365)` / `Some(4364)` across Boots 1–3
  - `s3-touch: gt911 addr=0x5D detected=true`, `s3-power: charger_active=false`
  - `s3-proof: board=x4-pro chip=esp32s3 shared_app=started`, `app: started`, and heartbeats `t_ms=2322`, `4322`, `6322`, `8322`
  - Scope note & hardware state: Run 2 was not restored back to CrossPoint
    yet (`app1 @ 0x7F0000` still holds `fw-s3-peripherals.bin`, now asleep),
    and each capture stopped at ~9.5 s before the `Home` idle timeout, so
    visual upright glass inspection, frontlight duty ramp, interactive
    touch/button actuation, and live `GPIO3` deep-sleep wake remain unverified.

Non-destructive `app1 @ 0x7F0000` proof flash, 3-boot live verification, and
CrossPoint restore (executed on `/dev/cu.usbmodem2101`):

```sh
# Flash the S3 runtime + EPD probe proof only into app1 (0x7F0000)
esptool --chip esp32s3 --port /dev/cu.usbmodem2101 --baud 460800 \
  write-flash 0x7F0000 target/bench/x4-pro-2026-11-16/fw-s3-proof.bin

# Restore CrossPoint app1 (0x7F0000) afterward
esptool --chip esp32s3 --port /dev/cu.usbmodem2101 --baud 460800 \
  write-flash 0x7F0000 target/bench/x4-pro-2026-11-16/crosspoint-app1-0x7f0000.bin
```

- Pre-flight & post-restore checks (`preflight-verify.log`, `restore-write.log`,
  `restore-verify.log`): `esptool v5.1.2` verified `0x521000` (`5,378,048`)
  bytes at `0x007f0000` against `crosspoint-app1-0x7f0000.bin` both before
  flashing and after writing (`Wrote 5378048 bytes (3627465 compressed) at
  0x007f0000 in 46.8 seconds (920.2 kbit/s). Hash of data verified.`), and
  `crosspoint-restore-boot.log` confirmed CrossPoint booting cleanly afterward
  (`Device: xteink_x4_pro`, `SDMMC card mounted`, `promoted SSD1677 -> UC8279
  800x480 (LUT_VER=68)`).
- Live `s3-proof` 3-boot serial capture (`s3-proof-boots.log`): across three
  consecutive `rst:0x15 (USB_UART_CHIP_RESET),boot:0xb (SPI_FAST_FLASH_BOOT)`
  boots with zero panics, `fw-s3` logged:
  - `s3-proof: epd_probe verdict=uc81xx-confirmed variant=uc8279-x4 ver=00 0F 68 01 00 flg=13 lut_ver=68 mtp_valid=true`
  - `s3-proof: epd_probe mtp_prefix=A5 A5 1F 20 25 25 3C 00 97 02 02 03 20 02 58 00`
  - `s3-proof: board=x4-pro chip=esp32s3 shared_app=started peripherals=epd-probe-only`
  - `app: started` / `s3-proof: synthetic empty catalog` / `s3-proof: shared render view=Home kind=Boot panel_io=false`
  - heartbeats `t_ms=2175`, `4175`, `6175` (Boots 1–2) and `2176`, `4176`, `6176` (Boot 3).

CrossPoint's local checkout was `867a1210` during inspection. Its documented
X4 Pro build/upload path is `pio run -e x4pro -t upload`; that command was
not run in this session and does not establish which revision is installed.
Reproduce the owner's successful flashing procedure and test rollback before
replacing the application with the first Calendula S3 image. GPIO0/Left must
not be used as an application recovery chord at reset; it is a boot strap.

## Implementation status & order

1. **C3 boundary (`#336`, landed in working tree):** renamed `fw` to `fw-c3`,
   root Cargo selects host members by default, `tools/cargo-c3.sh` isolates C3
   target/memory policy, and OTA identities/release asset names stay unchanged.
   Post-`fw-common` binary/descriptor/stack checks passed (`post-common-comparison.txt`);
   a physical X3/X4 smoke test is still required because only an X4 Pro S3 unit
   is connected.
2. **Shared runtime (`#337`, landed in working tree):** `fw-common` owns shared
   `app::run`, `channels`, `app_desc`, `log`, `bench`, `pad`, `upload`, and
   `catalog`; `storage` keeps its `Card`, `Host`, and `Platform` seams and
   routes logs through `fw_common::runtime_log!` without selecting a chip.
3. **Platform ownership (`#338`, landed in working tree):** moved C3 `board_probe`,
   `epd_probe`, and `rtc` into `fw-c3`; `hal-ext` retains MCU-neutral
   `bq27220` and `spi_dma` plus new `cw2017`, `bm8563`, and `gt911` drivers;
   `fw-common::pad` defines `PadHoldSpec` and `ReconciledOutput`; and
   `tools/check.sh dep-boundaries` (plus CI) enforces chip/crate boundaries.
4. **S3 proof (`#339`, verified on hardware via `app1 @ 0x7F0000`, CrossPoint restored):**
   pinned the Xtensa toolchain (`tools/s3-toolchain.env`, `tools/setup-s3.sh`,
   `tools/cargo-s3.sh`) and `esp-hal` rev `9a5810bbc` (`#6497`/`#6816`), wired
   `fw-s3` (`board.rs`, `epd_probe.rs`, `main.rs`) through `fw_common::app::run`
   and `ReconciledOutput`, booted `fw-s3-proof.bin` (`135,536` bytes) across
   3 consecutive `USB_UART_CHIP_RESET` runs on `/dev/cu.usbmodem2101`
   (`s3-proof-boots.log`, confirming `uc81xx-confirmed` / `uc8279-x4`
   `lut_ver=68` and `shared_app=started`), and restored and verified CrossPoint
   in `app1 @ 0x7F0000` (`restore-verify.log`, `crosspoint-restore-boot.log`).
5. **X4 Pro peripherals (`#391`, in progress — Wave 7A–7C serial bring-up verified; upright visual check, interactive/sleep & SDMMC/Wi-Fi/OTA open):**
   `proto::ota` chip-aware validation (`CHIP_ID_ESP32S3`, `IDENTITY_X4_PRO`),
   `display::epd::{uc8279_x4, probe::resolve_x4_controller}` (with
   `fill_gate_row_from_framebuffer` fixed to forward-order `fill_gate_row` after
   Wave 7C Run 1 rendered cleanly 180° flipped on glass), `hal-ext`
   `Cw2017`/`Bm8563`/`Gt911` and `EpdBus::{rst_mut, wait_uc_idle_high}`, and
   `fw-s3` Wave 7B modules (`board.rs`, `display_flush.rs`, `frontlight.rs`,
   `input.rs`, `rtc_sleep.rs`, and `main.rs` with `ui::app_render` +
   `RefreshPlanner`) booted cleanly across 3 consecutive runs in Wave 7C Run 2 on
   `/dev/cu.usbmodem2101` (`wave7c-peripherals-boots.log`, confirming live
   `epd_probe`, `uc8279-x4` init + `Home` full refresh, LEDC frontlight init,
   `Bm8563` RTC, `Cw2017` fuel gauge, and `Gt911` detection; Run 2 remains
   flashed in `app1 @ 0x7F0000`, asleep). Visual confirmation that Run 2's
   `Home` render is upright, user-facing LEDC frontlight ramp/illumination,
   interactive touch/button actuation, live `GPIO3` deep-sleep wake, 1-bit
   native SDMMC (`esp_hal::sdmmc`), and S3 Wi-Fi/OTA remain open.
6. **Qualification (unrun):** reader/storage/Wi-Fi/OTA flows, repeated remounts,
   alternating display/SD traffic, 50 sleep/wake cycles, standby current, and
   physical pad-hold voltage measurements with instrumentation, plus physical C3
   X4/X3 smoke tests. USB mass storage remains optional follow-up work.

For this board, retain the issue's corrected EPD pins (12/11/13/18/14/6),
SDMMC pins (41/42/40, enable 5 active-low), touch INT10/RST4 and active-low
power 2, and early peripheral enable GPIO1 high. Never reuse the C3 board
probe on S3 USB GPIO19/20. The panel rail stays powered in sleep, so EPD RST
must be held high; Sticky's gated-rail reset-low policy does not apply.

## Sources and limits

- [Issue #391](https://github.com/chongfun/calendula-os/issues/391) defines
  scope and the per-board hardware requirements.
- [FreeInk X4 Pro notes](https://github.com/Free-Ink/freeink-sdk/blob/main/docs/xteink-x4pro-support.md)
  provide reference wiring, not proof that every peripheral on this unit passed.
  The notes still contain an old reversed INT/RST pair in the I2C-summary prose;
  use the corrected touch section and issue pin map.
- [CrossPoint build configuration](https://github.com/crosspoint-reader/crosspoint-reader/blob/develop/platformio.ini)
  supplies the existing `x4pro` environment.

Raw local evidence: `board-info.log`, `passive-serial.log`, `reset-serial.log`,
`flash-backup.log`, and `backup-verify.log` in the capture directory above.
