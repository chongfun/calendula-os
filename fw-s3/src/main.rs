#![no_std]
#![no_main]
#![deny(unsafe_code)]

#[cfg(not(feature = "x4-pro"))]
compile_error!("select the x4-pro board for this S3 bring-up executable");

use display::epd::{uc8279_x4, RefreshMode};
use display::fb::Framebuffer;
use embassy_executor::Spawner;
use embassy_futures::select::{select4, Either4};
use embassy_time::{Duration, Instant, Timer};
use esp_bootloader_esp_idf as _;
use esp_hal::{
    clock::CpuClock,
    dma::{aligned::InternalMemory, DmaDescriptor, DmaRxBuf, DmaTxBuf},
    gpio::{Flex, Input, InputConfig, Level, Output, OutputConfig, Pin as _, Pull},
    i2c::master::{BusTimeout, Config as I2cConfig, I2c},
    peripherals::LPWR,
    spi::{
        self,
        master::{Config as SpiConfig, Spi},
    },
    time::Rate,
    timer::timg::TimerGroup,
};
use frontlight::Frontlight;
use fw_common::{
    catalog,
    pad::{PadLevel, ReconciledOutput},
    AppView, DisplayCommand, DisplayEvent, LibraryEvent, PowerEvent, ReaderState, RefreshPlanner,
    RenderKind, RenderRequest, StorageCommand,
};
use input::InputHardware;
use static_cell::ConstStaticCell;
use ui::{
    app_render::{render_request, render_sleep, UiRenderModel},
    UiBook, UiLibraryStatus, UiTocItem,
};

mod board;
mod display_flush;
mod epd_probe;
mod frontlight;
mod input;
mod rtc_sleep;

static FRAMEBUFFER: ConstStaticCell<Framebuffer> = ConstStaticCell::new(Framebuffer::new());
static DMA_RX_DESC: ConstStaticCell<InternalMemory<[DmaDescriptor; 1]>> =
    ConstStaticCell::new(InternalMemory::new([DmaDescriptor::EMPTY; 1]));
static DMA_RX_BUF: ConstStaticCell<InternalMemory<[u8; 4]>> =
    ConstStaticCell::new(InternalMemory::new([0u8; 4]));
static DMA_TX_DESC: ConstStaticCell<InternalMemory<[DmaDescriptor; 1]>> =
    ConstStaticCell::new(InternalMemory::new([DmaDescriptor::EMPTY; 1]));
static DMA_TX_BUF: ConstStaticCell<InternalMemory<[u8; 4092]>> =
    ConstStaticCell::new(InternalMemory::new([0u8; 4092]));

// Distinct from the planned shipping board identity (`board::BOARD_IDENTITY`,
// `"CalendulaOS X4-Pro u1"`): this runtime proof has no OTA updater yet.
#[allow(unsafe_code)] // The bootloader reads this retained, immutable descriptor.
#[link_section = ".flash.appdesc"]
#[used]
#[no_mangle]
pub static _ESP_APP_DESC: fw_common::app_desc::EspAppDesc =
    fw_common::app_desc::new(env!("CARGO_PKG_VERSION"), "CalendulaOS X4 Pro proof");

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    esp_println::println!();
    esp_println::println!("====================== PANIC ======================");
    esp_println::println!("{}", info);
    esp_println::println!();
    esp_println::println!("Backtrace:");
    let backtrace = esp_backtrace::Backtrace::capture();
    if backtrace.frames().is_empty() {
        esp_println::println!("No backtrace available - make sure to force frame-pointers.");
    } else {
        for frame in backtrace.frames() {
            esp_println::println!("{:#x}", frame.program_counter());
        }
    }
    esp_println::println!();
    // Wait briefly so the USB serial-JTAG FIFO can transmit pending panic output.
    let start = Instant::now();
    while start.elapsed().as_millis() < 100 {}
    esp_hal::system::software_reset()
}

#[embassy_executor::task]
async fn app() {
    fw_common::app::run(None).await;
}

fn with_builtin_model<R>(request: RenderRequest, f: impl FnOnce(&UiRenderModel<'_>) -> R) -> R {
    let book = catalog::active_book(request.book_id);
    let mut chapters = [UiTocItem {
        title: "",
        level: 1,
        page: 0,
        has_subsections: false,
        expanded: false,
        is_subsection: false,
        chapter: 0,
    }; catalog::CHAPTERS.len()];
    let count = (catalog::chapter_count() as usize).min(chapters.len());
    for (index, item) in chapters.iter_mut().take(count).enumerate() {
        if let Some(ch) = catalog::chapter_at(index) {
            *item = UiTocItem {
                title: ch.title,
                level: 1,
                page: 0,
                has_subsections: false,
                expanded: false,
                is_subsection: false,
                chapter: index as u16,
            };
        }
    }
    let total_chapters = catalog::chapter_count().max(1) as u32;
    let progress_permille =
        ((request.chapter as u32 * 1000) / total_chapters.saturating_sub(1).max(1)) as u16;
    let model = UiRenderModel {
        active_book: UiBook {
            title: book.title,
            author: book.author,
            progress_permille,
            cover: None,
        },
        library_status: UiLibraryStatus::Empty,
        library_entries: &[],
        library_folder: "",
        library_window_start: 0,
        chapters: &chapters[..count],
        chapters_window_start: 0,
        chapters_total: count as u16,
        chapter_subsections: &[],
        chapter_title: "",
        custom_font_name: "",
    };
    f(&model)
}

async fn enter_sleep(
    lpwr: LPWR<'static>,
    epd: &mut display_flush::Epd,
    fb: &mut Framebuffer,
    sdmmc_power: &mut Output<'static>,
    frontlight: &mut Frontlight,
    planner: &RefreshPlanner,
) -> ! {
    let sleep_req = planner
        .last_request()
        .unwrap_or_else(|| ReaderState::boot().render_request(RenderKind::Boot));
    with_builtin_model(sleep_req, |model| {
        render_sleep(fb, sleep_req, model);
    });
    if let Err(err) = display_flush::flush(epd, fb, RefreshMode::Full, false, true).await {
        esp_println::println!("s3-display: sleep frame flush err={:?}", err);
    }
    rtc_sleep::enter_deep_sleep(lpwr, epd, sdmmc_power, frontlight).await;
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    let mut p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::_240MHz));
    // X4 Pro's peripheral enable (`board::PERIPHERAL_ENABLE_PIN`), not a Sticky
    // power latch. Asserted HIGH before probing the panel bus; never touch USB
    // D-/D+ (`GPIO19`/`GPIO20`).
    assert_eq!(p.GPIO1.number(), board::PERIPHERAL_ENABLE_PIN);
    let _peripheral_enable = Output::new(p.GPIO1, Level::High, OutputConfig::default());

    // Drive every deep-sleep held rail output at a known idle level before
    // `release_deep_sleep_pad_holds()` clears the RTC pad holds below.
    assert_eq!(p.GPIO5.number(), board::SDMMC_POWER_PIN);
    let mut sdmmc_power = Output::new(p.GPIO5, Level::High, OutputConfig::default());
    assert_eq!(p.GPIO2.number(), board::TOUCH_POWER_PIN);
    let touch_power = Output::new(p.GPIO2, Level::Low, OutputConfig::default());

    fw_common::log::install(|args| esp_println::println!("{}", args));
    let timer = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timer.timer0);

    // Borrow the EPD pins (`board::EPD_*`); never touch USB `GPIO19`/`GPIO20`.
    assert_eq!(p.GPIO12.number(), board::EPD_SCLK_PIN);
    assert_eq!(p.GPIO11.number(), board::EPD_MOSI_PIN);
    assert_eq!(p.GPIO13.number(), board::EPD_CS_PIN);
    assert_eq!(p.GPIO18.number(), board::EPD_DC_PIN);
    assert_eq!(p.GPIO14.number(), board::EPD_RST_PIN);
    assert_eq!(p.GPIO6.number(), board::EPD_BUSY_PIN);
    let mut rst = Flex::new(p.GPIO14.reborrow());
    rst.apply_output_config(&OutputConfig::default());
    rst.set_high();
    rst.set_output_enable(true);
    let diag = epd_probe::probe(epd_probe::ProbePins {
        sclk: Flex::new(p.GPIO12.reborrow()),
        sda: Flex::new(p.GPIO11.reborrow()),
        cs: Flex::new(p.GPIO13.reborrow()),
        dc: Flex::new(p.GPIO18.reborrow()),
        rst: ReconciledOutput::with_idle(rst, PadLevel::High, board::release_deep_sleep_pad_holds),
    });
    esp_println::println!(
        "s3-proof: epd_probe verdict={} variant={} ver={:02X} {:02X} {:02X} {:02X} {:02X} flg={:02X} lut_ver={:02X} mtp_valid={}",
        diag.verdict.as_str(),
        diag.variant.as_str(),
        diag.ver[0],
        diag.ver[1],
        diag.ver[2],
        diag.ver[3],
        diag.ver[4],
        diag.flg,
        diag.lut_ver(),
        diag.mtp_valid,
    );
    if diag.mtp_valid {
        esp_println::println!(
            "s3-proof: epd_probe mtp_prefix={:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X}",
            diag.mtp[0],
            diag.mtp[1],
            diag.mtp[2],
            diag.mtp[3],
            diag.mtp[4],
            diag.mtp[5],
            diag.mtp[6],
            diag.mtp[7],
            diag.mtp[8],
            diag.mtp[9],
            diag.mtp[10],
            diag.mtp[11],
            diag.mtp[12],
            diag.mtp[13],
            diag.mtp[14],
            diag.mtp[15],
        );
    }

    assert_eq!(p.GPIO8.number(), board::FRONTLIGHT_COOL_PIN);
    assert_eq!(p.GPIO9.number(), board::FRONTLIGHT_WARM_PIN);
    let mut frontlight = Frontlight::init(p.LEDC, p.GPIO8, p.GPIO9);

    let dma_rx = DmaRxBuf::new(
        DMA_RX_DESC.take().get_mut().unsize(),
        DMA_RX_BUF.take().get_mut().unsize(),
    )
    .expect("static DMA RX buffer is valid");
    let dma_tx = DmaTxBuf::new(
        DMA_TX_DESC.take().get_mut().unsize(),
        DMA_TX_BUF.take().get_mut().unsize(),
    )
    .expect("static DMA TX buffer is valid");
    let epd_spi = Spi::new(
        p.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_hz(uc8279_x4::SPI_HZ))
            .with_mode(spi::Mode::_0),
    )
    .expect("SPI2 configuration is static and valid")
    .with_sck(p.GPIO12)
    .with_mosi(p.GPIO11)
    .with_dma(p.DMA_CH0)
    .with_buffers(dma_rx, dma_tx)
    .into_async();
    let mut epd = display_flush::Epd::new(
        epd_spi,
        Output::new(p.GPIO13, Level::High, OutputConfig::default()),
        Output::new(p.GPIO18, Level::Low, OutputConfig::default()),
        Input::new(p.GPIO6, InputConfig::default().with_pull(Pull::Up)),
        Output::new(p.GPIO14, Level::High, OutputConfig::default()),
    );
    if let Err(err) = display_flush::init_panel(&mut epd).await {
        esp_println::println!("s3-display: uc8279-x4 init err={:?}", err);
    }

    assert_eq!(p.GPIO39.number(), board::I2C_SDA_PIN);
    assert_eq!(p.GPIO38.number(), board::I2C_SCL_PIN);
    assert_eq!(p.GPIO10.number(), board::TOUCH_INT_PIN);
    assert_eq!(p.GPIO4.number(), board::TOUCH_RST_PIN);
    assert_eq!(p.GPIO0.number(), board::BTN_LEFT_PIN);
    assert_eq!(p.GPIO7.number(), board::BTN_RIGHT_PIN);
    assert_eq!(p.GPIO3.number(), board::BTN_POWER_PIN);
    assert_eq!(p.GPIO21.number(), board::CHARGER_STAT_PIN);

    let i2c = I2c::new(
        p.I2C0,
        I2cConfig::default()
            .with_frequency(Rate::from_khz(400))
            .with_timeout(BusTimeout::BusCycles(2000)),
    )
    .expect("I2C0 configuration is static and valid")
    .with_sda(p.GPIO39)
    .with_scl(p.GPIO38)
    .into_async();
    let input_hw = InputHardware {
        i2c,
        touch_power,
        touch_rst: Output::new(p.GPIO4, Level::High, OutputConfig::default()),
        touch_int: Flex::new(p.GPIO10),
        btn_left: Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::Up)),
        btn_right: Input::new(p.GPIO7, InputConfig::default().with_pull(Pull::Up)),
        btn_power: Input::new(p.GPIO3, InputConfig::default().with_pull(Pull::Up)),
        charger_stat: Input::new(p.GPIO21, InputConfig::default().with_pull(Pull::Up)),
    };

    spawner.spawn(input::run(input_hw).expect("the input task has not been spawned"));
    spawner.spawn(app().expect("the sole shared app task has not been spawned"));
    esp_println::println!("s3-proof: board=x4-pro chip=esp32s3 shared_app=started");

    let fb = FRAMEBUFFER.take();
    let mut planner = RefreshPlanner::new();
    let mut idle_dur = Duration::from_secs(app_core::idle_timeout_secs(AppView::Home));
    let mut idle_deadline = Instant::now() + idle_dur;
    let mut next_heartbeat = Instant::now() + Duration::from_secs(2);
    let lpwr = p.LPWR;

    loop {
        let next_wake = idle_deadline.min(next_heartbeat);
        match select4(
            fw_common::STORAGE_COMMANDS.receive(),
            fw_common::DISPLAY_COMMANDS.receive(),
            fw_common::POWER_EVENTS.receive(),
            Timer::at(next_wake),
        )
        .await
        {
            Either4::First(StorageCommand::LoadCatalogCache | StorageCommand::RefreshCatalog) => {
                fw_common::LIBRARY_EVENTS
                    .send(LibraryEvent::Scanned {
                        count: 0,
                        catalog_epoch: 1,
                    })
                    .await;
            }
            Either4::First(_) => {
                esp_println::println!("s3-proof: storage command unsupported");
            }
            Either4::Second(DisplayCommand::Render(request)) => {
                let mode = planner.mode_for(request);
                with_builtin_model(request, |model| {
                    render_request(fb, request, model);
                });
                let start = Instant::now();
                match display_flush::flush(&mut epd, fb, mode, false, false).await {
                    Ok(()) => {
                        planner.record_render(request, mode);
                        esp_println::println!(
                            "s3-display: flush ok view={:?} kind={:?} mode={:?} t_ms={}",
                            request.view,
                            request.kind,
                            mode,
                            start.elapsed().as_millis(),
                        );
                        fw_common::DISPLAY_EVENTS
                            .send(DisplayEvent::Settled {
                                chapter_cursor: None,
                            })
                            .await;
                    }
                    Err(err) => {
                        planner.record_failure();
                        esp_println::println!(
                            "s3-display: flush err={:?} view={:?} kind={:?}",
                            err,
                            request.view,
                            request.kind,
                        );
                        fw_common::DISPLAY_EVENTS
                            .send(DisplayEvent::RefreshFailed)
                            .await;
                    }
                }
            }
            Either4::Second(DisplayCommand::StageNext { .. } | DisplayCommand::Sleep { .. }) => {}
            Either4::Third(event) => match event {
                PowerEvent::Activity(view) => {
                    idle_dur = Duration::from_secs(app_core::idle_timeout_secs(view));
                    idle_deadline = Instant::now() + idle_dur;
                }
                PowerEvent::BrowserActivity => {
                    idle_dur = Duration::from_secs(app_core::idle_timeout_secs(AppView::Wireless));
                    idle_deadline = Instant::now() + idle_dur;
                }
                PowerEvent::SleepNow => {
                    enter_sleep(
                        lpwr,
                        &mut epd,
                        fb,
                        &mut sdmmc_power,
                        &mut frontlight,
                        &planner,
                    )
                    .await;
                }
                PowerEvent::DisplaySettled
                | PowerEvent::DisplayAsleep(_)
                | PowerEvent::DisplayRefreshFailed
                | PowerEvent::DisplaySleepFailed(_) => {}
            },
            Either4::Fourth(()) => {
                let now = Instant::now();
                if now >= idle_deadline {
                    enter_sleep(
                        lpwr,
                        &mut epd,
                        fb,
                        &mut sdmmc_power,
                        &mut frontlight,
                        &planner,
                    )
                    .await;
                }
                if now >= next_heartbeat {
                    esp_println::println!("s3-proof: heartbeat t_ms={}", now.as_millis());
                    next_heartbeat = now + Duration::from_secs(2);
                }
            }
        }
    }
}
