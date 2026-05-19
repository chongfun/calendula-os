# xteink-x4-os — Implementation Plan

This document details the step-by-step implementation plan for building the bare-metal Rust firmware for the Xteink X4 e-ink reader. It serves as our active blueprint for development, outlining the architectural boundaries, dependency graph, memory budgets, and layout strategies.

---

## Architectural & Memory Constraints

| Constraint | Value | Rationale |
|---|---|---|
| **CPU Target** | `riscv32imc-unknown-none-elf` | ESP32-C3 single-core RISC-V, no hardware FPU, no atomic A-extension |
| **Memory Limit** | ~380 KB DRAM | No external PSRAM; dynamic allocation is forbidden |
| **Concurrency** | Embassy Async Runtime | Cooperative multi-tasking without preemption or thread stack overhead |
| **Safety** | `#![forbid(unsafe_code)]` | Enforced at the application/library crate level |
| **Graphics** | Streaming 1bpp | Decoupled frame rendering using 48 KB buffer streamed over SPI DMA bands |

---

## Workspace Layout & Dependencies

The project is structured as a multi-crate cargo workspace to isolate hardware abstractions, rendering logic, and layout engines.

```
xteink-x4-os/
├── Cargo.toml                  # Workspace configuration
├── ARCHITECTURE.md             # Hardware research & registry sequences
├── IMPLEMENTATION_PLAN.md      # This document
│
├── fw/                         # Firmware binary crate (Embassy app)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs             # Core initialization & task spawner
│       └── tasks/
│           ├── display.rs      # SPI2 EPD DMA refresh loop
│           ├── input.rs        # ADC resistor ladder & GPIO3 polling
│           ├── power.rs        # RTC Deep Sleep state machine
│           └── wifi.rs         # esp-wifi synchronization task
│
├── hal-ext/                    # Thin ESP32-C3 async wrappers
│   └── src/
│       ├── spi_dma.rs          # Async SPI DMA helpers
│       ├── rtc.rs              # RTC / Deep Sleep controls
│       └── nvm.rs              # Key-value configuration storage
│
├── display/                    # EPD driver (no_std, zero-alloc)
│   └── src/
│       ├── lib.rs
│       ├── fb.rs               # 1bpp row-major Framebuffer
│       ├── epd.rs              # SSD1677 SPI transaction sets
│       └── render.rs           # In-place primitive & text drawing
│
├── ui/                         # Layout engine (no_std, zero-alloc, DOD)
│   └── src/
│       ├── lib.rs
│       ├── layout.rs           # Flat parallel array widget arena
│       └── font.rs             # Embedded flash bitmap font lookup
│
└── proto/                      # Streaming protocols (no_std, zero-alloc)
    └── src/
        ├── epub.rs             # Streaming ZIP / XML parser
        └── wifi_sync.rs        # Network file transfer protocol
```

---

## Technical Components Detail

### 1. Workspace Configuration
*   **Cargo.toml**: Pins a single size-optimized global release profile:
    ```toml
    [profile.release]
    opt-level = "s"          # Optimize for size
    lto = "fat"              # Full Link-Time Optimization
    codegen-units = 1
    panic = "abort"          # No stack unwinding
    ```
*   **rust-toolchain.toml**: Pins compiler `nightly-2025-10-01` and target `riscv32imc-unknown-none-elf`.
*   **.cargo/config.toml**: Configures standard compilation flags and sets `espflash` as our default runner.

### 2. display Crate
*   **Framebuffer**: Declares `[u8; 48000]` representing `800x480` pixels at 1bpp. Provides a `band(y, h)` slice method returning a reference to memory bounds for safe DMA transfers.
*   **EPD Driver**: Encodes Solomon Systech SSD1677 initialization commands via a zero-cost `SpiOp` enum (`Cmd`, `DelayMs`, `Reset`).
*   **Render**: Exposes basic shapes, inverse boxes, and font rendering, modifying the buffer directly.

### 3. ui Crate (Data-Oriented Design)
*   **Arena**: Bypasses the Rust graph lifetime problem by maintaining a parallel array representation. Widgets are indexed by simple `u8` handles rather than pointer allocations:
    ```rust
    pub struct Arena {
        kinds:   [WidgetKind; 64],
        rects:   [Rect; 64],
        parents: [u8; 64], // Parent handle; u8::MAX represents root
        visible: [bool; 64],
    }
    ```
*   **Font**: Standard 8x8 or custom bitmap fonts embedded directly in flash `.rodata`, indexed by binary-searched unicode lookup tables.

### 4. proto Crate (Streaming I/O)
*   **EPUB**: Streams compressed files from flash using an async pull-based parser, feeding tiny chunks into a streaming XML tokenizer to typeset text on the fly without parsing a heavy DOM tree.

### 5. hal-ext Crate
*   **SPI DMA**: Safely wraps `esp-hal` DMA registers to transmit frame bands in the background.
*   **RTC**: Integrates low-power sleep loops to drop the chip current down to `10–15 µA` during sleep cycles.

### 6. fw Crate
*   Coordinates the Embassy cooperative executor, spawning tasks that voluntarily yield to allow background hardware processing.

---

## Verification checklist

1.  **Crate Compiles**: `cargo check --target riscv32imc-unknown-none-elf --release` succeeds without errors.
2.  **Size Guard Checks**: Static DRAM size is audited to stay under 100 KB.
3.  **Zero-Alloc Audit**: Checking compiled crates for banned allocation libraries (`rg -nP '\b(Vec|Box|String|HashMap|BTreeMap|Rc|Arc)::'`).
4.  **Hardware Run**: Monitoring serial logs using `espflash flash --monitor`.
