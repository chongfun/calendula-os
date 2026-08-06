# CalendulaOS architecture

This firmware is a bare-metal Rust reader OS for the Xteink X4 and X3 e-ink
readers: ESP32-C3, monochrome e-paper panels, no PSRAM.

The design goal is not to imitate a desktop OS. It is a small data pipeline:

```text
buttons -> app state -> display command -> framebuffer -> EPD panel RAM -> refresh -> sleep
```

## Current architecture diagram

```mermaid
flowchart TD
    buttons["GPIO3 power button<br/>ADC button ladders"]
    input_task["input_task<br/>debounce + classify buttons"]
    app_task["app_task<br/>owns ReaderState reducer shell"]
    display_task["board I/O + display task<br/>single owner of EPD bus, SD CS,<br/>ReaderStore, framebuffer"]
    power_task["power_task<br/>idle timer + deep sleep"]
    wifi_task["wifi_task<br/>sync session + browser shelf"]

    app_core["app-core<br/>Copy message contracts<br/>ReaderState reducer<br/>RefreshPlanner"]
    display_crate["display<br/>1 bpp framebuffer<br/>drawing + fonts<br/>EPD transforms"]
    proto["proto<br/>bounded book/storage/text/cache models<br/>ZIP/EPUB/XHTML parser pieces"]
    hal_ext["hal-ext<br/>SPI DMA, RTC, NVM helpers"]
    ui["ui<br/>bounded layout/render helpers"]

    epd["EPD panel (X4: SSD1677, X3: UC8253)<br/>framebuffer / previous-frame RAM"]
    sd["microSD FAT<br/>/BOOKS + card root EPUBs<br/>/XTEINK cache + catalog + state"]
    sleep["ESP32-C3 deep sleep"]

    emulator["tools/emulator<br/>host reducer + panel protocol model<br/>scenario/golden-frame runner"]
    preview["tools/preview<br/>host render/export inspection"]
    fixtures["fixtures<br/>TOML scenarios + golden PNGs"]

    buttons -->|"raw samples"| input_task
    input_task -->|"InputEvent"| app_task
    app_task -->|"DisplayCommand::Render / Sleep"| display_task
    app_task -->|"StorageCommand"| display_task
    display_task -->|"DisplayEvent::Settled / Asleep / Failed<br/>LibraryEvent"| app_task
    app_task -->|"PowerEvent::Activity"| power_task
    display_task -->|"PowerEvent::DisplaySettled / DisplayAsleep"| power_task
    power_task -->|"DisplayCommand::Sleep"| display_task
    power_task --> sleep

    app_task -.-> app_core
    display_task -.-> app_core
    display_task -.-> display_crate
    display_task -.-> proto
    display_task -.-> hal_ext
    display_task -.-> ui

    display_task -->|"framebuffer flush<br/>full/fast refresh"| epd
    display_task -->|"SD session<br/>catalog, cache, progress"| sd

    emulator -.-> app_core
    emulator -.-> display_crate
    emulator -.-> proto
    emulator --> fixtures
    preview -.-> display_crate
    preview -.-> proto
```

## Rules

- `#![no_std]`, no heap allocation in the reading path. The one
  exception is the Wi-Fi sync session, which donates loaned buffers to
  esp-alloc and ends in a reset (see "Wi-Fi sync session").
- Two framebuffer allocations: active drawing buffer in main DRAM, previous-frame
  buffer in DRAM2. Each is 48,000 pixel bytes on X4 (800×480) or 52,272 on X3
  (792×528), 1 bpp.
- Display ownership is single-writer: only `display_task` touches the EPD bus.
- Reader state ownership is single-writer: only `app_task` mutates page/menu state.
- Messages are small `Copy` values. Bulk bytes stay in caller-owned buffers.
- Power requests display sleep through `display_task`; it never touches SPI.
- Hardware assumptions live in one of two places:
  - Board wiring in `fw/src/main.rs` and `fw/src/tasks/input.rs`.
  - Controller protocol in `display/src/epd/`.

## Workspace

```text
app-core/ app state reducer and Copy message contracts shared by firmware/tools
display/   framebuffer, drawing primitives, EPD controller constants and address math
hal-ext/   thin async wrappers over ESP HAL peripherals
fw/        boot, Embassy executor, task wiring, board-owned peripherals
ui/        shared shell rendering plus ui::reading, the reader page-plan seam
           (page bounds, ink measurement, wrapping) used by fw and host tools
proto/     bounded book/storage/text/cache models plus ZIP/EPUB/XHTML parser
           pieces, including the classic-ZIP container gate (`ContainerGate`):
           the resumable bounded validator behind source-store's
           `validate_container` hooks, enforcing `SourceContainerLimits` and
           rejecting ZIP64 and multi-disk archives deterministically; and
           `proto::source_http`, the parsing/formatting layer of the M0S
           logical-book HTTP contract (request IDs, headers, JSON bodies
           and responses), host-tested like `captive` and `upload`
source-store/ M0S source-transaction foundation: commit-sector record framing,
           the durable publication sequence (`durable_sync`), typed authority
           records (source metadata, tombstones, staging markers, idempotency
           receipts), strict fail-closed authority loading (decayed committed
           records never read as absent), startup source selection, the
           managed SRC namespace, the delete, create/replace, and
           explicit-recovery transactions, the request-ID lookup order
           (receipts, committed records, staging markers), unmanaged adoption
           and re-identification, restartable cleanup, resumable
           source-identity jobs (full SHA-256 plus the quick fingerprint),
           the mount-session validation contract (quick check, provisional
           cached-open gating, background full validation, external-
           modification quarantine), and the list-books view with
           per-book integrity status and allowed operations; host-tested
           with fault-injection and simulated power-cut replay over every
           authoritative record class
tools/emulator/ host-side development emulator and scenario runner
tools/cargo.sh  rustup-stable Cargo wrapper for firmware builds/checks
tools/bench/    serial bench harness for hardware timing, storage/cache,
                sleep, soak, and host channel-stress checks
```

## Embassy tasks

```text
app_task
  owns ReaderState
  InputEvent -> DisplayCommand::Render
  modes: Home, Library, Reading, Chapters, Sync, Settings

board_io/display task
  owns EpdBus, SD CS, ReaderStore, and Framebuffer
  DisplayCommand::Render -> pure framebuffer render from the current ReaderStore snapshot
  StorageCommand::* -> serialized SD/FAT/catalog/cache work on the shared SPI bus
  DisplayCommand::Sleep -> sleep screen full refresh -> EPD controller deep sleep -> PowerEvent::DisplayAsleep
  sends DisplayEvent::Settled (or RefreshFailed) to app_task when render completes

input_task
  polls GPIO3 and ADC ladders
  debounced ADC/power edges -> reader Button actions -> InputEvent
  owns GPIO3 until the deep-sleep path asks for it: a WAKE_PIN_REQUESTS ping
  stops the polling and surrenders the Input over WAKE_PIN_HANDOFF

power_task
  observes activity and display-settled events
  asks display_task to sleep the EPD controller, then enters ESP32-C3 deep sleep
  takes GPIO3 off input_task before arming it as the wake source, so the pin
  has a single owner when it is re-materialised

wifi_task
  parked until SyncCommand::Start arrives from the Wireless screen
  requests StorageCommand::LoanSyncMemory, receives the dismantled EPUB
  scratch as radio heap, joins Wi-Fi in STA mode, reports SyncEvents to
  app_task, then serves the browser shelf page at the device's LAN address
  SyncCommand::Exit (the done press) ends the session with a software reset
```

## Wireless session

The wireless session is one-way and modal because the radio blob needs ~100 KB of
heap this firmware does not have while reading. `fw::sync_mem` owns the
plumbing: the display task dismantles the EPUB scratch into raw regions
(`reader_cache::dismantle_scratch`), and the wifi task donates them to
esp-alloc. dram2 (the boot-loader shadow segment) no longer contributes a
radio-heap share: it holds only the previous-frame framebuffer, packed
against its top, and every byte underneath belongs to the main stack —
`fw/build.rs` raises `_stack_start` over the freed bytes and asserts the
reader's 27 KB deep-call floor. The radio makes do with the scratch
regions alone, sized for the upload workload by the `ControllerConfig` in
`tasks/wifi.rs`; heap slack is logged at join and after each upload so
that budget stays observable. The smaller scratch buffers are reused
directly as TCP socket and HTTP buffers. Once loaned, the reader pipeline cannot come back: leaving
the Wireless screen after the radio ran maps to `SyncCommand::Exit`, which is
a software reset; boot restore then reloads the saved position.

Before the loan the display task flushes any coalesced reading position
to durable state, because the session's only exit is the reset; a failed
flush refuses the loan rather than dismantling the scratch over an unsaved
position. The refusal is an answer, not a silence: `SYNC_LOANS` carries
`Result<SyncLoan, SyncError>`, the wifi task reports `SyncError::Storage`
to the Wireless screen and re-parks for the next Start, and Confirm
retries the session (nothing was loaned, so no reset is needed and the
position write is retried first). (An earlier
iteration also exchanged the position with a kosync server here; that
shipped unused and was removed — the session is purely a book server.)

Once joined, the wifi task serves a shelf page at the device's LAN
address (the Wireless screen's `Serving` status hands out the URL, with
Confirm as the done key). The
page lists the catalog, shows real upload progress, and offers per-book
removal. Routes: `GET /` serves the page, `GET /list` returns the catalog
snapshot shipped with the loan, `POST /upload?name=` streams raw EPUB
bytes, and `POST /delete?name=` removes a book (card-root entries carry
`root=1`; uploads always land in /BOOKS). Upload bytes reach the display
task — still the single SD owner — through `fw::upload`'s two-buffer
ping-pong: 4 KB chunks carry loaned buffers one way and the buffers come
back on a return channel once written. The display task holds one
interruptible SD session for the upload phase and writes
`/BOOKS/<8.3>.EPU` (the catalog scan accepts `.epu` alongside `.epub`),
recording the browser's original filename in a `/XTEINK/LABELS/<stem>.TXT`
sidecar so the shelf and Library can label the book with it. Power/idle
sleep and the done press abort an active writer through the upload-store
transaction (the staged file's FAT chain is reclaimed, replaced originals
untouched), close the SD session, and only then sleep or reset; the done
press waits for the stop acknowledgement so the reset never races an open
FAT writer. The boot rescan then surfaces the new books.

Station credentials come from `/XTEINK/WIFI.BIN` (written by the
onboarding portal below), falling back to compile-time `option_env!`
values (`XTEINK_WIFI_SSID`/`XTEINK_WIFI_PASS`) for dev builds.
At boot the display task reads WIFI.BIN once and reports the saved
network's name as `SyncEvent::NetworkSaved`, so the Wireless screen can
show which network is saved and offer connect/forget honestly instead of
guessing from build flags. Forget is a two-press flow (the browse key
arms it, Confirm deletes WIFI.BIN via
`StorageCommand::ForgetWifiCredentials`) and is only reachable while the
radio is untouched; it drops the screen back to the set-up offer — the
recovery path for a wrong password or a changed router that used to
require editing the card on a computer.

With no credentials anywhere, starting a session raises the onboarding portal
instead: a WPA2 hotspot (`XTEINK-X4` or `XTEINK-X3`, named for the board)
at 192.168.4.1 with a captive DHCP
server, a DNS catch-all (every name resolves to the portal, which makes
phones raise their sign-in sheet unprompted), and a credential form on
port 80. The hotspot's WPA2 PSK is minted per session from the hardware
RNG when the portal starts — the form's plaintext POST is at least
encrypted over RF, and because the PSK exists only in that session's
RAM, nothing secret lives in the repo or is extractable from a release
binary. It rides `SyncEvent::PortalUp` to the Wireless screen, which
encodes the join QR at render time (`ui/src/join_qr.rs`, Nayuki's
no-heap qrcodegen) and prints the password beside it for phones that
cannot scan; the display is the PSK's only channel (supporting both
QR scanning and manual password entry), so the on-screen credentials
and beacon cannot drift. Submitted credentials travel to the display
task as a `StoreWifiCredentials` Copy message, land in WIFI.BIN, and the
next session joins as a station. `proto::captive` holds the sans-IO
DHCP/DNS/HTTP codecs under host tests; the wifi task only owns sockets.

Embassy is used for cooperative waits: ADC retry delays, button polling, SPI DMA
transfers, BUSY waits, and sleep windows all yield instead of spinning. The real
battery win comes after display settle: the power task asks the display task to
draw a visible sleep screen, power down the EPD controller, then move the ESP32-C3 into
deep sleep. The power button also requests this same sleep path instead of being
treated as ordinary navigation input.

Input/render backpressure is intentionally coalesced. The app keeps at most one
display render in flight. While the display is refreshing, new button events
still update `ReaderState`, but they set a single pending-render flag instead of
queuing stale framebuffer renders. When `DisplayEvent::Settled` or
`RefreshFailed` arrives, the app renders the latest state once.

Storage is also explicit. Files/Home/Reading transitions enqueue
`StorageCommand`s after the visible render settles; render commands never scan
FAT, open EPUBs, build caches, or write progress. Open/extend requests whose
page already sits inside the loaded section window are answered from RAM without
an SD session or a redundant display render, and reading-progress writes are coalesced (at most one
alternating STATEA/STATEB generation per 15 s, flushed before display
sleep, with sleep deferred if the flush fails). The board I/O task is still
the single SPI owner, so display refresh and SD transactions cannot overlap, but
the user-facing view is always drawn from the latest already-owned snapshot.
SD/FAT access goes through an SD session: the board I/O task deselects the
display, clocks the bus down for the card (400 kHz identification with wake
clocks, then 25 MHz data), opens the FAT root, performs one storage action, and
restores the panel's display SPI clock before returning to EPD work. The card stays powered
between sessions while the device is awake, so only the first session runs the
full CMD0/ACMD41 init; later ones reuse the remembered card type and skip the
handshake, falling back to a cold init if a reused session cannot open the
volume. Deep sleep resets the chip and clears that state.

## Display model

`display::fb::Framebuffer` is the source of truth. White is bit `1`, black is
bit `0`, row-major.

Geometry and fast-refresh timing depend on the board:

- **X4**: SSD1677, 100-byte rows, ~421 ms fast waveform.
- **X3**: UC8253, 99-byte rows, ~379 ms fast waveform.

The full refresh (the only mode that reliably clears unknown pixels) and normal
page turns differ by controller:

- **X4 (SSD1677)**: The first refresh writes the current frame to both BW and
  RED RAM, then runs the multi-flash full waveform (~3.5 s). Normal turns write
  the current frame to BW RAM with the retained previous frame in RED RAM, then
  trigger the fast waveform.
- **X3 (UC8253)**: The full plan writes white to DTM1 and the current frame to
  DTM2, runs the full refresh, then stages the current frame into DTM1 and runs
  a fast settle pass. Normal turns write the current frame to DTM2; the previous
  frame is only written to DTM1 if it was not already staged.

`RefreshMode::FastClean` sits between those — a one-flicker clean:

- **X4 (SSD1677)**: Runs display-mode-1 with the temperature register forced to
  90 °C, selecting the hotter OTP LUT (~1.5 s, small contrast cost). The sensed
  temperature is restored afterward.
- **X3 (UC8253)**: Uploads the firmware-defined `HALF` LUT bank in absolute CDI
  mode — a similar short clean without temperature overrides.

Waking from the sleep screen and view/context changes use `FastClean`
instead of the full waveform, since the panel's contents are known.

`RefreshPolicy` in Settings: `FastOnly`, `FullOnWake` (default), or
`FullEveryTen` (legacy name — actually every eight fast refreshes).

`display::epd` contains transform constants validated during bring-up:

- **X4 (SSD1677)**: `MIRROR_X = true`, `MIRROR_Y = false`, and `REVERSE_BITS = true`.
  (`MIRROR_Y = true` was tested and rejected because it made glyphs vertically
  mirrored/upside down.)
- **X3 (UC8253)**: `MIRROR_X = true`, `MIRROR_Y = true`, and `REVERSE_BITS = true`.

The logical framebuffer API stays upright; firmware and host tools remap
bytes/bits before panel-RAM writes, fixing the observed byte and bit order
without leaking hardware orientation into rendering.

Physical orientation is an app/layout concern, not a hardware streaming concern.
The current readable build places logical top on the physical button side. The
reader state already carries a complete orientation enum:

```rust
enum DisplayOrientation {
    LandscapeButtonsBottom,
    LandscapeButtonsTop,
    PortraitButtonsLeft,
    PortraitButtonsRight,
}
```

Default reader mode is `PortraitButtonsLeft`, but the low-level display
transform above should stay fixed unless corruption returns.

Addressing handles the hardware specifics of each controller:

- **X4 (SSD1677)**:
  - SPI mode 0, 20 MHz (the write-mode datasheet maximum; the OpenX4 SDK's 40 MHz worked only on margin).
  - BUSY is active high.
  - X window is pixel-addressed, `0..799`.
  - Y gate scan is reversed, so the full Y window is `479..0`.
- **X3 (UC8253)**:
  - SPI mode 0, 20 MHz.
  - BUSY is active low.
  - Streams 792×528 visible pixels in direct row order; the controller is
    configured for 792×600 gates (extra gates fall outside the panel).

## Data-oriented design

State is plain data, not object graphs:

```text
InputEvent        Copy enum
ReaderState       view/book/page/chapter/settings/battery fields
RenderRequest     view/book/page/orientation/refresh/battery/dirty rect
Layout<N>         parallel arrays of kind/rect/parent/text span
Framebuffer       single flat byte array
```

`app-core` owns the reader reducer and the shared message contracts. The
firmware `app_task` is an Embassy shell around this pure reducer, and host tools
use the same reducer for deterministic navigation tests. This keeps button flow,
library events, restore events, orientation, refresh policy, and render requests
from drifting between device and emulator.

EPUB work keeps the same shape:

```text
SD file -> ZIP entry -> inflate window -> XML token -> flat cache record -> glyph blit
```

No DOM, no heap object graph, and no entire-book-in-RAM reader model. Parsers
are allowed to be state machines, but their output is immediately flattened into
bounded records.

`proto` owns the reader data contracts shared by Home, Files, Reading, Chapters,
and the host preview tool:

- `BookMeta`, `BookProgress`, and `ChapterMeta` for catalog and progress data.
- `BookStorage` and `FileCandidate` for microSD-backed `.epub` discovery.
- `ZipArchive` for host-side central-directory lookup and stored/deflated entry
  reads into caller-owned buffers.
- `ZipStream` for central-directory lookup and entry reads through a bounded
  `ReadAt` interface, which is the path storage-backed EPUBs use. Firmware ZIP
  reads stream deflate input through a reusable inflater scratch state, so large
  compressed members do not have to fit in the compressed scratch buffer.
- `EpubZipOps` as the narrow zip-entry interface cache loaders program
  against. Both zip front-ends implement it, and one shared streaming inflate
  engine sits behind them, so entry reads behave identically regardless of
  whether compressed bytes come from random-access or forward-only storage.
- `EpubPackage` for container/OPF metadata, manifest, and spine. Spine and
  manifest strings are stored as offset+length spans into the shared OPF
  text rather than inline strings, halving each item's size so long books
  (192-item spine cap, 224-item manifest cap) fit within the tight
  EPUB-open stack budget.
- `xhtml_blocks_to_sink` with `TextRole`, `FontStyle`, and `TextAlign` as the
  single XHTML extraction path feeding bounded block records.
- `BookV2Header` with `BookV2SectionRecord`, and `SectionV2Header` with
  `PageRecord`, `BlockRecord`, and `TocRecord`, for the bounded binary cache
  records the firmware reads and writes. The earlier `BookCacheHeader`,
  `SectionHeader`, `PageCacheHeader`, `LineRecord`, and `WordRecord` remain in
  `proto::cache` only for the disabled V1 migration path.

The firmware still ships one built-in catalog entry as a fallback, but the
board I/O task owns the shared SPI bus while it scans FAT16/FAT32
microSD cards for EPUBs under `/books` and then the card root. X4 SD pins are
configured on the shared SPI bus (SCK GPIO8, MOSI GPIO10, MISO GPIO7, SD CS
GPIO12). SD transactions and display refreshes remain serialized by that single
board-I/O owner.

## SD-backed reader cache

The SD reader uses a V2 whole-book cache. Opening an EPUB parses OPF/TOC/spine,
then builds the whole book up front: every spine item paginates into one or more
fixed-size sections, each section is written to its own file, and a book index
records where each section sits. After that the book reopens from cache in tens
of milliseconds; only the first build of a large book is slow (minutes for
something HPMOR-sized).

A chapter is a spine item, and a long one paginates into several sections. The
builder closes the current section and opens the next when its in-RAM arena
fills, where the text budget (16 KB) is the binding limit for prose, well ahead
of the block (384) and page (96) caps. Sections are invisible while reading: the
reader walks across them seamlessly, and the footer page-in-chapter counter
aggregates every section sharing a spine. The book index holds up to
`MAX_BOOK_SECTIONS` (320, on the order of 4,500 pages); a longer book caches
`partial`.

Each section header carries a `font_config` that packs `READER_LAYOUT_VERSION`
with the type size and spacing it was paginated under. A loaded section whose
version or size no longer matches is invalid and forces a rebuild, so bumping
`READER_LAYOUT_VERSION` retires every stale cache after a layout or
cache-encoding change; a spacing-only change re-walks line heights without a
reparse.

Cache paths use FAT 8.3-safe names because `embedded-sdmmc` operates on short
file names in the firmware path. The library list is a windowed catalog
snapshot at `/XTEINK/CATALOG.BIN` (v6: `X4CT` magic, u16 book count, 156-byte
records). Firmware streams it `LIBRARY_WINDOW` (16) entries at a time instead
of holding the whole list in RAM, so library size is bounded by the card, and
only window crossings re-read it. The currently open book sits in a separate
`active_entry` so the reading path never depends on where the list is
scrolled. On boot/refresh, firmware first loads a window from the cached
snapshot, then refreshes `/BOOKS` and card-root discovery in a storage
command, streaming the fresh catalog out in batches without ever holding it
whole. Discovery skips dot-prefixed entries, so the AppleDouble sidecar
(`._<book>.epub`) Finder writes beside every file it copies to a FAT card is
not catalogued as a phantom, unopenable duplicate. The scan reads long
filenames through a buffer sized to the FAT maximum, because an entry whose
long name does not fit is presented under its short name, and the sidecar's
short name (`_BOOK~1.EPU`) no longer carries the dot that identifies it.
Entries are labeled with the book's real title from its cached
`BOOK.BIN`, falling back to the stored original-filename label for uploaded
8.3-named books, then to the prettified file stem. Each fresh catalog write
also sweeps `CACHE2` and reclaims caches whose stored source identity no
longer matches any catalogued book, deleting the data files and the emptied
directories while leaving the durable state files intact. Files renders the current
snapshot immediately. It may show “Library unavailable” before any successful
cache/scan, and “No books found” only after a completed scan proves the card
has no EPUBs.

```text
/XTEINK/CACHE2/E<hash>/BOOK.BIN
/XTEINK/CACHE2/E<hash>/TOC.BIN
/XTEINK/CACHE2/E<hash>/COVER.BIN
/XTEINK/CACHE2/E<hash>/CONT.BIN
/XTEINK/CACHE2/E<hash>/SECTIONS/S000.BIN
/XTEINK/CACHE2/E<hash>/SECTIONS/S001.BIN
/XTEINK/CATALOG.BIN
/XTEINK/LABELS/<stem>.TXT
/XTEINK/STATEA.BIN
/XTEINK/STATEB.BIN
```

`BOOK.BIN` holds a `BookV2Header`, one `BookV2SectionRecord` per section (spine,
start page, page count, partial), TOC records, and a string blob for title,
author, and TOC titles. Section files hold a `SectionV2Header`, page records,
block records, per-block paragraph flags, and the UTF-8 text blob of that
section's pre-wrapped lines. `TOC.BIN` is a per-book chapter-list sidecar for
the Chapters overview, distinct from the TOC records inside `BOOK.BIN`.
`CONT.BIN` records the build's `push_block` stream — the settings-independent
half of the work — so a type-settings or orientation change replays it into the
same sink instead of re-reading and re-parsing the EPUB. It is purely an
accelerator: its header only says `complete` once a whole book has been
captured, and any read or decode failure deletes it and falls back to the EPUB.

A cold build does not run to the end before the reader sees the book. It
publishes as soon as the section holding the requested page is written, marking
`BOOK.BIN` partial, and finishes the spine in slices from an idle branch of the
display task's loop — so the first page arrives in about a second rather than
after the whole walk, and every other task keeps getting scheduled meanwhile.
Only the pages built so far are addressable until the walk finishes, and the
index says so in two separate ways: `partial` means pages are missing, while
`resume_spine` names the spine item a walk meant to come back for. The second is
what keeps a build interrupted by sleep from capping a book forever — the reader
is clamped to the advertised page count and so can never ask for the first
missing page, so an index nobody is still building is refused on the next open
and rebuilt (progressively again, so the first page still arrives quickly). The
suspend, announce, and partial-index policies are host-tested in
`app-core::storage_loop`, beside the open and sleep sequences.

The active firmware state keeps only loaded book
metadata, the full section index, the active section's page/block records and
text bytes, and small ZIP/XML scratch buffers. Spine XHTML members of any size
stream completely through the resumable block parser in bounded inflate
windows, so chapter content is never truncated by scratch-buffer limits.
`STATEA.BIN`/`STATEB.BIN` store the encoded `AppStateRecord` in alternating
checksummed generations (`proto::durable`), so a torn write never destroys
the last good position; a legacy `STATE.BIN` is still read as a fallback.
Record version 2 and later include the
SD source size and path-derived hash so boot restore can map saved progress
back onto the scanned SD catalog instead of trusting a volatile list index.
The current version 3 also persists the type settings (font size and line
spacing).

`COVER.BIN` is an optional Home-cover sidecar for the same cache key. It stores
a tiny header followed by a 202x303, 1-bit, row-packed bitmap matching the Dock
Clean cover slot. Firmware treats it as flat DOD data: valid records are drawn
directly, while missing or invalid records fall back to generated cover art. The
host preview tool can generate the sidecar from EPUB JPEG/PNG covers with
`--cover-bin` or write it directly to a mounted SD cache path with `--sd-root`.

Reading and chapter navigation typography use generated Literata bitmap assets.
The host generator downloads OFL Literata TTFs and emits Latin-1 glyph
metrics/bitmaps for Regular, Italic, Bold, and BoldItalic. Firmware does not
rasterize TTFs on-device. Glyphs are rasterized in FreeType's monochrome mode
rather than antialiased and thresholded, and the glyph box is taken from that
same mode so the stored metrics describe the stored bitmap. The box is a
pagination input — the wrap reads `x_offset + width` — so changing how it is
derived is a `READER_LAYOUT_VERSION` bump.

Regeneration is pinned across toolchain and configuration, because metrics and rasterization
are wrap and rendering inputs. The TTFs are fetched from immutable upstream commits and
checked against `FONT_SHA256` on every run, Pillow and FreeType are pinned to
`PILLOW_PIN` (`10.4.0`) and `FREETYPE_PIN` (`2.13.2`) — release 11 changed `getlength`
from the hinted advance to the unhinted one, which moves every advance in every face, and
different FreeType builds alter glyph rasterization and placement —, and `THRESHOLD` is
pinned to `128` (overrideable for experiments via `ALLOW_UNPINNED_THRESHOLD=1`). All
mismatches stop the run rather than quietly emitting different tables. Regenerate through a
throwaway environment holding the pin:

```sh
python3.12 -m venv .fontgen && .fontgen/bin/pip install 'pillow==10.4.0'
.fontgen/bin/python tools/generate_literata.py    # and the other generators
cargo fmt -p display                              # strips a trailing blank line
```

Under the pin every shipped table reproduces byte for byte, so a regeneration
diff contains only what was intended. Adopting newer metrics is a deliberate
typography change: move the pin, bump `READER_LAYOUT_VERSION`, and re-bless
the goldens and `display/tests/glyph_tables.rs` in the same commit.

## Development emulator

`tools/emulator` is a host-side parity tool for fast development loops. It has a
headless scenario runner for agents/CI and an optional egui frontend for manual
interactive testing. The default build is headless; the desktop window is built
with `--features gui` to keep routine checks lightweight.

The emulator intentionally models the behavior that is useful during ordinary
development:

- app reducer state transitions from button and library events
- selected-panel 1 bpp framebuffer rendering (X4 800x480 or X3 792x528)
- shared panel byte/bit transform from `display::epd`
- SSD1677-style BW/RED RAM, address counters/ranges, refresh mode history, and
  deep-sleep command validation
- UC8253 DTM1/DTM2, LUT/CDI, prestage, power, and sleep validation driven by
  the same allocation-free refresh-operation plan as firmware
- scripted scenarios that can assert final view/book/page/selection/panel state,
  dump PNG frames, and compare against golden frames

It does not model ESP32-C3 CPU timing, ADC noise, SPI DMA edge cases, BUSY
timings, voltage/temperature behavior, or true e-paper waveform physics. Those
remain hardware-validation concerns.

## Development bench

`tools/bench/bench.py` is the hardware-facing counterpart to the emulator. It
captures serial output with the same DTR/RTS behavior as `tools/serial_capture.py`,
parses structured `bench:` telemetry, writes JSONL logs under `target/bench/`,
and reports timing/storage/sleep summaries. Current hardware suites are guided
workflows; the firmware still has no interactive serial command channel.

Use it in tiers:

- `channel-stress --host` in ordinary development when queue/coalescing,
  refresh-plan, sync-session, reader state, display command, or storage command
  behavior changes.
- short `page-turn` and `sleep-sync` runs before trusting a flashed firmware
  after display, input, sleep, reader rendering, SD session, section cache, or
  progress-write changes.
- longer `reader-soak`, `storage-cache`, and `sleep-sync` runs before releases
  or risky merges.
- `thermal-run` for targeted refresh, ghosting, sleep-screen, enclosure, power,
  SD-card, or ambient-temperature investigations.

Typical commands:

```sh
tools/bench/bench.py channel-stress --host
tools/bench/bench.py page-turn --port /dev/cu.usbmodem101 --turns 50
tools/bench/bench.py storage-cache --port /dev/cu.usbmodem101 --reset-before --seconds 20 --strict
tools/bench/bench.py sleep-sync --port /dev/cu.usbmodem101 --cycles 10
tools/bench/bench.py report target/bench/latest.jsonl
```

Typical development loop:

```sh
cargo test -p app-core -p proto --target aarch64-apple-darwin
cargo test --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --no-default-features
cargo test --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --no-default-features --features device-x3
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --no-default-features -- --scenario fixtures/scenarios --check fixtures/golden
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --no-default-features -- --scenario fixtures/scenarios --dump target/emulator
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --no-default-features -- --scenario fixtures/scenarios --present-dump target/emulator-presented
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --features gui -- --gui
```

## Web emulator

`tools/web-emulator` compiles the shared crates (`app-core`, `ui`, `display`,
`proto`) to `wasm32-unknown-unknown` behind a small raw C ABI (no
wasm-bindgen). `web/index.html` is a single self-contained page that hosts the
framebuffer on a canvas inside a device mockup, feeds key presses and a
monotonic clock in, and simulates e-ink refresh behavior (fast updates redraw
with ghosting only; fast-clean flickers once; full runs inversion passes).
Reading progress persists in localStorage through the same
`PersistedAppState`/`LibraryEvent::Restored` shape the firmware uses.

Parity boundary: everything rendered by the shared crates tracks firmware
changes automatically. The firmware shell (`fw/`) is not compiled; the wasm
crate carries small stand-ins for it:

- a fake SD layer: three public-domain books plus a tour, parsed from
  `tools/web-emulator/books/*.txt` (regenerated by `books/convert.py`) into
  `BlockRecord`s and paginated with the real `ui::reading` walk
- a scripted Wi-Fi session ending at `SyncEvent::Serving`
- a copy of the SD reading-screen composition from `fw/views.rs` (page body,
  page-in-chapter footer, loading book plate) — a change to that chrome in
  firmware needs the same change mirrored in `tools/web-emulator/src/lib.rs`

Build and deploy:

```sh
cargo build --manifest-path tools/web-emulator/Cargo.toml --target wasm32-unknown-unknown --release
cp tools/web-emulator/target/wasm32-unknown-unknown/release/x4_web_emulator.wasm web/
python3 -m http.server -d web   # local check
```

`.github/workflows/pages.yml` runs the same build, checks the golden frames,
exports browser-presented scenario screenshots into `images/screens/`, and
publishes `web/` to GitHub Pages on every push to main that touches `web/`, the
wasm crate, shared crates, or scenario fixtures. A tagged release dispatches
the Pages workflow with its tag, which copies that release's flash images into
the Pages artifact so the ESP web flasher can fetch same-origin firmware. The
built `.wasm` and release images are gitignored; only sources are committed.

## Reader app model

The firmware now has the e-reader surfaces as explicit app state:

- `Home`: current book cover/metadata plus Continue, Library, Sync, and Settings.
- `Library`: selects a book or opens settings.
- `Reading`: owns the active book/page position.
- `Chapters`: selects a chapter within the current book.
- `Settings`: cycles seven rows -- typeface, type size, type weight, line
  spacing, refresh policy, `DisplayOrientation`, and the front-button layout.
  The orientation row offers three of the four holds; the buttons-above
  portrait variant stays in the enum for the persistence format only.

Every surface renders in one hold, so `Home`, `Library`, and `Settings` share
the reading posture rather than rotating independently. Calendula boots into
the portrait hold (`PortraitButtonsLeft`) as the sole documented boot default;
the landscape holds stay in the Settings cycle for the X4's side page buttons.
The shared orientation enum, its persisted byte values, and the cycle order
remain preserved for saved-state compatibility. Home is cover-led: the current
book is the visual anchor, with a restrained menu down the side for Continue,
Library, Sync, and Settings.
Reading mode keeps the page quiet: tiny book title, rendered-screen count within
the chapter, symbolic battery, and a thin whole-book progress bar. Home shows a
small battery percentage because it is a status surface. GPIO0 is sampled as the
current rough battery source using a 2:1 divider assumption and a simple
3300-4200 mV LiPo percentage curve. The current book may be the built-in
fallback or the restored/last-selected microSD EPUB. Home triggers SD scan and
state restore on first render, then `Read` resumes the current EPUB through the
same cache-loading path as Files. If there is no current SD EPUB, `Read` opens
Files when EPUBs are present and falls back to the built-in reader when the card
is empty or unavailable. SD EPUBs use the same flat book/chapter/page fields as
built-in content, but page bodies come from the SD-backed cache instead of
static text arrays.

## Current module map

`fw/src/tasks/display.rs` is intentionally the only task touching the EPD bus and
coordinating SD access. It is now the orchestration layer:

```text
display task orchestration
  receives DisplayCommand
  triggers SD scan and EPUB cache loading when needed
  calls view rendering into the framebuffer
  selects refresh mode
  flushes or sleeps the panel
  publishes display/power/library events
```

The deeper modules keep implementation complexity behind narrow data-oriented
interfaces:

```text
fw::display_flush       panel-plan execution, RAM streaming, BUSY waits, and sleep
fw::library_sd          FAT scan, SD chip-select handling, and file discovery
fw::sd_session          SD session open/close and the upload write pump
fw::reader_cache        EPUB-to-cache loading into bounded proto::cache records
fw::reader_cache_files  cache/state/credential/label file records on the card
fw::reader_layout       page indexing, line wrapping, style markers, measurements
fw::reader_store        bounded loaded-book/library state shared by cache and views
fw::catalog             the built-in fallback book's static content
fw::sync_mem            the one-way memory loan for the Wi-Fi session
fw::upload              browser-to-shelf upload ping-pong plumbing
fw::views               Home/Files/Reading/Chapters/Settings drawing
fw::tasks::display      task loop, refresh policy, and event publishing
```

Do not split this by moving bus access into a second task unless there is also a
proper request/response protocol for the shared SPI bus. The current invariant
that display refresh and SD reads cannot overlap is more important than file
size.

Persistent app state is represented by `hal_ext::nvm::AppStateRecord`, a compact
versioned/checksummed record for book id, chapter, rendered screen, shell
orientation, reading orientation, refresh policy, source hash, and source file
size. The firmware stores it in alternating `/XTEINK/STATEA.BIN`/`STATEB.BIN`
generations for SD reading progress (per-book positions use `POSA.BIN`/
`POSB.BIN` beside the book's cache, and Wi-Fi credentials `WIFIA.BIN`/
`WIFIB.BIN`, all framed by `proto::durable`);
flash/NVM fallback remains separate from the record format.

## Performance

| | |
|---|---|
| Page turn | ~424 ms press-to-settled on X3 (379 ms panel BUSY) |
| Wake from sleep | one flicker, ~1.5 s (deep-sleep Power-button wake only: the boot reads the RTC wake cause plus an RTC-RAM marker the sleep handshake writes after the sleep frame settles, and seeds the refresh planner with the sleep screen it knows the panel holds; a battery pull, crash, or a sleep whose final flush failed boots with unknown panel contents and pays the full 3.5 s) |
| Cold-boot full refresh | 3.5 s |
| Reopen a cached book | tens of milliseconds |
| RAM | 400 KB SRAM, no PSRAM |
| Usable stack | ~43 KB |
| Framebuffers | two (active + previous-frame), 1 bpp each: 48,000 B (X4) or 52,272 B (X3) |

## Bring-up checklist

1. Flash firmware and confirm the reader shell appears.
2. Measure BUSY on GPIO6 during reset and refresh.
3. Confirm full refresh timing.
4. Confirm `TL`, `TR`, `BL`, and `BR` are readable and map consistently.
   Current readable transforms depend on the board:
   - **X4**: `MIRROR_X=true`, `MIRROR_Y=false`, `REVERSE_BITS=true`
   - **X3**: `MIRROR_X=true`, `MIRROR_Y=true`, `REVERSE_BITS=true`
   Logical top currently appears on the physical button
   side; handle this later through `DisplayOrientation`.
5. Validate the Adafruit-scaled ADC ladder bands against this physical unit.
   Current calibrated bands are GPIO1 Back `2400..2700`, Confirm `1800..2150`,
   Left `1000..1250`, Right `0..100`; GPIO2 Up `1500..1800`, Down `0..100`. Raw
   hardware buttons then pass through a CrossPoint-style mapping layer into
   reader actions: front `BACK_CONFIRM_LEFT_RIGHT`, side `PREV_NEXT`. Both
   previous-page buttons emit `Previous`; both next-page buttons emit `Next`.
   Raw ADC serial logging and on-screen GPIO values are now behind debug
   constants so normal firmware only refreshes on debounced button edges.
6. Measure deep-sleep current.

Storage, saved progress, Wi-Fi sync, and the FastClean refresh mode have
all landed since this checklist was written; partial-window refresh
remains deliberately shelved.
