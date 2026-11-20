//! Cache narration through the MCU-neutral runtime line sink.
//! `esp-log` is retained as a feature name for callers; it no longer selects
//! a chip or an Espressif dependency. Firmware installs the sink at boot.

#[cfg(feature = "esp-log")]
macro_rules! cache_log {
    ($($arg:tt)*) => { fw_common::runtime_log!($($arg)*) };
}

// A no-op at runtime, but still a full type-check at compile time: the tokens go
// through `core::format_args!`, so a wrong placeholder count or an argument
// whose type has no `Display` is an error on the *host* build rather than a
// surprise on the first firmware build. Referencing the arguments alone was not
// enough for that -- it kept them "used" but never checked them against the
// format string.
#[cfg(not(feature = "esp-log"))]
macro_rules! cache_log {
    ($($arg:tt)*) => {{
        let _ = core::format_args!($($arg)*);
    }};
}
