//! Device-driven bench scenarios for `fw-s3` (`bench-selftest` feature only,
//! kept out of production releases).
//!
//! Thin S3 wrapper over [`fw_common::bench_selftest`].

pub use fw_common::bench_selftest::{note_settled, publish_view, SETTLED, SLEEP_WAKE_AFTER};

#[embassy_executor::task]
pub async fn run() {
    fw_common::bench_selftest::run_with_config(
        fw_common::DisplayOrientation::PortraitButtonsLeft,
        crate::input::log_injected_input,
    )
    .await;
}
