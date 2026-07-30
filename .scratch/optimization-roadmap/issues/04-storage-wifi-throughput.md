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

### Upload-ceiling investigation (S–M) — instrument before fixing

Upload throughput sits near **160 KB/s under every configuration tried**, and
nobody knows why. Ruled out by measurement (2026-07-11/12): radio RX buffers,
AMPDU-RX, SD write stalls, SPI chunking.

Remaining candidates, roughly in order of suspicion:

1. The 4 KB two-buffer ping-pong handshake between the wifi and storage tasks
   — each chunk's channel round-trip serializes against the SD write it
   triggers.
2. esp-radio power-save mode during the session — check whether PS is disabled
   while serving.
3. Single-stream TCP dynamics — a small congestion window against a receiver
   that reads in 4 KB bites.
4. The HTTP server's read pattern in the upload handler.

Tools already in place: `upload: heap used/free` per upload, `sd_stats`
counters, and the timed-upload A/B protocol that produced the D2 verdict —
same book, card, network and reading position, three runs, compare medians:

```
curl -sS -o /dev/null -H 'Expect:' --data-binary @book.epub \
  "http://<ip>/upload?name=book.epub" \
  -w '%{time_total}s %{speed_upload} B/s'
```

**Instrument first** — e.g. timestamp the ping-pong, fill-time against
write-time per chunk, to see which side starves — then fix only what the
numbers indict.

A believable outcome is 2–5× upload throughput. Another believable outcome is
that the radio/RF layer is the ceiling and uploads are already as fast as this
hardware goes. Both are useful answers; guessing is not.

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

Weigh 2–3 s per cold build against the cost of maintaining a fork of a pinned
dependency. `write_calls == write_blocks` in the bench counters answers
immediately whether this is still worth it. Note #18 moved the upload write
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
