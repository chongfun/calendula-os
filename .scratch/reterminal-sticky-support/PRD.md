# reTerminal Sticky support

Status: ready-for-human

## Dependency

This PRD depends on the **Multi-platform firmware architecture** work being complete.

Before Sticky implementation begins:

- `fw-c3` owns the existing ESP32-C3 X3/X4 firmware.
- `fw-s3` exists as an independently buildable, flashable, and bootable ESP32-S3 firmware executable.
- `fw-common` contains MCU-neutral firmware orchestration.
- `hal-ext` contains only MCU-neutral `embedded-hal` drivers/support.
- `fw-s3` does not inherit C3 linker, memory, RTC, OTA/MMU, single-core, or Wi-Fi memory policy.
- board identity is logically separate from MCU identity even if Sticky is initially the only `fw-s3` board.
- an S3 image can already boot, log reliably, and execute a minimal path through `fw-common`.
- the pinned Xtensa (`esp`) toolchain is installed and selected explicitly by tooling and CI, per the architecture PRD's toolchain policy.

This PRD does **not** introduce ESP32-S3 support. It adds the Seeed reTerminal Sticky as a device on the existing S3 platform.

## Problem

CalendulaOS has an ESP32-S3 firmware platform but does not yet support the reTerminal Sticky's hardware.

The Sticky differs substantially from X3/X4 at the board level:

- 800×480 SSD1677 e-paper display
- SPI MicroSD sharing the display bus
- GT911 capacitive touch
- physical Up and Down buttons
- a shared Confirm/Power button
- BQ27220 fuel gauge
- PWR_HOLD/PWR_LOCK power latch
- switched EPD, SD, and touch power rails
- S3-specific deep-sleep/wake wiring
- completely different GPIO assignments

The existing CalendulaOS product behavior should remain shared:

- library
- reader
- EPUB parsing
- pagination
- rendering semantics
- settings
- persistence
- Wi-Fi credentials
- sync
- semantic input actions
- sleep handshake

The Sticky implementation should therefore consist primarily of an `fw-s3` board profile, S3-specific hardware tasks, and reusable device drivers rather than changes to application logic.

## Goal

Run the existing CalendulaOS reader experience on the reTerminal Sticky while allowing the S3 implementation to use a hardware and memory architecture appropriate to the S3.

The port must:

1. boot reliably from battery and USB;
2. hold its own power latch correctly;
3. drive the SSD1677 at the intended full and fast refresh modes;
4. share SPI reliably between display and MicroSD;
5. report battery state through the BQ27220;
6. support the physical buttons;
7. connect and sync over Wi-Fi;
8. enter true deep sleep and wake from the physical power button;
9. support GT911 touch;
10. expose touch through existing semantic application actions initially;
11. make every existing CalendulaOS screen navigable;
12. leave X3/X4 unaffected;
13. avoid importing C3-specific memory or task constraints into `fw-s3`.

## Hardware context

### MCU

The Sticky uses an ESP32-S3.

The S3 platform implementation already owns:

- Xtensa toolchain
- S3 chip-feature selection
- S3 linker layout
- panic/backtrace configuration
- runtime/executor setup
- S3 atomics policy

This PRD must not modify `fw-c3` to accommodate Sticky.

### Display

The Sticky uses an 800×480 SSD1677 panel.

CalendulaOS already has an SSD1677 driver, so do not create another display driver.

However, the Sticky needs board-specific refresh configuration.

The existing X4-oriented fast-refresh command sequence must not be assumed correct for the Sticky panel. The known Sticky implementations use different update-control and border-waveform values for full versus fast refresh.

Extend the existing SSD1677 implementation with a small configuration object describing the waveform-specific values that genuinely vary by panel/board.

Conceptually:

```rust
pub struct Ssd1677Config {
    pub full_update_control: u8,
    pub fast_update_control: u8,
    pub full_border_waveform: u8,
    pub fast_border_waveform: u8,
}
```

The exact representation is implementation-defined.

The important rule is:

**panel waveform variation is data supplied to one SSD1677 driver, not a second Sticky driver.**

The existing X4 configuration must preserve byte-for-byte behavior unless separately changed by another PR.

### Display orientation

Treat orientation as board data.

Validate Sticky orientation against hardware rather than compensating in application rendering.

The display layer should continue rendering in CalendulaOS's logical coordinate system.

Do not add Sticky-specific coordinate transforms to `app-core`.

### Storage

The Sticky MicroSD and e-paper panel share an SPI bus with separate chip selects.

This matches the architectural model CalendulaOS already uses for X3/X4.

Reuse the existing generic shared-bus/session logic where possible:

- one underlying SPI/DMA bus
- explicit per-device CS
- SD identification at the required slow clock
- SD data access at the normal faster clock
- display transfers at their appropriate clock
- no peripheral selected while another owns the bus
- explicit bus-speed transitions

`fw-s3` owns the concrete S3 SPI peripheral, DMA channel, GPIOs, and power rails.

Do not create a second independent SPI bus in software merely because it is easier to express.

### Physical input

The Sticky has:

- Up
- Down
- shared Confirm/Power

The physical controls should enter the application through the existing semantic input boundary.

Short press of the shared button:

```text
Confirm
```

Long hold:

```text
Power / sleep request
```

Up and Down map to the appropriate existing navigation/page actions according to current Calendula behavior.

Do not synthesize a fake physical Back action.

There is no dedicated physical Back button. Full navigation therefore requires touch and is not a milestone-1 or milestone-2 acceptance criterion.

### Touch

The Sticky uses GT911 capacitive touch.

Implement the reusable GT911 protocol driver in `hal-ext`.

The generic driver owns:

- controller reset/address-selection sequence
- register reads/writes
- touch-status decoding
- contact extraction
- clearing/acknowledging the ready state
- controller-level error handling

It must operate through `embedded-hal` I2C and GPIO traits.

It must not depend on:

- `esp-hal`
- Sticky GPIO numbers
- Calendula application state
- screen orientation policy
- UI widgets

One boundary detail needs care. The GT911 samples its INT line during reset to select its I2C address, so INT is driven as an output during reset and only afterwards used as an input or interrupt source. `embedded-hal` has no pin-mode-change trait. Express the generic reset/address-selection sequence over plain output pins — a two-phase construction is fine — and let the platform physically reconfigure the pin between phases. Do not invent a dynamic-mode pin abstraction for this.

The Sticky `fw-s3` input implementation owns:

- concrete I2C peripheral
- reset pin
- interrupt pin if used
- polling/interrupt scheduling
- coordinate transformation
- gesture recognition
- conversion to semantic `app_core::Button` actions
- input idle-timer notification

### Battery

The Sticky uses BQ27220.

Reuse the existing generic BQ27220 driver from `hal-ext`.

Do not copy the X3 battery task merely because the gauge is the same.

`fw-s3` should implement its own battery sampling policy using the shared driver and emit the existing application-visible battery information.

Validate the exact Sticky I2C bus and power behavior on hardware.

### Power latch

The Sticky can remove its own power if the firmware does not assert its power-hold signal correctly.

The latch must therefore be established as early as possible in Sticky startup.

The ordering requirement is:

```text
reset
  ->
minimum CPU/runtime setup
  ->
assert PWR_HOLD/PWR_LOCK
  ->
normal peripheral initialization
```

Do not wait for:

- display initialization
- SD initialization
- application startup
- Wi-Fi
- touch
- book loading

before asserting the latch.

Failure to establish the latch is a boot-critical failure.

The latch must also survive deep sleep. An ordinary S3 GPIO output does not hold its level once the digital domain powers down; if PWR_HOLD is firmware-driven, the pad must be explicitly held through deep sleep using the S3 pad-hold/RTC-domain mechanism, otherwise "deep sleep" is actually power-off. Confirm against the schematic which signal keeps the board powered while asleep, and verify the behavior on hardware.

### Switched peripheral rails

The Sticky has independently switched power for hardware including:

- e-paper
- MicroSD
- touch

Represent rail control as Sticky/S3 platform behavior.

Do not add these rails to a universal cross-platform board trait.

Startup must establish a deterministic known rail state before initializing the corresponding peripheral.

Deep sleep must explicitly turn off rails that are not required for wake.

Peripheral code must not access a device whose rail is known to be off.

### Sleep and wake

Preserve CalendulaOS's shared sleep contract:

1. user requests sleep;
2. application reaches a safe sleep state;
3. persistent state is committed;
4. the intended sleep screen is rendered;
5. platform receives terminal permission to sleep;
6. platform shuts down peripheral rails;
7. platform configures wake;
8. SoC enters deep sleep;
9. wake behaves as a fresh boot.

The actual sleep instruction and wake-source configuration belong to `fw-s3`.

Use the S3 wake mechanism appropriate to the Sticky's physical power-button GPIO.

Do not reuse the C3 RTC GPIO helper.

Configure the power latch to remain asserted across deep sleep before executing the sleep instruction. A latch that drops when the digital domain powers down silently converts sleep into power-off, and the subsequent button press cold-boots the device in a way casual testing cannot distinguish from wake.

Wake validation must distinguish:

- real deep-sleep wake
- reset
- watchdog reset
- brownout
- loss and restoration of the board power latch

Logging the reset/wakeup reason during bring-up is strongly recommended.

### Wi-Fi

Reuse the shared CalendulaOS sync/session behavior from `fw-common`.

`fw-s3` owns:

- radio initialization
- S3 Wi-Fi controller resources
- RNG
- network executor/task placement
- radio buffers
- network heap/resources
- teardown policy

Do **not** reproduce C3's memory-conservation techniques merely because they already exist.

In particular, the C3 reader-scratch donation mechanism is not an S3 contract.

Start with the simplest memory arrangement that comfortably supports:

- reader state
- display framebuffers
- storage
- Wi-Fi
- TLS/HTTP
- representative EPUB workloads

Measure before optimizing.

## Board organization

Add an explicit Sticky board definition under the S3 platform.

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

Exact file boundaries may differ if a cleaner implementation emerges.

`sticky.rs` owns hardware facts such as:

- GPIO assignments
- SPI peripheral selection
- chip-select pins
- I2C assignments
- power rails
- power latch
- physical-button pins
- touch reset/interrupt pins
- display orientation
- display waveform configuration

It should not contain application policy.

The same `fw-s3` package must be able to gain another S3 board later without pretending that:

```text
ESP32-S3 == Sticky
```

Sticky may be the default or only board initially, but device selection must remain explicit.

## Input model

The first Sticky implementation maps touch gestures to the existing semantic actions rather than redesigning the UI.

At minimum support:

```text
tap / confirmation gesture
    -> Confirm

back gesture
    -> Back

previous / left gesture
    -> Previous or PagePrevious

next / right gesture
    -> Next or PageNext
```

The exact gesture policy should follow a simple, internally consistent model and be verified on hardware.

Prefer matching established Sticky interaction conventions where they map cleanly onto Calendula.

Do not expose touch coordinates to `app-core` merely to implement these actions.

Raw coordinates remain inside the S3 input layer.

## Touch coordinate handling

Establish one canonical transformation from raw GT911 coordinates to Calendula display coordinates.

The transform must account for:

- panel orientation
- X/Y axis ordering
- any mirrored axis
- physical versus logical width/height
- edge coordinates

Keep this transformation in one place.

Do not separately transform coordinates in the driver, gesture recognizer, and UI.

Validation must include all four corners and edge regions, not just the center of the screen.

## Gesture recognition

Keep the initial recognizer deliberately small.

Required concepts:

- press
- release
- movement
- tap threshold
- swipe direction
- movement threshold
- maximum tap duration if needed

Avoid:

- momentum
- multi-touch UI
- pinch gestures
- arbitrary gesture configuration
- touch-driven direct widget selection

unless a requirement emerges during hardware testing.

The initial goal is reliable semantic navigation.

## Application integration

Do not add:

```rust
#[cfg(device_sticky)]
```

branches to shared application behavior unless there is a genuine product requirement.

The application should continue consuming the same:

- `Button`
- `InputEvent`
- display events
- storage events
- sync events
- battery state
- sleep handshake

as other platforms.

A Sticky-specific requirement should first be examined as either:

1. board configuration;
2. S3 platform implementation;
3. generic driver capability;
4. genuinely shared application behavior.

Device checks inside the state machine are a last resort.

## Memory policy

Do not inherit C3 memory thresholds.

Establish a Sticky/S3 baseline during bring-up.

Record at minimum:

- static RAM use
- heap after startup
- heap while reader is open
- heap during image-heavy EPUB rendering
- heap before Wi-Fi startup
- heap during Wi-Fi/TLS activity
- minimum observed free heap
- largest allocation headroom where available
- significant task/future stack sizes

Start with internal RAM.

PSRAM is not part of initial Sticky support.

If the implementation comfortably fits in internal RAM, keep framebuffers and hot rendering data there.

PSRAM may be evaluated later for capabilities that justify its latency and complexity.

## Multicore policy

Do not use the second S3 core as part of this PRD.

Sticky support should first establish a stable baseline with the simplest S3 execution topology.

Do not introduce multicore merely to:

- distinguish the S3 from C3
- move existing tasks around
- hide performance problems
- increase apparent parallelism

Potential future work such as image decode, indexing, EPUB preprocessing, or selected network processing may be evaluated separately.

Nothing in this PRD should prevent such a future change.

## OTA

OTA is not part of initial Sticky support.

The firmware must not expose an apparently functional OTA operation that still uses C3-specific MMU or image policy.

Sticky is initially flashed through the supported wired S3 development path.

The shared UI must either:

- hide/disable OTA where the S3 platform reports it unavailable, or
- use an existing generic capability mechanism established by the multi-platform architecture.

Do not scatter `device_sticky` checks through settings code.

S3 OTA requires a separate PRD covering:

- partition layout
- image validation
- chip id
- slot selection
- rollback
- MMU/flash mapping
- power-loss behavior
- actual-device validation

## Other Sticky peripherals

The Sticky includes capabilities beyond those needed for the reader.

Out of scope:

- PDM microphone
- PCF8563 RTC
- SHT40 temperature/humidity sensor
- LSM6DS3TR-C IMU
- buzzer

Do not initialize them "because they are there."

Each increases:

- active power
- sleep complexity
- bus interaction
- driver surface
- test surface

Add them only when Calendula has a product use for them.

## Implementation milestones

Implement Sticky in three dependent, independently reviewable milestones.

---

# Milestone 1: Sticky board and core hardware bring-up

## Goal

Turn the existing generic `fw-s3` architecture-proof firmware into a hardware-aware Sticky diagnostic firmware.

Do not attempt full Calendula operation yet.

## Scope

Add:

- explicit Sticky board selection/profile
- hardware pin definitions
- early power-latch assertion
- deterministic switched-rail initialization
- SSD1677 Sticky configuration
- S3 display bus construction
- MicroSD on the shared SPI bus
- BQ27220 communication
- digital Up/Down/Confirm+Power input
- hardware diagnostic commands/logging needed for bring-up

Add any required generic SSD1677 configuration seam to `display`.

Reuse existing generic bus and BQ27220 support.

Do not add GT911 yet.

Do not add application-specific touch work.

## Bring-up order

Bring hardware up in this order:

1. PWR_HOLD/PWR_LOCK
2. diagnostic logging
3. basic physical buttons
4. display rail
5. display SPI
6. full display refresh
7. fast display refresh
8. SD rail and card
9. shared display/SD bus alternation
10. battery gauge
11. controlled peripheral shutdown

Do not debug several new peripherals simultaneously if an earlier layer is not stable.

## Acceptance criteria

### Power

- Device cold-boots from battery with USB disconnected.
- Releasing the physical power button after boot does not cut power.
- At least 50 cold boots succeed.
- Power-latch assertion occurs before nonessential peripheral initialization.

### Display

- black frame renders correctly;
- white frame renders correctly;
- checkerboard renders correctly;
- orientation is correct;
- existing Calendula 1-bit rendering works;
- full refresh is visually correct;
- fast refresh is visually correct;
- fast refresh is measurably faster than full refresh;
- fast refresh does not silently execute the panel's full waveform;
- fast-refresh border behavior does not leave a persistent black ring or equivalent artifact.

Record representative full and fast refresh times.

### Storage

- SD initialization succeeds from cold boot;
- known directory entries can be read;
- known files can be read correctly;
- SD initialization still works after display use;
- display still works after SD use.

Run at least several hundred alternating:

```text
SD operation
display operation
SD operation
display operation
```

cycles.

No evidence of:

- simultaneous CS assertion;
- bus-speed corruption;
- DMA ownership failure;
- SD reinitialization instability;
- display corruption.

### Battery

- BQ27220 responds repeatedly;
- readings remain plausible over time;
- battery sampling does not destabilize boot;
- GPIO/strapping interactions, if any, are tested across repeated cold boots.

### Digital input

- Up detected reliably;
- Down detected reliably;
- short Confirm/Power detected as Confirm;
- long Confirm/Power distinguishable from a short press;
- bouncing does not produce uncontrolled repeated actions.

Full UI navigation is not required.

### Regression

- shared driver changes preserve existing X3/X4 behavior;
- all normal X3/X4 checks remain green.

## Done when

Sticky power, display, SD, battery, and physical controls are individually reliable and can coexist in one S3 image.

The firmware does not yet need to be a fully usable reader.

---

# Milestone 2: Calendula runtime, Wi-Fi, and power integration

## Goal

Run the shared CalendulaOS application on Sticky using the hardware established in milestone 1.

This milestone establishes Sticky as a functional reader platform except for touch-dependent navigation.

## Scope

Wire the Sticky platform into `fw-common`:

- application runtime
- library/storage workflows
- display task
- reader lifecycle
- battery events
- physical input events
- Wi-Fi
- sync
- sleep handshake
- S3 deep sleep
- power-button wake
- switched-rail shutdown
- retained-panel behavior

Measure S3 memory behavior under real workloads.

Do not add GT911 yet.

## Wi-Fi

Use the normal shared Calendula sync/session behavior.

Start with a straightforward S3 radio allocation.

Do not import C3's reader-scratch donation unless measurements show a concrete need.

Verify:

- credential entry/storage through whatever temporary test setup is needed;
- connection;
- DHCP;
- DNS;
- normal Calendula HTTP/sync operations;
- disconnect/reconnect;
- repeated Wi-Fi sessions;
- returning from sync to reader operation.

## Reader

Exercise representative EPUB workloads including:

- small text-only EPUB;
- large EPUB;
- many-section EPUB;
- image-heavy EPUB;
- custom fonts where already supported;
- repeated page turns;
- closing and reopening books;
- library transitions.

All existing shared reader behavior should work without Sticky-specific branches.

## Memory

Record S3 memory measurements while:

1. idle in library;
2. opening a normal book;
3. opening a large book;
4. rendering an image-heavy page;
5. repeatedly changing pages;
6. starting Wi-Fi;
7. actively syncing;
8. returning from Wi-Fi to a book.

Do not introduce S3-specific memory optimization without evidence.

If the straightforward implementation has comfortable headroom, that is the desired outcome.

## Sleep

Implement the S3 terminal sleep operation behind the common sleep contract.

Sequence:

```text
sleep requested
    ->
application permits sleep
    ->
persistent state settled
    ->
sleep screen rendered
    ->
display no longer active
    ->
SD settled
    ->
Wi-Fi stopped
    ->
touch rail off, when applicable
    ->
SD rail off
    ->
EPD rail off
    ->
wake source configured
    ->
power latch held for sleep
    ->
deep sleep
```

The precise safe rail ordering may be adjusted from hardware evidence.

The semantic contract must remain unchanged.

## Wake

Wake from the physical power button.

Wake is a new boot rather than continuation of a suspended async runtime.

Record boot/reset/wakeup reason during validation.

Distinguish genuine deep-sleep wake from:

- external reset;
- watchdog;
- brownout;
- latch loss.

## Acceptance criteria

- Calendula reaches the library screen.
- SD-backed library contents load.
- known books open.
- text renders correctly.
- page rendering behaves normally.
- display refresh policy behaves normally.
- physical buttons produce the expected semantic input actions.
- Wi-Fi connects reliably.
- normal sync completes.
- Wi-Fi can be entered and exited repeatedly.
- no pathological S3 heap loss appears across repeated reader/sync transitions.
- sleep reaches genuine S3 deep sleep.
- unused peripheral rails are off during deep sleep.
- physical power button wakes the device.
- the recorded wakeup cause is a genuine deep-sleep wake, not power-on.
- previous safe persistent state is restored after wake.
- at least 50 sleep/wake cycles pass.
- unplugged standby behavior shows no obvious power regression.

## Navigation limitation

Do not require every Calendula screen to be reachable in milestone 2.

Sticky has no dedicated physical Back button.

Milestone 2 is complete when all functionality reachable through the available physical controls is stable enough to validate the platform.

Do not invent a device-specific long press or chord solely to bypass this limitation.

## Done when

Sticky runs the real Calendula application, reads books, renders pages, syncs, sleeps, and wakes reliably.

Touch is the only remaining major hardware capability required for normal navigation.

---

# Milestone 3: GT911 and complete input integration

## Goal

Make the complete existing CalendulaOS UI naturally navigable on Sticky.

## Scope

Add:

- generic GT911 driver to `hal-ext`;
- concrete Sticky I2C/GPIO wiring in `fw-s3`;
- controller reset/address-selection sequence;
- touch polling or interrupt integration;
- coordinate transformation;
- gesture recognition;
- semantic action mapping;
- input idle-timer participation;
- power-rail lifecycle;
- touch initialization after wake/cold boot.

## Driver tests

Host-test the generic GT911 logic wherever practical.

At minimum cover pure decoding functions with captured/synthetic frames:

- no touches;
- one valid touch;
- edge coordinates;
- malformed/short frame;
- status-ready behavior;
- maximum relevant coordinate;
- clearing the ready state;
- invalid contact count;
- controller I/O errors.

Keep transformation and gesture tests separate from protocol decoding where possible.

## Coordinate validation

On real Sticky hardware, verify touch at:

- top-left;
- top-right;
- bottom-left;
- bottom-right;
- center;
- near each edge.

The logical coordinate result must match the visible display orientation.

No duplicated transformations.

## Gesture model

Implement the smallest reliable gesture set needed by the existing UI.

Required:

- Confirm
- Back
- Previous/Left
- Next/Right

Where appropriate distinguish reader page navigation from ordinary list navigation using the same semantic mapping already used by physical controls/application state.

Prefer a consistent touch interaction model across screens.

Do not introduce direct widget hit-testing as part of this milestone.

## Input coexistence

Touch and physical buttons must work together.

Verify:

- touch does not disable physical input;
- physical input does not leave GT911 state stuck;
- long Confirm/Power still sleeps;
- touch activity resets the same idle timer as button activity;
- a touch in progress cannot accidentally cause sleep;
- touch rail cycling does not destabilize I2C or the battery gauge.

If GT911 and BQ27220 use separate I2C controllers, keep their ownership separate rather than forcing a shared abstraction.

## Acceptance criteria

Every existing Calendula screen is navigable on Sticky.

Verify at minimum:

- library navigation;
- opening a book;
- page forward;
- page backward;
- reader menus;
- Back;
- settings;
- changing representative settings;
- returning from settings;
- library/settings transitions;
- closing/reopening books;
- sleep request;
- wake;
- post-wake touch initialization.

Repeat representative touch operations long enough to expose:

- missed releases;
- duplicate taps;
- false swipes;
- stuck pressed state;
- edge dead zones;
- coordinate inversion;
- rail-reset failures.

## Done when

Sticky supports the complete existing CalendulaOS interaction model through physical controls plus GT911, without device-specific UI branches.

---

## Final hardware validation

After all three milestones, run an umbrella validation pass on the user's physical Sticky.

### Boot

- 50 battery-only cold boots.
- 50 USB-powered resets.
- no intermittent latch loss;
- reliable diagnostics.

### Display

- repeated full refresh;
- repeated fast refresh;
- mixed full/fast sequence;
- correct orientation;
- no persistent border artifact;
- measured fast-refresh behavior remains stable after long runtime.

### Display + SD contention

Run at least hundreds of alternating display and SD operations while opening and paging through real EPUBs.

No:

- card corruption;
- failed reads;
- unexpected filesystem remount;
- display corruption;
- SPI lockup.

### Reader soak

Exercise representative EPUBs for an extended reading session:

- frequent page turns;
- images;
- large books;
- library reopen;
- settings transitions;
- Wi-Fi sessions.

No progressive heap exhaustion or task failure.

### Touch

Exercise:

- taps;
- swipes;
- edges;
- repeated Back operations;
- alternating touch/buttons;
- touch immediately after boot;
- touch immediately after Wi-Fi;
- touch immediately after wake.

### Wi-Fi

Exercise repeated:

```text
reader
 ->
sync
 ->
reader
 ->
sync
```

cycles.

Track memory before and after.

### Sleep/wake

At least 50 successful:

```text
active
 ->
sleep handshake
 ->
true deep sleep
 ->
power-button wake
 ->
fresh boot
 ->
state restoration
```

cycles.

Include runs on battery with USB disconnected.

### Standby

Leave the device asleep long enough to identify gross standby-current regressions.

Confirm:

- EPD rail off;
- SD rail off;
- touch rail off;
- Wi-Fi off;
- no unexpected periodic wakeups.

A proper current measurement is preferred where practical.

## Cross-platform regression

Sticky support must not require behavioral changes to the C3 platform.

Run normal X3/X4 validation after shared-driver changes.

At minimum:

- all host tests;
- X3 build;
- X4 build;
- C3 stack-frame checks;
- C3 dependency-boundary checks;
- display-driver tests;
- BQ27220 tests;
- X3 hardware smoke test.

If the SSD1677 configuration seam affects X4 code, explicitly verify that the existing X4 configuration produces the same command behavior as before.

Do not require S3-specific checks in the normal C3-only local development path.

## Performance and resource baseline

Record Sticky-specific baselines rather than comparing them mechanically to C3.

Capture:

- release binary size;
- static RAM;
- free heap after boot;
- minimum free heap during representative reader workload;
- minimum free heap during Wi-Fi;
- display full-refresh time;
- display fast-refresh time;
- representative page-turn latency;
- representative book-open latency;
- sleep-entry reliability;
- wake latency.

These become the initial S3 baseline for future optimization work.

Do not set pass/fail thresholds based on X3/X4 measurements unless the threshold describes a user-visible product requirement.

## Non-goals

This PRD does not include:

- ESP32-S3 platform/toolchain creation;
- restructuring `fw-c3`;
- creating `fw-common`;
- a universal `BoardHardware` abstraction;
- PSRAM;
- multicore execution;
- S3 OTA;
- PDM microphone;
- PCF8563 RTC;
- SHT40 environmental sensor;
- LSM6DS3TR-C IMU;
- buzzer;
- direct touch hit-testing of UI widgets;
- a touch-first UI redesign;
- multitouch;
- pinch/zoom;
- runtime detection of unrelated S3 boards;
- Xteink X4 Pro support;
- changing C3 memory strategy;
- copying C3 memory thresholds to S3.

## Follow-up opportunities

Once Sticky support is stable, evaluate these independently.

### Direct touch UI

The semantic gesture backend gets Sticky working without changing Calendula's UI architecture.

A future PRD may add true coordinate-based hit testing for:

- buttons;
- menus;
- list entries;
- sliders;
- reader controls.

That should be a product/UI change shared where appropriate rather than a hidden Sticky special case.

### S3 OTA

Implement and validate S3-specific:

- partitioning;
- image validation;
- slot handling;
- rollback;
- MMU/flash mapping;
- update recovery.

### S3 memory improvements

Only if measurements justify them:

- different heap layout;
- larger caches;
- retained reader memory during Wi-Fi;
- PSRAM.

### Multicore

Only after profiling identifies an isolated workload worth moving.

Possible candidates:

- image decoding;
- image resizing;
- EPUB preprocessing;
- indexing;
- selected networking work.

### Other Sticky peripherals

Add only when Calendula has a concrete feature requiring:

- RTC;
- microphone;
- environmental sensor;
- IMU;
- buzzer.

### Additional S3 boards

The next S3 board should add another explicit `fw-s3` board profile rather than forking the application or turning target architecture into board identity.

Xteink X4 Pro is an obvious future candidate.

## Risks

### 1. Power-latch sequencing

Highest board-specific risk.

If firmware asserts the latch too late or releases it accidentally, apparently unrelated initialization failures may actually be self-power-off events.

Bring this up first.

### 2. Shared SPI sequencing

Display and SD both work individually more easily than they work reliably together.

Stress their alternation before using successful one-off reads as evidence of correctness.

### 3. SSD1677 waveform configuration

An incorrect update-control sequence can look functional while making every fast refresh behave like a full refresh.

Refresh timing is part of validation, not merely a performance benchmark.

### 4. Sleep/wake versus power-off

The Sticky's latch and switched rails make it possible to confuse deep sleep, reset, brownout, and actual power removal.

Record reset/wake reasons during bring-up.

The most likely silent failure is the latch dropping during deep sleep: every subsequent "wake" is really a cold power-on, and casual testing cannot tell the difference. The recorded wakeup cause is what catches it.

### 5. GT911 orientation

A nearly correct transform can survive casual testing while leaving mirrored edges or inconsistent gesture directions.

Test corners and edges explicitly.

### 6. S3 memory overengineering

The S3's larger memory budget creates a temptation to optimize before a baseline exists.

Start simple and measure.

### 7. Premature multicore work

A second core increases concurrency state space substantially.

It is explicitly deferred until Sticky is stable and profiling identifies a reason to use it.

## Architectural invariants

The port is considered architecturally successful only if these remain true:

1. `app-core` does not know Sticky exists.
2. `fw-common` does not know Sticky GPIOs exist.
3. `fw-common` does not depend on ESP32-S3 HAL types.
4. `hal-ext` GT911 and BQ27220 drivers remain MCU-neutral.
5. `fw-s3` owns concrete Sticky peripherals.
6. Sticky does not inherit C3 linker or memory policy.
7. Sticky does not inherit the C3 single-core assumption.
8. Sticky Wi-Fi does not depend on C3 scratch-memory donation.
9. physical and touch input converge at the existing semantic input boundary.
10. SSD1677 panel differences are configuration, not duplicated drivers.
11. C3 and S3 may evolve different runtime and memory strategies.
12. shared product behavior remains one implementation.

## References

- Seeed reTerminal Sticky hardware documentation and V01 schematic/pinout.

- FreeInk SDK `BoardConfig` Sticky profile for board pins, device capabilities, power rails, physical buttons, display configuration, touch configuration, and battery hardware.

- FreeInk SDK SSD1677 implementation and Sticky-specific configuration for update-control and border-waveform behavior.

- FreeInk SDK GT911 input implementation for controller initialization and raw frame handling.

- FreeInk SDK power-management implementation for Sticky rail shutdown and S3 wake behavior.

- CrossPoint Reader v1.5+ reTerminal Sticky support as a second independently working firmware implementation.

- CalendulaOS Multi-platform firmware architecture PRD for ownership boundaries between `fw-common`, `fw-c3`, `fw-s3`, and `hal-ext`.

## Done when

reTerminal Sticky support is complete when:

1. the device cold-boots reliably from battery;
2. its power latch is established deterministically;
3. SSD1677 full and fast refresh both behave correctly;
4. MicroSD and display reliably share their SPI bus;
5. the BQ27220 reports stable battery state;
6. physical Up/Down/Confirm+Power input works;
7. the normal Calendula reader runs through `fw-common`;
8. representative EPUBs work;
9. Wi-Fi and sync work without C3-specific memory assumptions;
10. true S3 deep sleep and physical-button wake are reliable;
11. GT911 touch works over the complete display area;
12. every existing Calendula screen is navigable;
13. touch and buttons share the normal idle/power behavior;
14. no Sticky-specific application-state branches were required;
15. C3/X3/X4 behavior remains unchanged;
16. S3 memory and performance baselines are recorded independently;
17. PSRAM, multicore, OTA, and unused Sticky peripherals remain optional future work rather than accidental prerequisites.

At that point, Sticky is not an ESP32-C3 port with exceptions.

It is the first complete device implementation on CalendulaOS's ESP32-S3 firmware platform.
