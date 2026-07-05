//! Xteink X4 board values.

use super::{Band, HardwareButton};
use core::ops::RangeInclusive;
use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, AdcPin, Attenuation};
use esp_hal::dma::DmaChannel0;
use esp_hal::gpio::{Flex, GpioPin, Input, Level, Output, Pull};
use esp_hal::peripherals::{
    Peripherals, ADC1, LPWR, RADIO_CLK, RNG, SPI2, SW_INTERRUPT, SYSTIMER, TIMG0, TIMG1, WIFI,
};

pub(crate) type AuxAdcPin = AdcPin<GpioPin<0>, ADC1, AdcCalCurve<ADC1>>;
pub(crate) type NavAdcPin = AdcPin<GpioPin<1>, ADC1, AdcCalCurve<ADC1>>;
pub(crate) type PageAdcPin = AdcPin<GpioPin<2>, ADC1, AdcCalCurve<ADC1>>;
pub(crate) type EpdSck = GpioPin<8>;
pub(crate) type EpdMosi = GpioPin<10>;
pub(crate) type EpdMiso = GpioPin<7>;

/// Physical GPIO backing the Power button; the input task holds the live
/// `Input<'static>` handle, but the deep-sleep wake path needs to
/// re-materialise the same pin as a wake source (see `steal_wake_button`).
const POWER_BUTTON_GPIO: u8 = 3;

/// Everything `main` needs off the chip, with the X4's pin map already
/// applied. A sibling board provides the same shape from its own GPIO
/// numbers; `main` never names a GPIO directly.
pub(crate) struct BoardResources {
    pub(crate) epd_cs: Output<'static>,
    pub(crate) epd_dc: Output<'static>,
    pub(crate) epd_rst: Output<'static>,
    pub(crate) epd_busy: Input<'static>,
    pub(crate) epd_sck: EpdSck,
    pub(crate) epd_mosi: EpdMosi,
    pub(crate) epd_miso: EpdMiso,
    pub(crate) sd_cs: Output<'static>,
    pub(crate) power_button: Input<'static>,
    pub(crate) adc1: Adc<'static, ADC1>,
    pub(crate) aux_adc_pin: AuxAdcPin,
    pub(crate) nav_adc_pin: NavAdcPin,
    pub(crate) page_adc_pin: PageAdcPin,
    pub(crate) timg0: TIMG0,
    pub(crate) timg1: TIMG1,
    pub(crate) spi2: SPI2,
    pub(crate) dma_ch0: DmaChannel0,
    pub(crate) sw_interrupt: SW_INTERRUPT,
    pub(crate) lpwr: LPWR,
    pub(crate) wifi: WIFI,
    pub(crate) systimer: SYSTIMER,
    pub(crate) rng: RNG,
    pub(crate) radio_clk: RADIO_CLK,
}

/// Claims the chip's peripherals and wires them to the X4's pin map. The
/// non-GPIO peripherals pass through unchanged; only the board module knows
/// which physical pin backs each logical role.
pub(crate) fn take(peripherals: Peripherals) -> BoardResources {
    let epd_cs = Output::new(peripherals.GPIO21, Level::High);
    let epd_dc = Output::new(peripherals.GPIO4, Level::High);
    let epd_rst = Output::new(peripherals.GPIO5, Level::High);
    let epd_busy = Input::new(peripherals.GPIO6, Pull::None);
    let sd_cs = Output::new(peripherals.GPIO12, Level::High);
    let power_button = Input::new(peripherals.GPIO3, Pull::Up);

    let mut adc_config = AdcConfig::new();
    let aux_adc_pin = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<ADC1>>(peripherals.GPIO0, Attenuation::_11dB);
    let nav_adc_pin = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<ADC1>>(peripherals.GPIO1, Attenuation::_11dB);
    let page_adc_pin = adc_config
        .enable_pin_with_cal::<_, AdcCalCurve<ADC1>>(peripherals.GPIO2, Attenuation::_11dB);
    let adc1 = Adc::new(peripherals.ADC1, adc_config);

    BoardResources {
        epd_cs,
        epd_dc,
        epd_rst,
        epd_busy,
        epd_sck: peripherals.GPIO8,
        epd_mosi: peripherals.GPIO10,
        epd_miso: peripherals.GPIO7,
        sd_cs,
        power_button,
        adc1,
        aux_adc_pin,
        nav_adc_pin,
        page_adc_pin,
        timg0: peripherals.TIMG0,
        timg1: peripherals.TIMG1,
        spi2: peripherals.SPI2,
        dma_ch0: peripherals.DMA_CH0,
        sw_interrupt: peripherals.SW_INTERRUPT,
        lpwr: peripherals.LPWR,
        wifi: peripherals.WIFI,
        systimer: peripherals.SYSTIMER,
        rng: peripherals.RNG,
        radio_clk: peripherals.RADIO_CLK,
    }
}

/// Re-materialises the Power button as a deep-sleep wake source.
///
/// SAFETY: only reached on the terminal deep-sleep path. The input task's
/// `Input<'static>` handle on this pin is about to be torn down by the chip
/// reset that ends deep sleep, so this second handle never coexists with a
/// live one.
#[allow(unsafe_code)]
pub(crate) fn steal_wake_button() -> Flex<'static> {
    Flex::new(unsafe { GpioPin::<POWER_BUTTON_GPIO>::steal() })
}

pub(crate) const NAV_BACK_MIN_MV: u16 = 2400;
pub(crate) const NAV_BACK_MAX_MV: u16 = 2700;

pub(crate) const NAV: &[Band] = &[
    // X4 front-button ladder on GPIO1. These bands scale Adafruit's current
    // 16-bit CircuitPython X4 thresholds to the 12-bit esp-hal ADC reads.
    Band {
        min: NAV_BACK_MIN_MV,
        max: NAV_BACK_MAX_MV,
        button: HardwareButton::Back,
    },
    Band {
        min: 1800,
        max: 2150,
        button: HardwareButton::Confirm,
    },
    Band {
        min: 1000,
        max: 1250,
        button: HardwareButton::Left,
    },
    Band {
        min: 0,
        max: 100,
        button: HardwareButton::Right,
    },
];

pub(crate) const PAGE: &[Band] = &[
    // X4 side-button ladder on GPIO2, scaled from Adafruit's thresholds.
    Band {
        min: 1500,
        max: 1800,
        button: HardwareButton::Up,
    },
    Band {
        min: 0,
        max: 100,
        button: HardwareButton::Down,
    },
];

/// Millivolt windows the boot-time recovery combo (Back + Up) must land
/// in; they mirror the Back and Up rungs of the NAV/PAGE tables above.
pub(crate) const RECOVERY_NAV_MV: RangeInclusive<u16> = 2400..=2700;
pub(crate) const RECOVERY_PAGE_MV: RangeInclusive<u16> = 1500..=1800;

/// Bytes claimed from `dram2_seg` for the radio heap. The segment is
/// ~64.8 KB and also hosts the previous-frame framebuffer (see
/// `sync_mem`), which was moved there so esp-wifi's static demand fits
/// in main DRAM without eating the stack region.
pub(crate) const DRAM2_HEAP_BYTES: usize = 16 * 1024;
