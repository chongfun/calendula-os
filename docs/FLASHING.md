# Flashing & release images

This firmware ships as a standard ESP32-C3 application image that boots under
the Xteink reader's **stock second-stage bootloader**. That's what makes it
installable the same way the other community firmwares (CrossPoint, CrossInk)
are — including, in principle, on *locked* units.

The USB development flow supports both X4 and X3. The locked-device and stock
SD-updater evidence in this document is from X4 hardware; treat the equivalent
X3 paths as unvalidated until they have been exercised on an X3.

## Unlocked vs. locked units

Some X4s — typically the ones bought from third-party sellers (AliExpress) —
ship with **USB flashing disabled in eFuse at the factory**. Units bought
directly from xteink.com are not locked.

To tell which you have: connect over USB-C and try to flash (`cargo run` or the
web flasher). If the device never appears as a serial port even after trying
another cable/port/browser, assume it's locked.

The author's own unit is unlocked, and **the locked-device path below has not
yet been validated on real locked silicon** — see [Status](#status).

## The layout

`partitions.csv` mirrors the stock dual-OTA layout so our app lands where the
stock bootloader expects it:

| Partition | Type | Offset | Size |
|---|---|---|---|
| nvs | data/nvs | `0x9000` | 20 KB |
| otadata | data/ota | `0xe000` | 8 KB |
| app0 | app/ota_0 | `0x10000` | 6.5 MB |
| app1 | app/ota_1 | `0x650000` | 6.5 MB |
| spiffs | data/spiffs | `0xc90000` | 3.4 MB |
| coredump | data/coredump | `0xff0000` | 64 KB |

The app is ~2 MB, so it fits `ota_0` with room to spare. `cargo run` now flashes
against this table (see `.cargo/config.toml`).

### Why the stock bootloader accepts our image

The X4 bootloader gates images on an eFuse block-revision range read from the
app descriptor (`esp_app_desc_t`). We emit that descriptor in `fw/src/main.rs`
(`_ESP_APP_DESC`, magic `0xABCD5432`) at image offset `0x20` — exactly where the
bootloader reads it — with `min_efuse_blk_rev_full = 0` and
`max_efuse_blk_rev_full = 65535`, i.e. "accept any revision". This is the same
gate the other firmwares defeat with a build-time patch; we satisfy it directly
in the descriptor. You can verify placement:

```sh
xxd -s 0x20 -l 4 target/release-images/x3/firmware.bin   # -> 3254 cdab (0xABCD5432 LE)
```

## Building the release images

```sh
tools/build-release.sh x4
tools/build-release.sh x3
```

Produces board-separated artifacts in `target/release-images/x4/` or
`target/release-images/x3/`. The board argument is required so a default X4
image cannot silently overwrite or be mistaken for an X3 image.

- **`firmware.bin`** — app image for `ota_0`. Flash to `0x10000`. Updates the
  app in place and leaves the bootloader untouched. This is what the web
  flasher, `esptool write_flash 0x10000`, and (once implemented) the in-app
  updater consume.
- **`update.bin`** — byte-identical to `firmware.bin`, under the filename the
  stock OEM SD-card updater looks for. The OEM updater writes it to the app
  slot at `0x10000`, so it is an **app image, not a full-flash image**.
- **`full-flash.bin`** — merged 16 MB image (bootloader + partition table +
  app) for programming a whole *unlocked* unit from scratch with
  `esptool write_flash 0x0`.

> [!CAUTION]
> Never put `full-flash.bin` on an SD card and never write it to `0x10000`. The
> OEM SD updater writes whatever it finds to the app slot; a full-flash image
> there lands a bootloader in the middle of the app partition and bricks the
> device. Writing to `0x0` is the fastest brick on any unit. The SD card and the
> app slot only ever take `update.bin`/`firmware.bin`.

## Flashing an unlocked unit

```sh
# X4: everyday dev flash + serial monitor:
cargo run -p fw --release

# X3: board selection must be explicit:
cargo run -p fw --release --no-default-features --features board-x3

# App-only, with esptool:
esptool.py --chip esp32c3 write_flash 0x10000 target/release-images/x3/firmware.bin

# Whole flash from scratch:
esptool.py --chip esp32c3 write_flash 0x0 target/release-images/x3/full-flash.bin
```

## Flashing a locked unit

> [!WARNING]
> On a locked unit, USB flashing is the recovery path of last resort and it's
> disabled. If you install a firmware that has **no over-the-air / SD update
> path of its own**, and USB re-locks, there is no way back. This firmware does
> not yet ship that recovery path (see [Status](#status)), so **do not install
> it on a locked unit you can't afford to brick.**

Two mechanisms exist, both pioneered by CrossPoint:

1. **Stock SD-card updater.** The OEM bootloader/app updates from an image on
   the SD card: copy **`update.bin`** to the card root, power on holding
   **Power + Up** while on USB power, and it writes the image to the app slot at
   `0x10000` (no bootloader replacement). Some builds also auto-flash a file
   named `force_update.bin` on boot with no button combo — handy as a recovery
   file to keep on the card. This path does **not** re-enable USB flashing. It
   is the standard install route for locked/AliExpress units.

2. **External unlocker tools** (CrossPoint's USB Unlocker / OTA Unlocker) that
   re-enable USB flashing or intercept the official OTA channel. These are
   separate desktop tools, out of scope for this repo; they officially support
   only CrossPoint/CrossInk.

## In-app update (the recovery net)

Once *any* build of this firmware is running, it can update itself from the card
with no computer — this is what keeps a locked unit from being a one-way trip:

1. Copy a new app image to the card root as **`FWUPDATE.BIN`** (the `firmware.bin`
   / `update.bin` an `tools/build-release.sh` produces; the `FWUPDATE.BIN` name
   is the one-shot trigger, kept distinct from a permanent `update.bin` you may
   also keep on the card).
2. Reboot. At boot, before the reader starts, the firmware validates the image
   (`proto::ota::validate_image`), writes it into the **inactive** OTA slot,
   flips `otadata` to select it (`proto::ota::plan_switch`), deletes
   `FWUPDATE.BIN` so it runs only once, and resets into the new firmware.

Only the inactive slot and inactive `otadata` sector are written, so a bad or
half-copied image never harms the running firmware — the bootloader keeps
booting the current slot until a complete, valid image flips `otadata`. This
works on an unlocked unit too (espflash's bootloader is ESP-IDF and honours
`otadata`), which is how to test it without a locked device.

If an update lands you on a firmware that boots but misbehaves, hold **Back + Up**
at reset: the recovery hatch repoints `otadata` back at slot 0 (the firmware
first installed there) and reboots into it. It can't help a firmware that won't
boot far enough to run the check — that would need a custom bootloader, which no
app-level firmware provides — so treat it as a strong safety net, not a
guarantee against every brick.

## X3 panel bring-up verification

The X3 driver's wire protocol and its ADC/heap constants are copied and scaled
from the hardware-proven X4 driver. They compile and pass host tests, but five
behaviors need evidence from *this* firmware on a real X3 panel before the X3
port can be called proven. Build the instrumented image and read the evidence
off the serial monitor:

```sh
cargo run -p fw --release --no-default-features --features board-x3,hw-verify
```

`--release` is not optional here — esp-hal's timing-sensitive peripherals
misbehave under the dev profile. The `hw-verify` feature is bring-up-only
(esp-alloc Max-usage watermark + raw-ADC stream); it is never in a release
image, since `tools/build-release.sh` passes only the board feature.

Capture the log with `tools/serial_capture.py` and grep for the markers below.

| Behavior | Drive it by | Serial evidence to confirm |
|---|---|---|
| **Full vs. fast contrast** | Cycle **Settings → refresh policy** (`FastOnly`/`FullEveryTen` drive fast turns; the sleep screen and `FullOnWake` drive a full), turning pages between changes | `display: x3 flush Full/Fast …`, `display: x3 LUT {mode}`, and `bench: render … {mode}`. Confirm by eye that `Full` is deep black with no ghost and `Fast` is the lighter/faster waveform, and that the logged mode matches what you see. |
| **BUSY release timing** | Any refresh, sleep, or wake | `display: x3 BUSY {phase} initial=… assertion=…/{ms} release=…/{ms} final=…`. Every phase (`init`, `power-on`, `refresh`, `post-full settle`, `power-off`) must show `assertion=Reached` then `release=Reached` with sane millisecond timings — never `TimedOut` (would mean the wait sailed through a live refresh). |
| **Sleep retention** | Press **Power**, or wait out the 10-minute idle timeout | `display: x3 deep-sleep armed check=0xa5 ok=true`, then nothing until `display: wake init` on the next Power press. The panel must hold the sleep image across that gap with the SoC powered down. |
| **Button ladders** | Press every button once (Back, Confirm, Left, Right on the front; Up, Down on the side; Power) | `input verify: nav=…mv->Some(Button) page=…mv->Some(Button) aux=…mv`. Each physical press must classify to the intended button; any `->None` is a band window (`fw/src/board/x3.rs` `NAV`/`PAGE`) that needs re-centering around the raw millivolts shown. |
| **Wi-Fi heap headroom** | Open **Wireless**, join a network, upload a book, press **done** | The `wifi: heap[after-loan/after-wifi-init/after-net-stack/after-dhcp]` trail plus the `heap[serving]` and `heap[post-upload]` `stats` dumps. `Max usage` in the dumps must stay clear of `Size` (the loaned budget = `DRAM2_HEAP_BYTES` + scratch); a near-full watermark or an alloc failure means the copied X3 budget is too tight. |

## Status

Implemented and verified on host tooling:

- [x] Stock-compatible dual-OTA partition table (`partitions.csv`).
- [x] App descriptor with the open eFuse range at offset `0x20` (bootloader-gate
      workaround), verified present in the built image.
- [x] Reproducible `firmware.bin` / `update.bin` / `full-flash.bin` release
      images (`tools/build-release.sh`). The SD `update.bin` is an app image
      written to `0x10000`, matching the OEM updater.
- [x] `cargo run` flashes the stock-compatible layout.
- [x] **Image validator** (`proto::ota::validate_image`) — the integrity gate
      (magic / segment walk / XOR checksum / SHA-256 trailer) that must pass
      before any candidate `.bin` is written to the inactive slot. Streaming,
      no heap; host-tested against synthetic valid and corrupt images.
- [x] **otadata layer** (`proto::ota`: `seq_crc`, `SelectEntry`, `plan_switch`,
      `active_app_slot`) — the OTA-slot select-entry format, the seq CRC
      (verified against the esp-bootloader-esp-idf algorithm *and* a real
      on-device value: `seq_crc(1) == 0x4743989A`), and the slot-switch math.
      Host-tested.
- [x] **Boot-time SD updater** (`fw::ota_update`) — on boot, `/FWUPDATE.BIN` is
      validated, written to the inactive OTA slot with `esp-storage`, selected
      via `otadata`, deleted, and the device resets into it. Only the inactive
      slot/sector are touched.
- [x] **Flash + otadata path validated on hardware** (2026-07-05, unlocked X4).
      A one-shot self-test (`fw::ota_update::run_selftest`, `ota-selftest`
      feature) copied the running image into the inactive slot with `esp-storage`
      and switched `otadata`; the device rebooted and the ESP-IDF bootloader
      loaded the app **from the far slot** (`Loaded app from partition at offset
      0x650000`) — proving the erase/write, the seq CRC (a wrong CRC would be
      ignored), and the switch. It settled on the new slot with **no rollback
      loop**, so `otadata` state NEW is fine here; no `UNDEFINED` change needed.
      The SD read path is separately confirmed from normal boot logs, and
      `validate_image` is host-tested — so every constituent of the SD updater
      is now exercised even though a full `FWUPDATE.BIN` run awaits a card
      reader (the author's machine has none).

- [x] **Boot-time recovery combo** (`fw::ota_update::recover_to_slot0`) — holding
      **Back + Up** at reset repoints `otadata` at slot 0 and reboots into it,
      the FreeInk `RecoveryBoot` escape hatch for backing out of a far-slot
      firmware that boots but misbehaves. Sampled in `main()` before any task
      owns the ADC. Verified on device that it does **not** false-trigger on an
      idle boot; the band values are the same ones the input task uses daily, and
      the otadata switch is the mechanism the self-test already proved.

Not yet done:

- [ ] **End-to-end `FWUPDATE.BIN` run** — the whole SD trigger in one go (drop
      the file, reboot, watch it flash + delete + reboot). Needs a way to write
      the card root; blocked only by the missing card reader, not by code.
- [ ] **Live recovery-combo press** — confirm a physical Back+Up hold detects and
      switches on device (detection reuses the input task's proven bands, so this
      is a formality). Optional on-panel progress during an update.
- [ ] **Locked-unit confirmation** — that our app-descriptor eFuse range
      satisfies the stock gate and the OEM SD updater accepts our `update.bin`.
      Needs a locked device; the author's is unlocked.
