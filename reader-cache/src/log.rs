//! The one firmware dependency the cache layer cannot shed, behind a feature.
//!
//! Everything else in this crate is host-buildable, which is the whole point of
//! it existing: the publish tail's failure choreography is what six review
//! rounds of B4 kept getting wrong, and it had no automated coverage because it
//! lived in `fw`. The logging is the only thing that genuinely needs the
//! target, so it is the only thing behind a feature flag. `fw` enables
//! `esp-log`; the host tests do not, and the macro expands to nothing.

#[cfg(feature = "esp-log")]
macro_rules! cache_log {
    ($($arg:tt)*) => { esp_println::println!($($arg)*) };
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
