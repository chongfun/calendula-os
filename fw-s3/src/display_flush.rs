use display::epd::probe::X4ControllerVariant;
use display::epd::{ssd1677, uc8279_x4, RefreshMode, SpiOp};
use display::fb::Framebuffer;
use display::Rect;
use embassy_time::{Instant, Timer};
use esp_hal::gpio::{Input, Output};
use esp_hal::spi::master::SpiDma;
use esp_hal::Async;
use hal_ext::spi_dma::{BusyError, EpdBus};

const _: () = assert!(display::WIDTH == 800 && display::HEIGHT == 480);

type Bus = EpdBus<
    SpiDma<'static, Async>,
    Output<'static>,
    Output<'static>,
    Input<'static>,
    Output<'static>,
>;

/// Owned panel bus and runtime controller state for the X4 Pro display task.
pub struct Epd {
    bus: Bus,
    variant: X4ControllerVariant,
    screen_powered: bool,
    old_plane_valid: bool,
    quick_fast_streak: u8,
}

impl Epd {
    pub fn new(
        spi: SpiDma<'static, Async>,
        cs: Output<'static>,
        dc: Output<'static>,
        busy: Input<'static>,
        rst: Output<'static>,
        variant: X4ControllerVariant,
    ) -> Self {
        Self {
            bus: EpdBus::new(spi, cs, dc, busy, rst),
            variant,
            screen_powered: false,
            old_plane_valid: false,
            quick_fast_streak: 0,
        }
    }

    pub const fn variant(&self) -> X4ControllerVariant {
        self.variant
    }

    pub fn rst_mut(&mut self) -> &mut Output<'static> {
        self.bus.rst_mut()
    }
}

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

/// Number of rows packed into each DMA RAM chunk (`480 % 20 == 0` and
/// `600 % 20 == 0`; `20 * 100 = 2000 <= 4092` DMA TX buffer).
const CHUNK_ROWS: usize = 20;
const CHUNK_BYTES: usize = CHUNK_ROWS * uc8279_x4::ROW_BYTES;

pub async fn init_panel(epd: &mut Epd) -> Result<(), PanelError> {
    match epd.variant() {
        X4ControllerVariant::Ssd1677 => init_ssd1677(epd).await,
        X4ControllerVariant::Uc8279X4 | X4ControllerVariant::Uc8179 => init_uc8279(epd).await,
    }
}

async fn init_ssd1677(epd: &mut Epd) -> Result<(), PanelError> {
    let start = Instant::now();
    esp_println::println!(
        "s3-display: ssd1677 busy pre-reset high={:?}",
        epd.bus.busy_is_high()
    );

    for op in ssd1677::INIT_SEQUENCE {
        match *op {
            SpiOp::Reset => epd.bus.reset().await,
            SpiOp::WaitBusy => epd.bus.wait_ready().await?,
            SpiOp::Command { cmd, data } => epd.bus.command(cmd, data).await?,
        }
    }

    epd.screen_powered = false;
    epd.old_plane_valid = false;
    epd.quick_fast_streak = 0;
    esp_println::println!(
        "s3-display: ssd1677 init done busy_high={:?} t_ms={}",
        epd.bus.busy_is_high(),
        start.elapsed().as_millis(),
    );
    Ok(())
}

async fn init_uc8279(epd: &mut Epd) -> Result<(), PanelError> {
    let start = Instant::now();
    esp_println::println!(
        "s3-display: busy pre-reset high={:?}",
        epd.bus.busy_is_high()
    );
    epd.bus.reset().await;
    Timer::after_millis(50).await;

    for op in uc8279_x4::INIT_SEQUENCE {
        match op {
            SpiOp::Reset => {
                epd.bus.reset().await;
            }
            SpiOp::WaitBusy => {
                let _ = epd.bus.wait_uc_idle_high().await?;
            }
            SpiOp::Command { cmd, data } => {
                epd.bus.command(*cmd, data).await?;
            }
        }
    }

    epd.screen_powered = false;
    epd.old_plane_valid = false;
    epd.quick_fast_streak = 0;
    esp_println::println!(
        "s3-display: uc8279-x4 init done busy_high={:?} t_ms={}",
        epd.bus.busy_is_high(),
        start.elapsed().as_millis(),
    );
    Ok(())
}

pub async fn flush(
    epd: &mut Epd,
    fb: &Framebuffer,
    prev_fb: &Framebuffer,
    mode: RefreshMode,
    dark_background: bool,
    turn_off: bool,
) -> Result<(), PanelError> {
    match epd.variant() {
        X4ControllerVariant::Ssd1677 => flush_ssd1677(epd, fb, prev_fb, mode, turn_off).await,
        X4ControllerVariant::Uc8279X4 | X4ControllerVariant::Uc8179 => {
            flush_uc8279(epd, fb, mode, dark_background, turn_off).await
        }
    }
}

async fn flush_ssd1677(
    epd: &mut Epd,
    fb: &Framebuffer,
    prev_fb: &Framebuffer,
    mode: RefreshMode,
    turn_off: bool,
) -> Result<(), PanelError> {
    if mode == RefreshMode::PowerDown {
        epd.old_plane_valid = false;
        epd.quick_fast_streak = 0;
        if epd.screen_powered {
            epd.bus
                .command(
                    ssd1677::CMD_DISPLAY_UPDATE_CTRL2,
                    &[ssd1677::update_control_2(
                        RefreshMode::PowerDown,
                        true,
                        false,
                    )],
                )
                .await?;
            epd.bus.command(ssd1677::CMD_MASTER_ACTIVATION, &[]).await?;
            epd.bus.wait_ready().await?;
            epd.screen_powered = false;
        }
        return Ok(());
    }

    let effective_mode =
        ssd1677::effective_refresh_mode(mode, epd.screen_powered, epd.old_plane_valid);
    let was_old_plane_valid = epd.old_plane_valid;

    // Invalidate RED-plane tracking before mutating RAM so any mid-flush error
    // forces the next flush not to assume the previous frame is staged.
    epd.old_plane_valid = false;
    if effective_mode != RefreshMode::Fast {
        epd.quick_fast_streak = 0;
    }

    write_ssd1677_ram(epd, ssd1677::CMD_WRITE_RAM_BW, fb).await?;
    if effective_mode == RefreshMode::Fast {
        if !was_old_plane_valid {
            write_ssd1677_ram(epd, ssd1677::CMD_WRITE_RAM_RED, prev_fb).await?;
        }
    } else {
        write_ssd1677_ram(epd, ssd1677::CMD_WRITE_RAM_RED, fb).await?;
    }

    if effective_mode == RefreshMode::FastClean {
        epd.bus
            .command(
                ssd1677::CMD_WRITE_TEMPERATURE,
                &ssd1677::FAST_CLEAN_TEMPERATURE,
            )
            .await?;
    }

    let ctrl2 = ssd1677::update_control_2(effective_mode, epd.screen_powered, turn_off);
    epd.bus
        .command(
            ssd1677::CMD_DISPLAY_UPDATE_CTRL1,
            &ssd1677::update_control_1(effective_mode),
        )
        .await?;
    epd.bus
        .command(ssd1677::CMD_DISPLAY_UPDATE_CTRL2, &[ctrl2])
        .await?;
    epd.bus.command(ssd1677::CMD_MASTER_ACTIVATION, &[]).await?;
    if !epd.screen_powered {
        // Assume powered once activation reaches the controller so a WaitBusy
        // error still issues PowerDown during sleep.
        epd.screen_powered = true;
    }

    let start = Instant::now();
    epd.bus.wait_ready().await?;
    let busy_ms = start.elapsed().as_millis();
    epd.screen_powered = !turn_off;

    if effective_mode == RefreshMode::FastClean {
        // Restore sensed ambient temperature after the 90 C FastClean override.
        // Preserve clock-off when `turn_off` requested disabling analog/clock.
        let restore_ctrl2 = ssd1677::UPDATE_SEQUENCE_LOAD_TEMP | u8::from(turn_off);
        epd.bus
            .command(ssd1677::CMD_DISPLAY_UPDATE_CTRL2, &[restore_ctrl2])
            .await?;
        epd.bus.command(ssd1677::CMD_MASTER_ACTIVATION, &[]).await?;
        epd.bus.wait_ready().await?;
    }

    if effective_mode == RefreshMode::Fast {
        epd.quick_fast_streak = epd.quick_fast_streak.saturating_add(1);
        epd.old_plane_valid = false;
    } else {
        epd.quick_fast_streak = 0;
        epd.old_plane_valid = true;
    }

    bench_log!(
        "s3-display: ssd1677 refresh mode={:?} busy_ms={} streak={}",
        effective_mode,
        busy_ms,
        epd.quick_fast_streak,
    );
    bench_log!(
        "bench: refresh mode={:?} busy_ms={} busy_low=false elapsed_ms={} screen_on={} t_ms={}",
        effective_mode,
        busy_ms,
        busy_ms,
        epd.screen_powered,
        Instant::now().as_millis(),
    );
    Ok(())
}

async fn flush_uc8279(
    epd: &mut Epd,
    fb: &Framebuffer,
    mode: RefreshMode,
    dark_background: bool,
    turn_off: bool,
) -> Result<(), PanelError> {
    let plan = uc8279_x4::flush_plan(
        mode,
        epd.screen_powered,
        epd.old_plane_valid,
        dark_background,
        turn_off,
    );

    // Mark the previous-frame plane invalid as soon as we start mutating RAM so
    // any mid-flush error leaves future flushes falling back to a Full pass.
    epd.old_plane_valid = false;
    epd.quick_fast_streak = 0;

    let mut last_cmd: Option<u8> = None;
    for step in plan.steps {
        match step {
            uc8279_x4::FlushStep::WritePlane { plane, source } => {
                stream_uc8279_plane(epd, plane.cmd(), fb, *source).await?;
                last_cmd = None;
            }
            uc8279_x4::FlushStep::Command { cmd, data } => {
                epd.bus.command(*cmd, data).await?;
                if *cmd == uc8279_x4::CMD_POWER_ON {
                    // Assume powered as soon as PON reaches the controller so a
                    // subsequent WaitBusy timeout still issues POF on sleep.
                    epd.screen_powered = true;
                }
                last_cmd = Some(*cmd);
            }
            uc8279_x4::FlushStep::WaitBusy => {
                let busy_ms = match last_cmd {
                    Some(uc8279_x4::CMD_DISPLAY_REFRESH) => {
                        epd.bus.wait_uc_idle_high_asserted().await?
                    }
                    _ => epd.bus.wait_uc_idle_high().await?,
                };
                match last_cmd.take() {
                    Some(uc8279_x4::CMD_POWER_ON) => {
                        epd.screen_powered = true;
                        bench_log!("s3-display: PON busy_ms={}", busy_ms);
                    }
                    Some(uc8279_x4::CMD_DISPLAY_REFRESH) => {
                        bench_log!(
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
                        epd.screen_powered = false;
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

    epd.old_plane_valid = true;
    Ok(())
}

/// Stage the settled frame into the controller's previous-frame RAM plane off
/// the page-turn critical path so the next `Fast` refresh only streams the new
/// plane.
pub async fn prestage_frame(epd: &mut Epd, fb: &Framebuffer) -> Result<(), PanelError> {
    if epd.old_plane_valid {
        return Ok(());
    }
    match epd.variant() {
        X4ControllerVariant::Ssd1677 => {
            write_ssd1677_ram(epd, ssd1677::CMD_WRITE_RAM_RED, fb).await?;
            epd.old_plane_valid = true;
            Ok(())
        }
        X4ControllerVariant::Uc8279X4 | X4ControllerVariant::Uc8179 => {
            stream_uc8279_plane(
                epd,
                uc8279_x4::RamPlane::Old.cmd(),
                fb,
                uc8279_x4::FrameSource::Current,
            )
            .await?;
            epd.old_plane_valid = true;
            Ok(())
        }
    }
}

pub async fn sleep_panel(epd: &mut Epd) -> Result<(), PanelError> {
    match epd.variant() {
        X4ControllerVariant::Ssd1677 => sleep_ssd1677(epd).await,
        X4ControllerVariant::Uc8279X4 | X4ControllerVariant::Uc8179 => sleep_uc8279(epd).await,
    }
}

async fn sleep_ssd1677(epd: &mut Epd) -> Result<(), PanelError> {
    if epd.screen_powered {
        epd.bus
            .command(
                ssd1677::CMD_DISPLAY_UPDATE_CTRL2,
                &[ssd1677::update_control_2(
                    RefreshMode::PowerDown,
                    true,
                    false,
                )],
            )
            .await?;
        epd.bus.command(ssd1677::CMD_MASTER_ACTIVATION, &[]).await?;
        epd.bus.wait_ready().await?;
        epd.screen_powered = false;
    }
    epd.bus
        .command(ssd1677::CMD_DEEP_SLEEP, &[ssd1677::DEEP_SLEEP_MODE1])
        .await?;
    epd.screen_powered = false;
    epd.old_plane_valid = false;
    epd.quick_fast_streak = 0;
    esp_println::println!("s3-display: ssd1677 deep sleep sent");
    Ok(())
}

async fn sleep_uc8279(epd: &mut Epd) -> Result<(), PanelError> {
    for step in uc8279_x4::sleep_plan(epd.screen_powered) {
        match step {
            uc8279_x4::SleepStep::PowerOff => {
                epd.bus.command(uc8279_x4::CMD_POWER_OFF, &[]).await?;
                let busy_ms = epd.bus.wait_uc_idle_high().await?;
                epd.screen_powered = false;
                esp_println::println!("s3-display: sleep POF busy_ms={}", busy_ms);
            }
            uc8279_x4::SleepStep::DeepSleep => {
                epd.bus
                    .command(uc8279_x4::CMD_DEEP_SLEEP, &[uc8279_x4::DEEP_SLEEP_CHECK])
                    .await?;
                epd.old_plane_valid = false;
                epd.quick_fast_streak = 0;
                esp_println::println!("s3-display: sleep DSLP sent");
            }
        }
    }
    Ok(())
}

async fn write_ssd1677_ram(
    epd: &mut Epd,
    ram_command: u8,
    fb: &Framebuffer,
) -> Result<(), PanelError> {
    let rect = Rect::FULL;
    epd.bus
        .command(ssd1677::CMD_SET_RAM_X_RANGE, &ssd1677::ram_x_range(rect))
        .await?;
    epd.bus
        .command(ssd1677::CMD_SET_RAM_Y_RANGE, &ssd1677::ram_y_range(rect))
        .await?;
    epd.bus
        .command(
            ssd1677::CMD_SET_RAM_X_COUNTER,
            &ssd1677::ram_x_counter(rect),
        )
        .await?;
    epd.bus
        .command(
            ssd1677::CMD_SET_RAM_Y_COUNTER,
            &ssd1677::ram_y_counter(rect),
        )
        .await?;

    epd.bus.begin_ram_write(ram_command).await?;
    let mut y = 0;
    let mut result = Ok(());
    while y < display::HEIGHT {
        let band = fb.band(y, CHUNK_ROWS);
        if let Err(err) = epd.bus.ram_chunk(band).await {
            result = Err(PanelError::from(err));
            break;
        }
        y += CHUNK_ROWS;
    }
    epd.bus.end_ram_write();
    result
}

async fn stream_uc8279_plane(
    epd: &mut Epd,
    ram_cmd: u8,
    fb: &Framebuffer,
    source: uc8279_x4::FrameSource,
) -> Result<(), PanelError> {
    let mut chunk = [0u8; CHUNK_BYTES];
    if matches!(source, uc8279_x4::FrameSource::White) {
        chunk.fill(0xFF);
    }

    epd.bus.begin_ram_write(ram_cmd).await?;
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
            if let Err(err) = epd.bus.ram_chunk(slice).await {
                result = Err(PanelError::from(err));
                break;
            }
        }
        gate += rows as u16;
    }
    epd.bus.end_ram_write();
    result
}
