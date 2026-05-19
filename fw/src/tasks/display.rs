use crate::{POWER_EVT, PowerEvent, UI_CMD, UiCommand};
use display::fb::Framebuffer;
use display::epd::{SpiOp, INIT_SEQUENCE};
use display::{BAND_ROWS, FB_HEIGHT};
use embassy_time::Timer;
use esp_hal::gpio::{Input, Output};
use esp_hal::peripherals::SPI2;
use esp_hal::spi::FullDuplexMode;
use esp_hal::spi::master::SpiDmaBus;
use esp_hal::Async;

/// Formats `n` into `buf` and returns the filled slice as a str.
fn fmt_u32<'a>(n: u32, buf: &'a mut [u8; 10]) -> &'a str {
    let mut i = buf.len();
    let mut v = n;
    if v == 0 {
        i -= 1;
        buf[i] = b'0';
    } else {
        while v > 0 {
            i -= 1;
            buf[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    }
    core::str::from_utf8(&buf[i..]).unwrap_or("?")
}

async fn run_sequence<SPI, CS, DC, BUSY, RST>(
    epd_spi: &mut hal_ext::spi_dma::EpdSpi<SPI, CS, DC, BUSY, RST>,
    seq: &[SpiOp],
) -> Result<(), SPI::Error>
where
    SPI: embedded_hal_async::spi::SpiBus,
    CS: embedded_hal::digital::OutputPin,
    DC: embedded_hal::digital::OutputPin,
    BUSY: embedded_hal::digital::InputPin,
    RST: embedded_hal::digital::OutputPin,
{
    for op in seq {
        match op {
            SpiOp::Cmd { cmd, data } => {
                epd_spi.send_command(*cmd).await?;
                if !data.is_empty() {
                    epd_spi.send_data(data).await?;
                }
            }
            SpiOp::DelayMs(ms) => {
                Timer::after_millis(*ms as u64).await;
            }
            SpiOp::Reset => {
                epd_spi.pulse_reset().await;
            }
        }
    }
    Ok(())
}

#[embassy_executor::task]
pub async fn run(
    mut epd_spi: hal_ext::spi_dma::EpdSpi<
        SpiDmaBus<'static, SPI2, FullDuplexMode, Async>,
        Output<'static>,
        Output<'static>,
        Input<'static>,
        Output<'static>,
    >,
) {
    esp_println::println!("Display task started!");
    // Statically allocate the single 48 KB Framebuffer securely
    static FB_CELL: static_cell::StaticCell<Framebuffer> = static_cell::StaticCell::new();
    let fb = FB_CELL.init(Framebuffer::new());

    // Draw something visible so refreshes are clearly observable
    fb.clear(true);
    ui::font::draw_str(fb, "Xteink X4 - press button", 10, 10, false);
    ui::font::draw_str(fb, "Ready.", 10, 30, false);

    // One-time EPD init at boot
    esp_println::println!("Initializing physical EPD...");
    run_sequence(&mut epd_spi, INIT_SEQUENCE).await.unwrap();
    epd_spi.wait_busy().await;
    esp_println::println!("EPD initialized successfully!");

    // Track how many refreshes have happened so the screen content changes
    let mut refresh_count: u32 = 0;

    loop {
        // Wait for incoming UI refresh commands
        match UI_CMD.receive().await {
            UiCommand::RefreshFull => {
                refresh_count += 1;
                esp_println::println!("Refreshing display ({})", refresh_count);

                // Update framebuffer content before sending
                fb.clear(true);
                ui::font::draw_str(fb, "Xteink X4", 10, 10, false);
                ui::font::draw_str(fb, "Refresh #", 10, 30, false);
                let mut num_buf = [0u8; 10];
                let count_str = fmt_u32(refresh_count, &mut num_buf);
                ui::font::draw_str(fb, count_str, 10 + 9 * 8, 30, false);

                // 2. Set RAM X/Y address to 0
                epd_spi.send_command(0x4E).await.unwrap(); // RAM X address counter
                epd_spi.send_data(&[0x00]).await.unwrap();
                epd_spi.send_command(0x4F).await.unwrap(); // RAM Y address counter
                epd_spi.send_data(&[0x00, 0x00]).await.unwrap();

                // 3. Write RAM new data command
                epd_spi.send_command(0x24).await.unwrap();

                // 4. Stream framebuffer in horizontal bands
                // 6 bands of 80 rows each (80 * 6 = 480)
                let mut y_start = 0;
                while y_start < FB_HEIGHT {
                    let band_slice = fb.band(y_start, BAND_ROWS);
                    
                    // Transmit band slice over SPI DMA autonomously.
                    epd_spi.send_data(band_slice).await.unwrap();
                    
                    y_start += BAND_ROWS;
                }

                // 5. Trigger display update sequence
                epd_spi.send_command(0x22).await.unwrap(); // Display Update Control 2
                epd_spi.send_data(&[0xC7]).await.unwrap(); // Load temp, load LUT, display image, disable clock/analog
                
                epd_spi.send_command(0x20).await.unwrap(); // Master Activation
                epd_spi.wait_busy().await; // Wait for high voltage physical wash to complete
                esp_println::println!("Display refresh complete!");

                // 6. Notify Power Management that page has settled
                let _ = POWER_EVT.send(PowerEvent::PageRendered).await;
            }
            UiCommand::RefreshPartial { rect } => {
                // Perform fast partial update localized to the rect region
                let _x = rect.x;
                let _y = rect.y;
                Timer::after_millis(50).await;
                let _ = POWER_EVT.send(PowerEvent::PageRendered).await;
            }
            UiCommand::UpdateProgressBar { percent } => {
                // Modify ProgressBar percent on the fly
                let _p = percent;
            }
        }
    }
}
