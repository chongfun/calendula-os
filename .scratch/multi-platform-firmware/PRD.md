# Multi-platform firmware architecture

Status: ready-for-human

## Problem

CalendulaOS currently has one firmware executable whose architecture, toolchain, linker layout, memory policy, sleep implementation, radio setup, and hardware construction are all rooted in the ESP32-C3/Xteink platform.

ESP32-S3 is not simply another board configuration.

It differs in:

- CPU architecture: Xtensa versus RISC-V
- Rust compiler/toolchain
- linker and memory layout
- core count
- atomics assumptions
- sleep/wake implementation
- radio resource policy
- panic/backtrace tooling
- potential PSRAM use
- potential multicore execution
- OTA/MMU implementation

Adding S3 by progressively adding target `cfg`s to the current executable would make the firmware package responsible for the union of unrelated platform policies.

Creating a separate repository would create the opposite problem by duplicating product behavior that should remain shared.

The desired architecture is:

**one CalendulaOS repository, one shared application/runtime layer, and separate MCU-platform firmware executables.**

## Decision

Split firmware into three conceptual layers:

```text
                    shared product crates
                  app-core / display / ui
                  proto / reader / storage
                           |
                           v
                       fw-common
                      /         \
                     v           v
                  fw-c3        fw-s3
                 /    \           |
                X3    X4       Sticky / future S3
```

`fw-c3` and `fw-s3` are separate Cargo packages and separate firmware executables.

Each owns its platform's:

- compiler/toolchain selection
- chip-specific Espressif dependencies
- startup
- linker configuration
- memory policy
- allocator
- executor topology
- concrete peripherals
- sleep/wake implementation
- radio construction
- panic/backtrace/logging transport
- OTA/MMU implementation

`fw-common` is a `no_std`, host-buildable library containing embedded runtime behavior that genuinely belongs to both platforms.

Do not create a universal `BoardHardware`, `Platform`, or equivalent mega-trait whose purpose is to hide every possible hardware difference.

Share behavior and messages rather than forcing identical resource ownership.

## Principles

### Share product behavior, not constraints

Common behavior includes:

- application state
- semantic input
- reader lifecycle
- library behavior
- pagination
- rendering semantics
- persistence
- refresh planning
- sync/session semantics
- storage workflows
- sleep handshake
- user-visible errors

Platform behavior may differ in:

- memory placement
- stack strategy
- framebuffer placement
- allocator
- scratch reuse
- radio heap
- DMA
- SPI construction
- physical input
- RTC/deep sleep
- executor layout
- core affinity
- logging backend
- PSRAM
- OTA/MMU

A mechanism needed because the C3 is constrained is not automatically part of CalendulaOS's shared architecture.

### Do not constrain S3 to C3 internals

C3 may continue using:

- custom framebuffer placement
- C3-specific stack limits
- single-core assumptions where valid
- reader-scratch donation to Wi-Fi
- other measured memory optimizations

S3 may independently use:

- conventional internal-RAM allocations
- different radio resources
- different task layout
- later PSRAM
- later second-core execution

Shared interfaces specify outcomes, not the internal mechanism.

## Package structure

Target structure:

```text
app-core/
display/
ui/
proto/
reader-cache/
upload-store/

hal-ext/

fw-common/
    src/
        runtime.rs
        app.rs
        book_build.rs
        catalog.rs
        library.rs
        upload.rs
        views.rs
        ...

fw-c3/
    Cargo.toml
    build.rs
    src/
        main.rs
        board/
        platform/
        tasks/

fw-s3/
    Cargo.toml
    src/
        main.rs
        board/
        platform/
        tasks/
```

Exact files may differ.

The dependency direction is normative.

## `app-core` versus `fw-common`

Keep these separate.

`app-core` owns deterministic product/domain behavior:

- state
- reducer
- semantic `Button`
- commands and events
- refresh decisions
- persistence policy
- sleep gating

`fw-common` owns MCU-neutral embedded orchestration:

- task-level workflows
- runtime channels
- book-build orchestration
- shared storage workflows
- shared display workflow
- upload/session behavior
- firmware coordination

`app-core` should remain highly host-testable and unaware of embedded runtime details.

## Runtime channels

Move shared firmware communication into a common runtime container or equivalent MCU-neutral ownership structure.

Conceptually:

```rust
pub struct Runtime {
    pub input_events: ...,
    pub display_commands: ...,
    pub display_events: ...,
    pub storage_commands: ...,
    pub library_events: ...,
    pub sync_commands: ...,
    pub sync_events: ...,
    pub power_events: ...,
}
```

Each firmware executable owns one static instance.

Do not place GPIO, DMA, radio, RTC, or other concrete HAL types in it.

The current channel statics use `CriticalSectionRawMutex`. Preserve that choice during extraction rather than substituting a single-core-only mutex.

`CriticalSectionRawMutex` is valid only while each target supplies an appropriate `critical-section` implementation.

The C3 platform must retain a correct C3 implementation.

The S3 platform must use a critical-section implementation that remains safe if work later runs on both cores.

Any future multicore PRD must explicitly revalidate this invariant.

## Espressif dependency ownership

MCU selection is by firmware package, not Cargo feature.

`fw-c3` selects exactly the C3 feature for:

- `esp-hal`
- `esp-rtos`
- `esp-radio`
- `esp-storage`
- `esp-backtrace`
- `esp-println`
- other chip-specific Espressif crates

`fw-s3` selects exactly the S3 feature.

Shared crates select neither.

Forbidden direct dependencies from `fw-common` include:

- `esp-hal`
- `esp-radio`
- `esp-rtos`
- `esp-storage`
- `esp-backtrace`
- `esp-println`
- `esp-alloc`

`esp-alloc` is platform policy even though it does not select a chip and is not appropriate for host compilation.

## `hal-ext`

Make `hal-ext` MCU-neutral.

It may contain drivers expressed through standard embedded traits, such as:

- BQ27220
- generic EPD bus helpers
- GT911
- future reusable peripheral drivers

Move C3-specific RTC/deep-sleep helpers to `fw-c3`.

After this migration, `hal-ext` must not depend on `esp-hal`.

## Rust toolchains

The C3/host and S3 firmware paths deliberately use different Rust compilers.

### Upstream toolchain

The repository default remains upstream Rust.

`rust-toolchain.toml` must specify an **exact Rust release**, not the moving `stable` channel.

For example, the eventual implementation uses the exact version validated by CI:

```toml
[toolchain]
channel = "<exact validated Rust release>"
```

Do not choose the version in this PRD. Select it during implementation from the newest release that passes all existing C3, host, wasm, and tooling checks.

The exact version becomes repository policy and changes only in an explicit toolchain-update change.

### Espressif Xtensa toolchain

`xtensa-esp32s3-none-elf` is not an upstream Rust target.

`fw-s3` therefore uses the Espressif Xtensa Rust fork installed by `espup`.

The repository must record one exact validated Xtensa toolchain version in a single source of truth consumed by:

- local setup tooling
- CI
- S3 checks
- S3 flash/build tooling

Do not rely on whichever `esp` toolchain happens to be installed on a developer machine.

The setup path must invoke `espup` with that exact toolchain version and only the targets/resources needed for the project.

The `espup` installer or CI action itself must also be version-pinned rather than tracking an unqualified latest release.

### Toolchain invocation

The repository default remains upstream Rust.

S3 commands explicitly select the Espressif toolchain, for example:

```text
cargo +esp ...
```

Do not switch the repository-wide default to `esp`.

Do not depend on a `fw-s3/rust-toolchain.toml` being selected by `cargo -p fw-s3` from the workspace root. Rustup selects toolchain overrides from the process's current directory, not from Cargo's selected package.

### Espressif environment

Installing `esp` is not sufficient by itself on all hosts.

On Unix, `espup` generates environment settings for the Xtensa/GCC/LLVM tooling.

Project-owned S3 commands must therefore either:

1. source/use the generated environment automatically; or
2. verify it is active and fail with an actionable setup error.

Do not depend on an undocumented shell-profile modification.

Local and CI setup must use the same logical bootstrap contract.

### LLVM analysis tools

S3 binary-analysis commands must use tools capable of understanding Xtensa objects.

If project tooling requires LLVM `objdump`, `nm`, or equivalent tools, the S3 bootstrap must install the full required LLVM tools, such as through the validated `espup --extended-llvm` path, or explicitly provide another validated Xtensa-capable toolchain.

Do not state that S3 stack/size/disassembly checks exist until the required binaries are installed and exercised in CI.

### Compiler compatibility

Shared crates must compile with both:

- the exact upstream compiler
- the exact Espressif compiler

The Espressif fork may lag upstream language/library support.

CI must therefore compile shared code through both compiler paths.

Adding a language/library feature supported only by the upstream compiler is not allowed until the pinned S3 compiler supports it.

### Toolchain identity checks

CI must log and validate:

```text
rustc -Vv
cargo -V
```

for both compiler paths.

S3 tooling must fail clearly if an installed `+esp` toolchain does not match the recorded expected version.

## Workspace invocations

Remove the repository-wide default embedded target.

The root workspace is virtual, so without package selection Cargo would otherwise operate on all members.

Set:

```toml
[workspace]
default-members = [
    # host-buildable/shared crates only
]
```

so:

```text
cargo check
cargo test
```

at the repository root remain meaningful host operations.

Firmware commands are always explicit.

Conceptually:

```text
cargo -p fw-c3 --target riscv32imc-unknown-none-elf ...
cargo +esp -p fw-s3 --target xtensa-esp32s3-none-elf ...
```

Project scripts should own the exact flags.

A single `cargo --workspace` invocation containing both firmware executables is unsupported because the packages require different targets/toolchains and combining both chip configurations can unify incompatible Espressif features.

## Cargo.lock

Keep one committed workspace `Cargo.lock`.

This provides:

- one repository-wide dependency-resolution baseline
- atomic dependency updates
- no independent C3 and S3 lockfiles drifting apart

Do **not** claim that a shared lockfile means every target builds an identical dependency graph.

Cargo may legitimately contain:

- target-specific dependencies
- different activated features
- more than one version of a crate where resolution requires it

Both firmware builds must use `--locked` in CI/release validation.

If exact dependency parity for a particular shared dependency is required, test that invariant explicitly rather than inferring it from the existence of one lockfile.

Also verify the committed lockfile remains readable by both pinned Cargo versions.

## Cargo target configuration

Target-specific runners, rustflags, link arguments, and atomic assumptions belong under their target-specific configuration.

The C3-only:

```text
portable_atomic_unsafe_assume_single_core
```

must apply only to the C3 build.

Do not enable `portable-atomic/unsafe-assume-single-core` through a shared dependency feature.

Move C3-specific HAL environment choices such as:

```text
ESP_HAL_CONFIG_PLACE_SWITCH_TABLES_IN_RAM=false
```

into C3-owned tooling/configuration.

S3 begins from S3 defaults and changes only from measurement.

## Input architecture

`InputEvent` remains shared.

C3 may acquire input through:

- ADC ladders
- physical GPIOs
- current battery mechanisms

S3 may acquire input through:

- digital buttons
- GT911 touch
- separate I2C devices

Both eventually emit shared semantic actions.

Do not expose C3 ADC types or Sticky coordinates to `app-core`.

## Display/storage architecture

Platform executables own:

- SPI peripheral
- DMA channel
- concrete GPIO
- chip selects
- rail control

Share generic transfer/session logic where its interfaces remain narrow.

Where Embassy task entry points require concrete hardware types, use thin platform-specific task wrappers calling generic/common async implementations.

Do not introduce dynamic dispatch or allocation solely to erase peripheral types.

## Power architecture

Keep the semantic sleep handshake common:

1. sleep requested
2. application determines safe point
3. persistent state settles
4. display completes the sleep frame
5. platform receives permission
6. platform enters terminal deep sleep
7. wake is treated as a fresh boot

Actual wake source and sleep instruction remain platform-specific.

C3 retains its existing mechanism.

S3 implements its own.

## Wi-Fi architecture

Common behavior:

- credential semantics
- sync commands/events
- HTTP/session behavior
- onboarding behavior
- user-visible failures

Platform-owned behavior:

- radio peripheral
- controller construction
- buffers
- RNG
- allocator/heap policy
- network executor placement
- teardown

The C3 reader-scratch donation mechanism remains C3-private.

S3 is not required to reproduce it.

## Logging

Shared code uses a platform-neutral logging facade.

Platform executables install their own backend.

C3 extraction must preserve existing machine-consumed benchmark/log records byte-for-byte where tooling depends on them.

S3 transport is platform policy.

## OTA and image identity

Initial S3 support does not implement OTA.

The existing C3 OTA/MMU mechanism remains owned by `fw-c3`.

Generic ESP image validation may be parameterized by target policy where appropriate.

### C3 rename invariant

Renaming the firmware package from `fw` to `fw-c3` must **not** change the OTA project identity used by deployed devices.

Fielded updaters validate candidate project identity against the running identity.

The canonical C3 identity remains sourced from the existing `proto::ota::IDENTITY_*` policy, not from:

- Cargo package name
- executable artifact name
- directory name

Milestone 1 must verify the serialized OTA/image identity before and after the package rename is byte-identical for X3/X4.

Any build-script or descriptor stamping based on `CARGO_PKG_NAME` must be audited.

## Memory ownership

Memory policy is platform-private.

### C3

Preserve current measured behavior, including where applicable:

- custom linker layout
- framebuffer placement
- stack floor
- stack-frame validation
- scratch lifetimes
- Wi-Fi memory donation

Do not redesign these during architecture extraction.

### S3

Start with a conventional, correct internal-RAM arrangement.

Do not import C3 thresholds.

Measure independently.

PSRAM is deferred.

## Multicore

Do not use the S3's second core during this architecture work.

Do not apply a single-core atomic assumption to S3.

The architecture must permit a later multicore design without changing `app-core` or restructuring firmware again.

Any later second-core work must revalidate:

- critical-section implementation
- channel mutexes
- allocator synchronization
- driver ownership
- core affinity
- executor topology

## Dependency-boundary validation

Add a small automated check based on Cargo metadata/tree output.

Verify:

- `fw-common` has no chip-specific ESP dependencies
- `fw-common` has no `esp-alloc`
- `hal-ext` has no `esp-hal`
- `fw-c3` does not depend on `fw-s3`
- `fw-s3` does not depend on `fw-c3`
- the C3 graph enables only C3 chip features
- the S3 graph enables only S3 chip features

Do not build a bespoke dependency-analysis framework.

## Migration milestones

### Milestone 1: Explicit C3 firmware boundary

Rename `fw` to `fw-c3`.

Scope:

- package/directory rename
- workspace membership
- tooling paths
- CI paths
- artifact paths
- documentation
- AGENTS.md
- remove global embedded default target
- add host `default-members`
- make C3 target explicit
- scope C3 atomic configuration
- scope C3 HAL environment
- pin the exact upstream Rust version

No runtime redesign.

#### Done when

- bare root `cargo check` and `cargo test` operate on host/default members
- X3 release builds normally
- X4 release builds normally
- stack-frame tooling works
- benchmark telemetry is unchanged
- X3 hardware smoke test passes
- OTA descriptor/project identity is byte-identical before and after rename
- existing C3 update compatibility is unchanged
- `AGENTS.md` describes the new host/firmware invocation rules

### Milestone 2: `fw-common`

Create the MCU-neutral runtime library.

Move only genuinely shared behavior.

Expected candidates:

- runtime channels
- application orchestration
- catalog/library behavior
- book-build workflow
- upload/session behavior
- shared rendering coordination
- sleep handshake

Use thin concrete task wrappers when needed.

#### Done when

- `fw-common` host-checks/tests
- no direct `esp-*`/`esp-alloc` dependency
- C3 uses `fw-common`
- C3 behavior remains unchanged
- channel extraction retains `CriticalSectionRawMutex`
- all C3 checks remain green
- C3 release flash/RAM size and stack-frame results are re-compared against the pre-extraction baseline; material deltas caused by the crate split are investigated (fat LTO usually restores cross-crate inlining, but measurement decides)

### Milestone 3: Clean platform ownership

Scope:

- remove `esp-hal` from `hal-ext`
- move C3 RTC/deep-sleep helpers to `fw-c3`
- keep generic drivers in `hal-ext`
- keep C3 linker/memory/radio/MMU/OTA under `fw-c3`
- parameterize generic image-validation policy where required
- add dependency-boundary CI

#### Done when

- `hal-ext` is MCU-neutral
- `fw-common` has no platform dependencies
- no shared task type contains a concrete ESP peripheral
- dependency checks enforce the architecture
- X3/X4 remain unchanged
- C3 flash/RAM/stack measurements are re-compared after the moves; a source-only relocation is not assumed free

### Milestone 4: S3 platform proof

Create `fw-s3`.

Before implementation, establish the reproducible S3 toolchain bootstrap:

- exact Espressif compiler version recorded
- espup/action version recorded
- Unix export environment handled
- required Xtensa LLVM/binutils tools installed
- tooling verifies compiler identity

Then add:

- S3 chip dependencies
- Xtensa target
- standard S3 linker layout
- panic/backtrace
- logging
- minimal Embassy runtime
- minimal path through `fw-common`

No Sticky peripheral support.

No PSRAM.

No multicore.

No OTA.

#### Done when

- a clean machine can install the documented pinned S3 toolchain
- CI and local tooling select that exact compiler
- required Xtensa analysis tools execute successfully
- shared crates compile under both pinned compilers
- `fw-s3` builds with `--locked`
- `fw-s3` flashes and repeatedly boots
- diagnostics are reliable
- a minimal `fw-common` path executes
- C3 builds do not enable S3 chip features
- S3 builds do not enable C3 chip features
- C3 behavior remains unchanged

## Risks

1. C3 regressions from crate extraction.
2. Extracting too much behind generic traits.
3. Extracting too little and duplicating product policy.
4. Espressif chip-feature leakage.
5. Cargo/workspace commands accidentally selecting the wrong packages or target.
6. **Compiler/toolchain drift.** Two firmware platforms use different rustc distributions. Exact versions, bootstrap tooling, environment setup, lockfile compatibility, and dual-compiler shared-crate checks are mandatory.
7. Premature S3 memory optimization.
8. Premature multicore work.
9. Future multicore execution invalidating assumptions in synchronization primitives unless the platform critical-section implementation is revalidated.

## Non-goals

- Sticky peripherals
- GT911
- PSRAM
- S3 OTA
- S3 multicore
- C3 memory redesign
- universal MCU abstraction
- universal board trait
- separate repositories
- identical runtime topology across C3 and S3

## Done when

1. C3 has its own firmware executable.
2. S3 has its own independently buildable/bootable executable.
3. shared firmware behavior lives in `fw-common`.
4. reusable drivers are MCU-neutral.
5. both compilers are exactly versioned and reproducible.
6. shared crates are validated with both compilers.
7. root Cargo commands remain useful for host development.
8. both firmware paths share one committed lockfile and build with `--locked`.
9. C3 and S3 chip features cannot leak into one another.
10. C3 OTA identity survives the package rename unchanged.
11. C3-specific memory/radio/sleep/OTA mechanisms stay private to C3.
12. S3 is free to develop different memory, PSRAM, multicore, radio, and OTA strategies later.

At that point ESP32-S3 is a first-class CalendulaOS firmware platform rather than an extension of the C3 executable.
