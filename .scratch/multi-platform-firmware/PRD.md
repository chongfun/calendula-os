# Multi-platform firmware architecture

Status: ready-for-human

## Problem

CalendulaOS currently has one firmware executable, `fw`, built specifically around the ESP32-C3 and the Xteink X3/X4 hardware family.

That organization was appropriate while every supported device shared the same MCU and broadly the same board architecture. It is no longer the right long-term boundary once ESP32-S3 devices are supported.

The S3 is not merely a C3 with different pins. It has a different Rust architecture and toolchain, a different memory layout, multiple cores, more available RAM, different sleep/wake facilities, different linker constraints, and potentially different future resource strategies such as PSRAM or work distributed across cores.

Adding S3 support directly to the existing `fw` crate would force increasingly unrelated platform choices behind target `cfg`s:

- C3 versus S3 linker scripts
- RISC-V versus Xtensa toolchains
- single-core versus multicore atomics assumptions
- C3 versus S3 sleep/wake implementation
- different memory and framebuffer strategies
- potentially different Wi-Fi memory policies
- different panic/backtrace configuration
- different OTA/MMU implementations
- different executor and task placement strategies

That would make the firmware abstraction boundary correspond to "all ESP32 hardware" rather than to actual shared behavior.

At the other extreme, creating a separate S3 repository or product would duplicate the parts of CalendulaOS that should remain common: reader state, EPUB handling, pagination, rendering semantics, library behavior, UI, sync protocol, storage formats, application state, and most device-independent firmware orchestration.

The desired architecture is therefore:

**one CalendulaOS codebase, one shared application, and separate MCU-specific firmware platforms.**

The C3 implementation must remain free to preserve the memory and scheduling optimizations required by X3/X4. The S3 implementation must be free to use its additional capabilities without reproducing C3 constraints merely for structural symmetry.

## Decision

Split the existing firmware into three conceptual layers:

```text
                     app-core
                    display/ui
                       proto
                  reader-cache
                  upload-store
                     hal-ext
                        |
                        v
                    fw-common
                   /         \
                  v           v
              fw-c3         fw-s3
              /  \             |
             X3  X4         Sticky / future S3 boards
```

The repository remains one Cargo workspace.

`fw-c3` and `fw-s3` are separate firmware packages and separate executables. Each owns its MCU-specific Espressif dependencies, startup, linker configuration, memory policy, task topology, and hardware integration.

`fw-common` is a `no_std` library containing firmware behavior that is genuinely independent of the MCU.

Existing domain crates remain shared.

Do not create a universal `Platform`, `BoardHardware`, or equivalent mega-abstraction that attempts to represent every peripheral on every MCU.

The primary abstraction boundary is messages and behavior, not a common bag of HAL objects.

## Architectural principles

### 1. Share product behavior, not resource constraints

The following concepts should remain common across all platforms:

- application state and transitions
- semantic input actions
- rendering requests and display events
- library commands and events
- reader lifecycle
- EPUB parsing and pagination
- persistent state formats
- catalog behavior
- sync commands and user-visible sync state
- upload protocol
- Wi-Fi credentials format
- sleep handshake semantics
- refresh planning
- UI rendering
- error semantics

The following may legitimately differ by platform:

- linker layout
- stack placement
- framebuffer placement
- heap availability
- scratch-memory ownership
- executor topology
- core affinity
- Wi-Fi controller setup
- radio-buffer sizing
- sleep/wake mechanism
- hardware input acquisition
- DMA setup
- SPI peripheral ownership
- logging transport
- panic/backtrace implementation
- OTA/MMU implementation
- use of PSRAM
- use of the second S3 core

A mechanism that exists solely because the C3 has little RAM is not a CalendulaOS contract.

### 2. No lowest-common-denominator S3 implementation

The initial S3 implementation should be simple and conservative, but the architecture must not require it to reproduce C3 constraints.

For example:

- C3 may continue placing the previous framebuffer using its custom DRAM layout.
- S3 may use an ordinary internal-RAM allocation if it fits comfortably.
- C3 may continue dismantling reader scratch and donating it to Wi-Fi.
- S3 may keep reader memory and allocate Wi-Fi resources independently if measurements show that is preferable.
- C3 remains single-core.
- S3 initially runs a simple single-core application topology, but may later use its second core for appropriate isolated workloads.
- S3 may later use PSRAM without requiring a corresponding C3 abstraction.

Shared interfaces describe what a subsystem accomplishes, not how it obtains memory or CPU time.

### 3. Platform packages own Espressif chip selection

A package must never support both ESP32-C3 and ESP32-S3 by conditionally enabling chip features inside one dependency graph.

`fw-c3` owns dependencies configured for `esp32c3`.

`fw-s3` owns dependencies configured for `esp32s3`.

This applies to:

- `esp-hal`
- `esp-rtos`
- `esp-radio`
- `esp-storage`
- `esp-backtrace`
- `esp-println`
- other chip-specific Espressif crates introduced later

The shared crates must not enable an Espressif chip feature.

This removes an entire class of accidental feature unification and target leakage.

### 4. Board identity and MCU identity are separate axes

An MCU target does not identify a physical board.

Today:

```text
fw-c3
  X3
  X4

fw-s3
  Sticky
```

Future hardware could be:

```text
fw-s3
  Sticky
  X4 Pro
  another S3 board
```

The architecture must therefore not permanently equate:

```text
xtensa-esp32s3-none-elf == Sticky
```

The platform package identifies the MCU family. Board selection inside that package identifies the hardware.

It is acceptable for `fw-s3` to contain only Sticky initially, but the separation must already exist conceptually.

### 5. Platform-specific Embassy task wrappers are acceptable

Do not contort shared code around Embassy's requirement that task entry points have concrete types.

The preferred pattern is:

```text
fw-common
  generic async behavior

fw-c3
  #[embassy_executor::task]
  concrete C3 wrapper
       |
       +--> fw_common::foo::run(...)

fw-s3
  #[embassy_executor::task]
  concrete S3 wrapper
       |
       +--> fw_common::foo::run(...)
```

A few lines of duplicated task startup code are preferable to exposing chip-specific peripheral types through shared crates.

### 6. Prefer narrow capabilities over a universal board trait

Do not create:

```text
trait Platform {
    type Spi;
    type Display;
    type Sd;
    type Input;
    type Battery;
    type Wifi;
    type Rtc;
    ...
}
```

That would encode the union of all hardware into one abstraction and make every platform depend on unrelated capabilities.

Instead, share behavior at the narrowest useful boundary.

Examples:

- a platform input task emits `InputEvent`
- a display implementation consumes `DisplayCommand`
- a power implementation consumes `PowerEvent`
- a sync implementation emits `SyncEvent`
- generic device drivers accept `embedded-hal` buses
- common async functions may be generic over the one or two capabilities they actually need

If two implementations naturally require different mechanisms, let the platform wrappers differ.

### 7. Shared crates must be host-buildable

`fw-common` must not depend directly or transitively on a chip-specific `esp-*` crate.

The same applies wherever practical to:

- `app-core`
- `display`
- `ui`
- `proto`
- `reader-cache`
- `upload-store`
- `hal-ext`

This allows shared logic to remain host-testable and prevents MCU choices from leaking upward into the application.

## Target repository structure

The intended structure is approximately:

```text
app-core/
display/
ui/
proto/
reader-cache/
upload-store/

hal-ext/
    src/
        bq27220.rs
        spi_dma.rs
        ...
    # embedded-hal based drivers only

fw-common/
    src/
        lib.rs
        runtime.rs
        app.rs
        book_build.rs
        catalog.rs
        custom_font.rs
        library.rs
        upload.rs
        views.rs
        ...
    # no esp-* dependencies

fw-c3/
    Cargo.toml
    build.rs
    src/
        main.rs
        board/
            mod.rs
            xteink.rs
        platform/
            input.rs
            power.rs
            wifi.rs
            memory.rs
            rtc.rs
            ota.rs
            mmu.rs
        tasks/
            ...
    # ESP32-C3 dependencies only

fw-s3/
    Cargo.toml
    src/
        main.rs
        board/
            mod.rs
        platform/
            input.rs
            power.rs
            wifi.rs
            memory.rs
        tasks/
            ...
    # ESP32-S3 dependencies only
```

Exact file placement may evolve during extraction. The dependency rules are more important than the directory names.

## `app-core` versus `fw-common`

Do not merge these crates.

They solve different problems.

`app-core` remains the platform-independent product/domain layer:

- `ReaderState`
- reducer behavior
- `Button`
- commands and events
- persistence policy
- refresh policy
- sleep gating policy
- pure state-transition helpers

It should remain highly deterministic and host-testable.

`fw-common` is the shared embedded runtime layer:

- Embassy-independent or generic async orchestration
- shared channel/message wiring
- book-build orchestration
- common storage workflows
- common display workflow
- upload/session behavior
- firmware-level coordination that is not product state
- code that is embedded-specific but not MCU-specific

This distinction prevents `app-core` from becoming an embedded-runtime dumping ground.

## Runtime communication

Move shared firmware channels into a common runtime container rather than exposing a collection of crate-global statics from an MCU executable.

Conceptually:

```rust
pub struct Runtime {
    pub input_events: ...,
    pub display_commands: ...,
    pub display_events: ...,
    pub library_events: ...,
    pub storage_commands: ...,
    pub power_events: ...,
    pub sync_commands: ...,
    pub sync_events: ...,
    ...
}
```

Each firmware executable owns one static `Runtime` instance and passes `&'static Runtime` to common and platform tasks.

Benefits:

- `fw-common` no longer imports globals from an executable crate.
- C3 and S3 instantiate the same semantic communication graph independently.
- platform-private channels remain outside `Runtime`
- host tests can construct isolated runtime components where useful

Do not put concrete GPIO, RTC, Wi-Fi, DMA, or other HAL types in `Runtime`.

The C3 power-button GPIO handoff, for example, remains private to `fw-c3`.

The existing channel statics already use `CriticalSectionRawMutex`. Keep that choice when they move into `Runtime`: it is sound on both MCU families, including a future multicore S3 topology. Do not downgrade shared channels to a single-core-only mutex as an extraction-time optimization.

## Input architecture

`InputEvent` remains the shared boundary.

C3 input remains free to use:

- ADC resistor ladders
- interrupt-priority polling
- X3 BQ27220 cached telemetry
- X4 ADC battery measurement
- C3 wake-pin handoff

S3 input may use:

- digital GPIO buttons
- GT911 touch
- separate I2C battery sampling
- a different polling/interrupt model

Both ultimately emit the semantic `InputEvent` understood by the application.

Do not add raw Sticky touch coordinates or C3 ADC types to application state solely to unify the implementations.

Raw diagnostic data may remain platform-specific.

## Display and storage architecture

Do not require C3 and S3 to construct identical DMA or SPI ownership graphs.

Reuse the generic pieces that already operate over `embedded-hal` traits.

Platform packages own:

- SPI peripheral selection
- DMA channels
- GPIO construction
- CS ownership
- switched power rails
- bus construction

Common code may own:

- display command sequencing
- framebuffer/render orchestration
- reader/storage state transitions
- SD filesystem algorithms
- refresh planning

Where the current display/storage task cannot move directly because its Embassy task signature contains concrete ESP types, split it into:

1. a platform-specific non-generic task wrapper
2. a shared generic async implementation

Do not introduce dynamic dispatch or heap allocation solely to erase concrete embedded types.

## Power architecture

The existing user-visible power contract remains common:

1. input requests sleep
2. application confirms that sleep is safe
3. display/storage flushes required state and renders the sleep frame
4. the power path enters terminal deep sleep
5. wake is a fresh boot
6. retained-panel state can be trusted only when the previous sleep handshake completed

The final sleep mechanism is platform-owned.

`fw-c3` may use the existing C3 RTC GPIO path.

`fw-s3` may use S3 `ext1` wake.

Common code must not depend on an RTC pin trait or a concrete `Rtc` type.

The common layer owns the handshake state machine. The platform layer owns the terminal instruction that actually sleeps the SoC.

## Wi-Fi architecture

Separate the user-visible sync/session behavior from radio resource policy.

Common:

- saved credentials semantics
- sync commands and events
- onboarding behavior
- HTTP/upload protocol
- request handling
- user-visible error mapping
- session lifecycle

Platform-owned:

- `WIFI` peripheral
- `esp-radio` controller construction
- radio buffer sizing
- heap provisioning
- RNG setup
- network executor placement
- light-sleep integration
- resource teardown

In particular, the C3 reader-scratch donation mechanism must not become an S3 platform requirement.

The existing C3 implementation may continue using it.

S3 must first be measured using its natural memory budget. It may use a simpler heap arrangement if that is sufficient.

## Logging

Shared code must not call `esp_println` directly.

Use the standard `log` facade or an equivalently small platform-neutral logging facade in `fw-common`.

Each firmware executable installs its own backend.

The C3 backend must preserve the existing log text required by benchmark and validation tooling, including `bench:` records. The extraction is not permission to silently change the telemetry protocol.

S3 may initially use UART0 or USB Serial/JTAG as appropriate for reliable bring-up.

Logging transport remains a platform decision.

## `hal-ext`

Turn `hal-ext` into a genuinely chip-neutral embedded driver/support crate.

Keep modules that operate solely through standard embedded traits, including:

- BQ27220
- generic EPD SPI bus helpers
- future GT911 driver
- other reusable peripheral drivers

Move MCU-specific RTC/sleep helpers out of `hal-ext` and into the corresponding platform package.

After this migration, `hal-ext` must not depend on `esp-hal`.

This is an architectural dependency rule, not merely a cleanup.

## OTA and image validation

Do not enable Sticky OTA as part of this work.

The current C3 updater and MMU implementation remain owned by `fw-c3`.

However, generic ESP image parsing and validation should not permanently hardcode ESP32-C3 policy inside `proto`.

Where shared OTA/image code currently embeds values such as:

- expected ESP chip id
- flash-mapped address windows
- other MCU-specific image rules

parameterize the validator with an image-target description and have `fw-c3` supply the C3 policy.

This does not implement S3 OTA.

It establishes the correct ownership boundary so S3 OTA can later provide an S3 target profile without modifying a supposedly generic parser.

C3 behavior and existing OTA tests must remain unchanged.

## Memory architecture

Memory policy is explicitly platform-private.

### ESP32-C3

Preserve the existing measured optimizations unless a separate PRD changes them:

- custom linker layout
- previous-framebuffer placement
- C3 stack floor
- C3 stack-frame verification
- reader-scratch lifetime
- one-way Wi-Fi memory donation
- size-oriented release tuning

This architecture work must not opportunistically redesign the C3 memory model.

### ESP32-S3

Start with the simplest correct internal-RAM layout supported by the toolchain.

Do not initially reproduce:

- C3 DRAM placement
- C3 stack thresholds
- C3 scratch-memory donation
- C3 linker assertions

Measure S3 independently.

PSRAM is not required initially.

The architecture must nevertheless permit S3 to use PSRAM later without changing C3 or `fw-common`.

## Multicore policy

ESP32-S3's second core is intentionally not used as part of this architecture migration.

The first S3 executable should use the simplest execution topology that boots reliably.

Do not:

- enable the C3 single-core atomic assumption
- introduce SMP complexity during platform bring-up
- build a cross-platform scheduler abstraction around C3's topology

The architecture must instead leave `fw-s3` free to introduce a second executor/core later.

Likely future candidates include isolated CPU-heavy work such as:

- image decoding
- image resizing
- EPUB preprocessing
- indexing
- selected networking work

Any such change requires measurement and its own PRD.

The application state machine does not need to become multicore merely because the S3 is capable of it.

## Cargo and toolchain architecture

Remove the repository-wide assumption that the default Cargo target is ESP32-C3.

A multi-platform workspace must make firmware targets explicit.

Conceptually:

```text
tools/check.sh x3
    -> fw-c3
    -> riscv32imc-unknown-none-elf
    -> device-x3

tools/check.sh x4
    -> fw-c3
    -> riscv32imc-unknown-none-elf
    -> default X4 board

tools/check.sh sticky
    -> fw-s3
    -> xtensa-esp32s3-none-elf
    -> Sticky board
```

Host commands such as:

```text
cargo test -p app-core
cargo test -p proto
cargo test -p fw-common
```

must not accidentally inherit an embedded target.

Keep target-specific runner and rustflags configuration under the corresponding target section.

The C3-only:

```text
portable_atomic_unsafe_assume_single_core
```

configuration must apply only to the C3 target.

Do not enable `portable-atomic/unsafe-assume-single-core` as a dependency feature shared with S3.

Move C3-only HAL environment settings such as:

```text
ESP_HAL_CONFIG_PLACE_SWITCH_TABLES_IN_RAM=false
```

out of repository-global configuration and into the C3 build/check path.

S3 starts from its own HAL defaults until measurements justify changes.

### Rust toolchains

The two MCU families do not share a compiler.

`riscv32imc-unknown-none-elf` is an upstream Rust target. C3 and host builds stay on the toolchain pinned in `rust-toolchain.toml`.

`xtensa-esp32s3-none-elf` is not an upstream Rust target. Building `fw-s3` requires the Espressif `esp` toolchain — the espup-distributed rustc fork carrying the Xtensa backend.

Rules:

- The repository default toolchain remains upstream Rust. Do not move `rust-toolchain.toml` to the `esp` channel for convenience; host crates, the wasm emulator, and C3 keep building on upstream.
- The S3 build path selects the esp toolchain explicitly, for example `cargo +esp` inside the sticky check/flash commands. rustup resolves `rust-toolchain.toml` from the invocation directory, not from the package being built, so a per-package toolchain file is not a reliable mechanism.
- Pin the esp toolchain version in the S3 tooling and CI the same way the upstream toolchain is pinned. Two machines must not silently build S3 firmware with different compiler forks.
- Shared crates are now compiled by two rustc versions, and the esp fork trails upstream. Shared code must not adopt language or library features newer than the pinned esp toolchain supports, and CI must compile the shared crates through both paths.
- S3 binary analysis (size, stack, disassembly) uses the esp toolchain's llvm-tools. The upstream toolchain's tools do not understand Xtensa objects.

### Workspace invocations

Once the default target is gone, a bare `cargo check` or `cargo test` at the workspace root would try to build the firmware executables for the host and fail. No single Cargo invocation can build both firmware packages either: they need different `--target` values and different toolchains, and resolving both in one feature-unified graph would combine mutually exclusive Espressif chip features.

Therefore:

- Set `workspace.default-members` to the host-buildable crates, so bare root-level `cargo check`/`cargo test` remains meaningful.
- Firmware packages build only through explicit `-p fw-c3 --target ...` and `-p fw-s3 --target ...` invocations owned by the project tooling.
- `--workspace` invocations that include both firmware packages are unsupported. CI and tooling must not rely on them.
- The workspace keeps one shared `Cargo.lock`. Both platforms resolve shared dependencies from the same lock, so a shared crate cannot silently run different dependency versions on C3 and S3.

## Package features

MCU choice is represented by package choice, not by features.

Bad:

```text
fw --features esp32c3
fw --features esp32s3
```

Good:

```text
fw-c3
fw-s3
```

Board variation remains a package feature or equivalent explicit board selection within a platform.

For compatibility, `fw-c3` may initially preserve:

```text
default -> X4
device-x3 -> X3
```

S3 board selection must remain logically distinct from the Xtensa target even while Sticky is its only supported device.

Invalid or ambiguous board selections must fail at compile time.

## Dependency rules

After this work:

### Allowed

```text
fw-c3 -> fw-common
fw-s3 -> fw-common

fw-c3 -> esp-*
fw-s3 -> esp-*

fw-common -> app-core
fw-common -> display
fw-common -> ui
fw-common -> proto
fw-common -> reader-cache
fw-common -> upload-store
fw-common -> hal-ext

hal-ext -> embedded-hal*
```

### Forbidden

```text
fw-common -> fw-c3
fw-common -> fw-s3

fw-common -> esp-hal
fw-common -> esp-radio
fw-common -> esp-rtos
fw-common -> esp-storage
fw-common -> esp-backtrace
fw-common -> esp-println
fw-common -> esp-alloc

hal-ext -> esp-hal

fw-c3 -> fw-s3
fw-s3 -> fw-c3
```

`esp-alloc` carries no chip feature, but it does not build on the host and installing the global allocator is platform policy. Each firmware executable owns its allocator setup; `fw-common` must stay allocator-agnostic.

Add an automated dependency-boundary check so future changes cannot casually reintroduce an ESP dependency into `fw-common` or `hal-ext`.

A simple Cargo metadata/tree check is sufficient. Do not build a custom dependency-analysis framework.

## Extraction rule

Do not move code merely because it might someday be shared.

A module belongs in `fw-common` when at least one of these is true:

1. it already expresses platform-independent firmware behavior, or
2. both C3 and S3 need the same behavior and it can be shared without importing MCU policy.

If extracting a piece of code requires a large trait representing the union of two unrelated hardware implementations, leave the thin implementation in each platform instead.

Small duplication in platform bootstrap code is acceptable.

Duplicated application policy is not.

## Migration plan

Implement this as four individually mergeable milestones.

### Milestone 1: Explicit C3 firmware boundary

Rename the existing firmware package from `fw` to `fw-c3`.

This is intentionally mechanical.

Scope:

- rename package/directory
- update workspace membership
- update build scripts
- update CI
- update benchmark tooling
- update flash commands
- update documentation and path references
- remove the workspace-wide default embedded target
- define `workspace.default-members` as the host-buildable crates so bare root cargo commands stay usable
- make X3/X4 commands select the C3 target explicitly
- scope the single-core atomic cfg to C3
- scope C3-only HAL environment configuration to C3 builds
- update AGENTS.md and other docs whose instructions assume the repository-wide firmware default target (the "pass an explicit host `--target`" rule inverts)

Do not extract common code yet.

Do not change runtime behavior.

#### Done when

- X3 release firmware builds through the normal project tooling.
- X4 release firmware builds through the normal project tooling.
- host crate tests run without an embedded default target.
- stack-frame tooling still operates on C3.
- benchmark tooling still recognizes the same telemetry.
- an X3 hardware smoke test passes.
- an X4 build and upstream/hardware verification path remains unchanged.
- release artifact naming changes, if any, are explicit and accounted for.
- the OTA app-descriptor identity and update hand-off are byte-identical across the rename: the identity comes from `proto::ota::IDENTITY_*`, not the package name, and fielded updaters validate it — verify nothing stamps `CARGO_PKG_NAME` or a changed artifact name into anything an updater checks.

### Milestone 2: Create `fw-common`

Create a new `no_std` library for shared firmware behavior.

Extract the clearest platform-neutral pieces first.

Expected candidates include:

- application task behavior
- common runtime channels
- book-build orchestration
- catalog logic
- library/storage algorithms
- upload/session behavior
- view/render preparation
- shared sleep handshake logic
- other firmware modules that have no inherent MCU dependency

Use platform-specific Embassy task wrappers where concrete task argument types prevent a direct move.

Introduce platform-neutral logging for moved code while preserving existing C3 text output.

Do not force C3 input, RTC, Wi-Fi bootstrap, DMA construction, or linker behavior through generalized traits just to increase the number of moved files.

#### Done when

- `fw-common` contains no `esp-*` dependency.
- `fw-common` can be checked/tested on the host.
- `fw-c3` depends on `fw-common`.
- C3 behavior is unchanged.
- the C3 binary contains no duplicate copy of application policy that has moved to `fw-common`.
- X3/X4 checks and hardware smoke validation remain green.

### Milestone 3: Clean platform ownership

Finish the dependency boundary before introducing S3.

Scope:

- move C3 RTC helpers out of `hal-ext`
- remove `esp-hal` from `hal-ext`
- keep generic `embedded-hal` drivers in `hal-ext`
- keep C3 linker/memory code in `fw-c3`
- keep C3 OTA/MMU code in `fw-c3`
- parameterize any otherwise-generic ESP image-validation policy that currently hardcodes C3
- keep the C3 Wi-Fi scratch-memory donation private to `fw-c3`
- ensure shared task/runtime types contain no concrete ESP peripheral
- add automated dependency-boundary checks

Do not redesign C3 behavior.

#### Done when

- `hal-ext` has no `esp-hal` dependency.
- `fw-common` has no direct or transitive chip-specific ESP dependency.
- C3-only memory, RTC, MMU, OTA, radio, and input mechanisms reside under `fw-c3`.
- shared crates contain no C3/S3 target-selection `cfg` needed to make the firmware architecture work.
- all existing host tests pass.
- X3/X4 firmware checks pass.
- C3 hardware behavior remains unchanged.

### Milestone 4: Prove the S3 platform boundary

Create `fw-s3` as a second firmware executable.

This milestone is an architecture proof, not Sticky hardware support.

Scope:

- ESP32-S3 dependency configuration
- Xtensa toolchain entry point
- pinned esp (Xtensa) toolchain selection wired into tooling and CI
- standard S3 linker/memory configuration
- S3 panic/backtrace output
- reliable diagnostic output
- minimal Embassy runtime
- instantiate/use enough `fw-common` code to prove the dependency direction is real
- no C3 single-core atomic assumption
- no C3 DRAM linker optimization
- no C3 OTA/MMU code
- no Sticky peripheral pinout

Do not add:

- GT911
- Sticky display
- Sticky SD
- Sticky battery
- Sticky switched rails
- Sticky Wi-Fi session
- PSRAM
- second-core execution
- OTA

Those belong to subsequent platform/device work.

#### Done when

- `fw-s3` builds using the S3 toolchain independently of `fw-c3`.
- the esp toolchain is pinned and selected explicitly by tooling; host and C3 builds remain on the upstream toolchain.
- it flashes to the available S3 development hardware.
- it boots repeatedly.
- diagnostics are reliable.
- it executes a minimal path through `fw-common`.
- building S3 does not enable C3 chip features.
- building C3 does not enable S3 chip features.
- X3/X4 builds remain unchanged.

## Resulting Sticky plan

After this PRD lands, the existing Sticky support plan should no longer implement multi-MCU support or a generic board seam inside the C3 firmware.

Those problems have already been solved at the correct architectural boundary.

Sticky becomes an `fw-s3` device effort:

### Sticky milestone 1: board and hardware bring-up

Implement under `fw-s3`:

- explicit Sticky board profile
- PWR_HOLD/PWR_LOCK
- switched EPD/SD/touch rails
- SSD1677 configuration
- shared display/SD SPI bus
- BQ27220
- physical buttons
- S3 deep-sleep/wake
- Wi-Fi
- S3-specific memory measurements

Use `fw-common` for shared application behavior.

Do not reproduce C3 memory mechanisms unless measurements independently justify them.

### Sticky milestone 2: GT911 and full navigation

Add:

- GT911 driver in generic `hal-ext`
- Sticky touch backend in `fw-s3`
- gesture-to-semantic-action mapping
- full navigation validation
- idle/power integration

Further S3 capabilities can then evolve independently.

## Validation

### Structural

Automate the following invariants:

- `fw-common` has no chip-specific ESP dependencies.
- `hal-ext` has no chip-specific ESP dependencies.
- `fw-c3` resolves only C3 Espressif chip features.
- `fw-s3` resolves only S3 Espressif chip features.
- no dependency exists from one platform package to the other.
- host-testable shared crates compile without selecting an ESP target.

### C3 regression

Architecture work is not allowed to become an optimization rewrite.

For X3/X4:

- build release firmware
- run the existing fast checks
- run stack-frame checks
- compare flash/RAM usage
- compare benchmark telemetry
- verify page rendering
- verify SD access
- verify input
- verify battery handling
- verify Wi-Fi
- verify sleep/wake
- verify OTA on the hardware where it is currently supported

Any material binary-size, stack, RAM, or performance change caused solely by crossing a crate boundary must be investigated.

Fat LTO should generally permit cross-crate optimization, but measurements decide whether the result is acceptable.

### S3 architecture proof

Before Sticky-specific hardware work:

- build `fw-s3`
- flash
- obtain boot output
- reset repeatedly
- verify panic output
- verify a minimal Embassy task runs
- execute a small shared `fw-common` path
- inspect memory layout
- record initial stack/RAM measurements

These measurements are informational at this stage. Do not invent S3 thresholds by copying C3 values.

## Non-goals

- Rewriting the application state machine.
- Changing EPUB behavior.
- Changing rendering semantics.
- Redesigning X3/X4 hardware support.
- Optimizing C3 memory.
- Optimizing S3 memory.
- Using the S3's second core.
- Using PSRAM.
- S3 OTA.
- Sticky peripheral support.
- GT911 support.
- A universal abstraction over all Espressif MCUs.
- A universal board trait.
- Supporting non-Espressif MCUs.
- Splitting CalendulaOS into multiple repositories.
- Guaranteeing identical task topology between MCU families.
- Guaranteeing identical memory strategy between MCU families.
- Removing small amounts of duplicated platform startup code.

## Risks

### 1. Extracting too much

The largest architectural risk is treating "shared" as a goal rather than a property.

If a common API begins accumulating MCU-specific associated types or capabilities, stop and leave the thin implementation platform-local.

### 2. Extracting too little

If `fw-c3` and `fw-s3` both contain copies of application state transitions, reader lifecycle policy, or sync semantics, the split has failed in the opposite direction.

Those behaviors belong in `app-core` or `fw-common`.

### 3. Accidental C3 regression

The current C3 firmware contains memory and scheduling choices based on hardware measurement.

A crate split can alter:

- inlining
- static placement
- stack use
- binary size
- task future sizes

Re-measure C3 after each extraction milestone rather than assuming a source-only move is free.

### 4. Cargo feature leakage

Espressif chip crates use mutually significant chip features.

Keeping C3 and S3 in separate firmware packages greatly reduces this risk, but CI must verify the resolved dependency trees rather than relying on source inspection alone.

### 5. Premature S3 optimization

The S3 will invite immediate use of:

- two cores
- PSRAM
- larger caches
- larger buffers
- different task partitioning

Do none of these during the architecture proof.

First establish a boring, correct S3 platform. Optimize later from measurements.

### 6. Toolchain split

C3 and S3 compile with different rustc distributions: upstream Rust for RISC-V, the espup-managed `esp` fork for Xtensa.

The fork trails upstream, so a shared-crate change accepted by one compiler can fail the other, and the two toolchains update on independent schedules.

Pin both, select the esp toolchain explicitly in tooling, and have CI compile the shared crates through both paths.

## Future direction

This architecture deliberately permits the two firmware platforms to evolve differently.

A plausible future state is:

```text
                     CalendulaOS
                         |
               shared application/runtime
                    /             \
                   /               \
            ESP32-C3              ESP32-S3
         constrained profile    capable profile
           X3 / X4             Sticky / X4 Pro
              |                     |
       tight internal RAM       larger internal RAM
       one core                 optional multicore
       scratch reuse            independent resources
       C3 OTA                   eventual S3 OTA
                                optional PSRAM
```

That divergence is healthy as long as user-visible product semantics remain shared.

The S3 should not be artificially constrained to behave internally like a C3.

The C3 should not become more complicated merely to expose capabilities it does not possess.

## Done when

This architecture PRD is complete when:

1. the existing firmware is an explicitly C3-owned executable;
2. shared firmware behavior lives in an MCU-neutral `fw-common`;
3. reusable hardware drivers are MCU-neutral;
4. C3-specific linker, memory, input, RTC, radio, MMU, and OTA mechanisms are owned by `fw-c3`;
5. `fw-s3` exists as an independently buildable and bootable executable;
6. no shared crate selects an Espressif chip;
7. C3 and S3 builds cannot accidentally enable each other's chip configuration;
8. host tests no longer inherit a repository-wide C3 target;
9. X3/X4 behavior and measured constraints remain intact;
10. S3 is free to adopt different memory, task, multicore, PSRAM, and OTA strategies later without restructuring the shared application again.

At that point, ESP32-S3 support is no longer being "bolted onto" the C3 firmware.

CalendulaOS has become a shared reader platform with two intentionally separate firmware implementations.
