# 01 — Multi-MCU build foundation (ESP32-S3)

Status: ready-for-human

Part of [reTerminal Sticky support](../PRD.md). First milestone; nothing else
starts until this lands.

## Problem

Calendula assumes ESP32-C3 throughout the build. `fw/Cargo.toml` selects
`esp32c3` on six crates, `.cargo/config.toml` sets
`build.target = "riscv32imc-unknown-none-elf"` and an espflash runner for that
target, `rust-toolchain.toml` installs the RISC-V target on stable, and
`tools/cargo.sh` hardcodes the stable toolchain and names the RISC-V target in
its error text.

The S3 target is `xtensa-esp32s3-none-elf` and needs Espressif's Xtensa Rust
toolchain (`espup`), not upstream stable. Getting a chip-neutral build is the
whole of this milestone: **no Sticky pins and no peripheral functionality here.**

## Context

Verified at the pinned versions: `esp-hal` 1.1.1, `esp-rtos` 0.3.0, `esp-radio`
1.0.0-beta.0, `esp-storage` 0.9.0, `esp-backtrace` 0.19.0 and `esp-println`
0.17.0 all expose an `esp32s3` feature. No version bumps are required.

### Device / chip split

The device is fully determined by **target triple × geometry feature**. The
Sticky is 800×480, which is already the default geometry (the absence of
`device-x3`), so **it needs no new geometry feature** — only the S3 target:

```text
riscv32imc-unknown-none-elf  × (none)      = X4      (default, today's build)
riscv32imc-unknown-none-elf  × device-x3   = X3      (792x528)
xtensa-esp32s3-none-elf      × (none)      = Sticky  (800x480)
xtensa-esp32s3-none-elf      × device-x3   = nonsense -> compile_error!
```

That is a 2×2 with one guarded cell, not a lattice — which is what keeps this
change inside `AGENTS.md`'s "features form a fixed matrix" rule.

**Chip selection: target-specific dependency tables**, not feature-forwarding.
C3 and S3 are different triples, so:

```toml
[target.riscv32imc-unknown-none-elf.dependencies]
esp-hal = { version = "1.1.1", features = ["esp32c3", "unstable"] }
# ...

[target.xtensa-esp32s3-none-elf.dependencies]
esp-hal = { version = "1.1.1", features = ["esp32s3", "unstable"] }
# ...
```

**Board selection: a `build.rs`-derived cfg, not a Cargo feature.** Since board
identity follows from the triple, deriving it removes a second source of truth
that could disagree with the target — and a cfg cannot be enabled independently,
cannot unify, and cannot be turned on in a C3 build. Emit
`cargo::rustc-cfg=board_sticky` from `fw/build.rs` alongside
`cargo::rustc-check-cfg`; without the latter the `unexpected_cfgs` lint fails a
`-D warnings` build. A real `device-*` board feature only becomes necessary when
a second S3 board exists (X4 Pro), and that guard is mutual exclusion within one
architecture — much smaller than the cross-MCU one.

### Feature unification: what is and is not a hazard

Verified empirically against this workspace's `resolver = "2"`: two target tables
selecting mutually exclusive chip features resolve to exactly one per target
(`--target riscv32imc-…` pulled only the `esp32c3` PAC, a second table's chip
only for its own target), and neither reaches a host build. Resolver 2 does not
unify features from target tables that do not match the build target, so **chip
selection needs no unification planning.**

Two things do:

1. **Every workspace crate that depends on an esp crate must use the same target
   tables.** `hal-ext/Cargo.toml` also hardcodes `esp-hal` with `esp32c3`. If
   `fw` moves to target tables and `hal-ext` does not, the two unify for the same
   target and the S3 build gets `esp32c3` *and* `esp32s3` on `esp-hal` — exactly
   the failure the tables prevent. Treat this as an invariant with a check
   (`cargo tree -f "{p} {f}" --target …`, asserting one chip feature per esp
   crate), not as a convention.
2. **`device-x3` remains an ordinary additive feature** on `display` and
   `reader-cache`, unified workspace-wide for a given target. That is the one
   place a `compile_error!` is genuinely needed: `device-x3` selected against the
   Xtensa target. Gate `ota-selftest` off on S3 too, since the OTA path is
   excluded there.

Side effect worth stating in the PR: `--all-features` becomes a hard compile
error rather than a nonsense build. Nothing in this repo runs it and `AGENTS.md`
prohibits it twice, so this is an improvement — but it is a behavior change.

### The two audits that matter

**`portable-atomic`.** `.cargo/config.toml`'s
`--cfg portable_atomic_unsafe_assume_single_core` is already scoped under
`[target.riscv32imc-unknown-none-elf]`, so it does **not** follow the S3 build.
The Cargo feature `unsafe-assume-single-core` in `fw/Cargo.toml` is the part that
does; move it into the C3 target dependency table. `esp-rtos` 0.3.0 only
schedules on the second core when `start_second_core()` is called, which
Calendula never will — so a single-executing-core invariant is achievable and
should be documented — but Xtensa LX7 has native compare-and-swap, so the S3
build should simply use real atomics rather than assume anything. Confirm from
the resolved build that the S3 image contains no single-core assumption; do not
infer it from the C3 config.

**`fw/build.rs`.** It is more C3-specific than it looks: it packs the
previous-frame framebuffer into `dram2_seg` and overrides `_stack_start` /
`_stack_start_cpu0` against the C3 linker layout, with a 27 KB
`MIN_STACK_BYTES` `ASSERT`. Gate that on the **target architecture**
(`CARGO_CFG_TARGET_ARCH` / `TARGET`), not on a device feature — the thing that is
C3-specific is the linker layout. On S3, emit `-Tlinkall.x` only and let the
framebuffers live in ordinary `.bss`. Optimize afterwards, against measurements.

Note for anyone building in a nested worktree under `.claude/worktrees/`: the
duplicated `-Tlinkall.x` problem documented in `.cargo/config.toml` applies here
too; use the `RUSTFLAGS` workaround rather than moving the link arg.

### Toolchain

Do not make the C3 workflow depend on `espup`. `rust-toolchain.toml` pins
`channel = "stable"`, and upstream stable has no Xtensa target — the S3 build
must come from espup's `esp` toolchain via `RUSTUP_TOOLCHAIN`. `tools/cargo.sh`
already honours `RUSTUP_TOOLCHAIN`, but its diagnostics assume stable and the
RISC-V target, so they need to name the right toolchain and target for a Sticky
invocation. `AGENTS.md` forbids changing `rust-toolchain.toml` as a side effect
of unrelated work, so state this change deliberately in the PR.

### Chip-neutral compilation

`fw/src/mmu.rs` (`MMU_TABLE = 0x600C_5000`) and `proto::ota`
(`MMU_ENTRY_COUNT = 128`, `EXPECTED_CHIP_ID = 5` — the C3's image chip id; the
S3's is 9) are C3-validated. OTA is a non-goal for Sticky, so exclude those paths
from the S3 build at compile time rather than compiling an untested version of
them.

`main.rs`'s `PROJECT_NAME` still needs a value on the S3, and it is
**`IDENTITY_STICKY = "CalendulaOS Sticky u1"`** — settled 2026-08-06, see
[`ota-identity-rename`](../../ota-identity-rename/PRD.md), which drops the
`(MarigoldOS)` suffix across all boards and moves X3/X4 to `u2`. Land that first
so this milestone adds a constant in a settled format rather than choosing one.
Do not let the descriptor default to an X4 identity.

`hal-ext/Cargo.toml` also hardcodes `esp-hal` with `esp32c3` and needs the same
target-table treatment.

## Scope

### Files

- **[MODIFY]** `fw/Cargo.toml` — target-specific dependency tables; coherence
  `compile_error!`s; document the 2×2 matrix in comments as the existing features
  are documented
- **[MODIFY]** `hal-ext/Cargo.toml` — the same target tables, mandatory (see
  unification hazard 1)
- **[MODIFY]** `.cargo/config.toml` — `[target.xtensa-esp32s3-none-elf]` runner
  (`espflash … --chip esp32s3`); leave the C3 section untouched
- **[MODIFY]** `fw/build.rs` — target-arch gate on the DRAM2/stack layout;
  conservative S3 path; derive the board cfg + `rustc-check-cfg`
- **[MODIFY]** `rust-toolchain.toml` — document the Xtensa toolchain requirement
  without making it a stable-toolchain dependency
- **[MODIFY]** `tools/cargo.sh` — toolchain/target-aware diagnostics
- **[MODIFY]** `tools/check.sh` — add a Sticky build/clippy entry point that does
  not run in the default C3 targets
- **[MODIFY]** `fw/src/main.rs`, `fw/src/mmu.rs`, `fw/src/ota_update.rs` —
  compile-time exclusion of the C3-only OTA/MMU paths from the S3 build
- **[NEW]** documentation of the S3 build/flash workflow (`docs/FLASHING.md` and
  the `README`)

### Dependencies

- Depends on: `ota-identity-rename` (supplies `IDENTITY_STICKY`; land it first)
- Blocks: `02-board-profile-extraction`, and everything after it.

### Notes

- A tiny/full firmware that builds, flashes, boots and logs on the S3 is the bar.
  No display, SD, touch or radio work belongs in this branch.
- Serial logging on Sticky: FreeInk routes Sticky logs through the IDF/ROM
  console rather than USB CDC (`FREEINK_LOG_TRANSPORT_ROM_PRINTF`), because the
  on-board WCH bridge is wired to UART0 rather than native USB. Expect
  `esp-println` transport selection to need attention before "it boots" is
  observable.
- Record the S3 stack/RAM high-water numbers separately. The C3 numbers are
  layout-derived and do not transfer.

## Done when

- `fw` and `hal-ext` select exactly one chip feature per build, for each of
  `esp-hal`, `esp-rtos`, `esp-radio`, `esp-storage`, `esp-backtrace` and
  `esp-println` — **asserted by a `cargo tree -f` check on both targets**, not by
  inspection of the manifests.
- An incompatible device/target combination (`device-x3` on Xtensa,
  `ota-selftest` on S3) fails to compile with a message naming the problem.
- Board identity is derived from the target in `fw/build.rs`, not selectable as a
  Cargo feature, and the emitted cfg is declared with `rustc-check-cfg` so
  `-D warnings` stays green.
- `fw/build.rs` emits the DRAM2/stack layout only for the C3 target; the S3 build
  links with the standard internal-RAM layout.
- The S3 build carries no `portable-atomic` single-core assumption, verified
  against the resolved build rather than inferred from config.
- The C3-only OTA/MMU paths are excluded from the S3 build at compile time.
- A firmware binary builds reproducibly for `xtensa-esp32s3-none-elf`, flashes,
  boots, and logs over serial on Sticky hardware.
- The C3 workflow is unchanged and still works without `espup` installed.
- `tools/check.sh all` passes (X4 and X3 unchanged), plus the new Sticky
  build/clippy entry point.
- S3 stack and `.bss` figures are recorded separately from the C3 ones.
