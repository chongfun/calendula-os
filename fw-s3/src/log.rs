//! High-frequency serial telemetry, gated behind the `serial-log` feature.
//!
//! The `bench:` lines feed the bench harness (`tools/bench/bench.py`).
//! Only telemetry belongs behind this macro; error paths and boot-identity
//! lines remain direct `esp_println::println!` calls.

#[cfg(feature = "serial-log")]
macro_rules! bench_log {
    ($($arg:tt)*) => { esp_println::println!($($arg)*) };
}

#[cfg(not(feature = "serial-log"))]
macro_rules! bench_log {
    ($($arg:tt)*) => {{
        if false {
            let _ = core::format_args!($($arg)*);
        }
    }};
}
