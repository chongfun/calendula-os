# WS-D: Storage & Wi-Fi throughput — SD bandwidth, upload speed, session setup, onboarding

Status: ready-for-agent

Owns: `fw/src/sd_session.rs`, `fw/src/tasks/wifi.rs`, `fw/src/upload.rs`, `fw/src/sync_mem.rs`, vendored/pinned `embedded-sdmmc`.
Note: `sd_session.rs` changes speed up WS-B's reader path too — this workstream owns the file; WS-B must not modify it.

Baseline: no measured upload MB/s exists — **first task is a baseline**: timed `curl --data-binary @book.epub "http://<ip>/upload?name=book.epub"` plus `sd_stats` counters. Station join ~21 s (`wifi.rs:36`). Cold cache build I/O share: 537 wr + 723 rd blocks.

## D1 (Tier 1, S): SD SPI tier — chunk 64→512 B, data clock 20→25 MHz

Every SD block moves through a 64-B bounce buffer, 8 DMA transactions + copies per 512-B block (`SD_SPI_CHUNK_BYTES`, `fw/src/sd_session.rs:132`; loops at `:186-236`). Raise to 512 (one block = one transaction); this also sizes the shared RX DMA buffer (`dma_buffers!` in `fw/src/main.rs:236`) — costs ~448 B DRAM against stack headroom (affordable; re-check the link ASSERT on X3). Separately bump `SD_DATA_FREQ_MHZ` 20→25 (`sd_session.rs:21`; SPI-mode spec ceiling).

- Impact: roughly 2× SD bandwidth on both reads and writes — benefits uploads, cold builds, catalog scans, reopens (50–85 → ~40–70 ms).
- Risk: shared display/SD bus signal integrity at 25 MHz (X3 UC8253 restore handled per-panel at `sd_session.rs:22-26`); marginal cards — watch warm-retry counters for silent slowdowns.
- Verify: `bench.py storage-cache --reset-before --strict` before/after (rd/wr call-to-block ratios prove chunking), `page-turn --turns 50` for reader regression, `sleep-sync` warm reuse, `reader-soak` for marginal-card stability, timed curl A/B.

## D2 (Tier 1, S): Restore radio RX buffering + AMPDU-RX; yield during SD writes

Radio is trimmed to 4 static RX / 8 dyn RX / 8 dyn TX, AMPDU off (`fw/src/tasks/wifi.rs:95-106`) for a "short kosync exchange" that **has since been removed** — the comment itself says revisit for the upload phase. Raise static RX 4→8–10 (~1.6 KB each), dynamic RX 8→16–32, enable `ampdu_rx_enable` (upload is RX-dominant; AMPDU-TX stays off). Budget against the loaned heap — measure first: heap used/free is already logged at join (`wifi.rs:817-821`); add a log after each upload, then spend observed slack.

Also: `write_one_book` (`fw/src/sd_session.rs:508-551`) calls fully blocking `file.write` on 4 KB (10–30 ms, no await) on the shared thread-mode executor, starving `net_task` → RX drops → TCP stalls. Slice writes 512 B at a time with `embassy_futures::yield_now().await` between slices. Optionally grow ping-pong buffers 4→8–16 KB from the loaned heap (`wifi.rs:281-283`) once yielding makes large chunks harmless.

- Impact: 2–4× radio RX throughput; +20–50% from yielding on top of D1 — combined plausibly 3–5× end-to-end upload.
- Risk: heap exhaustion inside the radio blob crashes the loaned-buffer session (only recovery is the reset) and AMPDU reorder buffers allocate under load — soak with 10+ book uploads while watching heap high-water.
- Verify: timed curl A/B, heap logs, `bench channel-stress --host` (its charter is exactly this ping-pong), serial for TCP timeouts.

## D3 (Tier 1, S): WPA2-PSK the onboarding hotspot

The portal AP is open (`AccessPointConfig::default().with_ssid(PORTAL_SSID)`, `wifi.rs:548`) and the home SSID+password POST to `/save` travels plaintext over RF. Users join via a build-time QR (`tools/generate_qr.py`) anyway — generate a random PSK at build time, bake it into both the QR payload (`WIFI:T:WPA;S:...;P:<psk>;;`) and a WPA2-Personal AP config, from a single build-time source so they can't drift.

- Impact: closes a real credential-disclosure hole; zero UX change. Secondary (lower priority): `/upload` and `/delete` are unauthenticated to the whole LAN (`wifi.rs:332-406`) — a per-session token in the served URL would close it cheaply.
- Verify: phone QR-join + captive sheet still auto-raises + form submit, iOS and Android.

## D4 (Tier 2, M): Directed station join — persist channel/BSSID

`Session::join` (`wifi.rs:826-851`) sets only SSID/password/auth → full all-channel scan → ~21 s (docs flag "the 20 s join timeout deserves headroom or scan tuning"; considered, never done). The pinned esp-radio `StationConfig` supports `bssid`/`channel`/scan-method. After a successful join, record channel (+ optionally BSSID) alongside credentials in `/XTEINK/WIFI.BIN` (`hal_ext::nvm::WifiCredentialsRecord` via `StorageCommand::StoreWifiCredentials`, `fw/src/tasks/display.rs:779-791`); next session does a directed join, falling back to full scan on failure (retry loop at `wifi.rs:149-159` exists).

- Impact: repeat-session join ~21 s → ~3–6 s.
- Risk: stale channel after router change must degrade gracefully without eating the 35 s JOIN_TIMEOUT twice; WIFI.BIN gains fields — keep old records readable.
- Verify: serial Start→Serving timestamps across repeated sessions; deliberately change router channel and confirm fallback; `sleep-sync` lifecycle regression.

## D5 (Tier 2, M): Portal → station handoff in one session

Today: portal captures credentials → SAVED page says "press done, then run sync again" (`wifi.rs:523-532`) → reset → reboot → user re-enters Wireless → new session → ~21 s join. `run_portal` (`wifi.rs:538-587`) never returns. Instead: after `handle_portal_request` captures credentials (`wifi.rs:676-694`), tear down portal servers, `controller.set_config` to Station on the same `WifiController`, build the STA embassy_net stack from the loaned heap (as the AP path does, `wifi.rs:558-559`), fall through to the join loop and `upload_server`. Loan/reset lifecycle unchanged — still exactly one reset at session end.

- Impact: removes 3 user steps + a full reset/rejoin (~40–60 s); with D4, first-ever sync becomes one continuous flow.
- Risk: AP→STA reconfig on a live controller is the least-exercised esp-radio path — hardware validation required; two net_tasks must not both run (swap runners or quiesce AP stack first); Wireless screen needs a portal→connecting SyncEvent sequence without reboot (`fixtures/` sync-*.toml scenarios + goldens).
- Verify: phone onboarding end-to-end on hardware (also covers the DNS sign-in-sheet path, itself flagged untested); emulator scenario + goldens; reset still restores reading position.

## D6 (Tier 3, L): Multi-block CMD18/CMD25 in the pinned embedded-sdmmc

Above the SPI layer, `embedded-sdmmc` `File::write` write-backs one 512-B block per call → every block is its own CMD24 with command/response/CRC + 1–3 ms card programming; reads are per-block CMD17. Its CMD25 path exists but is unreachable. Patch the pinned crate (`fw/Cargo.toml:39`) to batch cluster-contiguous whole-block runs, or write payload blocks directly using FAT only for allocation/metadata. Same for CMD18 on sequential reads (`SdFileReadAt::read_at` 8 KB chunks, `reader_cache.rs:1356`); IMPLEMENTATION_PLAN already names CMD18 as one of two "material wins". Do this only if post-D1/D2 profiling still shows SD-bound transfer (`write_calls == write_blocks` in bench counters answers it immediately).

- Risk: fork maintenance; FAT correctness (byte-compare uploaded files); CS/timeout behavior on the shared bus.

## Do not re-propose

kosync (removed on purpose); re-donating dram2 to the radio heap (removed to restore stack — don't win D2's heap back that way). The radio-trim revisit (D2) and join tuning (D4) are documented intentions, not rejections. Doc fix to fold in: ARCHITECTURE.md:131-133 still describes the removed 16 KB dram2 claim.

Suggested order: baseline measurement → D1 + D2 + D3 (one hardware A/B session covers all) → D4 → D5 → D6 only if counters still show SD-bound.
