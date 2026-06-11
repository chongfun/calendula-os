# xteink-x4-os

Bare-metal Rust firmware for the Xteink X4 e-ink reader: ESP32-C3,
800x480 SSD1677 panel, no PSRAM.

## What it does

- Reads EPUBs from the microSD card (`/BOOKS` and the card root),
  parsing them on-device into a binary cache so books reopen fast.
- Literata type with adjustable size and line spacing, italic/bold
  runs, blockquotes, and chapter navigation.
- Page turns take about half a second; a refresh planner decides when
  to spend a full anti-ghosting refresh.
- Deep-sleeps the ESP32-C3 behind a sleep screen; reading position is
  saved to the card.
- Syncs reading progress with a [kosync](https://github.com/koreader/koreader-sync-server)
  server (KOReader-compatible). The radio needs more heap than the
  firmware has free, so the sync session loans the reader's buffers to
  Wi-Fi and resets on exit.
- Onboards Wi-Fi credentials through a captive portal: the device
  raises a hotspot with a QR code and a credential form.
- Serves a shelf page after each sync for uploading and removing books
  from a browser.

## How it works

The firmware is a small data pipeline:

```text
buttons -> app state -> display command -> framebuffer -> SSD1677 RAM -> refresh -> sleep
```

Pure logic lives in host-testable crates (`app-core`, `proto`, `ui`,
`display`); the firmware crate (`fw`) owns tasks, pins, and DMA. A host
emulator replays TOML scenarios through the same reducer and panel
protocol model and compares output against golden frames in
`fixtures/golden`.

See [ARCHITECTURE.md](ARCHITECTURE.md) for tasks, memory strategy, and
refresh policy, and [CONTEXT.md](CONTEXT.md) for a glossary of the
terms used throughout.

## Building and flashing

Needs the pinned nightly toolchain with the `riscv32imc-unknown-none-elf`
target (rustup picks both up from `rust-toolchain.toml`) and
[espflash](https://github.com/esp-rs/espflash).

```sh
cargo check --target riscv32imc-unknown-none-elf --release   # build firmware
cargo run -p fw --release                                    # flash + serial monitor
```

The kosync account is compile-time for now: set `XTEINK_KOSYNC_HOST`
(host or host:port, plain HTTP), `XTEINK_KOSYNC_USER`, and
`XTEINK_KOSYNC_PASS` when building. Wi-Fi credentials come from the
onboarding portal, or from `XTEINK_WIFI_SSID`/`XTEINK_WIFI_PASS` for
dev builds.

## Host tooling

The workspace's default target is the ESP32-C3, so host runs name the
host triple explicitly (`aarch64-apple-darwin` below):

```sh
cargo test -p app-core -p proto --target aarch64-apple-darwin
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --no-default-features -- --scenario fixtures/scenarios --check fixtures/golden
cargo run --manifest-path tools/emulator/Cargo.toml --target aarch64-apple-darwin --features gui -- --gui
```

The emulator's `--gui` mode drives the full UI on the desktop;
`tools/preview` renders typography output without hardware in the loop.
