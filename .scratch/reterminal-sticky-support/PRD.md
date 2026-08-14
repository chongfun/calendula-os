# reTerminal Sticky support

Status: ready-for-human

## Dependency

This PRD begins only after the Multi-platform firmware architecture PRD is complete.

Required prerequisites:

- `fw-c3` owns X3/X4.
- `fw-s3` independently builds, flashes, and boots.
- the exact Espressif Xtensa toolchain is recorded and reproducibly installed.
- project S3 tooling handles the required espup environment automatically or diagnoses its absence.
- Xtensa binary-analysis tooling is available.
- `fw-common` is MCU-neutral.
- `hal-ext` is MCU-neutral.
- S3 does not inherit C3 linker, allocator, radio-memory, sleep, OTA/MMU, or single-core policy.

This PRD adds a **board/device implementation** to an already-existing S3 platform.

## Goal

Run the existing CalendulaOS experience on the Seeed reTerminal Sticky while using hardware and resource policies appropriate to ESP32-S3.

The port must provide:

- reliable battery and USB boot
- correct power-latch behavior
- SSD1677 display
- MicroSD
- BQ27220 battery state
- physical buttons
- Wi-Fi/sync
- true deep sleep and power-button wake
- GT911 touch
- complete existing Calendula navigation

X3/X4 behavior must remain unaffected.

## Board ownership

Create an explicit Sticky board profile under `fw-s3`.

Conceptually:

```text
fw-s3/
  src/
    board/
      mod.rs
      sticky.rs
    platform/
      display.rs
      storage.rs
      input.rs
      battery.rs
      power.rs
      wifi.rs
```

Board identity remains distinct from MCU identity.

Do not encode:

```text
ESP32-S3 == Sticky
```

as a long-term architectural invariant.

`sticky.rs` owns hardware facts:

- pins
- buses
- power rails
- latch pins
- display configuration
- touch wiring
- physical input
- orientation

It does not own application policy.

## Power latch

The Sticky power latch is boot-critical.

Known board mapping uses:

- `PWR_HOLD`: GPIO45
- `PWR_LOCK`: GPIO46

Both are ordinary digital GPIOs/strapping pins on ESP32-S3, not RTC GPIOs.

### Boot behavior

Assert the required latch lines as early as the S3 runtime permits:

```text
reset
  ->
minimal runtime initialization
  ->
establish PWR_HOLD/PWR_LOCK asserted HIGH
  ->
normal peripheral initialization
```

Do not wait for:

- display
- SD
- application
- touch
- Wi-Fi
- book loading

Confirm from schematic/hardware which latch lines must remain asserted for battery-powered operation.

Repeated cold-boot testing must include battery-only operation.

### Deep-sleep hold

Do not describe the latch generically as an “RTC-domain” hold.

GPIO45/46 require the ESP32-S3 **digital GPIO deep-sleep hold path**.

Before deep sleep:

1. latch outputs are configured in their asserted state;
2. individual latch holds are enabled;
3. all required switched-rail pads are already at their sleep levels and individually held;
4. the wake source is configured and verified;
5. the global digital deep-sleep hold is enabled;
6. deep sleep is entered immediately.

In ESP-IDF terms this corresponds conceptually to:

```text
gpio_hold_en(latch_pad)
gpio_deep_sleep_hold_en()
```

The Rust implementation should use the appropriate `esp-hal`/low-level mechanism rather than depending on C APIs specifically.

The behavioral requirement is normative.

If the pinned `esp-hal` does not expose this digital pad-hold path, implement it as a narrow, documented register-level helper inside `fw-s3`. Do not silently skip the hold because a convenient API is missing.

### Early-boot hold reconciliation

A held digital GPIO may retain its physical state while its normal GPIO configuration has reset — and pad hold is not exclusively a deep-sleep mechanism. Hold can survive system reset, watchdog reset, and reflashing, and a previous firmware may have left latch or rail pads held.

The boot path must therefore survive at minimum:

- deep-sleep wake with holds active;
- watchdog/software reset while a pad is held;
- USB-powered reset after sleep;
- reflashing while a prior firmware left a pad held;
- booting after another Sticky firmware that uses pad hold.

On every boot, regardless of reset/wakeup cause, assume latch and switched-rail pads may retain a previous hold state.

For PWR_HOLD/PWR_LOCK: configure the desired asserted output state while any existing hold remains active, then release the individual hold so the physical pin transitions directly to the asserted state without a glitch.

For switched rails: establish the desired early-boot state while held. Rails intended to remain off may stay individually held until their peripheral is initialized. Before enabling a peripheral, configure its enable pin to the required ON state and only then release that pad's hold.

Do not rely on the recorded wake cause to decide whether this reconciliation is necessary.

The latch output must never glitch LOW during this handoff.

Validate electrically if practical, or at minimum with repeated battery-only deep-sleep cycles designed to expose accidental power loss.

A power-on reset after the wake button is not acceptable evidence of successful deep-sleep wake.

### Global hold reconciliation

The global digital deep-sleep hold is distinct, sticky state: individual pad holds choose which pads participate, while the global enable arms their retention across deep sleep — and it stays armed for every subsequent deep sleep until explicitly disabled.

Early boot must therefore also normalize the global state. After the required latch and rail output states have been established and their inherited individual holds have been reconciled, disable the global deep-sleep hold. Individual rail pads intentionally kept held while powered off may remain individually held; disabling the global deep-sleep hold does not require releasing them during active execution.

In ESP-IDF terms this corresponds conceptually to `gpio_deep_sleep_hold_dis()`.

The normal sleep path re-enables the global deep-sleep hold only after all latch and rail pads that must survive deep sleep have been individually placed into their intended sleep state and held.

The resulting lifecycle:

```text
boot
  ->
reconcile latch/rail pin states
  ->
release/retain individual holds as intended
  ->
disable global deep-sleep hold
  ->
normal runtime
  ->
terminal sleep re-enables it as the last step before the sleep instruction
```

Without this, a later individual hold — including one introduced by future platform code — silently becomes deep-sleep persistent before the deliberate sleep sequence ever arms it.

### Strapping-pin caution

GPIO45 and GPIO46 are ESP32-S3 strapping pins, and the BQ27220 bus places SCL on GPIO0 — a third strapping pin.

Their strapping values are sampled before firmware runs, but software must not introduce unnecessary pull configuration or startup behavior that conflicts with the board's boot circuit.

GPIO0 and `PWR_LOCK` GPIO46 jointly control ESP32-S3 boot mode, and the ROM samples them before any firmware executes, so early-boot reconciliation cannot repair a bad strap combination:

- GPIO0 HIGH: normal SPI boot; GPIO46 is ignored.
- GPIO0 LOW with GPIO46 LOW/floating: ROM serial-download mode.
- GPIO0 LOW while GPIO46 is held HIGH: not a valid download entry.

GPIO0 must not acquire an internal pull or other software-controlled state that interferes with its required reset strap level.

The Sticky implementation must explicitly validate the wired firmware-download/recovery path while GPIO46 may retain a HIGH pad hold from deep sleep. Do not assume the ordinary GPIO0-low serial-download sequence is valid while GPIO46 is held HIGH.

If entering the selected recovery/download mechanism requires GPIO46 LOW, provide a cooperative reboot-to-download path that safely releases the GPIO46 hold before reset, or document and validate the required power-cycle recovery procedure. Do not promise recovery without full power removal until the actual Sticky flashing path proves it.

GPIO45 also has an ESP32-S3 strapping role: on generic S3 parts it selects the VDD_SPI voltage at reset (LOW: 3.3 V, HIGH: 1.8 V) when `VDD_SPI_FORCE` is not programmed. Sticky uses an ESP32-S3R8, whose VDD_SPI configuration is expected to be eFuse-forced to 3.3 V, so GPIO45 is ignored for that purpose and may serve as the asserted `PWR_HOLD` signal across reset.

During initial hardware bring-up, verify the actual chip variant and read the relevant VDD_SPI eFuse state (`VDD_SPI_FORCE` / voltage selection) once on the target Sticky. Do not generalize the GPIO45-high latch strategy to another S3 board or chip variant without making the same check.

Verify repeated:

- battery cold boot
- USB cold boot
- reset
- deep-sleep wake

before declaring the latch implementation stable.

## Switched peripheral rails

Sticky has switched power for at least:

- EPD
- SD
- touch

Rail ownership belongs to `fw-s3`.

Startup establishes deterministic rail states before peripheral initialization.

Deep sleep explicitly powers down unused rails.

Do not access a peripheral while its rail is off.

Known board mapping uses:

- EPD power: GPIO47
- SD power: GPIO10
- touch power: GPIO42

All are ordinary digital GPIOs. Confirm the mapping and each rail's inactive level from the schematic.

### Switched-rail deep-sleep hold

The global S3 digital deep-sleep hold preserves only pads whose individual hold is enabled. Writing a rail-enable pin low before sleep is not sufficient: an unheld digital pad can revert or float once the digital domain powers down, leaving a peripheral powered, draining standby, or making wake behavior intermittent.

Before deep sleep, drive every switched peripheral rail to its intended sleep level and individually enable digital pad hold on every rail-enable GPIO whose level must survive deep sleep. For Sticky this includes the EPD, SD, and touch power-enable pads.

Only after the required rail pads and power-latch pads are individually held may the platform enable the global digital deep-sleep hold.

### Rail hold release

Rail pads follow the early-boot hold reconciliation contract in the power-latch section: on every boot, establish each pad's desired state while any hold remains active, and release a pad's hold only after its output configuration is established.

Rails that should remain off during early boot may remain held off until their peripheral is intentionally initialized; configure the enable pin to its required ON state before releasing that pad's hold.

### Upstream reference: this is solved on the same silicon

Everything above was derived from the S3 documentation. **freeink-sdk implements it on ESP32-S3 and names the Sticky by hardware characteristic**, so the requirements below are no longer only inferred. Provenance: `libs/hardware/PowerManager/src/PowerManager.cpp` (`holdRailOff`, `powerDownRailsForSleep`, `deepSleep`), `libs/hardware/BoardConfig/include/BoardConfig.h` (`releaseSdRail`), `libs/hardware/XteinkDetect/src/XteinkDetect.cpp`, `libs/display/FreeInkDisplay/src/bus/EpdBus.cpp`, commit `61f0b2b`, swept 2026-08-13.

It is C++/ESP-IDF and we are Rust/`esp-hal`, so **the API does not port and the sequencing does**. Three rules there that this PRD did not state:

**1. The panel RESET pin is a held pad, and its sleep level depends on whether the panel rail is gated.**

Upstream derives it as `resetSleepLevel = display.powerEnable >= 0 ? LOW : HIGH` and explains both directions:

- **Gated rail — this is Sticky.** Driving an unpowered panel's RESET input HIGH back-powers the controller through its RESET protection diode, "turning sleep into a milliamp-level drain". Hold RESET **LOW** alongside a switched-off rail.
- **Rail stays powered** (their X4 Pro). Hold RESET **HIGH** so the controller cannot drift out of deep sleep and restart its analog booster.

Sticky is the gated case. A generic "hold RESET at its idle level" would pick the wrong one and the symptom is standby drain, not a functional failure — it would pass every wake test and fail the battery.

**2. Rail-enable polarity is per-rail, and "off" is the inactive level, not LOW.** Upstream carries an active-high/active-low flag per rail because at least one of their boards powers its card *while held LOW*. Sticky's EPD/SD/touch enables must each be confirmed against the schematic, and the sleep write must be the inactive level for that pad rather than a blanket zero. Section "Switched peripheral rails" already says to confirm the inactive level from the schematic; this is why it matters at sleep entry and not only at init.

**3. Release-before-write is required at every consumer, not only at boot reconciliation.** Upstream calls `gpio_hold_dis` immediately before configuring a pad in *six* places, and the comment is the same each time: a retained hold makes the subsequent write a silent no-op. Their `holdRailOff` even releases before re-holding, because a hold left from a previous cycle would defeat the write it is about to make. Our global early-boot reconciliation covers the common path; it does not cover a consumer that touches a held pad before reconciliation has run, which is the subject of the next two sections.

## Display

Sticky uses an 800×480 SSD1677.

Reuse the existing SSD1677 driver.

Do not add a second Sticky-specific driver.

Add a small panel configuration seam for values that genuinely differ, including the Sticky's refresh/update and border-waveform behavior.

Existing X4 behavior must remain unchanged.

### Whatever issues the first RESET pulse must release the RESET hold itself

The sleep path holds RESET at a board-safe level (LOW on Sticky — see "Upstream reference"), and **a per-pin hold survives the wake reset**. Any code that pulses RESET before global hold reconciliation has run must therefore call the release itself, or every write bounces off the retained latch and the pulse never reaches the panel.

Upstream states this as a hard ordering constraint because their controller detection runs before display bus init, and a probe whose reset pulse silently did nothing **selects the wrong driver** rather than failing — the worst available outcome, since it produces a plausible-looking wrong answer.

For `fw-s3` this applies to whichever of these touches RESET first on a given boot, and the answer must be established rather than assumed:

- runtime panel controller detection, if the Sticky path runs the probe at all;
- the SSD1677 driver's own init;
- any early diagnostic or bring-up path added during Milestone 1.

Requirement: release the RESET pad's hold immediately before configuring it, at each such site, rather than relying on reconciliation having already run. This is the same discipline as the SD rail rescue above and fails the same silent way.

### Validation

Verify:

- black
- white
- checkerboard
- orientation
- full refresh
- fast refresh
- existing 1-bit rendering

Measure full and fast refresh times.

A “fast” refresh that takes full-refresh time is a correctness failure because the controller may have selected the wrong waveform.

Verify no persistent fast-refresh border artifact.

## Storage

Sticky MicroSD shares SPI with the EPD using separate chip selects.

Reuse shared-bus/session logic where possible.

### The SD rail must be powered before the first display transaction

**A shared bus plus a switched SD rail plus pad hold combine into a failure that looks like a dead panel.** Upstream hit it and rescues the rail explicitly (`BoardConfig::releaseSdRail`): a previous firmware's sleep path latches the SD rail off with a pad hold, the hold survives reset *and reflashing*, and on a board where SD shares the display's SPI bus **an unpowered card clamps SCLK/MOSI so the panel never hears a command**.

Sticky has all three preconditions. The consequences are worth stating plainly because they shape debugging:

- The symptom is a display that never initializes, on a build whose display code is correct.
- It survives a reflash, so "flash a known-good image" does not clear it and will actively mislead.
- It is reachable on a **first** boot of `fw-s3` on a unit that previously ran any firmware using pad hold — including an earlier `fw-s3`.

Requirement: before the first display transaction, release any inherited hold on the SD rail-enable pad, drive it to its **ON** level, and deselect the card's chip select. Do this whether or not storage is otherwise needed on that boot path, and do it even when the boot does not intend to mount the card.

Validate it directly rather than by inference: enter deep sleep with the SD rail held off, then cold-boot and confirm the display initializes. A test that only ever boots from a fully-powered state cannot see this.

`fw-s3` owns:

- S3 SPI peripheral
- DMA
- GPIO
- chip selects
- power rail

Validate hundreds of alternating display/SD transactions.

There must be no:

- simultaneous selection
- clock-retuning corruption
- DMA ownership failure
- card corruption
- display corruption

## Battery

Reuse the generic BQ27220 driver.

`fw-s3` owns battery sampling policy and its concrete I²C bus.

Known board mapping:

```text
BQ27220:
  SDA = GPIO1
  SCL = GPIO0
  address = 0x55
  separate S3 I2C controller from GT911
```

GPIO0 is an S3 strapping pin that participates in ROM boot-mode selection (see Strapping-pin caution). The battery bus implementation must not introduce an internal pull or idle drive on GPIO0 that interferes with its required reset strap level.

Do not copy X3 task implementation merely because the gauge model matches.

Emit the normal shared application battery state.

## Physical buttons

Sticky provides:

- Up
- Down
- shared Confirm/Power

Short shared-button press emits Confirm.

Long hold requests Power/sleep.

Do not synthesize a fake Back button.

Physical input is sufficient for board/runtime bring-up but not complete UI navigation.

## GT911 architecture

Implement the reusable GT911 protocol in `hal-ext`.

The generic portion owns:

- register protocol
- frame decoding
- touch-status handling
- contact decoding
- ready-state acknowledgement
- reset/address-selection timing logic that can operate on supplied output pins
- protocol errors

It must not depend on:

- `esp-hal`
- Sticky pin numbers
- app state
- orientation policy
- UI widgets

## GT911 reset/address boundary

GT911 selects its I²C slave address by sampling INT during power-on/reset.

INT therefore changes physical role:

```text
reset/address-selection phase:
    INT = host output

normal operation:
    INT = floating/input/interrupt source
```

`embedded-hal` does not provide a universal pin-mode transition abstraction.

Do not invent one.

Use a two-phase construction boundary.

Conceptually:

1. platform constructs temporary output handles for RESET and INT;
2. generic helper performs the documented reset/address-selection sequence;
3. helper returns/finishes;
4. platform releases/reconfigures INT as an input/interrupt pin;
5. normal GT911 driver is constructed from the I²C bus and selected address.

The exact Rust ownership pattern may differ.

The important invariant is that dynamic pin mode remains **platform responsibility**, while GT911 timing/protocol remains reusable logic.

When INT is in normal input state, configure it according to GT911 electrical requirements rather than leaving platform-default pulls enabled.

## Touch coordinates

Create one canonical transformation:

```text
GT911 raw coordinate
    ->
Sticky panel-native coordinate
    ->
Calendula logical coordinate
```

Handle in one place:

- axis ordering
- swap
- X mirror
- Y mirror
- panel orientation
- edge bounds

Do not repeat transforms in driver, gesture recognizer, and UI.

Validate:

- four corners
- center
- all edges

## Gesture mapping

Initial touch integration maps gestures to existing semantic actions.

Required actions:

- Confirm
- Back
- Previous/Left
- Next/Right

Keep the recognizer deliberately small:

- press
- movement
- release
- tap threshold
- swipe threshold
- swipe direction

No:

- multitouch UI
- momentum
- pinch
- configurable gesture engine
- direct widget hit testing

unless separately specified later.

## Wi-Fi

Reuse shared sync/session behavior.

`fw-s3` owns:

- S3 radio
- controller
- RNG
- network executor placement
- allocator/heap
- radio buffers

Do not copy C3's scratch-memory donation by default.

Use a straightforward S3 allocation first.

Measure:

- free heap after startup
- reader-open heap
- image-heavy workload
- pre-Wi-Fi heap
- active Wi-Fi/TLS heap
- post-sync heap
- repeated reader/sync cycles

Change S3 memory policy only if measurements justify it.

## Sleep/wake contract

Preserve the common semantic sequence:

```text
sleep requested
  ->
application permits sleep
  ->
persistent state settled
  ->
sleep frame rendered
  ->
display inactive
  ->
SD settled
  ->
Wi-Fi stopped
  ->
EPD / SD / touch rails driven inactive
  ->
rail-enable pads individually held
  ->
wake source configured
  ->
PWR_HOLD / PWR_LOCK asserted
  ->
power-latch pads individually held
  ->
global digital deep-sleep hold enabled
  ->
S3 deep sleep
```

The physical power button is the wake source.

Wake is a fresh application boot.

Log or otherwise record:

- reset cause
- deep-sleep wake cause

during validation.

## Genuine-wake requirement

Every sleep/wake acceptance test must distinguish:

**success**

```text
deep sleep entered
power remained latched
power button triggered configured wake source
reset/wakeup cause reports deep-sleep wake
```

from:

**failure disguised as success**

```text
latch dropped
board powered off
user presses power button
board cold-boots
```

A normal-looking boot screen is not sufficient evidence.

## Memory policy

Establish S3 baselines independently.

Record:

- static RAM
- heap after boot
- heap in reader
- image-heavy heap
- Wi-Fi/TLS minimum free heap
- significant task/future sizes
- framebuffer use

Do not copy C3 stack or memory thresholds.

PSRAM is deferred.

## Multicore

Do not use the second S3 core in this PRD.

Sticky first establishes a stable single-execution-topology baseline.

Later multicore work gets its own PRD and must revalidate synchronization and allocator assumptions.

## OTA

Sticky OTA is out of scope.

Do not expose a path backed by C3-specific MMU/image logic.

Initial Sticky firmware is flashed through the wired S3 development path.

Future S3 OTA requires its own PRD.

## Implementation milestones

### Milestone 1: Board and core hardware

Add:

- Sticky board selection
- latch boot behavior
- digital pad-hold primitives for latch and rail pads
- unconditional boot-time hold reconciliation
- deterministic rail setup
- SSD1677 configuration
- display SPI
- SD/shared bus
- BQ27220
- digital buttons

No GT911.

No full app integration requirement.

#### Done when

- 50 battery cold boots pass
- latch survives button release
- the documented Sticky reset/reflash path is validated from a state where latch and rail pads were left held; strap levels are verified compatible with that path, and any case requiring a power cycle is explicitly documented
- the target identifies as the expected ESP32-S3R8 and its VDD_SPI eFuse configuration confirms GPIO45 does not control flash/PSRAM voltage
- after boot reconciliation the global digital deep-sleep hold is disabled for normal runtime
- display full/fast modes work and are timed
- SD works
- hundreds of SD/display alternations pass
- **the display initializes on a cold boot entered from a state where the SD rail was left held off** — the shared-bus clamp described under Storage. Reaching this state deliberately is part of the test, not a hazard to avoid
- **every site that pulses RESET releases the RESET hold first**, verified by inspection of each such site rather than by the display happening to work on a boot that had no inherited hold
- battery readings are plausible
- physical controls work
- shared-driver changes do not regress C3

### Milestone 2: Runtime, Wi-Fi, sleep/wake

Wire Sticky hardware into `fw-common`.

Add:

- library
- reader
- display runtime
- Wi-Fi/sync
- battery events
- physical input
- sleep handshake
- S3 wake source
- complete digital-latch deep-sleep hold
- switched-rail sleep levels with per-pad hold
- wake-path exercise of the boot-time hold reconciliation

#### Done when

- library loads
- books open/render
- representative EPUBs work
- Wi-Fi/sync works repeatedly
- memory baseline is recorded
- genuine S3 deep sleep is reached
- wakeup cause confirms actual deep-sleep wake
- no latch glitch/power-off occurs
- 50 battery-only sleep/wake cycles pass
- EPD, SD, and touch rails are verified off during genuine deep sleep, measured at the rail or enable pad rather than inferred from pre-sleep writes
- **the panel RESET pad is verified LOW during deep sleep**, not merely held — Sticky's gated EPD rail makes a held-HIGH RESET a back-power path through the controller's protection diode, and the cost is standby current rather than a failed wake
- **standby current is measured**, because every failure mode in the two items above is invisible to a functional sleep/wake test and shows up only as drain. A cycle count that passes 50/50 is not evidence the hold levels are right

Full navigation is not required because Back still requires touch.

### Milestone 3: GT911 and complete navigation

Add:

- generic GT911 driver
- two-phase reset/address initialization
- Sticky I²C/pin ownership
- INT reconfiguration
- coordinate transform
- gesture recognition
- semantic input
- idle-timer integration

#### Done when

- all corners/edges map correctly
- physical and touch input coexist
- Confirm/Back/Previous/Next work
- every existing screen is navigable
- post-wake GT911 initialization is reliable
- repeated gestures produce no stuck/duplicate state
- touch activity resets the shared idle timer, and an in-progress touch cannot trigger sleep
- touch rail cycling does not destabilize I²C or battery sampling

## Final validation

Run on physical Sticky:

- 50 battery cold boots
- 50 USB resets
- repeated display full/fast refresh
- hundreds of SD/display alternations
- extended EPUB reading
- image-heavy EPUBs
- repeated reader ↔ Wi-Fi sync transitions
- touch corners and edges
- repeated Back/navigation gestures
- mixed button/touch interaction
- 50 battery-only genuine deep-sleep wake cycles
- standby soak with rails verified off
- memory/resource measurements

For deep-sleep tests, record wake cause. A power-on reset fails the test.

Where practical, instrument PWR_HOLD/PWR_LOCK during sleep and wake to prove they never pulse inactive.

## C3 regression

After shared-driver changes run:

- host tests
- X3 build
- X4 build
- C3 stack-frame checks
- dependency-boundary checks
- display tests
- battery-driver tests
- X3 hardware smoke test

If SSD1677 configuration changes shared code, prove X4 emits the same existing command behavior.

## Non-goals

- S3 toolchain architecture
- `fw-common` creation
- C3 restructuring
- universal board trait
- PSRAM
- multicore
- S3 OTA
- microphone
- RTC feature
- environmental sensor
- IMU
- buzzer
- touch-first UI redesign
- direct widget hit testing
- multitouch
- X4 Pro

## Architectural invariants

1. `app-core` does not know Sticky exists.
2. `fw-common` contains no Sticky GPIOs.
3. `hal-ext` GT911/BQ27220 remain MCU-neutral.
4. `fw-s3` owns concrete peripherals.
5. GPIO45/46 latch handling and held rail-enable pads use the S3 digital deep-sleep hold mechanism rather than an RTC-GPIO assumption.
6. every boot reconciles both individual pad holds and the global digital deep-sleep-hold state before entering normal runtime, restoring held latch and rail-enable outputs to their intended configuration before any hold release.
7. genuine deep-sleep wake is distinguished from cold boot.
8. touch pin-mode switching remains platform-owned.
9. input converges at the semantic application boundary.
10. S3 memory policy remains independent from C3.
11. C3 behavior remains unchanged.

## Risks

1. **Power-latch sequencing.** A mistake can turn normal boot, sleep, or wake into apparent random shutdown.
2. **Hold release.** Releasing a held pad before restoring its output configuration — on any boot, not only deep-sleep wake — can glitch it to its reset/default level: on GPIO45/46 that removes board power; on a rail pad it can pulse a peripheral rail. A stale hold left by a previous boot or firmware can likewise pin a rail off and make peripheral initialization fail despite correct writes.
3. **Unheld rail pads.** The global digital deep-sleep hold preserves only individually held pads. A rail-enable pin written low but not held can revert or float in deep sleep, leaving a peripheral powered or destabilizing wake.
4. **Strap/download interaction.** GPIO0 (BQ27220 SCL) and GPIO46 (`PWR_LOCK`) jointly select ROM boot mode before firmware runs; GPIO46 held HIGH blocks the ordinary GPIO0-low download entry, so the recovery/reflash path must be proven, not assumed.
5. **Shared SPI.** Display and SD may work alone while failing under repeated alternation.
6. **SSD1677 waveform selection.** Incorrect fast mode can look correct but actually perform full refresh.
7. **Deep sleep versus power-off.** A cold boot after latch loss can masquerade as wake unless reset/wakeup cause is checked.
8. **GT911 reset/address sequencing.** INT must be output during selection and input afterwards.
9. **GT911 orientation.** Center taps can work while edges remain mirrored/offset.
10. **S3 memory overengineering.** Measure before importing C3 workarounds.
11. **Premature multicore.** Explicitly deferred.

## References

- Seeed reTerminal Sticky hardware documentation and V01 schematic/pinout (latch wiring, rails, buttons, touch).
- FreeInk SDK Sticky `BoardConfig` (pin map including `PWR_HOLD` GPIO45 / `PWR_LOCK` GPIO46, rail enables EPD GPIO47 / SD GPIO10 / touch GPIO42, and BQ27220 on SDA GPIO1 / SCL GPIO0 with its GPIO0-strap warning), SSD1677 update-control/border-waveform values, GT911 initialization, and power-management behavior (rails driven inactive and individually held before the global deep-sleep hold).
- CrossPoint Reader v1.5+ Sticky support as a second independently working implementation.
- ESP32-S3 Technical Reference Manual and ESP-IDF GPIO documentation for the digital pad-hold path (`gpio_hold_en`, `gpio_deep_sleep_hold_en`) and the strapping roles of GPIO0/45/46; the ESP32-S3 datasheet/eFuse documentation for the S3R8 VDD_SPI forcing that frees GPIO45 for `PWR_HOLD`.
- CalendulaOS Multi-platform firmware architecture PRD for `fw-common`/`fw-s3`/`hal-ext` ownership boundaries and the S3 toolchain contract.

## Done when

Sticky support is complete when:

1. battery cold boot is reliable;
2. PWR_HOLD/PWR_LOCK are established early;
3. the required latch and switched-rail states survive deep sleep;
4. every boot reconciles and releases latch and rail holds without a power-dropping or rail-pulsing glitch;
5. recorded wake cause proves genuine S3 deep-sleep wake;
6. SSD1677 full/fast refresh works correctly;
7. SD and display reliably share SPI;
8. BQ27220 works;
9. physical buttons work;
10. the normal shared Calendula reader runs;
11. Wi-Fi/sync works without C3 memory assumptions;
12. GT911 initialization and touch work reliably;
13. every existing screen is navigable;
14. C3/X3/X4 remain unchanged;
15. S3 resource/performance baselines are recorded;
16. PSRAM, multicore, OTA, and unused Sticky peripherals remain optional follow-up work.

At that point Sticky is a complete device on the S3 platform, not a C3 port with board-specific exceptions.
