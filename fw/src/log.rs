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

// Same type-checking trick as reader-cache's cache_log!: the tokens still go
// through `core::format_args!`, so a wrong placeholder count or a non-Display
// argument fails the build in both feature states rather than only when the
// chatter is compiled in.
//
// The `if false` is what makes "no-op" true. Call sites pass expressions --
// `Instant::now()`, `.elapsed()`, an SD read -- and `format_args!` borrows
// its operands, so evaluating it evaluates them: without the dead branch a
// telemetry-free build still pays for every timer read behind a line it
// never prints. A `false` condition is folded away before codegen while the
// body is still type-checked.
#[cfg(not(feature = "serial-log"))]
macro_rules! bench_log {
    ($($arg:tt)*) => {{
        if false {
            let _ = core::format_args!($($arg)*);
        }
    }};
}
