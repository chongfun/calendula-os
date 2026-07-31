# WS-D: Storage & Wi-Fi throughput — SD bandwidth, upload speed, session setup, onboarding

Status (2026-07-30): D1 and D3 are on `main`. **D2 was implemented, measured,
and rejected** — read its entry under "Do not re-propose" before touching
upload throughput. Open: D4, then the upload-ceiling investigation, then D5.
D6 only if the counters still indict SD.

Owns: `fw/src/sd_session.rs`, `fw/src/tasks/wifi.rs`, `fw/src/upload.rs`,
`fw/src/sync_mem.rs`, the vendored/pinned `embedded-sdmmc`.
Note: `sd_session.rs` changes speed up WS-B's reader path too. This
workstream owns the file; WS-B must not modify it.

## Open

### D4 (M): directed station join — persist channel and BSSID

`Session::join` sets only SSID, password and auth, so every join does a full
all-channel scan: **~21 s**. The pinned esp-radio `StationConfig` supports
`bssid`, `channel` and a scan method. After a successful join, record the
channel (and optionally the BSSID) alongside the credentials in
`/XTEINK/WIFI.BIN`; the next session does a directed join and falls back to a
full scan on failure, through the retry loop that already exists.

Folded in from the upstream sweep: **strongest-AP join for duplicate SSIDs**
(crosspoint `e03aa163`) — when several APs advertise the saved SSID, pick the
strongest rather than the first found. It shares this item's scan/join
surface; bundle it rather than porting separately.

- Impact: repeat-session join ~21 s → ~3–6 s; better AP choice on mesh and
  repeater networks.
- Risk: a stale channel after a router change must degrade gracefully without
  eating the 35 s JOIN_TIMEOUT twice.
- Framing note: WIFI.BIN has been a `proto::durable` two-generation A/B record
  since #29, so the new fields ride that framing with a version bump inside
  it — not a bare struct widening. The durable layer already handles torn
  writes and the legacy single-file fallback.
- Verify: serial Start→Serving timestamps across repeated sessions;
  deliberately change the router's channel and confirm the fallback;
  `sleep-sync` lifecycle regression.

**Implemented on `opt/d4-directed-wifi-join`, complete, needs only the device
run.** Audited 2026-07-30: fully wired (read + SSID-hash match in the storage
task, loan carries it, learn-and-write fire-and-forget), and `decode` rejects
short buffers, bad magic/version/checksum, channel 0, channel > 14 and an
all-zero BSSID before any of it reaches the radio.

One design improvement over the spec above, and the branch is right: it does
**not** widen `WifiCredentialsRecord`. `reader-cache`'s record reader compares
`file.length() as usize != total` **exactly**, so a widened record would read
as *absent* to older firmware and lose the saved password. The hint instead
gets its own `WIFHA`/`WIFHB.BIN` durable pair with a distinct magic, deleted
alongside the credentials on forget.

Two things for the reviewer: the worst case is a real regression bound — `hint`
is bound once outside the retry loop and never cleared after a miss, so a stale
hint costs 10 s on *every* retry and a single attempt's ceiling becomes
10 + 35 = **45 s against main's 35 s** (it self-heals after one successful
fallback join, so exposure is one failed-session sequence). And the SSID hash
input is spelled differently on the read and write sides — identical for valid
UTF-8, but make it one expression.

### Upload-ceiling investigation (S–M) — instrument before fixing

Upload throughput sits near **160 KB/s under every configuration tried**, and
nobody knows why. Ruled out by measurement (2026-07-11/12): radio RX buffers,
AMPDU-RX, SD write stalls, SPI chunking.

**Correction, 2026-07-30 — D2's post-mortem has been misread for three
rounds.** It says "neither radio RX nor SD write *stalls* is the limiter",
which is true as written: D2 tested executor **starvation** (yield pacing) and
radio **buffering**. It never tested SD write **bandwidth**, and that has been
read as "SD is ruled out". It is not.

The arithmetic, from this roadmap's own measured numbers: 3.2 MB / 19.3 s =
166 KB/s = **3.08 ms of budget per 512-byte block**, against a measured
**2.23 ms per block write**. That is **~72% of upload wall time in single-block
SD writes**, and the pure SD ceiling (512 B / 2.23 ms = 229 KB/s) sits only
**1.4×** above the observed throughput. *Load-bearing caveat: the 2.23 ms was
measured on the cold-build workload (scattered small cache writes), not on
sequential 4 KB appends, and could be materially lower here. That single
unknown decides the whole workstream — which is why the first task is
instrumentation, not a fix.*

**Two candidates are now dead from source, no hardware needed:**

- **Power save (candidate 2) — REFUTED.** `WifiController::new` already calls
  `set_power_saving(PowerSaveMode::default())` and that `#[default]` is
  `None`, with the upstream comment "the blob default is not the best for
  bandwidth". PS is off for the entire session. Delete this suspect.
- **The HTTP read pattern (candidate 4) — no waste found.** `stream_book`'s
  inner loop reads directly into the staging buffer with a correctly clamped
  window, one copy, no intermediate buffering; the leftover-body handling is
  correct and copies once.

**Radio-blob starvation is refuted structurally, and it explains D2's null
result.** `esp-rtos` runs the radio driver's tasks as preemptive RTOS threads,
so a blocking SD write blocks only the embassy thread-mode executor
(`net_task` + the wifi task), never packet reception. That is why D2's yield
pacing bought nothing — and it means the *only* thing a blocking write costs is
socket drain and ACK generation.

**Candidate 1 survives, but not for the stated reason.** It is not that the
channel round-trip serializes against the write. With `UPLOAD_CHUNKS` and
`UPLOAD_RETURNS` both at capacity 2 and a two-buffer pool, the producer fills
and sends A, then fills and sends B — both `send`s complete on first poll — so
the writer finds B *already queued* and runs **two 4 KB writes back to back
with no yield point between them**: a ~36 ms executor blackout against a 16 KB
socket RX buffer, which is the receive window (smoltcp has no window scaling).
16 KB is ~26 ms of headroom at 600 KB/s, so the window closes and costs an RTT
to reopen. Fitting: 8192 B / (2 × 17.8 ms + X) = 166 KB/s → X = 15.6 ms, a very
plausible zero-window reopen. **The fix is a yield at the chunk boundary — 780
yields per 3.2 MB, not D2's rejected 512-B pacing at ~6,250 — so this is a
different change from the one that was measured and rejected.**

**Free window headroom, if it comes to that:** `dismantle_scratch` gives the
24 KB region to the radio heap and the 16 KB region to `tcp_rx`. Swapping them
yields a 24 KB receive window for 8 KB of loaned heap — **no new memory, just a
different assignment of already-loaned regions**, so it does not repeat D2's
mistake of *spending* heap. Rank it below the yield fix (same target, more
risk), and watch the per-upload heap log.

Tools already in place: `upload: heap used/free` per upload, `sd_stats`
counters, and the timed-upload A/B protocol that produced the D2 verdict —
same book, card, network and reading position, three runs, compare medians:

```
curl -sS -o /dev/null -H 'Expect:' --data-binary @book.epub \
  "http://<ip>/upload?name=book.epub" \
  -w '%{time_total}s %{speed_upload} B/s'
```

**Instrument first** — and the instrumentation is now specified, ~25 lines in
two files, copying the `write_micros` accumulator pattern that already exists
in `book_build.rs`. Three accumulators plus the `sd_stats` delta, printed next
to the existing per-upload heap log:

- `sd_write_us` around `pending.write` in `write_one_book` → **SD-bound**
- `wait_buffer_us` around the `UPLOAD_RETURNS.receive()` arm → **writer-bound**
- `socket_read_us` around the `socket.read` arm → **network-bound**

```
upload: bytes=N wall_ms=W sd_write_ms=S wait_buf_ms=B sock_read_ms=R
        wr_calls=.. wr_blocks=.. rd_calls=.. rd_blocks=..
```

**One flash, one `curl`, one capture, and the decision table resolves it:**

| Result | Verdict |
|---|---|
| `sd_write_ms / wall_ms` ≳ 0.65 | SD write bandwidth is the ceiling. **Promote D6.** The ping-pong and window items are second-order. |
| `sd_write_ms` ≈ 0.3–0.5 **and** `sock_read_ms` large | Window/RTT bound. Do the chunk-boundary yield first, then the buffer swap. D6 stays deferred. |
| `sd_write_ms` ≲ 0.3, `sock_read_ms` dominant, window never closing | RF layer is the ceiling. **Close this investigation** — uploads are as fast as this hardware goes. |
| `wr_blocks` ≫ `ceil(bytes/512)` | The FAT tax below is real; size it before forking. |

A believable outcome is 2–5× upload throughput. Another believable outcome is
that the radio/RF layer is the ceiling and uploads are already as fast as this
hardware goes. Both are useful answers; guessing is not. **Do this before
writing any fix — it is the only thing standing between this workstream and a
second D2.**

**Two adjacent costs the same capture sizes for free:**

- **FAT mirror writes.** Every cluster allocation issues `update_fat` twice,
  each writing back to *both* FATs — **four random-region CMD24s per cluster
  crossing**, plus 2–3 FAT block reads, interleaved into an otherwise
  sequential stream (and each one breaks the card's sequential-write streak).
  At 32 KB clusters that is +6% writes; at 8 KB clusters, common on smaller
  cards, **+25%**. Free to quantify: `wr_blocks` for a 3.2 MB upload should be
  6,250 with no FAT overhead; the excess *is* the tax. Fixing it (preallocating
  the chain, or deferring mirror writes to close) needs the D6 fork and touches
  FAT consistency — high risk, measure before considering.
- **`with_sd_bounce` memsets 512 bytes for every one-byte SPI transaction.**
  Every command byte, response poll and busy poll is a full
  `SdSpiDevice::transaction` that unconditionally fills the whole
  `SD_SPI_CHUNK_BYTES` bounce buffer, though only `..len` needs the 0xFF idle
  pattern (and `write_chunked` needs no fill at all, since `copy_from_slice`
  overwrites it). **D1 made this 8× worse** by taking the chunk 64 → 512 at the
  same time it made the bulk path 8× cheaper — a concrete candidate for why D1
  delivered −5.4% instead of the hoped 2×. ~0.8–1.3 µs per transaction at
  160 MHz, times an uncounted multiplier of ~13 sub-16-byte transactions per
  block write plus N busy polls. Fix is ~10 lines (thread the length through);
  **count the transactions before quoting a percentage** — add an `SPI_TXNS`
  counter beside `sd_stats` and one `storage-cache` run. Under ~20 per block
  write, the win is <1% and it should be dropped.

**Also cheap, unrelated to throughput:** the loan path opens **three
back-to-back `with_root` sessions** (`flush_pending_progress`,
`load_wifi_credentials`, `write_catalog_listing`), each paying two
`apply_config` reconfigures, a 400 kHz wake sequence, `open_volume` and
`open_root_dir`. Merging them to one saves ~80–140 ms of pre-join latency
against the 40–70 ms warm-reopen figure. Each session also emits six serial
lines — see WS-C's C7 for why that is not free.

- Risk: heap exhaustion inside the radio blob crashes the loaned-buffer
  session (the only recovery is the reset), and AMPDU reorder buffers allocate
  under load. Soak with 10+ book uploads while watching the heap high-water
  mark.
- Verify: timed curl A/B, heap logs, `bench channel-stress --host` (its
  charter is exactly this ping-pong), serial for TCP timeouts.

### D5 (M): portal → station handoff in one session

Today the portal captures credentials, the SAVED page says "press done, then
run sync again", and the device resets, reboots, and makes the user re-enter
Wireless for a fresh ~21 s join. Instead: after the portal captures
credentials, tear down the portal servers, `set_config` the same
`WifiController` to Station, build the STA embassy_net stack from the loaned
heap as the AP path does, then fall through to the join loop and
`upload_server`. The loan/reset lifecycle is unchanged — still exactly one
reset at session end.

- Impact: removes 3 user steps and a full reset/rejoin (~40–60 s). With D4,
  the first-ever sync becomes one continuous flow.
- Rebase note: #35 rewrote this surface. The portal now re-reads saved
  credentials through the boot-path read before reporting success, and serves
  a nearby-network list. The handoff must **preserve verify-after-save** — the
  STA join is the stronger verification, but the storage read-back still
  guards the post-reset boot.
- Risk: AP→STA reconfiguration on a live controller is the least-exercised
  esp-radio path and needs hardware validation; two `net_task`s must not both
  run (swap runners or quiesce the AP stack first); the Wireless screen needs
  a portal→connecting event sequence with no reboot.
- Verify: phone onboarding end-to-end on hardware (this also covers the DNS
  sign-in-sheet path, itself flagged untested); emulator scenario and goldens;
  a reset still restores the reading position.

### D6 (L, fork maintenance): multi-block CMD18/CMD25 in the pinned embedded-sdmmc

**Evidence (2026-07-12, X3, 11.7 MB EPUB cold build):** `sd_stats` showed
`write_calls == write_blocks` (1,944 each) and a per-block write cost of
2.23 ms against ~0.16 ms of wire time at 25 MHz. So ~2 ms per block is CMD24
command/response/CRC overhead plus card programming latency, and only
multi-block batching can amortize it. Total write time was 4.34 s, so CMD25
could plausibly recover 2–3 s of every large cold build.

`embedded-sdmmc`'s `File::write` writes back one 512-B block per call, and its
CMD25 path exists but is unreachable. Patch the pinned crate to batch
cluster-contiguous whole-block runs, or write payload blocks directly using
FAT only for allocation and metadata. Same for CMD18 on sequential reads.

**Status change 2026-07-30: this item's own stated precondition was satisfied
in July and it stayed deferred anyway.** "`write_calls == write_blocks` in the
bench counters answers immediately whether this is worth it" — they were equal,
1,944 each, on 2026-07-12. Combined with the upload arithmetic above (~72% of
upload wall time in single-block writes, ceiling only 1.4× above observed),
this is now the largest identified lever in the workstream rather than a
tier-3 maybe. Confirmed by reading the pinned crate: `VolumeManager::write`
loops per block calling `block_cache.write_back()`, always a 1-block device
write; the CMD25 branch beside it already does `ACMD23` pre-erase and drops
CMD24 + CMD13 per block, and is unreachable only because the caller never
passes more than one block.

If per-block cost falls to a typical 0.6–0.9 ms, a 3.2 MB upload goes 19.3 s →
**~10–12 s** (network then binds) and the 4.34 s of cold-build write time →
~1.5 s. *Estimate: the current per-block cost is measured, the post-CMD25 cost
is not.* Note `upload-store` is `#![forbid(unsafe_code)]`, so reinterpreting a
4 KB chunk as `[Block; 8]` has to happen on the fw side.

**Still gate it on the instrumentation above** — if `sd_write_ms` comes in
under ~35% of upload wall time, do not fork. Weigh 2–3 s per cold build against
the cost of maintaining a fork of a pinned dependency. Note #18 moved the upload write
path into the `upload-store` crate; reader-cache writes still go through fw's
SD session.

- Risk: fork maintenance, FAT correctness (byte-compare uploaded files),
  CS/timeout behaviour on the shared bus.

## Done

- **D1** (#14) — SD SPI tier: the 64-B bounce buffer became 512 B (one block,
  one transaction) and the data clock went 20 → 25 MHz. **Measured: cold build
  −5.4%, `write_ms` −9.5%, progress write −35%.** This is the honest number;
  the PRD originally projected ~2× SD bandwidth and that framing was wrong.
- **D3** (#19) — the onboarding hotspot was open, so the home SSID and
  password crossed the air in plaintext. Shipped as a **per-session runtime
  PSK** with on-device QR encoding, not the build-time PSK originally
  proposed. Still open and lower priority: `/upload` and `/delete` are
  unauthenticated to the whole LAN; a per-session token in the served URL
  would close it cheaply.

## Do not re-propose

- **D2 — radio RX buffers 8/24 + AMPDU-RX + SD writes paced in 512-B slices
  with yields.** Implemented, measured on hardware 2026-07-11, rejected.
  Timed upload A/B, X3, 3.2 MB EPUB, same card and network: main **19.3 s**
  median, D1+D2 **21.1 s**, D1+buffers-only **~20.2 s**. The 512-B slice and
  yield pacing cost ~1 s per upload (~6,300 yield/reschedule cycles); the
  buffers and AMPDU-RX bought nothing while spending ~6.6 KB of loaned heap at
  join (free 27,300 → 20,700). Throughput sits near 160 KB/s under every
  configuration, so neither radio RX nor SD write stalls is the limiter —
  main's blocking 4 KB writes demonstrably do not stall TCP. Only the
  per-upload heap log shipped. Code comments at the radio config and the
  upload write loop record the verdict. **Do not re-try either half without
  new evidence from the investigation above.**
- **kosync** — removed on purpose.
- **Re-donating dram2 to the radio heap** — removed to restore stack. Do not
  win D2's heap back this way.

The radio-trim revisit and join tuning are documented *intentions*, not
rejections — that is D4 and the investigation above.
