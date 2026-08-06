# PRD: CACHE3 + POS3 On-Disk Architecture

Status: Draft

## Executive Summary

Replace the `CACHE2` multi-file, 28-bit key collision, `CFG.BIN` registry, and mixed-generation migration system with a deterministic, physically isolated two-slot cache layout (`CACHE3`) and a separate full-identity reading position store (`POS3`).

## Goals

1. **Eliminate 28-Bit Directory Key Collisions**: Address book directories using full `(source_hash, source_size)` 64-bit identity paths (`/XTEINK/CACHE3/<hash-8>/<size-8>/`).
2. **Fixed On-Disk Physical Slots (`SLOT0/` & `SLOT1/`)**: Remove `CFG.BIN`, registry reconstruction, unlisted index sweeps, and multi-file eviction logic.
3. **Atomic Slot Commit Record (`BOOK.BIN`)**: `SLOTx/BOOK.BIN` is written last as the commit manifest for a slot. Uncommitted or partial writes on power-loss leave the slot invalid and cleanly reclaimable.
4. **Decouple User Reading Position (`POS3`)**: Store reading position at `/XTEINK/POS3/<hash-8>/<size-8>/POSA.BIN` and `POSB.BIN`, completely isolated from cache invalidation or cache clear commands.
5. **Advisory Recency (`RECENTA.BIN` / `RECENTB.BIN`)**: Use two-generation durable records for slot recency, falling back deterministically if corrupted without risking cache corruption.
6. **Obsolete Legacy Caches (`CACHE2`)**: Perform background best-effort deletion of `/XTEINK/CACHE2/` without supporting in-place pagination migration.

## On-Disk Structure

```text
/XTEINK/
  CACHE3/
    <source-hash-8>/
      <source-size-8>/
        COMMON/
          TOC.BIN
          COVER.BIN
          CONT.BIN
        SLOT0/
          BOOK.BIN
          SECTIONS/
            S000.BIN
            S001.BIN
            ...
        SLOT1/
          BOOK.BIN
          SECTIONS/
            S000.BIN
            S001.BIN
            ...
        RECENTA.BIN
        RECENTB.BIN
  POS3/
    <source-hash-8>/
      <source-size-8>/
        POSA.BIN
        POSB.BIN
```

## Protocol Overview

### Publication & Slot Selection
1. Open book directory at `/XTEINK/CACHE3/<hash-8>/<size-8>/`.
2. Inspect `SLOT0/BOOK.BIN` and `SLOT1/BOOK.BIN`.
3. If a valid header matches the requested layout configuration key, load and serve from that slot.
4. Otherwise, select a victim slot (invalid slot, or least-recently-used slot per `RECENTA/B.BIN`).
5. Completely empty the victim slot directory.
6. Write section files to `SLOTx/SECTIONS/`.
7. Write `SLOTx/BOOK.BIN` **last** as the commit record.
8. Update advisory recency (`RECENTA/B.BIN`).

### Reading Position (`POS3`)
- Read from `/XTEINK/POS3/<hash-8>/<size-8>/POSA.BIN` or `POSB.BIN`.
- If missing, fall back to reading legacy `POS.BIN` under `/XTEINK/CACHE2/<key>/` (if present) and write forward to `POS3`.
- Write updates directly to `/XTEINK/POS3/<hash-8>/<size-8>/`.

### Legacy Cleanup
- Treat `/XTEINK/CACHE2/` as obsolete rebuildable data.
- Background/startup sweep performs best-effort removal of `/XTEINK/CACHE2/`.
