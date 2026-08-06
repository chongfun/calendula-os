# CACHE3 + POS3 Architecture Implementation Plan

Status: proposed

## Overview

Replace the existing `CACHE2` multi-file, 28-bit key collision, `CFG.BIN` registry, and mixed-generation migration system with a deterministic, physically isolated two-slot cache layout (`CACHE3`) and a separate full-identity reading position store (`POS3`).

---

## Phase 1: Protocol Constants & Identity Encoding (`proto`)

- **Location**: `proto/src/cache.rs`
- **Tasks**:
  1. Define `CACHE_V3_DIR` (`"CACHE3"`), `POSITION_V3_DIR` (`"POS3"`), `SLOT0_DIR` (`"SLOT0"`), `SLOT1_DIR` (`"SLOT1"`), `COMMON_DIR` (`"COMMON"`).
  2. Define recency durable generation filenames `RECENT_GENERATIONS` (`["RECENTA.BIN", "RECENTB.BIN"]`) and magic `RECENT_DURABLE_MAGIC` (`*b"MGRC"`).
  3. Add `source_identity_hex(hash: u32, size: u32)` helpers to format 8-character hex strings for directory paths (`/XTEINK/CACHE3/<hash-8>/<size-8>/` and `/XTEINK/POS3/<hash-8>/<size-8>/`).
  4. Preserve existing layout key computation (`layout_cache_key`) and `BookV2Header` format.

---

## Phase 2: Separate Durable Reading Position (`reader-cache` & `fw`)

- **Location**: `reader-cache/src/files.rs`, `fw/src/book_build.rs`
- **Tasks**:
  1. Update `write_position_file` and `read_position_file` to use `/XTEINK/POS3/<hash-8>/<size-8>/` with durable generations `POSA.BIN` / `POSB.BIN`.
  2. Add legacy position fallback: if `POS3` record is missing, attempt reading legacy position from `/XTEINK/CACHE2/<key>/POS.BIN` or `POSA/B.BIN` once, and migrate forward to `POS3`.
  3. Decouple position reading/writing from cache directory operations so cache clearing/eviction never touches position records.

---

## Phase 3: Implement Physical Two-Slot Cache Engine (`reader-cache`)

- **Location**: `reader-cache/src/files.rs`
- **Tasks**:
  1. **Directory Helpers**: Create helpers to open/ensure `COMMON/`, `SLOT0/`, `SLOT1/` under `/XTEINK/CACHE3/<hash-8>/<size-8>/`.
  2. **Slot Matching & Reading**:
     - `read_cache_header`: Inspect `SLOT0/BOOK.BIN` and `SLOT1/BOOK.BIN`. Return `CacheHeader::Present(header)` if a valid header matches requested layout.
     - `read_v2_section_cache_in`: Read sections directly from `SLOTx/SECTIONS/S<nnn>.BIN`.
     - `read_v2_toc_window`: Read `COMMON/TOC.BIN`.
     - `load_v2_cover_cache`: Read `COMMON/COVER.BIN`.
  3. **Slot Eviction & Advisory Recency**:
     - `read_recent_slot` / `write_recent_slot`: Use `RECENTA.BIN` / `RECENTB.BIN` to track most recently used slot index (0 or 1).
     - Slot Selection: If requested layout matches a slot, return that slot and update recency. If neither matches, pick an uncommitted/invalid slot, or the least-recently-used slot as victim.
  4. **Publication Protocol**:
     - Step A: Completely empty victim slot directory `SLOTx/` (delete `BOOK.BIN` and `SECTIONS/`).
     - Step B: Write section files to `SLOTx/SECTIONS/S<nnn>.BIN`.
     - Step C: Write `SLOTx/BOOK.BIN` **last** as the commit record.
     - Step D: Write advisory recency update to `RECENTA/B.BIN`.
  5. **Shared Assets (`COMMON/`)**:
     - Write `TOC.BIN`, `COVER.BIN`, `CONT.BIN` into `COMMON/`.
  6. **Cache Clearing**:
     - `empty_cache_dir`: Completely remove `/XTEINK/CACHE3/<hash-8>/<size-8>/`.

---

## Phase 4: Firmware Integration & Obsolete Cache Cleanup (`fw`)

- **Location**: `fw/src/book_build.rs`, `fw/src/library_sd.rs`
- **Tasks**:
  1. Update `book_build.rs` to invoke the `CACHE3` slot publication flow and `POS3` position storage.
  2. Update `library_sd.rs` orphan sweep to iterate `/XTEINK/CACHE3/<hash-8>/<size-8>/` directories, parsing full source identity directly from directory names.
  3. Add background `CACHE2` sweep: perform best-effort removal of `/XTEINK/CACHE2/` tree on startup / library scan.

---

## Phase 5: Test Suite Modernization (`reader-cache/tests`)

- **Location**: `reader-cache/tests/publish_faults.rs`, `reader-cache/tests/transaction.rs`
- **Tasks**:
  1. Remove tests for `CFG.BIN`, 28-bit key collision purges, and unlisted index reconstruction.
  2. Add unit/integration tests for `CACHE3`:
     - Two-slot layout coexistence (`SLOT0` and `SLOT1`).
     - Atomic commit: uncommitted slot (missing/corrupted `BOOK.BIN`) is ignored and reclaimed on next open.
     - Recency tracking (`RECENTA/B.BIN`) and fallback when recency record is missing/corrupted.
     - Shared asset handling (`COMMON/TOC.BIN`, `COVER.BIN`).
     - `POS3` position isolation across cache clears and slot evictions.
     - Legacy `CACHE2` background cleanup.

---

## Verification Criteria

1. `tools/check.sh fast` (host unit tests).
2. `tools/check.sh emulator` (X4 & X3 visual scenarios & goldens).
3. `tools/check.sh firmware` (X4 & X3 firmware compilation, clippy, stack budget).
4. `tools/check.sh all` (full workspace verification).
