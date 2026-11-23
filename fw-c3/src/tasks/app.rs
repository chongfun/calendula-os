//! Concrete Embassy wrapper; orchestration is shared with the S3 executable.
#[embassy_executor::task]
pub async fn run(wake_into_reading: bool) {
    let ssid =
        crate::tasks::wifi::credentials().and_then(|(ssid, _)| app_core::WifiSsid::new(ssid));
    fw_common::app::run_with_options(ssid, false, wake_into_reading).await;
}
