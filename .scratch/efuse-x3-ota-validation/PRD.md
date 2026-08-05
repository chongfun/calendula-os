# Investigate eFuse block validation for X3 OTA

Status: needs-triage

## Problem

Crosspoint includes a linker override of `bootloader_common_check_efuse_blk_validity` because the X3's bootloader misreads the app descriptor through a misaligned `bootloader_mmap` pointer, producing garbage eFuse revision values that fail OTA validation.

CalendulaOS uses esp-hal (bare-metal, no ESP-IDF) and writes OTA partitions directly in `fw/src/ota_update.rs`. The question is whether the ESP32-C3 bootloader's own validation (which runs on the NEXT boot after OTA writes `otadata`) hits this same misaligned read. If it does, the device could fail to boot the new OTA slot and fall back or brick.

## Context

The app descriptor in `fw/src/main.rs` sets `max_efuse_blk_rev_full = 65535`, which may make this a non-issue: any revision ≤ 65535 passes the bootloader check even with garbage reads. This reasoning needs confirmation.

### Investigation steps

1. Read `fw/src/ota_update.rs` to understand the OTA write path.
2. Check whether CalendulaOS sets `min_efuse_blk_rev_full` / `max_efuse_blk_rev_full` in the app descriptor (it does: 0 / 65535 in `EspAppDesc`).
3. Since `max_efuse_blk_rev_full` is 65535, the bootloader check should always pass even with garbage reads (any revision ≤ 65535). Verify this reasoning.
4. If the check CAN fail, implement the linker wrap override.

## Scope

### Files

- **[READ]** `fw/src/ota_update.rs` — OTA write path
- **[READ]** `fw/src/main.rs` — `EspAppDesc` with eFuse revision fields
- **[POSSIBLY NEW]** linker wrap for `bootloader_common_check_efuse_blk_validity` (only if needed)

### Dependencies

- None.

### Notes

- The app descriptor in `fw/src/main.rs` sets `max_efuse_blk_rev_full = 65535`, which may make this a non-issue.
- Needs confirmation by tracing the bootloader's validation logic for the ESP32-C3.

## Done when

- The bootloader's eFuse block validation path is traced and documented for the ESP32-C3 bare-metal (esp-hal) case.
- A determination is made: either the `max_efuse_blk_rev_full = 65535` value makes the check a no-op, or a linker wrap override is needed.
- If an override is needed, it is implemented and tested.
- Findings are documented in this file's Comments section.
