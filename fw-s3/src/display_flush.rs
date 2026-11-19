use core::sync::atomic::{AtomicBool, Ordering};

use display::epd::{uc8279_x4, RefreshMode, SpiOp};
use display::fb::Framebuffer;
use embassy_time::{Instant, Timer};
use esp_hal::gpio::{Input, Output};
use esp_hal::spi::master::SpiDma;
use esp_hal::Async;
use hal_ext::spi_dma::{BusyError, EpdBus};

pub type Epd = EpdBus<
    SpiDma<'static, Async>,
    Output<'static>,
    Output<'static>,
    Input<'static>,
    Output<'static>,
>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelError {
    Spi(esp_hal::spi::Error),
    Busy(BusyError),
}

impl From<esp_hal::spi::Error> for PanelError {
    fn from(err: esp_hal::spi::Error) -> Self {
        Self::Spi(err)
    }
}

impl From<BusyError> for PanelError {
    fn from(err: BusyError) -> Self {
        Self::Busy(err)
    }
}

/// Number of gate lines packed into each DMA RAM chunk (`600 % 20 == 0`).
const CHUNK_ROWS: usize = 20;
const CHUNK_BYTES: usize = CHUNK_ROWS * uc8279_x4::ROW_BYTES;

static SCREEN_POWERED: AtomicBool = AtomicBool::new(false);
static OLD_PLANE_VALID: AtomicBool = AtomicBool::new(false);

pub async fn init_panel(epd: &mut Epd) -> Result<(), PanelError> {
    let start = Instant::now();
    esp_println::println!("s3-display: busy pre-reset high={:?}", epd.busy_is_high());
    epd.reset().await;
    Timer::after_millis(50).await;

    for op in uc8279_x4::INIT_SEQUENCE {
        match op {
            SpiOp::Reset => {
                epd.reset().await;
            }
            SpiOp::WaitBusy => {
                let _ = epd.wait_uc_idle_high().await?;
            }
            SpiOp::Command { cmd, data } => {
                epd.command(*cmd, data).await?;
            }
        }
    }

    SCREEN_POWERED.store(false, Ordering::Relaxed);
    OLD_PLANE_VALID.store(false, Ordering::Relaxed);
    esp_println::println!(
        "s3-display: uc8279-x4 init done busy_high={:?} t_ms={}",
        epd.busy_is_high(),
        start.elapsed().as_millis(),
    );
    Ok(())
}

pub async fn flush(
    epd: &mut Epd,
    fb: &Framebuffer,
    mode: RefreshMode,
    dark_background: bool,
    turn_off: bool,
) -> Result<(), PanelError> {
    let plan = uc8279_x4::flush_plan(
        mode,
        SCREEN_POWERED.load(Ordering::Relaxed),
        OLD_PLANE_VALID.load(Ordering::Relaxed),
        dark_background,
        turn_off,
    );

    // Mark the previous-frame plane invalid as soon as we start mutating RAM so
    // any mid-flush error leaves future flushes falling back to a Full pass.
    OLD_PLANE_VALID.store(false, Ordering::Relaxed);

    let mut last_cmd: Option<u8> = None;
    for step in plan.steps {
        match step {
            uc8279_x4::FlushStep::WritePlane { plane, source } => {
                stream_plane(epd, plane.cmd(), fb, *source).await?;
                last_cmd = None;
            }
            uc8279_x4::FlushStep::Command { cmd, data } => {
                epd.command(*cmd, data).await?;
                if *cmd == uc8279_x4::CMD_POWER_ON {
                    // Assume powered as soon as PON reaches the controller so a
                    // subsequent WaitBusy timeout still issues POF on sleep.
                    SCREEN_POWERED.store(true, Ordering::Relaxed);
                }
                last_cmd = Some(*cmd);
            }
            uc8279_x4::FlushStep::WaitBusy => {
                let busy_ms = match last_cmd {
                    Some(uc8279_x4::CMD_DISPLAY_REFRESH) => {
                        epd.wait_uc_idle_high_asserted().await?
                    }
                    _ => epd.wait_uc_idle_high().await?,
                };
                match last_cmd.take() {
                    Some(uc8279_x4::CMD_POWER_ON) => {
                        SCREEN_POWERED.store(true, Ordering::Relaxed);
                        esp_println::println!("s3-display: PON busy_ms={}", busy_ms);
                    }
                    Some(uc8279_x4::CMD_DISPLAY_REFRESH) => {
                        esp_println::println!(
                            "s3-display: DRF mode={:?} busy_ms={}",
                            plan.effective_mode,
                            busy_ms,
                        );
                        bench_log!(
                            "bench: refresh mode={:?} busy_ms={} busy_low=false elapsed_ms={} screen_on=true t_ms={}",
                            plan.effective_mode,
                            busy_ms,
                            busy_ms,
                            Instant::now().as_millis(),
                        );
                    }
                    Some(uc8279_x4::CMD_POWER_OFF) => {
                        SCREEN_POWERED.store(false, Ordering::Relaxed);
                        esp_println::println!("s3-display: POF busy_ms={}", busy_ms);
                    }
                    other => {
                        esp_println::println!(
                            "s3-display: wait cmd={:?} busy_ms={}",
                            other,
                            busy_ms,
                        );
                    }
                }
            }
        }
    }

    OLD_PLANE_VALID.store(true, Ordering::Relaxed);
    Ok(())
}

pub async fn sleep_panel(epd: &mut Epd) -> Result<(), PanelError> {
    for step in uc8279_x4::sleep_plan(SCREEN_POWERED.load(Ordering::Relaxed)) {
        match step {
            uc8279_x4::SleepStep::PowerOff => {
                epd.command(uc8279_x4::CMD_POWER_OFF, &[]).await?;
                let busy_ms = epd.wait_uc_idle_high().await?;
                SCREEN_POWERED.store(false, Ordering::Relaxed);
                esp_println::println!("s3-display: sleep POF busy_ms={}", busy_ms);
            }
            uc8279_x4::SleepStep::DeepSleep => {
                epd.command(uc8279_x4::CMD_DEEP_SLEEP, &[uc8279_x4::DEEP_SLEEP_CHECK])
                    .await?;
                OLD_PLANE_VALID.store(false, Ordering::Relaxed);
                esp_println::println!("s3-display: sleep DSLP sent");
            }
        }
    }
    Ok(())
}

async fn stream_plane(
    epd: &mut Epd,
    ram_cmd: u8,
    fb: &Framebuffer,
    source: uc8279_x4::FrameSource,
) -> Result<(), PanelError> {
    let mut chunk = [0u8; CHUNK_BYTES];
    if matches!(source, uc8279_x4::FrameSource::White) {
        chunk.fill(0xFF);
    }

    epd.begin_ram_write(ram_cmd).await?;
    let mut result = Ok(());
    let mut gate = 0u16;
    while gate < uc8279_x4::TRES_HEIGHT {
        let rows = usize::from((uc8279_x4::TRES_HEIGHT - gate).min(CHUNK_ROWS as u16));
        match source {
            uc8279_x4::FrameSource::Current | uc8279_x4::FrameSource::InvertedCurrent => {
                let invert = matches!(source, uc8279_x4::FrameSource::InvertedCurrent);
                let (row_chunks, _) = chunk.as_chunks_mut::<{ uc8279_x4::ROW_BYTES }>();
                for (row_idx, row_buf) in row_chunks.iter_mut().take(rows).enumerate() {
                    uc8279_x4::fill_gate_row_from_framebuffer(
                        fb,
                        gate + row_idx as u16,
                        invert,
                        row_buf,
                    );
                }
            }
            uc8279_x4::FrameSource::White => {}
        }

        let bytes = rows * uc8279_x4::ROW_BYTES;
        if let Some(slice) = chunk.get(..bytes) {
            if let Err(err) = epd.ram_chunk(slice).await {
                result = Err(PanelError::from(err));
                break;
            }
        }
        gate += rows as u16;
    }
    epd.end_ram_write();
    result
}
