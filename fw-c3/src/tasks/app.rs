//! Concrete Embassy wrapper; orchestration is shared with the S3 executable.
#[embassy_executor::task]
pub async fn run() {
    let ssid =
        crate::tasks::wifi::credentials().and_then(|(ssid, _)| app_core::WifiSsid::new(ssid));
    fw_common::app::run(ssid).await;
}
