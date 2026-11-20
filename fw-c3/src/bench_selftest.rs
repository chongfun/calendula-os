//! Device-driven bench scenarios (`bench-selftest` feature only, kept out of
//! release builds).
//!
//! Thin C3 wrapper over [`fw_common::bench_selftest`].

pub use fw_common::bench_selftest::{note_settled, publish_view, SETTLED, SLEEP_WAKE_AFTER};

#[embassy_executor::task]
pub async fn run() {
    fw_common::bench_selftest::run_with_config(
        crate::DisplayOrientation::LandscapeButtonsBottom,
        crate::tasks::input::log_injected_input,
    )
    .await;
}
