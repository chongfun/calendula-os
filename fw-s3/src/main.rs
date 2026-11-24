#![no_std]
#![no_main]
#![deny(unsafe_code)]

extern crate alloc;

#[cfg(not(feature = "x4-pro"))]
compile_error!("select the x4-pro board for this S3 bring-up executable");

use display::epd::{probe::X4ControllerVariant, ssd1677, uc8279_x4};
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Instant;
use esp_bootloader_esp_idf as _;
use esp_hal::{
    clock::CpuClock,
    dma::{aligned::InternalMemory, DmaDescriptor, DmaRxBuf, DmaTxBuf},
    gpio::{Flex, Input, InputConfig, Level, Output, OutputConfig, Pin as _, Pull},
    i2c::master::{BusTimeout, Config as I2cConfig, I2c},
    interrupt::Priority,
    rng::Rng,
    rtc_cntl::{reset_reason, wakeup_cause, SocResetReason, WakeupSource},
    sdmmc::{Config as SdConfig, SdHostController, SlotConfig},
    sha::Sha,
    spi::{
        self,
        master::{Config as SpiConfig, Spi},
    },
    system::Cpu,
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_rtos::embassy::InterruptExecutor;
use frontlight::Frontlight;
use fw_common::pad::{PadLevel, ReconciledOutput};
use input::InputHardware;
use sd_session::SdmmcBus;
use static_cell::{ConstStaticCell, StaticCell};

#[macro_use]
mod log;

#[cfg(feature = "bench-selftest")]
pub mod bench_selftest;
mod board;
mod display_flush;
mod epd_probe;
mod frontlight;
mod hw_sha;
mod input;
mod mmu;
mod ota_update;
mod probe_report;
mod sd_session;
mod sleep_marker;
mod sync_mem;
mod tasks;
mod views;

pub static SYNC_LOANS: Channel<
    CriticalSectionRawMutex,
    Result<sync_mem::SyncLoan, app_core::SyncError>,
    1,
> = Channel::new();

static SDHOST_CTRL: StaticCell<SdHostController<'static>> = StaticCell::new();
static INPUT_EXECUTOR: StaticCell<InterruptExecutor<2>> = StaticCell::new();

static DMA_RX_DESC: ConstStaticCell<InternalMemory<[DmaDescriptor; 1]>> =
    ConstStaticCell::new(InternalMemory::new([DmaDescriptor::EMPTY; 1]));
static DMA_RX_BUF: ConstStaticCell<InternalMemory<[u8; 4]>> =
    ConstStaticCell::new(InternalMemory::new([0u8; 4]));
static DMA_TX_DESC: ConstStaticCell<InternalMemory<[DmaDescriptor; 1]>> =
    ConstStaticCell::new(InternalMemory::new([DmaDescriptor::EMPTY; 1]));
static DMA_TX_BUF: ConstStaticCell<InternalMemory<[u8; 4092]>> =
    ConstStaticCell::new(InternalMemory::new([0u8; 4092]));

static STORAGE_PLATFORM: storage::platform::Platform = storage::platform::Platform {
    with_sha256: |f| hw_sha::with_sha256(|engine| f(engine)),
    random_u32: || Rng::new().random(),
};

#[allow(unsafe_code)] // The bootloader reads this retained, immutable descriptor.
#[link_section = ".flash.appdesc"]
#[used]
#[no_mangle]
pub static _ESP_APP_DESC: fw_common::app_desc::EspAppDesc =
    fw_common::app_desc::new(env!("CARGO_PKG_VERSION"), board::BOARD_IDENTITY);

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    esp_println::println!();
    esp_println::println!("====================== PANIC ======================");
    esp_println::println!("{}", info);
    esp_println::println!();
    esp_println::println!("Backtrace:");
    let backtrace = esp_backtrace::Backtrace::capture();
    if backtrace.frames().is_empty() {
        esp_println::println!("No backtrace available.");
    } else {
        for frame in backtrace.frames() {
            esp_println::println!("{:#x}", frame.program_counter());
        }
    }
    esp_println::println!();
    let start = Instant::now();
    while start.elapsed().as_millis() < 100 {}
    esp_hal::system::software_reset()
}

fn recovery_combo_confirmed(btn_right: &Input<'_>, btn_power: &Input<'_>) -> bool {
    for _ in 0..5 {
        if !(btn_right.is_low() && btn_power.is_low()) {
            return false;
        }
        embassy_time::block_for(embassy_time::Duration::from_millis(8));
    }
    true
}

#[embassy_executor::task]
async fn app(wake_into_reading: bool) {
    let initial_ssid =
        tasks::wifi::credentials().and_then(|(ssid, _)| fw_common::WifiSsid::new(ssid));
    fw_common::app::run_with_options(initial_ssid, true, wake_into_reading).await;
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    let mut p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::_240MHz));
    assert_eq!(p.GPIO1.number(), board::PERIPHERAL_ENABLE_PIN);
    let _peripheral_enable = Output::new(p.GPIO1, Level::High, OutputConfig::default());

    assert_eq!(p.GPIO5.number(), board::SDMMC_POWER_PIN);
    let sdmmc_power = Output::new(p.GPIO5, Level::High, OutputConfig::default());
    assert_eq!(p.GPIO2.number(), board::TOUCH_POWER_PIN);
    let touch_power = Output::new(p.GPIO2, Level::Low, OutputConfig::default());

    fw_common::log::install(|args| esp_println::println!("{}", args));
    #[cfg(feature = "bench-selftest")]
    fw_common::bench::install(&fw_common::bench::Hooks {
        note_settled: bench_selftest::note_settled,
        publish_view: bench_selftest::publish_view,
    });
    let timer = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timer.timer0);

    // Check the recovery chord before initializing EPD, frontlight, or SD
    // peripherals so a broken app1 image can always recover back to app0.
    // GPIO0 is a boot strap: never require holding Left across reset.
    assert_eq!(p.GPIO7.number(), board::BTN_RIGHT_PIN);
    assert_eq!(p.GPIO3.number(), board::BTN_POWER_PIN);
    let btn_right = Input::new(p.GPIO7, InputConfig::default().with_pull(Pull::Up));
    let btn_power = Input::new(p.GPIO3, InputConfig::default().with_pull(Pull::Up));
    if recovery_combo_confirmed(&btn_right, &btn_power) && ota_update::recover_to_slot0() {
        esp_hal::system::software_reset();
    }

    let reset = reset_reason(Cpu::ProCpu);
    let wake = wakeup_cause();
    let woke_by_button = reset == Some(SocResetReason::CoreDeepSleep)
        && (wake.contains(WakeupSource::Gpio)
            || wake.contains(WakeupSource::Ext0)
            || wake.contains(WakeupSource::Ext1));
    #[cfg(feature = "bench-selftest")]
    let woke_from_sleep = woke_by_button
        || (reset == Some(SocResetReason::CoreDeepSleep) && wake.contains(WakeupSource::Timer));
    #[cfg(not(feature = "bench-selftest"))]
    let woke_from_sleep = woke_by_button;
    let sleep_screen = sleep_marker::take_sleep_screen();
    let deep_sleep_wake = sleep_screen.filter(|_| woke_from_sleep);
    esp_println::println!(
        "main: deep_sleep_wake={} (gpio={}, sleep_image={})",
        deep_sleep_wake.is_some(),
        woke_by_button,
        sleep_screen.is_some()
    );
    esp_println::println!(
        "s3-boot: reset={:?} wake={:?} deep_sleep_wake={}",
        reset,
        wake,
        deep_sleep_wake.is_some(),
    );

    // Build the EPUB scratch on the shallow boot stack before spawning tasks.
    let epub_scratch = tasks::display::init_epub_scratch();

    hw_sha::install(Sha::new(p.SHA));
    storage::platform::install(&STORAGE_PLATFORM);

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
        "s3-boot: epd_probe verdict={} variant={} driver={} lut_ver={:02X}",
        diag.verdict.as_str(),
        diag.variant.as_str(),
        probe_report::active_driver_token(diag.variant),
        diag.lut_ver(),
    );

    assert_eq!(p.GPIO8.number(), board::FRONTLIGHT_COOL_PIN);
    assert_eq!(p.GPIO9.number(), board::FRONTLIGHT_WARM_PIN);
    let frontlight = Frontlight::init(p.LEDC, p.GPIO8, p.GPIO9);

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
    let (spi_hz, busy_cfg) = match diag.variant {
        X4ControllerVariant::Ssd1677 => (ssd1677::SPI_HZ, InputConfig::default()),
        X4ControllerVariant::Uc8279X4 | X4ControllerVariant::Uc8179 => (
            uc8279_x4::SPI_HZ,
            InputConfig::default().with_pull(Pull::Up),
        ),
    };
    let epd_spi = Spi::new(
        p.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_hz(spi_hz))
            .with_mode(spi::Mode::_0),
    )
    .expect("SPI2 configuration is static and valid")
    .with_sck(p.GPIO12)
    .with_mosi(p.GPIO11)
    .with_dma(p.DMA_CH0)
    .with_buffers(dma_rx, dma_tx)
    .into_async();
    let epd = display_flush::Epd::new(
        epd_spi,
        Output::new(p.GPIO13, Level::High, OutputConfig::default()),
        Output::new(p.GPIO18, Level::Low, OutputConfig::default()),
        Input::new(p.GPIO6, busy_cfg),
        Output::new(p.GPIO14, Level::High, OutputConfig::default()),
        diag.variant,
    );

    assert_eq!(p.GPIO41.number(), board::SDMMC_CLK_PIN);
    assert_eq!(p.GPIO42.number(), board::SDMMC_CMD_PIN);
    assert_eq!(p.GPIO40.number(), board::SDMMC_D0_PIN);
    let sdhost = SDHOST_CTRL.init(
        SdHostController::new(p.SDHOST, SdConfig::default()).expect("SDHOST init config is valid"),
    );
    let sd_slot = sdhost
        .slot::<0>(SlotConfig::default())
        .expect("SDHOST slot 0 is free")
        .with_clk(p.GPIO41)
        .with_cmd(p.GPIO42)
        .with_data0(p.GPIO40);
    let sdmmc_bus = SdmmcBus::new(sd_slot, sdmmc_power);

    assert_eq!(p.GPIO39.number(), board::I2C_SDA_PIN);
    assert_eq!(p.GPIO38.number(), board::I2C_SCL_PIN);
    assert_eq!(p.GPIO10.number(), board::TOUCH_INT_PIN);
    assert_eq!(p.GPIO4.number(), board::TOUCH_RST_PIN);
    assert_eq!(p.GPIO0.number(), board::BTN_LEFT_PIN);
    assert_eq!(p.GPIO21.number(), board::CHARGER_STAT_PIN);

    let btn_left = Input::new(p.GPIO0, InputConfig::default().with_pull(Pull::Up));
    ota_update::mark_running_slot_valid();

    let i2c = I2c::new(
        p.I2C0,
        I2cConfig::default()
            .with_frequency(Rate::from_khz(400))
            .with_timeout(BusTimeout::BusCycles(2000)),
    )
    .expect("I2C0 configuration is static and valid")
    .with_sda(p.GPIO39)
    .with_scl(p.GPIO38);
    let input_hw = InputHardware {
        i2c,
        touch_power,
        touch_rst: Output::new(p.GPIO4, Level::High, OutputConfig::default()),
        touch_int: Flex::new(p.GPIO10),
        btn_left,
        btn_right,
        btn_power,
        charger_stat: Input::new(p.GPIO21, InputConfig::default().with_pull(Pull::Up)),
    };

    // Run input polling on an interrupt-priority executor (`FROM_CPU_INTR2` at
    // `Priority1`; `FROM_CPU_INTR0`/`1` are reserved by `esp-hal` for IPC) so
    // blocking SDMMC I/O and cold EPUB indexing on the thread executor cannot
    // starve physical buttons or touch. Priority 1 sits below higher-priority
    // radio/DMA interrupts, `input::run` has exclusive ownership of async
    // `I2C0` (yielding `Poll::Pending` across transfers without blocking), and
    // cross-task channels use `CriticalSectionRawMutex`.
    let input_executor = INPUT_EXECUTOR.init(InterruptExecutor::new(p.FROM_CPU_INTR2));
    let input_spawner = input_executor.start(Priority::Priority1);
    input_spawner.spawn(input::run(input_hw).expect("input task not yet spawned"));
    let wake_into_reading = deep_sleep_wake.is_some_and(|screen| screen.reading);
    spawner.spawn(
        tasks::display::run(
            epd,
            sdmmc_bus,
            frontlight,
            deep_sleep_wake,
            diag,
            epub_scratch,
        )
        .expect("display task not yet spawned"),
    );
    spawner.spawn(tasks::power::run(p.LPWR).expect("power task not yet spawned"));
    spawner.spawn(tasks::wifi::run(spawner, p.WIFI).expect("wifi task not yet spawned"));
    spawner.spawn(app(wake_into_reading).expect("app task not yet spawned"));
    #[cfg(feature = "bench-selftest")]
    spawner.spawn(bench_selftest::run().expect("bench_selftest task not yet spawned"));

    esp_println::println!("s3-boot: tasks spawned board=x4-pro chip=esp32s3");
}
