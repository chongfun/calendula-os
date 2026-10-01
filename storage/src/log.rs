//! Serial output, routed at esp-println only when `fw` asks for it.
//!
//! Both macros type-check their arguments in every feature state, the way
//! reader-cache's `cache_log!` does, so a wrong placeholder fails the host
//! build rather than the first firmware build. The `if false` keeps a disabled
//! line from evaluating operands such as `Instant::now()`.

/// Errors, boot identity and storage narration: always on in firmware.
#[cfg(feature = "esp-log")]
macro_rules! slog {
    ($($arg:tt)*) => { esp_println::println!($($arg)*) };
}

#[cfg(not(feature = "esp-log"))]
macro_rules! slog {
    ($($arg:tt)*) => {{
        if false {
            let _ = core::format_args!($($arg)*);
        }
    }};
}

/// `bench:` telemetry, which `fw`'s `serial-log` feature can turn off.
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
