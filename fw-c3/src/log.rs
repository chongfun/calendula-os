//! High-frequency serial telemetry, gated behind the `serial-log` feature.
//!
//! The `bench:` lines and per-refresh driver chatter feed the bench harness
//! (`tools/bench/bench.py`). They still cost formatting and FIFO writes:
//! esp-println uses USB Serial/JTAG under a critical section, with bounded
//! waits if the host is disconnected. Output may be dropped when the FIFO
//! cannot drain; there is no fallback to the ROM UART flush path.
//! `serial-log` stays default-on for host-connected bench captures; a shipped
//! build can omit this chatter with `--no-default-features` (re-adding
//! `device-x3` and other needed features).
//!
//! Only telemetry belongs behind this macro. Error paths, panic/diagnostic
//! output, and boot-identity lines remain unconditional
//! `esp_println::println!` calls, though a connected host is needed to
//! reliably capture them.

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
