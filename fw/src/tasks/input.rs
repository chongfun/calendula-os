use crate::{PAGE_REQ, PageRequest, UI_CMD, UiCommand};
use embassy_time::Timer;
use esp_hal::gpio::Input;

#[embassy_executor::task]
pub async fn run(mut home_button: Input<'static>) {
    esp_println::println!("Input task started! (Listening on GPIO3 / Home Button)");
    loop {
        home_button.wait_for_falling_edge().await;

        // try_send so the input task never blocks on a full channel
        let _ = PAGE_REQ.try_send(PageRequest::NextPage);
        let _ = UI_CMD.try_send(UiCommand::RefreshFull);

        Timer::after_millis(250).await;
    }
}
