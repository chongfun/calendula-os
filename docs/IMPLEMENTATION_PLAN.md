# Implementation plan

The repo has moved from hardware proof into a minimal reader shell. The current
target is still reliability before feature breadth.

## X4 Pro platform work (#391 / #343)

Issue `#391` implements full ESP32-S3 Xteink X4 Pro runtime, release packaging
(`tools/build-release.sh x4-pro`, `.github/workflows/release.yml`), and CI gates
(`tools/check.sh s3`). Foundational multi-platform work (`#336`–`#339`, `#343`,
and `#391`) includes:

- **C3 boundary (`#336`):** `fw` is renamed to `fw-c3`; root Cargo defaults
  select host crates, `tools/cargo-c3.sh` selects the RISC-V target and memory
  policy, and release asset names and C3 OTA identities remain unchanged. Post-`fw-common`
  extraction comparisons confirmed 256/256-byte `_ESP_APP_DESC` parity on X4
  and X3, `.text` reductions (`-22,654 B` X4 / `-22,852 B` X3), `.bss` `+56 B`,
  stack regions `38,400 B` (X4) / `29,216 B` (X3), and `tools/check.sh
  stack-frames` passing across all four C3 configurations.
- **Shared runtime (`#337`):** `fw-common` (`#![no_std]`,
  `#![forbid(unsafe_code)]`) owns `app::run`, `channels`, `app_desc`, `log`,
  `bench`, `pad` (`PadHoldSpec`, `ReconciledOutput`), `upload`, and `catalog`,
  while `fw-c3` retains `WAKE_PIN_REQUESTS`, `WAKE_PIN_HANDOFF`, `SYNC_LOANS`,
  and C3 task wiring.
- **MCU-neutral HAL (`#338`):** C3-only `board_probe`, `epd_probe`, and `rtc`
  moved into `fw-c3`; `hal-ext` holds MCU-neutral `embedded-hal` /
  `embedded-hal-async` drivers (`Bq27220`, `Cw2017`, `Bm8563`, `Gt911`,
  `spi_dma` including Wave 7A `EpdBus::rst_mut` and
  `EpdBus::wait_uc_idle_high`).
- **Chip-aware OTA & UC8279 foundation:** `proto::ota` adds
  `CHIP_ID_ESP32C3`/`CHIP_ID_ESP32S3`, `IDENTITY_X4_PRO`/`BOARD_X4_PRO`, and
  `validate_image_for_chip` / `validate_flash_image_for_chip` /
  `is_flash_mapped_for_chip`; `display::epd` adds `uc8279_x4` (including Wave
  7A `fill_gate_row_from_framebuffer`) and `probe::resolve_x4_controller`.
- **S3 proof & CI gates (`#339`):** `fw-s3` is in its own Cargo workspace with a
  pinned Xtensa toolchain (`tools/s3-toolchain.env`, `tools/setup-s3.sh`,
  `tools/cargo-s3.sh`), `esp-hal` rev `9a5810bbc` startup/linker fix
  (`#6497`/`#6816`, `.flash.appdesc`), `fw-s3/src/board.rs`, and
  `fw-s3/src/epd_probe.rs`. Non-destructive `app1 @ 0x7F0000` hardware boot of
  `fw-s3-proof.bin` on `/dev/cu.usbmodem2101` passed across 3 consecutive
  `USB_UART_CHIP_RESET` runs (`s3-proof-boots.log`: `uc81xx-confirmed` /
  `uc8279-x4` `lut_ver=68` and `shared_app=started`), and CrossPoint was
  restored and verified booting in `app1 @ 0x7F0000` (`restore-verify.log`,
  `crosspoint-restore-boot.log`). `tools/check.sh dep-boundaries` (included in
  `fast`) and `tools/check.sh s3`, plus CI jobs `dep-boundaries` and
  `firmware-s3` in `.github/workflows/ci.yml`, enforce chip/crate boundaries.
- **S3 X4 Pro firmware implementation & hardware checks (`#391`):**
  `fw-s3` now wires the full reader pipeline under `_ESP_APP_DESC =
  "CalendulaOS X4-Pro u1"`: `.dram2_prev_fb` dual framebuffer placement in
  `fw-s3/build.rs`, native 1-bit `esp_hal::sdmmc` session (`sd_session.rs`,
  `CLK=GPIO41`, `CMD=GPIO42`, `D0=GPIO40`, active-low power `GPIO5`),
  `storage`/`reader-cache`/`ui::app_render` (`views.rs`, `tasks/display.rs`),
  board-gated Settings Frontlight brightness/warmth rows persisted in
  `LIGHTA.BIN`/`LIGHTB.BIN` (`proto::nvm`, `frontlight.rs`), orientation-aware
  `Gt911` touch + direct row selection (`InputEvent::ListRowTap` across
  `app-core`, `ui::render::hit_test_list_row`, and `fw-s3/src/input.rs` with
  disclosure icon slop), `GPIO3` deep sleep/wake (`tasks/power.rs`), one-way
  Wi-Fi loan + AP/STA HTTP upload (`sync_mem.rs`, `tasks/wifi.rs`), and the
  `FWUPDX4P.BIN` dual-OTA updater (`mmu.rs`, `ota_update.rs`, `proto::ota`
  S3 helpers). Removed `-C force-frame-pointers` from `fw-s3/cargo-config.toml`
  because LLVM Xtensa emitted 4-byte `movsp` stack adjustments that broke
  alignment for stack-passed `ShortFileName` / `heapless::Vec<u8, 3>`
  extensions during EPUB upload installs.
- **Hardware checks completed on X4 Pro (`/dev/cu.usbmodem2101`, `app0 @ 0x10000` / `app1 @ 0x7F0000`):**
  upright display & page turns, EPUB open/index/navigation (`Mirai no
  Mirai.epub`, `353` pages in `23496 ms`; `Too Many Losing Heroines! - Volume
  08`, `388` pages), direct TOC & Library row taps, Settings Frontlight &
  Warmth persistence across `GPIO3` sleep/wake, Wi-Fi AP onboarding
  (`192.168.4.1`), STA connection (`192.168.1.239`), interrupted install
  recovery replay, fresh wireless EPUB upload + open, dual-slot `ota-selftest`
  (`app0 -> app1` with `otadata` flip and boot into slot 1), and
  `bench.py page-turn` (`553ms` median page turn, progressive `SOF2` DC JPEG
  decodes).
- **Remaining follow-ups:** live SD-card `FWUPDX4P.BIN` OTA trigger run on
  hardware, inter-turn light-sleep parity with `fw-c3` (currently idle deep
  sleep only), long-soak / 50 sleep-wake / instrumented standby-current runs,
  physical C3 X4/X3 smoke tests, and optional web-flasher manifest/link
  support for `firmware-x4-pro.bin`.

See [2026-11-16 bring-up record](plans/2026-11-16-x4-pro-bring-up.md) and
[2026-11-16 handoff](plans/2026-11-16-x4-pro-handoff.md) for measured baseline
data, recovery hashes, and unrun hardware checks.

## Phase 1: panel and buttons

Exit criteria:

- Build succeeds for `riscv32imc-unknown-none-elf`.
- Boot draws a simple reader shell framebuffer.
- Display task initializes SSD1677 and performs a full refresh.
- Power task requests SSD1677 deep sleep through the display task before MCU sleep.
- GPIO3 and ADC ladder buttons produce `InputEvent`s.
- App task updates a page counter from input.
- Power task can enter timer deep sleep after display settle.

Current code status:

| Area | Status |
| --- | --- |
| Workspace and target setup | Done |
| Embassy executor | Done |
| Task boundaries | Done |
| Single 48 KB framebuffer | Done |
| SSD1677 init sequence | SDK-aligned, panel responds |
| Refresh path | Reader shell readable with `MIRROR_X=true`, `MIRROR_Y=false`, `REVERSE_BITS=true`; first update uses full refresh, normal page turns use deterministic fast differential refresh |
| Input backpressure | App accepts input while a render is in flight and coalesces display work to the latest state |
| Input polling | Measured calibrated ADC ladder bands plus CrossPoint-style layout mapping applied; screen shows reader-facing `PREV`/`NEXT`/`BACK`/`OK` labels |
| Reader app shell | Portrait Home/Library/Settings plus landscape Reading/Chapters present with catalog-backed book data |
| Battery display | GPIO0-derived rough battery mV/percent flows through input/app/render |
| Deep sleep | Idle/power sleep now renders a visible sleep screen before SSD1677 deep sleep; GPIO wake pending |
| Partial refresh | Deferred; full-screen fast refresh present |
| NVM progress | Deferred |
| Storage / EPUB / Wi-Fi | Explicit storage commands, FAT scan, `/books` then card-root discovery, catalog snapshot cache, and SD-backed hybrid-light section cache present; Wi-Fi still pending |
| Typography | Literata Latin-1 bitmap assets generated; Reading uses Literata for demo text |

## Phase 2: measured board support

- Current calibrated ADC bands on this unit: GPIO1 Back `2400..2700`, Confirm
  `1800..2150`, Previous-front `1000..1250`, Next-front `0..100`; GPIO2
  Previous-side `1500..1800`, Next-side `0..100`. Current layout mapping is
  direct front `BACK_CONFIRM_LEFT_RIGHT` and side `PREV_NEXT`. Raw
  GPIO0/GPIO1/GPIO2 serial logging is available behind `RAW_LOG_ENABLED`.
- GPIO0 battery sampling is present as a rough 2:1 divider estimate; calibrate
  against measured pack voltage.
- Add GPIO wake for the power/home button.
- Measured BUSY timings (June 10 2026, on-device, room temperature, commit
  `14c7eba` + page-plan seam, Unsong.epub 1001 pages / 78 chapters; from the
  permanent `bench:` serial lines via `tools/serial_capture.py`):
  - Fast-waveform refresh BUSY: **421 ms**, constant across 44 refreshes.
  - Full-waveform refresh BUSY: **3.53–3.61 s**. Dominates every view change,
    boot (~4.1 s to Home), and wake (~3.8 s to Home); software around it is
    noise, so any future improvement here is waveform/temperature
    configuration, not SPI or layout work.
  - Whole-frame RAM stream over band DMA: 22–24 ms per plane (BW or RED).
  - Layout + framebuffer draw: Reading 20–36 ms, menu views 82–90 ms.
  - EPUB page turn, button press to panel settled: **~470 ms median**
    (~89% is the fast BUSY wait); sustained held-button rate ≈ 2 pages/s.
    RED prestaging held on every fast turn (each streamed BW only), and
    SD-backed section extends (~50–85 ms v2 cache hits) never stalled a
    render. Add ~40–80 ms ADC poll + debounce ahead of the logged press for
    true finger-to-eye latency.
  - Book open with a v2 cache hit: cache ready in 50–85 ms; perceived ~4 s is
    the full refresh that paints the first page.
- Measured cold cache build (July 9 2026, on-device, 117-page/35-chapter EPUB,
  same book and restored position both runs; from `bench: storage_build` via
  `tools/bench/bench.py storage-cache`):
  - Before the build-path work: **7.9 s** total (4.6 s writing section files;
    3,903 write + 1,267 read block transactions).
  - After write staging, held SECTIONS dir, dirty-gated pagination rebuilds,
    and the 8 KB read clamp: **3.9 s** total (0.86 s writes; 537 write + 723
    read block transactions) — ~34 ms per page, ~2× faster. The remainder is
    ~70% CPU (inflate + parse + measure); the next material win would be
    multi-block CMD18 reads (needs changes inside the pinned embedded-sdmmc)
    or a progressive open that publishes the first section early.
- Use the on-screen input calibration panel to record raw GPIO1/GPIO2 values for every button.

## Phase 3: reader core

- Add persistent page index.
- `AppStateRecord` exists as a versioned/checksummed storage record; flash-backed
  load/store implementation pending.
- Tiny in-flash/static book source is present as reader-shell pages.
- Home, library, active reading view, chapter navigation, and settings view are
  present as explicit app state. Home now uses the landscape Dock Clean layout
  with the four hardware-adjacent actions on the left and the current book on
  the right. Storage-backed EPUB entries fill the same model.
- `DisplayOrientation` exists with landscape buttons-bottom/top and portrait
  buttons-left/right modes; the default is portrait buttons-left. Settings
  exposes three of the four as Orientation: buttons down / buttons up /
  portrait.
- Keep app state as flat structs and render requests as small `Copy` messages.

## Phase 4: storage and EPUB

- `proto::storage` defines a bounded `BookStorage` trait, `/books`/card-root
  candidates, and case-insensitive `.epub` filtering.
- `proto::book` defines shared `BookMeta`, `BookProgress`, `ChapterMeta`, and
  catalog primitives used by Home, Files, Reading, and Chapters.
- `proto::epub` can locate ZIP central directories, read stored/deflated entries
  into caller-owned buffers, parse `META-INF/container.xml`, parse OPF metadata,
  manifest, and spine, and map XHTML tags into styled text runs.
- `proto::epub::ZipStream` can locate and read ZIP entries through a bounded
  `ReadAt` interface, so EPUBs no longer need to fit in memory. The firmware
  path now streams compressed deflate input in chunks, and XHTML spine entries
  can decode a bounded prefix for the first section cache instead of failing
  just because the section is larger than the XHTML scratch buffer.
- `proto::text` defines Literata/Merriweather-ready font/style roles and a
  deterministic one-screen paginator over bounded styled runs.
- `proto::cache` defines bounded binary cache records for book, TOC, section,
  page, line, word, and block data.
- Firmware Files/Home/Reading now consume the shared catalog/cache model through
  the refactored `ReaderStore`. Rendering and display-task coordination use
  ReaderStore query methods for catalog entries, active-book labels, selected
  cover data, advertised page counts, and source identity. The current in-flash
  demo book remains a fallback source while SD EPUB loading is hardened.
- The selected-book preview path has been replaced by `build_or_load_book_cache`.
  First open writes `/READER/CACHE2/E<hash>/BOOK.BIN`, builds the requested
  section into `/READER/CACHE2/E<hash>/SECTIONS/SNNN.BIN`, and renders from those
  flat records. Near-end NEXT requests a larger cached page target before
  rendering, so partial section caches can extend on demand.
- Home can now draw a selected-book cover bitmap from
  `/READER/CACHE2/E<hash>/COVER.BIN`. The firmware format is a fixed 202x303,
  1-bit, row-packed DOD bitmap; if it is absent or invalid, the Dock Clean
  fallback artwork is used. Host preview tooling can generate this cache file
  from an EPUB cover image.
- Current limitation: partial caches for very large single-XHTML spine items
  can render the first decoded chunk, but true byte-accurate resume inside that
  same compressed member is still pending.
- `BOOK.BIN` stores book/spine/TOC records plus a shared string blob. Section
  files store a section header, page records, block records, paragraph flags,
  and UTF-8 text bytes. Line/word cache records remain defined in `proto::cache`
  for the next rendering refinement; the current firmware renderer still draws
  styled block text with Literata.
- The encoded `AppStateRecord` for SD reading progress is written to
  `/READER/STATEA.BIN` and `/READER/STATEB.BIN` alternately, so a write torn
  by a power cut leaves the previous generation readable; a legacy
  `/READER/STATE.BIN` is still read when neither generation is valid.
  Version 2 stores the volatile book id plus stable SD source identity
  derived from path and file size; boot/Home restore scans the card, maps the
  record back to the matching EPUB, and keeps v1 decode fallback for older state
  files.
- Home `Read` now resumes the restored/last-selected SD EPUB. If no current EPUB
  exists, it opens Files when SD books exist and falls back to the built-in book
  when the card is empty or unavailable.
- X4 SD pins are configured on the shared SPI bus: SCK GPIO8, MOSI GPIO10, MISO
  GPIO7, SD CS GPIO12. `embedded-sdmmc` is present with default features
  disabled.
- Render is side-effect free. `DisplayCommand::Render` only draws the current
  `ReaderStore` snapshot and flushes the panel. Rendering and task coordination
  read catalog, active-book, cover, TOC, page-count, and source-identity data
  through ReaderStore query methods rather than reassembling its parallel arrays
  directly. SD discovery, EPUB cache construction, and progress writes run only
  through explicit `StorageCommand`s after the visible render settles.
- Firmware SD/FAT work now goes through `fw_c3::sd_session`, which owns the shared
  SPI card-mount/root-open/restore-speed ceremony. EPUB cache file persistence
  lives in `fw_c3::reader_cache_files`, leaving `fw_c3::reader_cache` focused on the
  EPUB-to-section-cache pipeline.
- The board I/O/display task remains the single runtime coordinator for
  serialized EPD and SD transactions, while SD sessions, SD discovery, cache
  file I/O, EPUB cache construction, reader layout, view drawing, and EPD
  flushing now live in deeper `fw-c3` modules.
- Files is instant and catalog-backed. `/READER/CATALOG.BIN` stores a flat fixed
  record list of discovered EPUBs. Firmware may show “Library unavailable” while
  no catalog snapshot exists; it shows “No books available” only after a
  completed scan proves there are no EPUBs.

## Phase 4b: typography and preview

- `tools/generate_literata.py` downloads OFL Literata TTFs and generates Latin-1
  bitmap tables for Regular, Italic, Bold, and BoldItalic.
- `display::font` renders generated bitmap glyphs directly into the framebuffer.
- Reading mode and the chapter navigation screen use Literata; tiny 5x7 remains
  for debug/status chrome and non-reader utility views.
- `tools/preview` exports PBM/PNG snapshots for Home, Files, Reading, Chapters,
  and Settings into `target/previews`. It can also render EPUB parser previews
  from host-side files for layout inspection before flashing.

## Phase 4c: development emulator

- `app-core` now owns the shared reader message types and pure `ReaderState`
  reducer. Firmware keeps the Embassy channels and task shell, while host tools
  can drive the same navigation/library/restore logic without ESP HAL.
- `display::epd::fill_transformed_band` exposes the validated X4 panel byte/bit
  transform so firmware and emulator stream the same panel RAM layout.
- `tools/emulator` provides a deterministic headless runner plus an optional
  egui frontend behind `--features gui`. Headless mode accepts one TOML scenario
  or a scenario directory, applies scripted button/library events, validates app
  and panel state, writes PNG frame dumps, and compares against golden PNGs.
- The emulator includes per-controller protocol models. The SSD1677 model tracks
  BW/RED RAM and update controls; the UC8253 model validates complete DTM1/DTM2
  writes, LUT/CDI readiness, prestaging, power, and sleep. UC8253 refresh order
  comes from the same allocation-free `display::epd::uc8253::FlushPlan` that
  firmware executes, so the host model cannot drift into a second driver.
- Scenarios live under `fixtures/scenarios`; matching golden frames live under
  `fixtures/golden`. Add or update both before/alongside UI/navigation changes
  so agents can verify behavior before flashing hardware.

## Phase 5: Wi-Fi sync

- esp-wifi 0.12.0 linked from crates.io on the esp-hal 0.23.1 stack
  (esp-hal-embassy 0.6, embassy-executor 0.7, embassy-time 0.4, embassy-net
  0.6); trimmed `ESP_WIFI_CONFIG_*` buffer counts live in `.cargo/config.toml`.
- RAM accounting after linking the radio: PREV_FB moved into dram2, a 16 KB
  dram2 heap claim, and the loaned EPUB scratch give the session ~84 KB of
  esp-alloc heap in three regions while the stack region holds at ~41 KB
  (down from 43 KB; re-measure the EPUB open chain with -Zemit-stack-sizes
  before deepening that path).
- kosync progress sync implemented end to end pending hardware validation:
  Sync screen (Home -> sync key) -> SyncCommand::Start -> memory loan ->
  STA join -> DHCP -> GET/PUT /syncs/progress with KOReader partial-MD5
  document ids -> SyncEvents back to the screen -> Exit resets the device.
  Scenario coverage in `fixtures/scenarios/sync-*.toml`; protocol host tests
  in `proto::kosync`.
- Dev credentials are compile-time: build with `CALENDULA_WIFI_SSID`,
  `CALENDULA_WIFI_PASS`, `CALENDULA_KOSYNC_HOST` (host or host:port, plain HTTP),
  `CALENDULA_KOSYNC_USER`, `CALENDULA_KOSYNC_PASS`.
- On-device validation (June 11 2026, esp-hal 0.23 stack): the full session
  ran on hardware — Confirm started it, the loan + book gather + radio init
  completed, the station joined and got 192.168.0.233 via DHCP (~21 s from
  Start to address; the 20 s join timeout deserves headroom or scan tuning),
  status repaints rode the 438 ms fast refresh, USB serial survived radio
  init, Back reset the device, and the saved position (page 619/ch 87) came
  back intact after reboot. A later same-day run validated the kosync
  exchange in both directions against a LAN server: push (GET 404 -> PUT,
  document id `bfc024...` partial-MD5, percentage 0.549), then pull (server
  planted at 0.9 -> `kosync: pulled permille=900` -> StoreProgress -> boot
  restore at screen 1016/chapter 127). Still pending: a heap high-water
  reading (log esp_alloc::HEAP stats during a session) and interop against
  a real kosync server implementation rather than a protocol stub.
- AP-mode web onboarding validated on hardware June 11 2026: the hotspot
  raised, the phone leased an address, and submitted credentials landed in
  WIFI.BIN (the user typed the portal URL manually that run; the DNS
  responder's silence on AAAA/HTTPS queries was stalling captive
  detection and is fixed since, untested on a phone). Still to observe on
  serial: a station join sourced from WIFI.BIN rather than compile-time
  credentials, and the auto-raised sign-in sheet. Details: with no
  WIFI.BIN and no compile-time credentials, Confirm on the Sync screen
  raises a `CALENDULA-XXXXXX` hotspot at 192.168.4.1 with hand-rolled captive
  DHCP/DNS (proto::captive, host-tested) and a credential form; submitted
  credentials persist through StoreWifiCredentials into /READER/WIFI.BIN
  and the next session joins as a station. The join QR is encoded at
  render time by ui/src/join_qr.rs from the session PSK and this device's
  PortalSsid, neither of which is a build-time constant;
  tools/generate_qr.py bakes only the fixed portal-URL QR. Stack region
  after the portal: ~38.9 KB.
- Browser EPUB upload validated end to end on hardware June 11 2026: a
  2.4 MB EPUB traveled browser -> Wi-Fi -> loaned buffers -> /BOOKS and
  surfaced on the shelf after the rescan; the page lists the catalog with
  per-book removal (both /BOOKS and card-root entries) and shows real
  upload progress. Found live and fixed: a literal newline inside the
  page's JS string (parse error masked every other symptom), the kosync
  exchange clobbering the catalog in the loaned http_b, missing UTF-8
  charset, and the catalog snapshot hiding new books from the boot scan.
  Implementation: after the
  kosync exchange the session keeps serving at the device's LAN address
  (SyncStatus::Serving screen hands out the URL); POST /upload streams raw
  EPUB bytes through a two-buffer ping-pong into the display task, which
  holds one SD session for the upload phase and writes /BOOKS/<8.3>.EPU
  (the catalog filter accepts .epu alongside .epub). Books appear after the
  session-ending reset's rescan. Stack region ~36.7 KB after the upload
  futures; the EPUB-open chain's ~30 KB watermark is the floor to respect.
- Next: kosync account onboarding via the portal form, and TLS for the
  official sync server.

## Verification commands

```sh
tools/cargo-c3.sh check -p fw-c3 --release
tools/check.sh dep-boundaries
tools/check.sh s3
tools/build-release.sh x4-pro
tools/cargo.sh test -p app-core -p proto --target aarch64-apple-darwin
cargo test --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --no-default-features
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --no-default-features -- --scenario fixtures/scenarios --check fixtures/golden
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --no-default-features -- --scenario fixtures/scenarios --dump target/emulator
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --features gui -- --gui
tools/cargo-c3.sh clippy --workspace --release -- -D warnings
cargo run --manifest-path tools/preview/Cargo.toml --target aarch64-apple-darwin
tools/bench/bench.py channel-stress --host
```

Hardware confidence checks after relevant firmware changes:

```sh
tools/bench/bench.py page-turn --port /dev/cu.usbmodem101 --turns 50
tools/bench/bench.py page-turn --port /dev/cu.usbmodem2101 --board x4-pro --turns 50
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --reset-before --seconds 20 --strict
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 5
tools/bench/bench.py report target/bench/latest.jsonl
```
