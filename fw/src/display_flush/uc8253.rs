//! UC8253 flush backend — Xteink X3. Ported from CrossPoint's production
//! `Uc8253X3Driver` (MIT), BW path only (no grayscale reader here).
//!
//! Plane model: DTM1 holds the "old" frame, DTM2 the "new". A `Fast` turn
//! diffs DTM2 against DTM1 with the turbo bank; `Full` writes a white-DTM1
//! baseline then a settle pass; `FastClean` scrubs every pixel to target
//! with the absolute half bank. As on the X4, `prestage_previous` stages
//! the just-shown frame into the old plane (DTM1) off the page-turn
//! critical path, so the next `Fast` finds its diff base already loaded —
//! `flush`'s `prev_staged` says whether that happened.
//!
//! The two-phase BUSY timing, panel orientation, ported waveforms,
//! and SPI clock are hardware-verified on the X3.

use super::{Epd, PanelError};
use display::epd::uc8253::{
    bank_for, flush_plan, partial_flush_plan, partial_window_data, sleep_plan, FlushStep,
    FrameSource, LutBank, RamPlane, SleepStep, CDI_INTERVAL, CMD_DATA_STOP, CMD_DEEP_SLEEP,
    CMD_DISPLAY_REFRESH, CMD_DTM1, CMD_DTM2, CMD_LUT_BB, CMD_LUT_BW, CMD_LUT_VCOM, CMD_LUT_WB,
    CMD_LUT_WW, CMD_PARTIAL_IN, CMD_PARTIAL_OUT, CMD_PARTIAL_WINDOW, CMD_POWER_OFF, CMD_POWER_ON,
    CMD_VCOM_DATA_INTERVAL, DEEP_SLEEP_CHECK, INIT_SEQUENCE, PARTIAL_PRESTAGE_STEPS,
    PRESTAGE_STEPS,
};
use display::epd::{RefreshMode, SpiOp};
use display::fb::Framebuffer;
use display::{Rect, BAND_ROWS, HEIGHT, ROW_BYTES};
use embassy_time::{Instant, Timer};
// riscv32imc has no CAS; portable-atomic provides plain load/store here.
use portable_atomic::{AtomicBool, Ordering};

/// Mirrors the controller's charge-pump state (`_isScreenOn`). The panel is
/// left powered between page turns for speed; only `sleep_panel` powers it
/// down. `init_panel` resets it to off. Single writer (the display task).
static SCREEN_POWERED: AtomicBool = AtomicBool::new(false);

pub(crate) async fn init_panel(epd: &mut Epd) -> Result<(), PanelError> {
    // Bring-up probe: a live UC8253 twitches BUSY across a hardware reset;
    // a line that reads high at every sample never left power-up, which is
    // a board/RST problem, not a command-stream one.
    esp_println::println!("display: x3 busy pre-reset high={:?}", epd.busy_is_high());
    epd.reset().await;
    esp_println::println!("display: x3 busy post-reset high={:?}", epd.busy_is_high());
    // The X3 needs an extra settle after reset beyond the shared pulse.
    Timer::after_millis(50).await;

    for op in INIT_SEQUENCE {
        if let SpiOp::Command { cmd, data } = *op {
            epd.command(cmd, data).await?;
        }
    }
    esp_println::println!("display: x3 busy post-init high={:?}", epd.busy_is_high());

    // UC8253 has no auto RAM clear; whiten both planes so the first
    // differential diffs against white rather than power-on garbage.
    fill_plane(epd, CMD_DTM1, 0xFF).await?;
    epd.command(CMD_DATA_STOP, &[]).await?;
    fill_plane(epd, CMD_DTM2, 0xFF).await?;
    epd.command(CMD_DATA_STOP, &[]).await?;

    SCREEN_POWERED.store(false, Ordering::Relaxed);
    esp_println::println!("display: x3 init done t_ms={}", Instant::now().as_millis());
    Ok(())
}

pub(crate) async fn flush(
    epd: &mut Epd,
    fb: &Framebuffer,
    prev_fb: &Framebuffer,
    _screen_on: bool,
    mode: RefreshMode,
    prev_staged: bool,
) -> Result<(), PanelError> {
    let plan = flush_plan(mode, SCREEN_POWERED.load(Ordering::Relaxed), prev_staged);
    bench_log!(
        "display: x3 flush requested={:?} effective={:?}",
        plan.requested_mode,
        plan.effective_mode
    );
    execute_steps(epd, fb, prev_fb, plan.effective_mode, plan.steps, None).await?;
    Ok(())
}

pub(crate) async fn flush_window(
    epd: &mut Epd,
    fb: &Framebuffer,
    prev_fb: &Framebuffer,
    _screen_on: bool,
    _mode: RefreshMode,
    rect: Rect,
    prev_staged: bool,
) -> Result<(), PanelError> {
    let plan = partial_flush_plan(SCREEN_POWERED.load(Ordering::Relaxed), prev_staged);
    let window_bytes = (rect.w as usize / 8) * rect.h as usize;
    let plane_writes = plan
        .steps
        .iter()
        .filter(|s| matches!(s, FlushStep::WriteWindowPlane { .. }))
        .count();
    let ram_bytes = window_bytes * plane_writes;
    let flush_start = Instant::now();
    let busy_ms = execute_steps(
        epd,
        fb,
        prev_fb,
        plan.effective_mode,
        plan.steps,
        Some(rect),
    )
    .await?;
    let elapsed_ms = flush_start.elapsed().as_millis();
    bench_log!(
        "bench: partial_flush mode={:?} rect=(x={},y={},w={},h={}) window_bytes={} ram_bytes={} busy_ms={} elapsed_ms={} t_ms={}",
        plan.effective_mode,
        rect.x,
        rect.y,
        rect.w,
        rect.h,
        window_bytes,
        ram_bytes,
        busy_ms.unwrap_or(0),
        elapsed_ms,
        Instant::now().as_millis(),
    );
    Ok(())
}

/// Stage the just-shown frame into DTM1 ("old" RAM) so the next fast turn's
/// diff base is loaded off the critical path. The X4's `prestage_previous`
/// analogue (RED RAM there, DTM1 here).
pub(crate) async fn prestage_previous(epd: &mut Epd, fb: &Framebuffer) -> Result<(), PanelError> {
    execute_steps(epd, fb, fb, RefreshMode::Fast, PRESTAGE_STEPS, None).await?;
    Ok(())
}

pub(crate) async fn prestage_previous_window(
    epd: &mut Epd,
    fb: &Framebuffer,
    rect: Rect,
) -> Result<(), PanelError> {
    execute_steps(
        epd,
        fb,
        fb,
        RefreshMode::Fast,
        PARTIAL_PRESTAGE_STEPS,
        Some(rect),
    )
    .await?;
    Ok(())
}

pub(crate) async fn sleep_panel(epd: &mut Epd) -> Result<(), PanelError> {
    let start = Instant::now();
    bench_log!(
        "bench: sleep phase=power_down_start t_ms={}",
        start.as_millis()
    );
    for step in sleep_plan(SCREEN_POWERED.load(Ordering::Relaxed)) {
        match step {
            SleepStep::PowerOff => power_off(epd, start).await?,
            SleepStep::DeepSleep => {
                epd.command(CMD_DEEP_SLEEP, &[DEEP_SLEEP_CHECK]).await?;
                bench_log!(
                    "bench: sleep phase=deep_sleep elapsed_ms={} t_ms={}",
                    start.elapsed().as_millis(),
                    Instant::now().as_millis()
                );
            }
        }
    }
    Ok(())
}

async fn execute_steps(
    epd: &mut Epd,
    fb: &Framebuffer,
    prev_fb: &Framebuffer,
    mode: RefreshMode,
    steps: &[FlushStep],
    window: Option<Rect>,
) -> Result<Option<u64>, PanelError> {
    let mut busy_ms = None;
    for step in steps {
        match *step {
            FlushStep::LoadBank(mode) => {
                let (bank, cdi) = bank_for(mode);
                load_bank(epd, cdi, bank).await?;
            }
            FlushStep::WritePlane { plane, source } => {
                let command = match plane {
                    RamPlane::Old => CMD_DTM1,
                    RamPlane::New => CMD_DTM2,
                };
                match source {
                    FrameSource::White => fill_plane(epd, command, 0xFF).await?,
                    FrameSource::Current => send_plane(epd, command, fb).await?,
                    FrameSource::Previous => send_plane(epd, command, prev_fb).await?,
                }
            }
            FlushStep::WriteWindowPlane { plane, source } => {
                let command = match plane {
                    RamPlane::Old => CMD_DTM1,
                    RamPlane::New => CMD_DTM2,
                };
                let rect = window.unwrap_or(Rect::FULL);
                match source {
                    FrameSource::White => fill_plane(epd, command, 0xFF).await?,
                    FrameSource::Current => send_window_plane(epd, command, fb, rect).await?,
                    FrameSource::Previous => send_window_plane(epd, command, prev_fb, rect).await?,
                }
            }
            FlushStep::SetPartialWindow => {
                epd.command(CMD_PARTIAL_IN, &[]).await?;
                let win = partial_window_data(window.unwrap_or(Rect::FULL));
                epd.command(CMD_PARTIAL_WINDOW, &win).await?;
            }
            FlushStep::PartialOut => {
                epd.command(CMD_PARTIAL_OUT, &[]).await?;
            }
            FlushStep::DataStop => epd.command(CMD_DATA_STOP, &[]).await?,
            FlushStep::PowerOn => power_on(epd).await?,
            FlushStep::DisplayRefresh => {
                let ms = display_refresh(epd, mode).await?;
                busy_ms = Some(ms);
            }
            FlushStep::DelayMs(ms) => Timer::after_millis(u64::from(ms)).await,
        }
    }
    Ok(busy_ms)
}

async fn power_on(epd: &mut Epd) -> Result<(), PanelError> {
    epd.command(CMD_POWER_ON, &[]).await?;
    let ms = epd.wait_two_phase().await?;
    bench_log!(
        "display: x3 PON busy_low=true {}ms level_high={:?}",
        ms,
        epd.busy_is_high()
    );
    SCREEN_POWERED.store(true, Ordering::Relaxed);
    Ok(())
}

async fn display_refresh(epd: &mut Epd, mode: RefreshMode) -> Result<u64, PanelError> {
    let start = Instant::now();
    epd.command(CMD_DISPLAY_REFRESH, &[]).await?;
    let ms = epd.wait_two_phase().await?;
    bench_log!(
        "display: x3 DRF busy_low=true {}ms level_high={:?}",
        ms,
        epd.busy_is_high()
    );
    bench_log!(
        "bench: refresh mode={:?} busy_ms={} busy_low={} elapsed_ms={} screen_on={} t_ms={}",
        mode,
        ms,
        true,
        start.elapsed().as_millis(),
        SCREEN_POWERED.load(Ordering::Relaxed),
        Instant::now().as_millis(),
    );
    Ok(ms)
}

async fn power_off(epd: &mut Epd, start: Instant) -> Result<(), PanelError> {
    epd.command(CMD_POWER_OFF, &[]).await?;
    let ms = epd.wait_two_phase().await?;
    bench_log!("display: x3 POF busy_low=true {}ms", ms);
    bench_log!(
        "bench: sleep phase=power_off busy_ms={} busy_low={} elapsed_ms={} t_ms={}",
        ms,
        true,
        start.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
    SCREEN_POWERED.store(false, Ordering::Relaxed);
    Ok(())
}

async fn load_bank(epd: &mut Epd, cdi0: u8, bank: &LutBank) -> Result<(), PanelError> {
    epd.command(CMD_VCOM_DATA_INTERVAL, &[cdi0, CDI_INTERVAL])
        .await?;
    epd.command(CMD_LUT_VCOM, bank.vcom).await?;
    epd.command(CMD_LUT_WW, bank.ww).await?;
    epd.command(CMD_LUT_BW, bank.bw).await?;
    epd.command(CMD_LUT_WB, bank.wb).await?;
    epd.command(CMD_LUT_BB, bank.bb).await?;
    Ok(())
}

/// Stream one already-panel-native framebuffer into a RAM plane, band by band.
///
/// Deliberately does NOT send DATA_STOP: the hardware-proven reference
/// never puts one between a DTM2 write and the refresh that displays it —
/// only after DTM1 syncs and white fills. Callers add it where the
/// reference does.
async fn send_plane(epd: &mut Epd, ram_cmd: u8, fb: &Framebuffer) -> Result<(), PanelError> {
    epd.begin_ram_write(ram_cmd).await?;
    let mut y = 0;
    let mut result = Ok(());
    while y < HEIGHT {
        let band = fb.band(y, BAND_ROWS);
        if let Err(err) = epd.ram_chunk(band).await {
            result = Err(err);
            break;
        }
        y += BAND_ROWS;
    }
    epd.end_ram_write();
    Ok(result?)
}

/// Stream only the dirty rectangle's rows and byte columns into a RAM plane.
async fn send_window_plane(
    epd: &mut Epd,
    ram_cmd: u8,
    fb: &Framebuffer,
    rect: Rect,
) -> Result<(), PanelError> {
    epd.begin_ram_write(ram_cmd).await?;
    let min_bx = (ROW_BYTES - 1) - ((rect.x + rect.w) / 8 - 1) as usize;
    let max_bx = (ROW_BYTES - 1) - (rect.x / 8) as usize;
    let mut result = Ok(());
    for y in (rect.y as usize)..((rect.y + rect.h) as usize) {
        let row_start = y * ROW_BYTES;
        let slice = &fb.bytes()[row_start + min_bx..=row_start + max_bx];
        if let Err(err) = epd.ram_chunk(slice).await {
            result = Err(err);
            break;
        }
    }
    epd.end_ram_write();
    Ok(result?)
}

/// Fill a RAM plane with a constant byte (0xFF = white), row by row.
///
/// Deliberately does NOT send DATA_STOP, for the same reason as `send_plane`
/// above: callers add it where the reference driver does.
async fn fill_plane(epd: &mut Epd, ram_cmd: u8, fill: u8) -> Result<(), PanelError> {
    let mut row = [0u8; ROW_BYTES];
    row.fill(fill);
    epd.begin_ram_write(ram_cmd).await?;
    let mut result = Ok(());
    for _ in 0..HEIGHT {
        if let Err(err) = epd.ram_chunk(&row).await {
            result = Err(err);
            break;
        }
    }
    epd.end_ram_write();
    Ok(result?)
}
