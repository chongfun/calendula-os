# X4 Pro implementation handoff — 2026-11-16

## Start here

This is unfinished work for [#391](https://github.com/chongfun/calendula-os/issues/391)
(**Wave 7C serial hardware bring-up verified across Run 1 & Run 2;
180° orientation fix flashed to `app1 @ 0x7F0000` and currently asleep on hardware;
upright visual check, interactive/sleep checks, and remaining `#391` subsystems open**).
The owner explicitly authorized including the shared S3 prerequisites (#343,
including #336–#339), and connected an X4 Pro on `/dev/cu.usbmodem2101` for
verification. The non-destructive `#339` runtime/EPD-probe proof
(`fw-s3-proof.bin`) and Wave 7C Run 1 (`fw-s3-peripherals.bin`, `467,328` B)
each verified 3/3 boots on `app1 @ 0x7F0000` and restored CrossPoint; on Run 1,
the owner visually confirmed the CalendulaOS display rendered cleanly on the EPD
glass, flipped 180°. The double-inversion bug in
`display::epd::uc8279_x4::fill_gate_row_from_framebuffer` was fixed, and Wave 7C
Run 2 rebuilt (`fw-s3-peripherals.bin`, `467,264` B), flashed to `app1 @ 0x7F0000`,
and verified 3/3 boots on serial. **Important hardware state:** after Wave 7C
Run 2 Boot 3, `fw-s3-peripherals.bin` (`467,264` B) remained in `app1 @ 0x7F0000`
and entered deep sleep after its `Home` idle timeout (`/dev/cu.usbmodem2101` is
currently absent). Pressing Power (`GPIO3`) wakes `fw-s3-peripherals.bin`
directly.

Checkout: `/Users/kyle/Documents/GitHub/calendula-os`, branch
`codex/391-x4-pro-platform-start`, branched from `b60b75b7`.
Waves 1–7C (`#336`–`#339`, `#343`, and `#391` bring-up) are committed locally on
`codex/391-x4-pro-platform-start` (not pushed).
No push, release, tag, or PR was created. Issue #391 was assigned to chongfun;
project insertion failed because the token lacks `read:project`. No issue
comment was posted.

Read repository `AGENTS.md`, architecture, implementation plan and relevant
`docs/agents/` before continuing. Read the companion
[hardware report](2026-11-16-x4-pro-bring-up.md) for measured evidence, wiring,
recovery hashes and qualification requirements. Both documents track Wave 7C
progress: the shared runtime, MCU-neutral drivers, chip-aware OTA/UC8279
foundations, `#339` live S3 runtime/EPD-probe hardware proof, and Wave 7C Run 2
serial peripheral bring-up (`fw-s3-peripherals.bin`, 3/3 boots, upright row-order
fix flashed on `app1 @ 0x7F0000`) are in place, while visual confirmation of the
fixed upright render, interactive/sleep checks, 1-bit SDMMC storage, S3
Wi-Fi/OTA, qualification, and CrossPoint restoration remain open under `#391`.

## Implemented in the working tree

### C3 boundary (#336)

- Renamed `fw` to `fw-c3`, including Cargo package, scripts, workflows and
  operational documentation. Existing release asset names and OTA project
  identities remain unchanged.
- Root Cargo defaults to host library members. Embedded C3 commands use
  `tools/cargo-c3.sh`, which passes an explicit target. C3-specific environment
  policy is in `fw-c3/cargo-config.toml`; the root compiler pin is unchanged.
- The wrapper must pass `--target` on the command line. An earlier config-only
  target failed under external `cargo clippy`; that failure was fixed.
- The rename alone passed both C3 firmware matrices, release packaging and
  descriptor/memory comparisons. Those results predate subsequent extraction
  and must not be treated as final verification of the current tree.

### Shared runtime (#337)

- Added host-testable, `no_std`, unsafe-forbidden `fw-common`.
- Moved the real app orchestration from `fw-c3/src/tasks/app.rs` into
  `fw-common/src/app.rs`. The C3 task wrapper obtains the initial saved SSID and
  calls `fw_common::app::run`. The shared future has no executor task attribute.
- Moved platform-neutral channels, cancellation history, catalog and upload
  types into common (`fw_common::channels`, `fw_common::catalog`,
  `fw_common::upload`). `fw-c3` retains `WAKE_PIN_REQUESTS`,
  `WAKE_PIN_HANDOFF`, and `SYNC_LOANS` carrying hardware `Input` and wireless
  memory-loan values. Preserve these ownership boundaries.
- Added a safe critical-section-protected logging callback, installed by C3
  and S3 (`fw_common::log`). Storage and reader-cache retain their `esp-log`
  feature names, but route through `fw_common::runtime_log!` instead of
  depending on `esp-println` or selecting a chip.
- Added optional bench callback hooks (`fw_common::bench`); C3 installs them
  under `bench-selftest`.
- Extracted the ESP application descriptor constructor into
  `fw-common/src/app_desc.rs` (`EspAppDesc`, `new`, `desc_field`) and switched
  C3 to it, with unit tests covering layout and X4/X3 fields.
- Added MCU-neutral pad sleep-level policy and deep-sleep hold reconciliation
  in `fw-common/src/pad.rs` (`PadLevel`, `Polarity`, `epd_reset_sleep_level`,
  `HoldCapability`, `PadHoldSpec`, `ReconciledOutput`) with host tests for
  Sticky, X4 Pro, C3 X3/X4, and error-preserving hold release.
- Common deliberately does not select portable-atomic's critical-section
  fallback. An early attempt conflicted with C3's single-core policy; removed.

### Neutral HAL, display, and OTA (#338 + #391 prep)

- Moved C3-only `board_probe`, `epd_probe`, and `rtc` from `hal-ext/src/` to
  `fw-c3/src/`, updating callers and resolving the unused `rtc.rs` light-sleep
  helper clippy issue. Removed chip-specific dependencies from `hal-ext` and
  added it to host workspace defaults.
- `hal-ext` holds MCU-neutral `embedded-hal` / `embedded-hal-async` drivers:
  `bq27220`, `spi_dma` (including Wave 7A `EpdBus::rst_mut` and
  `EpdBus::wait_uc_idle_high` for `BusyPolarity::UcIdleHigh`), plus `cw2017`
  (`Cw2017`, `X4_PRO_BATINFO`, `decode_millivolts`, `DEFAULT_ADDR = 0x63`),
  `bm8563` (`Bm8563`, `RtcDateTime`, `DEFAULT_ADDR = 0x51`), and `gt911`
  (`PRIMARY_ADDR = 0x5D`, `ALT_ADDR = 0x14`, `TouchTransform` with X4 Pro
  `coords_at_byte0` landscape mapping, and `STATUS_HAVE_KEY` Home key decode).
- `proto::ota` adds `CHIP_ID_ESP32C3 = 5`, `CHIP_ID_ESP32S3 = 9`,
  `IDENTITY_X4_PRO = "CalendulaOS X4-Pro u1"`, `BOARD_X4_PRO = "X4-Pro"`,
  `is_flash_mapped_for_chip`, `validate_image_for_chip`, and
  `validate_flash_image_for_chip` while preserving C3 OTA defaults.
- `display::epd` adds `uc8279_x4` (sans-IO 800×480 UC8279 X4-family protocol:
  `TRES_HEIGHT = 600`, `GATE_OFFSET = 120`, `PSR_INIT`/`PSR_OTP`,
  `INIT_SEQUENCE`, `flush_plan`, `sleep_plan`, and
  `fill_gate_row_from_framebuffer`, fixed in Wave 7C Run 2 to delegate directly
  to forward-order `fill_gate_row(fb.bytes(), ...)` matching
  `Uc8279X4Driver::streamPlane` without double-inverting `Framebuffer`'s baked
  orientation) and `probe::resolve_x4_controller`
  (`X4ControllerVariant::{Ssd1677, Uc8179, Uc8279X4}`).
- Added `tools/check_dep_boundaries.py`, `tools/check.sh dep-boundaries` (wired
  into `tools/check.sh fast`), `tools/check.sh s3`, and `.github/workflows/ci.yml`
  jobs `dep-boundaries` and `firmware-s3`.

### S3 proof (#339, verified on hardware via `app1 @ 0x7F0000`; CrossPoint restored)

- Added separate `fw-s3` Cargo workspace (excluded from the root workspace) to
  avoid unifying C3 and S3 HAL features, with its own `fw-s3/Cargo.lock`.
- Added `tools/s3-toolchain.env`, `tools/setup-s3.sh`, and `tools/cargo-s3.sh`.
  Exact pins: espup `0.19.0` and Espressif Rust `1.100.0.0`, toolchain name
  `calendula-esp-1.100.0.0`, compiler commit
  `860c29730837cf4d09ed05567438603730388d2a`. Installer SHA-256 values are in
  `tools/s3-toolchain.env`. This Espressif release is a prerelease; root Rust
  was not bumped.
- Resolved the earlier `__pre_init` `l32r` link failure and `esp-backtrace`
  usage by pinning `esp-hal` / `esp-bootloader-esp-idf` / `esp-rtos` /
  `esp-println` / `esp-backtrace` in `fw-s3/Cargo.toml` to `esp-rs/esp-hal` rev
  `9a5810bbc4b8fed8cb218200e45d4f6a612b80da` (including `#6497` — fixing the
  `#6690`-era `rustc 1.99+` `global_asm!` `__pre_init` literal ordering in
  `xtensa-lx-rt` — and `#6816` for Rust `1.100.0.0` alignment), placing
  `_ESP_APP_DESC` in `#[link_section = ".flash.appdesc"]` with
  `esp-bootloader-esp-idf` linked (`"CalendulaOS X4 Pro proof"`).
- Implemented `fw-s3/src/board.rs` (X4 Pro GPIO map, 10 kHz 10-bit LEDC
  frontlight gamma table and `frontlight_duties`, `SLEEP_PAD_SPECS`,
  `release_deep_sleep_pad_holds` leaving USB `pad19`/`pad20` untouched) and
  `fw-s3/src/epd_probe.rs` (half-duplex bit-banged VER/FLG/RMTP probe using
  `ReconciledOutput` and `display::epd::probe::resolve_x4_controller`).
- `fw-s3/src/main.rs` (Wave 6 `#339` proof, hardware-verified) initialized the
  HAL, raised X4 Pro `GPIO1`, ran `epd_probe::probe`, spawned
  `fw_common::app::run(None)`, supplied synthetic empty-catalog/render-settled
  responses, and printed heartbeats. It booted cleanly across three consecutive
  `USB_UART_CHIP_RESET` runs on `/dev/cu.usbmodem2101` (`s3-proof-boots.log`),
  confirming `uc81xx-confirmed` / `uc8279-x4` (`lut_ver=68`) and
  `shared_app=started`.

### X4 Pro peripherals (`#391` Wave 7B/7C — serial boot verified; upright visual check, interactive/sleep & storage/radio open)

- `fw-s3/Cargo.toml` (and `fw-s3/Cargo.lock`) added `ui`, `embedded-hal-async`,
  `embassy-sync`, and `static_cell`.
- `fw-s3/src/display_flush.rs`: `uc8279_x4` SPI2 DMA panel init (`init_panel`),
  flush (`flush` using `uc8279_x4::flush_plan` + `stream_plane` packing
  `CHUNK_ROWS = 20` via `uc8279_x4::fill_gate_row_from_framebuffer`), and panel
  deep sleep (`sleep_panel`), using `EpdBus::wait_uc_idle_high`.
- `fw-s3/src/frontlight.rs`: `Frontlight::{init, set, off}` driving 10 kHz
  10-bit LEDC PWM on `GPIO8` (cool, Channel0) and `GPIO9` (warm, Channel1) via
  `board::frontlight_duties`.
- `fw-s3/src/input.rs`: `run(InputHardware)` and `take_wake_pins()`
  (`WAKE_PIN_REQUESTS` / `WAKE_PIN_HANDOFF` / `WakePins`) over shared `I2C0`
  (`GPIO39`/`GPIO38`, 400 kHz) and digital GPIOs:
  - `Bm8563` RTC (`0x51`) init and time read
  - `Cw2017` fuel gauge (`0x63`) `ensure_profile(&X4_PRO_BATINFO)` and
    periodic `sample()` / retry
  - `Gt911` capacitive touch (`0x5D` primary / `0x14` alt, `TOUCH_INT_PIN=10`,
    `TOUCH_RST_PIN=4`, `TOUCH_POWER_PIN=2`) with
    `TouchTransform::X4_PRO_LANDSCAPE`, `home_key` -> `Button::Back`, and
    `classify_tap_or_swipe(..., uc8279_x4::WIDTH)`
  - Active-low digital buttons (`BTN_LEFT_PIN=0` -> `Button::Previous`,
    `BTN_RIGHT_PIN=7` -> `Button::Next`, `BTN_POWER_PIN=3` -> `Button::Power`
    via `PowerEdge`) plus `CHARGER_STAT_PIN=21` logging.
- `fw-s3/src/rtc_sleep.rs`: `enter_deep_sleep` turning off `frontlight`,
  running `display_flush::sleep_panel`, taking wake pins, latching `GPIO14`
  (EPD `/RST`), `GPIO5` (`SDMMC_POWER_PIN`), and `GPIO2` (`TOUCH_POWER_PIN`)
  HIGH with `set_pad_hold(true)` (matching `board::SLEEP_PAD_SPECS`), waiting
  for `GPIO3` release, configuring `Event::LowLevel` low-power wakeup, and
  entering `LowPower::new(lpwr).sleep_deep(RtcSleepConfig::deep())`.
- `fw-s3/src/main.rs`: 240 MHz CPU clock, static `Framebuffer` + DMA buffers
  (`DMA_TX_BUF` 4092 B), `SPI2` @ `uc8279_x4::SPI_HZ`, `epd_probe::probe` with
  `ReconciledOutput` -> `board::release_deep_sleep_pad_holds`,
  `Frontlight::init`, `display_flush::init_panel`, `I2C0` + `input::run`,
  `fw_common::app::run(None)`, `ui::app_render::{render_request, render_sleep}`
  with `with_builtin_model` + `RefreshPlanner`, and `idle_timeout_secs` /
  `PowerEvent::SleepNow` deep sleep. Storage commands still return synthetic
  empty catalog (`StageNext` and `DisplayCommand::Sleep` remain no-ops); 1-bit
  native SDMMC (`esp_hal::sdmmc`) and S3 Wi-Fi/OTA are still unimplemented.

## Next implementation steps (what's left for `#391`)

1. **Visual & interactive verification of upright `fw-s3-peripherals.bin`**
   (already flashed in `app1 @ 0x7F0000`, currently in deep sleep):
   - Press Power (`GPIO3`) to wake `fw-s3-peripherals.bin` directly from deep
     sleep while capturing serial on `/dev/cu.usbmodem2101`, confirming the
     `Home`/`Sleep` screen renders **upright** on the glass (post-fix check)
     and `GPIO3` wake + `ReconciledOutput` pad-hold release (`GPIO14`/`5`/`2`)
     recover cleanly.
   - Exercise physical `GPIO0` (`Button::Previous`), `GPIO7` (`Button::Next`),
     `GPIO3` (`Button::Power`), and `Gt911` touch/swipe/Home-key (`0x814E & 0x10`)
     across `Home`, `Settings`, and the built-in `Reading` book, verifying
     whether `TouchTransform::X4_PRO_LANDSCAPE` matches the upright portrait and
     landscape UI frames.
   - Wire user-facing frontlight brightness/warmth settings/controls in `fw-s3`
     (`Frontlight::init` currently configures `(0, 0)` duty; `Frontlight::set`
     is implemented) and visually verify cool/warm PWM illumination.
2. **1-bit native SDMMC storage session (`fw-s3`)**:
   - Implement the SD card session on `SDMMC` (`CLK=GPIO41`, `CMD=GPIO42`,
     `D0=GPIO40`, active-low power `GPIO5` pulsed HIGH->LOW before mount and
     held HIGH in sleep) bridging `esp_hal::sdmmc` into `storage` /
     `reader-cache` so `STORAGE_COMMANDS` serve real SD catalogs and EPUBs
     instead of the synthetic empty catalog.
3. **S3 Wi-Fi sync & dual-OTA updater (`fw-s3`)**:
   - Port/wire the S3 Wi-Fi sync session and OTA updater (`app0 @ 0x10000` /
     `app1 @ 0x7F0000`, `otadata @ 0xE000`,
     `proto::ota::validate_image_for_chip(..., CHIP_ID_ESP32S3, BOARD_X4_PRO)`),
     and switch `_ESP_APP_DESC` from `"CalendulaOS X4 Pro proof"` to
     `board::BOARD_IDENTITY` (`"CalendulaOS X4-Pro u1"`).
4. **Hardware qualification, CrossPoint restore & C3 regression checks**:
   - Qualify reader/storage/Wi-Fi/OTA flows, repeated SD remounts, 50
     sleep/wake cycles, pad-hold voltage levels, and standby current with
     instrumentation, plus physical C3 X4/X3 smoke tests when C3 units are
     attached.
   - Restore `crosspoint-app1-0x7f0000.bin` to `app1 @ 0x7F0000` (commands
     below) whenever returning the X4 Pro to CrossPoint.
   - Run full top-level verification suites after code changes, review the
     entire diff, and keep unrun checks explicitly reported (no commit/push is
     authorized by this handoff).

## Hardware and recovery state

Connected port was `/dev/cu.usbmodem2101`. ESP32-S3 rev 0.2, 40 MHz crystal,
16 MB flash, embedded 8 MB PSRAM; secure boot/encryption disabled. Existing
CrossPoint boot logs identify the actual panel as **UC8279 800×480**, promoted
from initial SSD1677 detection, LUT version 0x68. CrossPoint reports SDMMC
mount, RTC found and frontlight attached. These are serial observations, not
physical touch/light/button/display-quality or current measurements.

Original stock dump, untouched:
`/Users/kyle/Documents/GitHub/crosspoint-reader/x4-pro-flash-backup-20261115.bin`
(16,777,216 bytes), SHA-256
`5ad993ad14b8b6f252542137f14baded2cb05300df659f9a0fbf9c866c8bc7c1`.

Verified current CrossPoint dump:
`target/bench/x4-pro-2026-11-16/crosspoint-stable-flash.bin`, same size, SHA-256
`ee9076e21f6370873d1cdbb667f370a26fd239f4ebf6efc2cc351153671e918c`.
Read at 460800 baud with `--after no-reset-stub`, verified with
`--before no-reset` before allowing the app to run. Earlier verification after
reboots differed only in NVS sector 0xB000; a separate 921600 read failed.
Use the stable dump. Dumps are private local artifacts excluded from Git.

After the earlier `#339` proof (`s3-proof-boots.log`) and Wave 7C Run 1,
CrossPoint was restored to `app1 @ 0x7F0000` (`crosspoint-restore-boot.log` and
`wave7c-crosspoint-restore-boot.log`). **Right now, after Wave 7C Run 2**,
`app1 @ 0x7F0000` still holds the updated `fw-s3-peripherals.bin` (`467,264` B,
SHA-256 `1dfeec02...`), and the device entered deep sleep after its `Home` idle
timeout (`/dev/cu.usbmodem2101` is absent). Pressing `GPIO3` (Power) wakes
`fw-s3-peripherals.bin` directly; restore `crosspoint-app1-0x7f0000.bin`
afterward when returning the unit to CrossPoint. For bounded reset capture,
pyserial was opened with DTR/RTS cleared (`dtr=False, rts=False`) before
`esptool.reset.HardReset(serial, uses_usb=True)` while retaining the port. Do not
probe GPIO19/20 (USB). See companion report for complete corrected pins.

### Non-destructive `app1 @ 0x7F0000` proof & CrossPoint restore (Waves 4 & 5D+)

Inspecting `crosspoint-stable-flash.bin` confirmed `otadata0` (`0xE000`) has
`seq=3, state=2` and `otadata1` (`0xF000`) has `seq=4, state=2`, selecting
`app1 @ 0x7F0000` (size `0x7E0000`). Artifacts in
`target/bench/x4-pro-2026-11-16/`:

- `crosspoint-app1-0x7f0000.bin`: **5,378,048 bytes**, SHA-256
  `bbcfb288ee0a0ba6acf665d9ed9efb86565f188754fc6125162027f06825d034` (verified
  byte-identical to `crosspoint-stable-flash.bin[0x7F0000..]` for its full
  length).
- `fw-s3-wave4-serial-only.bin`: **129,104 bytes**, SHA-256
  `da616a3019c9af9eb4ffebbb1318034b76ec07e385ec6a6480908bd385a124b4` (earlier
  Wave 4 serial-only build).
- `fw-s3-proof.bin`: **135,536 bytes**, SHA-256
  `b413adadec394100c4276674ccfd28b87fb097b846bde6ebebe9429f37338415` (flashed
  post-Wave-5D runtime + `epd_probe` image; `esptool --chip esp32s3 image-info`:
  Image version 1, `Segments: 6`, entry `0x4037872c`, `Chip ID: 9 (ESP32-S3)`,
  `16MB` `80m` `DIO`, `Checksum: 0x2d (valid)`, `Validation hash:
  53f2bfaa23705b15ffba5aaf37f8598a150f082a8f674a209844e560574872a2 (valid)`,
  `Project name: CalendulaOS X4 Pro proof`, `App version: 0.9.0`, `ELF file
  SHA256: 5f2a8124f998f3a518f513d3b2516c64ea999b1b8a973363203059a1c0de17f5`).

Non-destructive `app1 @ 0x7F0000` flash and CrossPoint-restore sequence
(preserving bootloader, partition table, `otadata`, NVS, `app0`, SPIFFS, and
coredump; **executed and verified on `/dev/cu.usbmodem2101`**):

```sh
# Flash proof binary to app1 (0x7F0000)
esptool --chip esp32s3 --port /dev/cu.usbmodem2101 --baud 460800 \
  write-flash 0x7F0000 target/bench/x4-pro-2026-11-16/fw-s3-proof.bin

# Restore CrossPoint app1 (0x7F0000)
esptool --chip esp32s3 --port /dev/cu.usbmodem2101 --baud 460800 \
  write-flash 0x7F0000 target/bench/x4-pro-2026-11-16/crosspoint-app1-0x7f0000.bin
```

- `preflight-verify.log`: pre-flash `verify-flash` matched `0x521000`
  (`5,378,048`) bytes at `0x007f0000` against `crosspoint-app1-0x7f0000.bin`.
- `s3-proof-boots.log`: 3 consecutive `rst:0x15 (USB_UART_CHIP_RESET),boot:0xb (SPI_FAST_FLASH_BOOT)`
  boots with zero panics; each boot logged `s3-proof: epd_probe
  verdict=uc81xx-confirmed variant=uc8279-x4 ver=00 0F 68 01 00 flg=13
  lut_ver=68 mtp_valid=true`, `mtp_prefix=A5 A5 1F 20 25 25 3C 00 97 02 02 03
  20 02 58 00`, `board=x4-pro chip=esp32s3 shared_app=started
  peripherals=epd-probe-only`, `app: started`, `synthetic empty catalog`,
  `shared render view=Home kind=Boot panel_io=false`, and heartbeats at `2175` /
  `4175` / `6175` ms (`2176` / `4176` / `6176` ms on Boot 3).
- `restore-write.log` & `restore-verify.log`: `Wrote 5378048 bytes (3627465
  compressed) at 0x007f0000 in 46.8 seconds (920.2 kbit/s). Hash of data
  verified.`, followed by `verify-flash` digest match over all `5,378,048` bytes.
- `crosspoint-restore-boot.log`: after restore reset, CrossPoint booted cleanly
  (`Device: xteink_x4_pro`, `SDMMC card mounted`, `promoted SSD1677 -> UC8279
  800x480 (LUT_VER=68)`).

### Wave 7B/7C `fw-s3-peripherals.bin` (Run 2 flashed on `app1 @ 0x7F0000`, serial verified 3/3 boots, currently in deep sleep)

Wave 7B built the initial `fw-s3-peripherals.bin` (`467,328` bytes, SHA-256
`32061f9b...`), which Wave 7C Run 1 flashed to `app1 @ 0x7F0000`, verified on
serial, rendered cleanly on EPD glass (flipped 180° per owner observation), and
restored back to CrossPoint (`wave7c-restore-write.log`: `46.0` s / `934.8
kbit/s`; `wave7c-restore-verify.log`; `wave7c-crosspoint-restore-boot.log`).

After fixing the 180° double-inversion in `uc8279_x4::fill_gate_row_from_framebuffer`,
Wave 7C Run 2 rebuilt `fw-s3-peripherals.bin`, flashed it non-destructively to
`app1 @ 0x7F0000` on `/dev/cu.usbmodem2101`, verified 3 consecutive serial boots,
and left it resident in `app1 @ 0x7F0000` (now in deep sleep after idle timeout):

- `fw-s3-peripherals.bin`: **467,264 bytes**, SHA-256
  `1dfeec02545d7f90232ff6ce85131dd61ef2d9a8a2ffe5075a5eb31bc200b41f`.
- `esptool --chip esp32s3 image-info target/bench/x4-pro-2026-11-16/fw-s3-peripherals.bin`:
  Image version `1`, Entry point `0x403788c8`, `Segments: 6` (`0`: `0x35f30`
  DROM `@ 0x3c000020`; `1`: `0x0a0c0` DRAM `@ 0x3fc8bc08`; `2`: `0x2bbc8` IROM
  `@ 0x42040020`; `3`: `0x02908` DRAM `@ 0x3fc95cc8`; `4`: `0x00400` IRAM
  `@ 0x40378000`; `5`: `0x03808` IRAM `@ 0x40378400`), `Flash size: 16MB`,
  `80m`, `DIO`, `Chip ID: 9 (ESP32-S3)`, `Checksum: 0x66 (valid)`,
  `Validation hash: 3411cc12d388a928b01edc2201b03436b08c5d00c7786d8342fe4f3ff64b863d (valid)`,
  `Project name: CalendulaOS X4 Pro proof`, `App version: 0.9.0`,
  `ELF file SHA256: 7b9ba0cb50ba7b5995fd570d64dc6120415881219a17f56ba381e720b294d9e6`.
- `xtensa-esp32s3-elf-size -A fw-s3/target/xtensa-esp32s3-none-elf/release/fw-s3`:
  `.rwtext` `14,344` B, `.vectors` `1,024` B, `.rwdata_dummy` `15,368` B,
  `.data` `51,656` B, `.bss` `16,716` B, `.flash.appdesc` `256` B, `.rodata`
  `220,720` B, `.rotext_dummy` `262,144` B, `.text` `179,135` B (`-72` B vs Run 1),
  `.stack` `258,020` B (`Total` `1,019,582`).

Wave 7C Run 2 `app1 @ 0x7F0000` logs (executed on `/dev/cu.usbmodem2101`):

- `wave7c-preflight-verify.log`: `esptool v5.1.2` verified `0x521000`
  (`5,378,048`) bytes at `0x007f0000` against `crosspoint-app1-0x7f0000.bin`
  (`Verification successful (digest matched).`).
- `wave7c-flash.log`: `Wrote 467264 bytes (208516 compressed) at 0x007f0000 in 3.3 seconds (1131.4 kbit/s). Hash of data verified.`
- `wave7c-peripherals-boots.log`: 3 consecutive `rst:0x15 (USB_UART_CHIP_RESET),boot:0xb (SPI_FAST_FLASH_BOOT)`
  boots with 0 panics/errors; each boot logged:
  - `s3-proof: epd_probe verdict=uc81xx-confirmed variant=uc8279-x4 ver=00 0F 68 01 00 flg=13 lut_ver=68 mtp_valid=true`
  - `s3-proof: epd_probe mtp_prefix=A5 A5 1F 20 25 25 3C 00 97 02 02 03 20 02 58 00`
  - `s3-frontlight: init ok cool_gpio=8 warm_gpio=9 freq_hz=10000 bits=10 duty=(0, 0)`
  - `s3-display: busy pre-reset high=Some(true)` and `s3-display: uc8279-x4 init done busy_high=Some(true) t_ms=92`
  - `s3-proof: board=x4-pro chip=esp32s3 shared_app=started` and `app: started`
  - `s3-display: PON busy_ms=39`, `s3-display: DRF mode=Full busy_ms=1321`, `s3-display: flush ok view=Home kind=Boot mode=Full t_ms=1454`
  - `s3-rtc: bm8563 init=Ok(0)` with live times `2026-11-17 08:11:32` (Boot 1), `08:11:42` (Boot 2), `08:11:52` (Boot 3) (`weekday: 2`)
  - `s3-gauge: cw2017 ready=Ok(true) soc=Some(100)` with `mv=Some(4364)` (Boot 1), `Some(4365)` (Boot 2), `Some(4364)` (Boot 3)
  - `s3-touch: gt911 addr=0x5D detected=true` and `s3-power: charger_active=false`
  - `s3-proof: heartbeat t_ms=2322`, `4322`, `6322`, `8322`

**Commands for the next agent (once woken on `/dev/cu.usbmodem2101` via `GPIO3`):**

```sh
# 1. Verify current app1 slot holds Wave 7C Run 2 fw-s3-peripherals.bin
esptool --chip esp32s3 --port /dev/cu.usbmodem2101 --baud 460800 \
  verify-flash 0x7F0000 target/bench/x4-pro-2026-11-16/fw-s3-peripherals.bin

# 2. Flash a new fw-s3 build to app1 (0x7F0000)
esptool --chip esp32s3 --port /dev/cu.usbmodem2101 --baud 460800 \
  write-flash 0x7F0000 target/bench/x4-pro-2026-11-16/fw-s3-peripherals.bin

# 3. Restore CrossPoint app1 (0x7F0000) & verify when returning device to CrossPoint
esptool --chip esp32s3 --port /dev/cu.usbmodem2101 --baud 460800 \
  write-flash 0x7F0000 target/bench/x4-pro-2026-11-16/crosspoint-app1-0x7f0000.bin
esptool --chip esp32s3 --port /dev/cu.usbmodem2101 --baud 460800 \
  verify-flash 0x7F0000 target/bench/x4-pro-2026-11-16/crosspoint-app1-0x7f0000.bin
```

## Verification ledger and limitations

Host target was `aarch64-apple-darwin`; determine it again with `rustc -vV`.
Pass explicit `--target` for host Cargo commands. Logs/artifacts are under
`target/bench/391-baseline/`; preserve them for before/after comparisons.

### Rename-only stage (passed unless stated)

- `tools/cargo.sh build -p fw --release --target riscv32imc-unknown-none-elf`
  and the same with `--features device-x3`: baseline binaries saved as
  `fw-x4` and `fw-x3`.
- `tools/cargo-c3.sh build -p fw-c3 --release --locked`, default and
  `--features device-x3`: saved as `fw-c3-x4` and `fw-c3-x3`.
- Offline ELF descriptor comparison: all 256 bytes identical on both boards.
  RAM sections unchanged; stack regions X4 38,456 B / X3 29,272 B unchanged.
  Read-only data grew 40/48 B respectively. Frame analysis found no stack-size
  change (one X3 zero-frame formatter clone suffix changed). Extract raw
  objdump symbols before demangling; parsing demangled disassembly directly
  produced misleading offsets in an earlier attempt.
- `tools/cargo.sh check --target aarch64-apple-darwin --locked` and
  `tools/cargo-c3.sh check -p fw-c3 --locked`: passed.
- `tools/check.sh all`: fast stages and golden scenarios on both boards passed;
  stopped at emulator GUI dependency `objc2` 0.4.1 E0277 (`Iterator`). Same GUI
  failure reproduced on original commit in a temporary worktree, subsequently
  removed. This is an existing pinned-toolchain/dependency failure, not proof
  that all verification passed. One initial attempt to isolate `objc2` used an
  unsupported feature command; the full original GUI compile reproduced it.
- `tools/check.sh firmware`: initial wrapper-target failure fixed; rerun passed
  entire C3 feature matrix and stack gates (`check-firmware-final.log`).
- `tools/build-release.sh x4` and `tools/build-release.sh x3`: passed.
- `tools/check.sh web-emulator`: passed both host and wasm board builds.
- Headless emulator tests using `tools/cargo.sh test --manifest-path
  tools/emulator/Cargo.toml --target aarch64-apple-darwin`, default and
  `--features device-x3`: passed; headless clippy with `-- -D warnings`
  passed both boards (X4 within the all gate, X3 separately).
- `tools/check.sh fmt`, shell syntax checks for edited scripts, host metadata
  inspection and `git diff --check`: passed after fixing one trailing blank.
  `ruff format tools/powercut_campaign.py` left the file unchanged; ruff gate
  passed within `all`.

### Wave 2B: Post-`fw-common` extraction C3 verification (`post-common-comparison.txt`)

Recorded in `target/bench/391-baseline/post-common-comparison.txt` on toolchain
`1.100.0-aarch64-apple-darwin` (`tools/cargo-c3.sh build -p fw-c3 --release --locked`
for X4 and `--features device-x3`, plus `tools/check.sh stack-frames` exit 0):

- `_ESP_APP_DESC` (256 bytes at `.rodata` offset 0, `0x3C000020`): **256/256
  bytes identical** vs `fw-x4` / `fw-x4-layout.txt` and `fw-x3` /
  `fw-x3-layout.txt` (`magic=0xABCD5432`, `version="0.9.0"`,
  `project_name="CalendulaOS X4 u2"` / `"CalendulaOS X3 u2"`,
  `date="2026-05-20"`).
- Stack region (`_stack_start - _stack_end`): X4 `38,400 B` (`-56 B` vs
  `38,456 B` baseline); X3 `29,216 B` (`-56 B` vs `29,272 B` baseline).
- ELF section deltas vs `fw-x4` / `fw-x3` baselines (`llvm-size -A`):
  - `.bss`: `+56 B` on both (`192,832 -> 192,888 B` X4; `197,728 -> 197,784 B`
    X3; `fw_c3::tasks::app::run::POOL` `+48 B` plus `fw_common::log::SINK.0`
    `+8 B` including alignment).
  - `.data`: `+0 B` (`66,836 B` X4; `66,844 B` X3).
  - `.rodata`: `+104 B` vs `fw-x4` (`+64 B` vs `fw-c3-x4`, `2,216,868 B` X4);
    `+112 B` vs `fw-x3` (`+64 B` vs `fw-c3-x3`, `2,218,668 B` X3).
  - `.text`: `-22,654 B` on X4 (`1,293,148 -> 1,270,494 B`) and `-22,852 B` on
    X3 (`1,303,478 -> 1,280,626 B`), primarily from routing `storage` and
    `reader-cache` log sites through `fw_common::runtime_log!` ->
    `fw_common::log::write`.
  - `.eh_frame`: `-4,688 B` on X4 (`124,112 -> 119,424 B`); `-4,728 B` on X3
    (`125,768 -> 121,040 B`).
  - ELF file size: `5,557,116 B` X4 (`-34,028 B` vs `fw-x4`); `5,581,484 B` X3
    (`-38,348 B` vs `fw-x3`).
- `tools/check.sh stack-frames` (exit 0; 32 Python unit tests OK; budget
  `24,576 B`; top 5 frames `10,512`, `7,584`, `6,272`, `4,928`, `4,592 B` across
  all 4 builds):
  - X4 default: stack region `38,400 B`, `3843` functions (5 exempt unresolved).
  - X4 `--features light-sleep`: stack region `38,296 B`, `3849` functions.
  - X3 `--features device-x3`: stack region `29,216 B`, `3870` functions.
  - X3 `--features device-x3,no-light-sleep`: stack region `29,312 B`, `3864` functions.

### Wave 3–7C status & unrun checks

- Wave 4 produced the `app1 @ 0x7F0000` CrossPoint slice
  (`crosspoint-app1-0x7f0000.bin`) and initial serial-only image
  (`fw-s3-wave4-serial-only.bin`).
- Wave 5–6A added `fw-common/src/pad.rs`, `hal-ext/{cw2017,bm8563,gt911}.rs`,
  `proto::ota` chip-aware validation, `display::epd::{uc8279_x4, probe::resolve_x4_controller}`,
  `fw-s3/src/{board,epd_probe}.rs`, `tools/check_dep_boundaries.py`, and CI
  `dep-boundaries` / `firmware-s3` gates.
- `#339` physical USB hardware proof & CrossPoint restore passed on
  `/dev/cu.usbmodem2101`: `preflight-verify.log`, `s3-proof-boots.log` (3 boots
  of `fw-s3-proof.bin`, `135,536` bytes, live `epd_probe` confirming
  `uc8279-x4` `lut_ver=68` and `shared_app=started`), `restore-write.log`,
  `restore-verify.log`, and `crosspoint-restore-boot.log`.
- Wave 7A–7C added `EpdBus::{rst_mut, wait_uc_idle_high}` in `hal-ext/src/spi_dma.rs`,
  `fill_gate_row_from_framebuffer` in `display/src/epd/uc8279_x4.rs` (fixed in
  Wave 7C Run 2 to forward-order `fill_gate_row(fb.bytes(), ...)` after Run 1
  rendered cleanly 180° flipped on glass), and the `fw-s3` peripheral modules
  (`display_flush.rs`, `frontlight.rs`, `input.rs`, `rtc_sleep.rs`, `main.rs`
  with `ui::app_render` + `RefreshPlanner`), and completed live serial hardware
  verification of Run 2 `fw-s3-peripherals.bin` (`467,264` bytes, currently still
  in `app1 @ 0x7F0000`, asleep) across 3 consecutive boots on
  `/dev/cu.usbmodem2101` (`wave7c-preflight-verify.log`, `wave7c-flash.log`,
  `wave7c-peripherals-boots.log`; Run 1 also verified CrossPoint restoration in
  `wave7c-restore-verify.log` and `wave7c-crosspoint-restore-boot.log`).
- Consolidated post-edit verification runs after final handoff doc edits
  (`tools/check.sh fmt`, `ruff`, `dep-boundaries`, `s3`, `fast`, `git diff
  --check`).

**Remaining open / unrun items for `#391`:**

- Unrun physical/interactive checks on the updated `fw-s3-peripherals.bin`
  (`467,264` B, already on `app1 @ 0x7F0000`): visual confirmation that the
  fixed `Home`/`Sleep` render is upright on EPD glass, `GPIO3` deep-sleep wake
  + `ReconciledOutput` pad-hold release (`GPIO14`/`5`/`2`), user-facing LEDC
  frontlight brightness/warmth wiring + visual illumination, and interactive
  `Gt911` touch/swipe/Home-key (confirming `TouchTransform::X4_PRO_LANDSCAPE`
  matches the upright frames) + physical `GPIO0`/`GPIO7`/`GPIO3` button actuation.
- Unimplemented `#391` S3 subsystems: 1-bit native SDMMC storage session
  (`esp_hal::sdmmc`) and S3 Wi-Fi/OTA updater (followed by switching
  `_ESP_APP_DESC` to `"CalendulaOS X4-Pro u1"`).
- Full `#391` hardware qualification (reader/storage/Wi-Fi/OTA flows, physical
  pad-hold voltages, standby current with instrumentation, 50 sleep/wake cycles),
  restoring CrossPoint `app1` when done, and physical C3 X4/X3 smoke/bench
  checks (no C3 unit connected).
