//! High-frequency serial telemetry, gated behind the `serial-log` feature.
//!
//! The `bench:` lines and the per-refresh driver chatter are the bench
//! harness's entire input (`tools/bench/bench.py` parses them), but they are
//! not free on the device: with no USB host attached there is no SOF, so
//! esp-println's `auto` printer falls back to blocking 115200-baud UART
//! inside a critical section for every print. `serial-log` is default-on, so
//! nothing changes for development, bench captures, or CI; a shipped build
//! can turn the chatter off with `--no-default-features` (re-adding
//! `device-x3` and friends as needed).
//!
//! Only telemetry belongs behind this macro. Error paths, panic/diagnostic
//! output, and boot-identity lines stay on unconditional
//! `esp_println::println!` so a release build still says who it is and why
//! it failed.

#[cfg(feature = "serial-log")]
macro_rules! bench_log {
    ($($arg:tt)*) => { esp_println::println!($($arg)*) };
}

// Same shape as reader-cache's cache_log!: a no-op at runtime, but the
// tokens still go through `core::format_args!`, so a wrong placeholder count
// or a non-Display argument fails the build in both feature states rather
// than only when the chatter is compiled in.
#[cfg(not(feature = "serial-log"))]
macro_rules! bench_log {
    ($($arg:tt)*) => {{
        let _ = core::format_args!($($arg)*);
    }};
}
