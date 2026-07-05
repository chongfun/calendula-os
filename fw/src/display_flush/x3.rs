//! UC8253 flush path for the Xteink X3 panel.

use super::Epd;
use display::epd::{
    fill_transformed_band, lut_for_mode, LutBank, RefreshMode, SpiOp, CMD_DATA_STOP,
    CMD_DEEP_SLEEP, CMD_DISPLAY_REFRESH, CMD_LUT_BB, CMD_LUT_BW, CMD_LUT_VCOM, CMD_LUT_WB,
    CMD_LUT_WW, CMD_POWER_OFF, CMD_POWER_ON, CMD_VCOM_DATA_INTERVAL, CMD_WRITE_RAM_NEW,
    CMD_WRITE_RAM_OLD, DEEP_SLEEP_CHECK, INIT_SEQUENCE,
};
use display::fb::Framebuffer;
use display::{BAND_BYTES, BAND_ROWS, FB_BYTES, HEIGHT};
use embassy_time::{Instant, Timer};
use esp_hal::spi::master::SpiDmaBus;
use esp_hal::Async;

pub(crate) async fn init_panel(epd: &mut Epd) {
    let start = Instant::now();
    let mut commands = 0u8;
    for op in INIT_SEQUENCE {
        match *op {
            SpiOp::Reset => {
                esp_println::println!("display: x3 init reset");
                epd.reset().await;
                // UC8253 needs a longer settle than SSD1677 before init.
                Timer::after_millis(30).await;
            }
            SpiOp::WaitBusy => wait_ready_x3(epd, "init").await,
            SpiOp::Command { cmd, data } => {
                epd.command(cmd, data).await.unwrap();
                commands = commands.saturating_add(1);
            }
        }
    }
    esp_println::println!(
        "display: x3 init configured commands={} {}ms",
        commands,
        start.elapsed().as_millis()
    );

    // UC8253 has no auto-fill command. Establish a known white old/new
    // baseline before the first differential refresh.
    let baseline_start = Instant::now();
    fill_white_plane(epd, CMD_WRITE_RAM_OLD).await.unwrap();
    epd.command(CMD_DATA_STOP, &[]).await.unwrap();
    fill_white_plane(epd, CMD_WRITE_RAM_NEW).await.unwrap();
    epd.command(CMD_DATA_STOP, &[]).await.unwrap();
    esp_println::println!(
        "display: x3 init white baseline {}ms total={}ms",
        baseline_start.elapsed().as_millis(),
        start.elapsed().as_millis()
    );
}

pub(crate) async fn flush(
    epd: &mut Epd,
    fb: &Framebuffer,
    _prev_fb: &Framebuffer,
    tx_band: &mut [u8; BAND_BYTES],
    screen_on: bool,
    mode: RefreshMode,
    _old_holds_prev: bool,
) -> Result<(), <SpiDmaBus<'static, Async> as embedded_hal_async::spi::ErrorType>::Error> {
    let flush_start = Instant::now();
    esp_println::println!("display: x3 flush {:?} screen_on={} start", mode, screen_on);

    let lut_start = Instant::now();
    let bank = lut_for_mode(mode);
    load_lut(epd, bank).await?;
    esp_println::println!(
        "display: x3 LUT {:?} {}ms",
        mode,
        lut_start.elapsed().as_millis()
    );

    if mode != RefreshMode::Fast {
        let old_start = Instant::now();
        fill_white_plane(epd, CMD_WRITE_RAM_OLD).await?;
        epd.command(CMD_DATA_STOP, &[]).await?;
        esp_println::println!(
            "display: x3 white old plane {}ms",
            old_start.elapsed().as_millis()
        );
    }
    let new_start = Instant::now();
    write_plane(epd, CMD_WRITE_RAM_NEW, fb, tx_band).await?;
    esp_println::println!(
        "display: x3 new plane {}ms",
        new_start.elapsed().as_millis()
    );

    // Full/clean updates intentionally re-power the pump, matching the
    // hardware-proven X3 driver. Fast updates keep it on between turns.
    if !screen_on || mode != RefreshMode::Fast {
        epd.command(CMD_POWER_ON, &[]).await?;
        wait_ready_x3(epd, "power-on").await;
    }
    epd.command(CMD_DISPLAY_REFRESH, &[]).await?;
    wait_ready_x3(epd, "refresh").await;

    if mode != RefreshMode::Fast {
        // The first differential immediately after a full waveform garbles
        // on real X3 panels. Sync DTM1, then spend a no-op fast refresh to
        // leave the controller in its proven post-fast state.
        write_plane(epd, CMD_WRITE_RAM_OLD, fb, tx_band).await?;
        load_lut(epd, lut_for_mode(RefreshMode::Fast)).await?;
        write_plane(epd, CMD_WRITE_RAM_NEW, fb, tx_band).await?;
        epd.command(CMD_DISPLAY_REFRESH, &[]).await?;
        wait_ready_x3(epd, "post-full settle").await;
    }

    esp_println::println!(
        "display: x3 flush {:?} complete {}ms",
        mode,
        flush_start.elapsed().as_millis()
    );
    Ok(())
}

pub(crate) async fn prestage_red(
    epd: &mut Epd,
    fb: &Framebuffer,
    tx_band: &mut [u8; BAND_BYTES],
) -> Result<(), <SpiDmaBus<'static, Async> as embedded_hal_async::spi::ErrorType>::Error> {
    write_plane(epd, CMD_WRITE_RAM_OLD, fb, tx_band).await?;
    epd.command(CMD_DATA_STOP, &[]).await
}

pub(crate) async fn sleep_panel(
    epd: &mut Epd,
) -> Result<(), <SpiDmaBus<'static, Async> as embedded_hal_async::spi::ErrorType>::Error> {
    epd.command(CMD_POWER_OFF, &[]).await?;
    wait_ready_x3(epd, "power-off").await;
    epd.command(CMD_DEEP_SLEEP, &[DEEP_SLEEP_CHECK]).await
}

async fn wait_ready_x3(epd: &mut Epd, phase: &str) {
    let outcome = epd.wait_ready(display::board::BUSY_ACTIVE_HIGH).await;
    esp_println::println!(
        "display: x3 BUSY {} initial={:?} assertion={:?}/{}ms release={:?}/{}ms final={:?}",
        phase,
        outcome.initial_active,
        outcome.assertion,
        outcome.assertion_ms,
        outcome.release,
        outcome.release_ms,
        outcome.final_active,
    );
}

async fn load_lut(
    epd: &mut Epd,
    bank: &LutBank,
) -> Result<(), <SpiDmaBus<'static, Async> as embedded_hal_async::spi::ErrorType>::Error> {
    epd.command(CMD_VCOM_DATA_INTERVAL, &bank.cdi).await?;
    epd.command(CMD_LUT_VCOM, &bank.vcom).await?;
    epd.command(CMD_LUT_WW, &bank.ww).await?;
    epd.command(CMD_LUT_BW, &bank.bw).await?;
    epd.command(CMD_LUT_WB, &bank.wb).await?;
    epd.command(CMD_LUT_BB, &bank.bb).await
}

async fn fill_white_plane(
    epd: &mut Epd,
    command: u8,
) -> Result<(), <SpiDmaBus<'static, Async> as embedded_hal_async::spi::ErrorType>::Error> {
    const WHITE: [u8; 256] = [0xFF; 256];
    epd.begin_ram_write(command).await?;
    let mut remaining = FB_BYTES;
    let mut result = Ok(());
    while remaining > 0 {
        let len = remaining.min(WHITE.len());
        if let Err(err) = epd.ram_chunk(&WHITE[..len]).await {
            result = Err(err);
            break;
        }
        remaining -= len;
    }
    epd.end_ram_write();
    result
}

async fn write_plane(
    epd: &mut Epd,
    command: u8,
    fb: &Framebuffer,
    tx_band: &mut [u8; BAND_BYTES],
) -> Result<(), <SpiDmaBus<'static, Async> as embedded_hal_async::spi::ErrorType>::Error> {
    epd.begin_ram_write(command).await?;
    let mut y = 0;
    let mut result = Ok(());
    while y < HEIGHT {
        let len = fill_transformed_band(fb, y, tx_band);
        if let Err(err) = epd.ram_chunk(&tx_band[..len]).await {
            result = Err(err);
            break;
        }
        y += BAND_ROWS;
    }
    epd.end_ram_write();
    result
}
