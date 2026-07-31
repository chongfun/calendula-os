# Flashing & release images

This firmware ships as a standard ESP32-C3 application image that boots under
the Xteink X4/X3 **stock second-stage bootloader**. That's what makes it
installable the same way the other community firmwares (CrossPoint, CrossInk)
are — including, in principle, on *locked* units.

## Unlocked vs. locked units

Some X4s and X3s — typically the ones bought from third-party sellers (AliExpress) —
ship with **USB flashing disabled in eFuse at the factory**. Units bought
directly from xteink.com are not locked.

To tell which you have: connect over USB (USB-C on X4, the 4-pin pogo cable on
X3) and try to flash (`cargo run` or the web flasher). If the device never
appears as a serial port even after trying another cable/port/browser, assume
it's locked.

Both the original author's X4 and the current maintainer's X3 are unlocked, so
the locked-device path still needs a real locked-unit confirmation — see
[Status](#status).

## The Calendula/CrossPoint layout

`partitions.csv` defines the layout used by Calendula full-flash images and the
CrossPoint/Marigold firmware family:

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
xxd -s 0x20 -l 4 target/release-images/firmware.bin   # -> 3254 cdab (0xABCD5432 LE)
```

## Building the release images

Release builds require Rust from `rustup`, the firmware target, and `espflash`:

```sh
rustup target add riscv32imc-unknown-none-elf
cargo install espflash
```

```sh
tools/build-release.sh        # X4 (default)
tools/build-release.sh x3     # X3
```

Produces local images in `target/release-images/`:

- **`firmware.bin`** — app image for `ota_0`. Flash to `0x10000`. Updates the
  app in place and leaves the bootloader untouched. This is what the web
  flasher, `esptool write_flash 0x10000`, and the in-app SD updater consume.
  The web installer explicitly forbids whole-chip erase for
  these app-only manifests; CI verifies that contract before Pages deployment.
- **`firmware-x3.bin`** — the same app image contract for X3 builds.
- **`update.bin`** — byte-identical to `firmware.bin`, under the filename the
  X4 stock OEM SD-card updater looks for. The OEM updater writes it to the app
  slot at `0x10000`, so it is an **app image, not a full-flash image**.
- **`update-x3.bin`** — byte-identical to `firmware-x3.bin`; rename it to
  `update.bin` on the card for X3 OEM bootloaders that use that filename.
- **`FWUPDATE.BIN`** — byte-identical to `firmware.bin`, under the filename
  CalendulaOS itself looks for on the card root at boot.
- **`FWUPDX3.BIN`** — the X3 SD-card trigger filename.
- **`full-flash*.bin`** — merged 16 MB images (bootloader + partition table +
  app) for local bench recovery on unlocked units only.

Tagged GitHub releases publish only the public app/SD assets:
`firmware-x4.bin`, `firmware-x3.bin`, `update.bin`, `update-x3.bin`,
`FWUPDATE.BIN`, and `FWUPDX3.BIN`. `firmware-x4.bin` is the release-time name
for the default X4 `target/release-images/firmware.bin`; `FWUPDATE.BIN` is the
same X4 app image under Calendula's in-app updater trigger name.

> [!CAUTION]
> Never put `full-flash*.bin` on an SD card and never write it to `0x10000`. The
> OEM SD updater writes whatever it finds to the app slot; a full-flash image
> there lands a bootloader in the middle of the app partition and bricks the
> device. Writing to `0x0` is the fastest brick on any unit. The SD card and the
> app slot only ever take `update.bin`/`FWUPDX3.BIN` or the app-only
> `firmware-*.bin` images.

## Xteink X3

The X3 is the X4's sibling: same ESP32-C3 and 16 MB flash, but a smaller
792×528 UC8253 panel with a BQ27220 battery gauge instead of the X4's ADC
divider. Stock X3 units may retain a different dual-OTA partition table
(`ota_1` at `0x780000`, 7.44 MB slots) from the Calendula/CrossPoint layout
(`ota_1` at `0x650000`, 6.25 MB slots). Support lives behind the `device-x3`
feature; the web flasher and release pipeline publish first-class X3 images.

Build the X3 images:

```sh
tools/build-release.sh x3
```

Produces in `target/release-images/`: **`firmware-x3.bin`** (flash to
`0x10000`), **`update-x3.bin`** (rename to `update.bin` for the stock OEM
updater), **`FWUPDX3.BIN`** (Calendula's X3 one-shot trigger), and
**`full-flash-x3.bin`** (unlocked bench units only). Both app-image aliases
are published to cover both bootloader conventions.

> [!NOTE]
> The X3 charges and flashes through a **4-pin magnetic pogo connector**, not
> USB-C. The 2-pin variant of that cable is charge-only and will not enumerate
> as a serial port. Serial is behind the same native USB-Serial-JTAG as the X4.

When testing, **capture the serial log** (`cargo run` or `espflash monitor`):
panel init, BUSY-wait completions, per-refresh timings, and `bq27220` battery
reads are the key bring-up signals.

## Flashing an unlocked unit

```sh
# Everyday dev flash + serial monitor:
tools/cargo.sh run -p fw --release

# App-only, with esptool:
esptool.py --chip esp32c3 write_flash 0x10000 target/release-images/firmware.bin
esptool.py --chip esp32c3 write_flash 0x10000 target/release-images/firmware-x3.bin

# Whole flash from scratch, local unlocked bench units only:
esptool.py --chip esp32c3 write_flash 0x0 target/release-images/full-flash.bin
```

## Flashing a locked unit

> [!WARNING]
> On a locked unit, USB flashing is the recovery path of last resort and it's
> disabled. If you install a firmware that has **no over-the-air / SD update
> path of its own**, and USB re-locks, there is no way back. This firmware
> fully implements the SD updater and recovery anchor, but the flow has not
> yet been validated on a genuinely locked production unit (see [Status](#status)).
> Do not install it on a locked unit you cannot afford to brick.

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

Once a build of this firmware is running, it can update itself from the card
with no computer — this is what keeps a locked unit from being a one-way trip:

> [!IMPORTANT]
> This is the path for updating *between* anchor builds. Moving an install from
> before the anchor onto the first anchor build is a one-time migration, and it
> has to go through a computer or the OEM updater — see
> [Migrating an install from before the anchor](#migrating-an-install-from-before-the-anchor).

1. Copy a new app image to the card root as **`FWUPDATE.BIN`** (the file
   `tools/build-release.sh` produces for X4; the `FWUPDATE.BIN` name
   is the one-shot trigger, kept distinct from a permanent `update.bin` you may
   also keep on the card). On the **X3** the trigger is **`FWUPDX3.BIN`**
   instead — each build only picks up an image named for its own panel, so a
   card is safe to carry between an X4 and an X3 without either grabbing the
   other's image (they share a SoC, but not a display controller or battery
   gauge, so a cross-flash is a black screen).
2. Reboot. At boot, before the reader starts, the firmware validates the image
   (`proto::ota::validate_image`), locates the **update slot** in the installed
   partition table, writes it there, deletes `FWUPDATE.BIN` so it can run only
   once, flips `otadata` to select it (`proto::ota::plan_switch`), and resets
   into the new firmware.
   On the first boot after any OTA-slot install (including CrossInk's
   Settings -> SD firmware update flow), Calendula marks the selected `otadata`
   entry valid before the reader starts, so rollback-enabled bootloaders do not
   return to the previous firmware on the next deep-sleep reset.

Only the update slot and inactive `otadata` sector are written, so a bad or
half-copied image never harms the running firmware — the bootloader keeps
booting the current slot until a complete, valid image flips `otadata`. This
works on an unlocked unit too (espflash's bootloader is ESP-IDF and honours
`otadata`), which is how to test it without a locked device.

### Slot 0 is an anchor, not half of an A/B pair

In-app updates always land in **slot 1**. **Slot 0 is never written** by the
updater: it keeps whatever was first installed there — by the web flasher,
`esptool`, or the OEM SD updater, all of which target `0x10000` — so the
recovery hatch below always has a known firmware to fall back to. This follows
the FreeInk `RecoveryBoot` convention, where the recovery slot is deliberately
never reflashed. Plain A/B alternation would eventually write an update over the
very image the hatch returns to, and would leave the hatch a no-op on every boot
that happened to be running from slot 0.

The trade-off: an update staged while you are *already* running from slot 1
can't be written in place, because that would erase the running firmware. That
boot instead points `otadata` back at the anchor and resets **without** deleting
`FWUPDATE.BIN`, so the anchor boot applies it into slot 1 on the next pass. You
see two reboots instead of one; slot 0 still never gets written.

Calendula only bounces into an anchor that could actually finish the job. Each
build stamps a firmware identity into its app descriptor —
`CalendulaOS <board> u<updater-generation> (MarigoldOS)` — and the anchor's must
match exactly. That rules out four cases which would otherwise leave you on a
firmware that ignores your trigger file, with no way back (the hatch is a no-op
once you are *on* slot 0):

- a **foreign** firmware, from a mixed install with CrossPoint or the stock app;
- a build for the **other board**, which drives a different panel and looks for
  the other trigger filename (`FWUPDATE.BIN` vs `FWUPDX3.BIN`);
- a build from an **older updater generation**, which may not recognise this
  trigger at all;
- a **corrupt or half-written** image — an interrupted flash leaves slot 0's
  first bytes intact and its tail missing, so the anchor is checked the same way
  a staged update is: segment walk, checksum, and appended SHA-256.

In any of those, the update is refused and slot 1 is left as it was. The trigger
file is deleted on the way out — a refusal that left it in place would repeat on
every boot — so it is no longer on the card. Copying it back would only refuse
again for the same reason: the fix is to flash the image with a computer or the
OEM updater, both of which write slot 0 and so restore a usable anchor.

Which slot is *executing* is a separate question, and `otadata` does not answer
it: it records which slot the bootloader was *asked* to boot, not which one it
did. ESP-IDF verifies the selected image and, if it fails, quietly boots the
other app partition instead — leaving `otadata` pointing at the slot it
rejected. A firmware that trusted `otadata` could conclude it was running from slot 0,
decide slot 1 was idle, and erase the very image it was executing, destroying
the last bootable copy on the device. So the firmware asks the flash MMU which
partition is mapped for execution, the way `esp_ota_get_running_partition()`
does; if that lookup cannot resolve, the update is refused rather than guessed
at. The anchor check above answers something else — whether a bounce could
finish the job — and both boot log lines are printed on every boot.

The image on the card is checked the same way before it is installed: it must be
for **this board** and retain the current **updater generation** (such as `u1`).
An in-app update must retain the current updater generation so that the permanent
slot-0 anchor remains capable of servicing future updates for the installed
firmware. Moving to a new updater generation requires first replacing or
re-establishing the slot-0 anchor through the computer/OEM installation path.
An image for the other board, a foreign image, or a build with a different updater
generation is refused.

### Migrating an install from before the anchor

Builds from before the anchor stamp a product-only identity
(`CalendulaOS (MarigoldOS)`, with no `<board> u<generation>`) and update by plain
A/B alternation: they write whichever slot is inactive. So they cannot be relied
on to put the first anchor build where it has to go, and whether the in-app path
leaves you with a working recovery net is a coin flip you cannot see:

- if the old build happened to be running from **slot 1**, its write lands in
  slot 0, the anchor gets the new identity, and everything works from there;
- if it happened to be running from **slot 0**, the write lands in slot 1 and
  slot 0 keeps the old build. The new firmware boots and runs normally, but
  every later in-app update is refused (`NoUsableAnchor`), because the anchor is
  not an image it will hand off to — and nothing on the device can repair that,
  since the updater never writes slot 0.

**Install the first anchor build with a computer or the OEM updater.** The web
flasher, `esptool` at `0x10000`, and `update.bin` on the card all write slot 0,
which is what this migration needs. None of them touch `otadata`, so if it was
selecting slot 1 the device still boots the *old* build sitting there — with the
new one unused in slot 0. **Hold Back + Up at the first reset after the flash**,
every time: builds from before the anchor carry the same hatch, and it points
`otadata` at slot 0. If `otadata` already selected slot 0, the hold is a no-op.

Confirm from the second-stage bootloader's own line, which prints before any app
runs and so does not depend on which firmware is executing:

```text
I boot: Loaded app from partition at offset 0x10000
```

`0x10000` is the anchor. Any other offset means you are still on the update slot:
either the hold did not register — try again — or the bootloader was asked for
slot 0 and refused it, in which case the flash did not take and slot 0 needs
writing again. `E boot: OTA app partition slot 0 is not bootable` immediately
above tells the two apart.

Once you are running the new build, it prints the same pair itself on every boot,
which is the more convenient check from then on:

```text
ota: otadata requests slot Some(0), executing slot Some(0)
```

- **requests 0, executing 0** — done. The anchor is the new build.
- **requests 1, executing 1** — `otadata` still selects the update slot. Hold
  **Back + Up** at reset to switch over.
- **requests 0, executing 1** — the bootloader was asked for slot 0, refused it,
  and fell forward. **Back + Up will not help here**: it only moves `otadata`,
  which already names slot 0. Write slot 0 again.

If you already took the in-app path and updates are now being refused, the
firmware itself is fine; reflashing to `0x10000` by either route restores the
anchor. Release notes for the first anchor build should say so, rather than
presenting it as an ordinary in-app update.

### Backing out a bad update

If an update lands you on a firmware that boots but misbehaves, hold
**Back + Up** at reset: the recovery hatch confirms the hold across ~12 ms of
continuous reads, repoints `otadata` back at the slot 0 anchor, and reboots into
it. It can't help a firmware that won't boot far enough to run the check — that
would need a custom bootloader, which no app-level firmware provides — so treat
it as a strong safety net, not a guarantee against every brick.

## Status

Implemented and verified on host tooling:

- [x] Calendula/CrossPoint dual-OTA partition table (`partitions.csv`) plus
      runtime discovery of the different stock X3 OTA offsets for app-only
      installations.
- [x] App descriptor with the open eFuse range at offset `0x20` (bootloader-gate
      workaround), verified present in the built image.
- [x] Reproducible app/SD images (`firmware.bin`, `firmware-x3.bin`,
      `update.bin`, `update-x3.bin`, `FWUPDATE.BIN`, `FWUPDX3.BIN`) plus
      local-only `full-flash*.bin` bench images (`tools/build-release.sh`). The
      SD images are app images written to `0x10000`, matching the OEM updater.
- [x] `cargo run` flashes the stock-compatible layout.
- [x] **Image validator** (`proto::ota::validate_image`) — the integrity gate
      (magic / segment walk / XOR checksum / SHA-256 trailer) that must pass
      before any candidate `.bin` is written to the update slot. Streaming,
      no heap; host-tested against synthetic valid and corrupt images.
- [x] **otadata layer** (`proto::ota`: `seq_crc`, `SelectEntry`, `plan_switch`,
      `active_app_slot`) — the OTA-slot select-entry format, the seq CRC
      (verified against the esp-bootloader-esp-idf algorithm *and* a real
      on-device value: `seq_crc(1) == 0x4743989A`), and the slot-switch math.
      Host-tested.
- [x] **Boot-time SD updater** (`fw::ota_update`) — on boot, `/FWUPDATE.BIN` is
      validated for structure and for descriptor identity, written with
      `esp-storage` to the **update slot** (slot 1) located in the installed
      partition table, deleted, selected via `otadata`, and the device resets
      into it. A trigger found while already running from slot 1 bounces through
      the anchor instead (see [Slot 0 is an anchor](#slot-0-is-an-anchor-not-half-of-an-ab-pair)).
      Only the update slot and the inactive `otadata` sector are touched.
- [x] **OTA rollback acknowledgement** (`fw::ota_update::mark_running_slot_valid`)
      — early boot rewrites an active `NEW`/`PENDING_VERIFY` select entry as
      `VALID`. This covers installs launched from CrossInk/CrossPoint's
      Settings -> SD firmware update path, where a rollback-enabled bootloader
      may otherwise boot Calendula once and then return to CrossInk after sleep.
- [x] **Flash + otadata path validated on hardware** (2026-07-05, unlocked X4).
      A one-shot self-test (`fw::ota_update::run_selftest`, `ota-selftest`
      feature) copied the running image into the inactive slot with `esp-storage`
      and switched `otadata`; the device rebooted and the ESP-IDF bootloader
      loaded the app **from the far slot** (`Loaded app from partition at offset
      0x650000`) — proving the erase/write, the seq CRC (a wrong CRC would be
      ignored), and the switch. It settled on the new slot with **no rollback
      loop** on that bootloader; rollback-enabled installs are covered by the
      boot-time mark-valid step above.
      The SD read path is separately confirmed from normal boot logs, and
      `validate_image` is host-tested — so every constituent of the SD updater
      is now exercised even though a full `FWUPDATE.BIN` run awaits a card
      reader (the maintainer's machine has none).

- [x] **Boot-time recovery combo** (`fw::ota_update::recover_to_slot0`) — holding
      **Back + Up** at reset repoints `otadata` at slot 0 and reboots into it,
      the FreeInk `RecoveryBoot` escape hatch for backing out of a far-slot
      firmware that boots but misbehaves. Sampled in `main()` before any task
      owns the ADC. Verified on device that it does **not** false-trigger on an
      idle boot; the band values are the same ones the input task uses daily, and
      the otadata switch is the mechanism the self-test already proved.
      The combo must now read held on 4 consecutive polls 4 ms apart — 12 ms of
      continuous hold, giving up after 28 ms on an idle boot — so a single
      reading taken while the ADC settles can neither arm nor miss the switch.
      (N readings span N-1 delays, so those windows are derived constants,
      `CONFIRM_WINDOW_MS`/`MAX_WINDOW_MS`, asserted against a replay of the poll
      loop rather than written down.)
      Detection is derived from the input task's own ladder tables
      (`app_core::buttons`), so a recalibrated band cannot move the hatch off the
      buttons documented here. The confirm state machine
      (`app_core::buttons::ComboConfirmer`) is sans-IO and host-tested: a steady
      hold confirms, unsettled first readings still confirm, a transient blip
      never does, and an idle boot always gives up inside the budget.

- [x] **Slot 0 pinned as the recovery anchor** (`fw::ota_update`) — updates
      always target slot 1, and nothing in the updater writes slot 0, so the
      hatch's fallback image is the one first installed and cannot be consumed by
      an update. An update staged while running from slot 1 bounces through the
      anchor (one extra reboot, trigger file preserved) rather than erasing the
      running firmware; an anchor that could not apply the update — foreign,
      built for the other board, or from an older updater generation — is
      detected by the app-descriptor firmware identity and refused. Descriptor
      offset (`0x50`) and both boards' identities verified against built images.

- [x] **Slot policy host-tested across reboots** (`proto::ota`) — the decisions
      (`plan_update_action`, `plan_recovery_switch`, `project_name`) are sans-IO
      and tested against a simulated device that carries real `otadata` sectors
      through successive boots. Covered: an update staged from the anchor lands
      in one reboot; one staged from the update slot bounces and then lands, with
      the trigger surviving the first reset; a foreign anchor refuses without
      stranding the user; the hand-off always terminates and bounces at most
      once; four updates in a row never write slot 0; and the hatch still finds
      an intact anchor after an update installed through the bounce.

Not yet done:

- [ ] **End-to-end `FWUPDATE.BIN` run** — the whole SD trigger in one go (drop
      the file, reboot, watch it flash + delete + reboot). Needs a way to write
      the card root; blocked only by the missing card reader, not by code.
- [ ] **Live recovery-combo press** — confirm a physical Back+Up hold detects and
      switches on device (detection reuses the input task's proven bands, so this
      is a formality). Optional on-panel progress during an update.
- [ ] **Bounce-through-the-anchor run on hardware** — the decision and the
      reboot sequencing are host-tested (above); what remains unproven on device
      is the I/O around them, specifically that the trigger file really survives
      the first reset on a physical card. Blocked on the same missing card reader
      as the end-to-end run above.
- [ ] **Locked-unit confirmation** — that our app-descriptor eFuse range
      satisfies the stock gate and the OEM SD updater accepts our `update.bin`.
      Needs a locked device; the maintainer's is unlocked.
- [x] **Xteink X3 bring-up** — the `device-x3` build now has hardware-verified
      UC8253 panel and BQ27220 gauge support, and release tooling publishes X3
      app/SD images beside the X4 images.
