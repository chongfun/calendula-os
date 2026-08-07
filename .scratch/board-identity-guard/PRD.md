# Board identity guard at boot

Status: ready-for-agent

## Problem

Nothing stops a user flashing X3 firmware onto an X4, or the reverse. The two are
the same ESP32-C3 board with different panels, so the image passes every
bootloader check, boots, and then drives the wrong panel controller at the wrong
geometry. What the user sees is a device that appears dead or garbled, with no
indication of why.

The *update* path is already guarded: `main.rs` stamps `PROJECT_NAME` as
`"CalendulaOS X4 u1 (MarigoldOS)"` / `"…X3 u1…"`, and `proto::ota` compares that
identity so an OTA image built for the other board is refused rather than
applied. The reasoning is recorded at `fw/src/main.rs:44` — bouncing into an
anchor built for the other board "would boot firmware for the wrong hardware
*and* strand the update."

The initial flash has no equivalent check. This closes that gap.

## Context

### What is already safe, and what is not

**ESP32-C3 ↔ ESP32-S3 is fail-safe.** ESP image headers carry a chip id that the
bootloader validates; `proto::ota::EXPECTED_CHIP_ID = 5` is this project's own
copy of that fact for the C3. A Sticky (S3) image flashed to an X3/X4 refuses to
boot rather than running wrong. Adding the Sticky does not widen this problem.

**X3 ↔ X4 is the dangerous case.** Same chip, so every automated check passes and
the image runs. This is the only silent-corruption path, and it is the one a
non-technical user is most likely to hit, because the two products look similar
and the download names differ by two characters.

### Detection

FreeInk ships a production X3/X4 fingerprint (`libs/hardware/XteinkDetect/`)
that Calendula can port directly: probe the **X3-only I²C peripherals** on
SDA=GPIO20 / SCL=GPIO0 — BQ27220 at `0x55`, DS3231 at `0x68`, QMI8658 at
`0x6B`/`0x6A`. The X4 has none of them. Two independent passes each scoring ≥2
hits confirms an X3; zero hits in both passes confirms an X4; anything else —
passes disagreeing, or a single stray ACK — is **inconclusive**. The probe
releases the bus and returns both pins to `INPUT` when done, and is documented
safe to call before any other hardware bring-up.

Calendula already talks to the BQ27220 on exactly these pins in its X3 build
(`fw/src/main.rs`, I2C0 at 400 kHz with the raised bus timeout), so the hardware
path is proven in-tree; what is new is running it in the **X4** build, where
GPIO0 is the battery ADC divider and GPIO20 is the C3's `U0RXD`. Briefly driving
those and releasing them is what FreeInk does in production on this board, but it
is the one thing in this issue that must be validated on hardware rather than
assumed.

### Why a guard rather than one unified X3/X4 binary

FreeInk and CrossPoint do compile both profiles into a single C3 binary and
select at runtime. That is the right call for them and the wrong one here:

- Framebuffers are statically sized — X4 48,001 B, X3 52,273 B, two allocations.
  A unified image sizes both for the X3, so every X4 gives up 4,272 B in `.bss`
  plus 4,272 B in `dram2_seg`. `_stack_end` is exactly
  `ADDR(.bss) + SIZEOF(.bss)`, so that is **~8.5 KB straight off the X4's main
  stack**, against a 27 KB floor that has already shipped silent `.bss`
  corruption once.
- Geometry is compile-time deliberately, and that is where the performance came
  from: the byte-run rasterizer fast paths (#24/#46) and the portrait glyph
  transpose (#50, 2.5×) are specialised against constant dimensions. Making
  width/height runtime data risks the measured 13 ms portrait layout to solve a
  distribution problem.
- Goldens, `tools/emulator` (built twice today), and `reader-cache` would all
  need both geometries live at once.

FreeInk can unify because it is C++ with heap-flexible buffers and runtime board
profiles. Calendula's no-heap, budgeted-RAM, geometry-specialised rules are
exactly what make unification expensive. A guard costs no RAM and no throughput.

### The awkward part: a mismatched build probably cannot paint

If X4 firmware is running on an X3, the wrong *controller* driver is compiled in
too — SSD1677 command sequences sent to a UC8253, and vice versa. The panel will
most likely not refresh at all. So the guard cannot rely on the screen to deliver
its message.

It can rely on the SD card: same pins on both boards (CS GPIO12, shared display
bus), and `fw/src/sd_session.rs` already owns that path. FreeInk uses the same
trick for probe diagnostics — persist them "somewhere a user can retrieve
WITHOUT serial access (locked units): e.g. a file on the SD card."

So the guard's job is to **stop, and leave evidence a non-technical user can
retrieve.** Write a plain-text file to the card root naming what was detected,
what this firmware is, and which download to use; attempt the panel message
best-effort in case the controller does respond; then halt rather than proceed.

Without this, the user sees a dead screen and no explanation. With it, they see a
dead screen and a text file on the SD card telling them exactly which file to
download — recoverable without a serial cable, a forum post, or a support
request.

## Scope

### Files

- **[NEW]** `hal-ext/src/board_probe.rs` — the two-pass I²C fingerprint, releasing
  the bus and restoring pin modes; `BoardVerdict { X3Confirmed, X4Confirmed,
  Inconclusive }`
- **[MODIFY]** `hal-ext/src/lib.rs` — export the module
- **[MODIFY]** `fw/src/main.rs` — run the probe early; on a confirmed mismatch,
  take the refusal path instead of normal bring-up
- **[NEW]** refusal path — SD diagnostic file, best-effort panel message, halt
- **[MODIFY]** `proto/src/ota.rs` — reuse `parse_identity` so the compiled-in
  board is read from one place, not re-derived. It only ever parses *this
  build's* identity, which is always current-format
  (`CalendulaOS X3 u2` after `ota-identity-rename`), so no legacy-form handling
  is needed here

### Design rules

1. **Refuse only on a confirmed mismatch.** `Inconclusive` proceeds normally. A
   flaky probe must never brick a correctly flashed device — this is the property
   that makes a guard safer than a unified binary, which would silently drive the
   wrong panel on a mis-detect.
2. **The message must be actionable.** Name the detected board, the firmware's
   board, and the exact file to download. Plain language, short lines — these
   readers are stuck and possibly not technical.
3. **No dependency on network, serial, or a working panel.**
4. **C3 only.** The probe pins are native USB D+ and a strapping pin on the
   ESP32-S3; FreeInk's header calls this out explicitly. Compile the probe out of
   the Sticky build entirely rather than guarding it at runtime.
5. **Halt, don't reboot.** A boot loop looks identical to a dead device and would
   rewrite the diagnostic file endlessly.

### Dependencies

- Relates to `panel-controller-detection`: both are early-boot probes on the same
  pins-before-peripherals budget, and they must be sequenced deliberately. This
  one answers "which board", that one answers "which controller on this board" —
  run this first, since a wrong-board verdict makes the controller question moot.
- Relates to `reterminal-sticky-support` issue 01, which must choose a
  `PROJECT_NAME` for the S3 build; this issue is the reason that choice should
  not default to an X4 identity.
- Would move behind the board layer from `reterminal-sticky-support` issue 02 if
  that lands first, rather than hardcoding pins in `main.rs`.

### Notes

- **Hardware availability limits validation.** The owner has no X4, so the
  X4-detected-and-matching case cannot be tested. Both *mismatch* cases are
  testable on an X3 alone: flashing the X4 build to an X3 is precisely the
  scenario, and it is safe because a failed guard is recoverable by reflashing.
  The X3-confirmed positive case is testable. Say which cases were exercised.
- Cost is a handful of I²C transactions at boot on the X3, and two probe passes
  on the X4 that find nothing. Measure it against boot-to-first-paint rather than
  assuming it is free.
- Consider writing the verdict to the diagnostic file on *every* boot, not only
  on mismatch — it makes remote diagnosis of "my screen looks wrong" answerable
  without hardware access.

## Done when

- The two-pass I²C fingerprint returns `X3Confirmed` on an X3 and does not
  disturb the fuel gauge, the ADC, or serial afterwards.
- The probe runs before display and SD bring-up and restores both pins.
- A confirmed mismatch halts before any panel or geometry-dependent
  initialisation.
- The refusal path writes a legible plain-text diagnostic to the SD card root
  naming detected board, firmware board, and the correct download.
- `Inconclusive` proceeds with normal boot and is recorded, not acted on.
- The probe is absent from the S3 build.
- Flashing the X4 build to an X3 produces the diagnostic file instead of a
  garbled or dead device, verified on hardware.
- Boot-to-first-paint is measured before and after; the added cost is stated.
- `tools/check.sh all` passes for X4 and X3.
