//! Xteink X4 Pro (ESP32-S3) board identity, pin map, frontlight curve, and
//! deep-sleep pad hold contract.

use fw_common::pad::{HoldCapability, PadHoldSpec, PadLevel, Polarity};

/// X4 Pro OTA firmware identity (`esp_app_desc_t.project_name`, <= 32 bytes).
pub const BOARD_IDENTITY: &str = proto::ota::IDENTITY_X4_PRO;

/// ESP-IDF image header `chip_id` expected for ESP32-S3 images.
pub const CHIP_ID: u16 = proto::ota::CHIP_ID_ESP32S3;

// --- Power & rail enables ---

/// Active-high peripheral rail enable asserted at early boot before probing or
/// initializing panel, touch, or SD peripherals.
pub const PERIPHERAL_ENABLE_PIN: u8 = 1;

// --- EPD SPI bus (800x480 panel: SSD1677 / UC8179 / UC8279) ---

pub const EPD_SCLK_PIN: u8 = 12;
pub const EPD_MOSI_PIN: u8 = 11;
pub const EPD_CS_PIN: u8 = 13;
pub const EPD_DC_PIN: u8 = 18;
pub const EPD_RST_PIN: u8 = 14;
pub const EPD_BUSY_PIN: u8 = 6;

// --- SDMMC 1-bit slot ---

pub const SDMMC_CLK_PIN: u8 = 41;
pub const SDMMC_CMD_PIN: u8 = 42;
pub const SDMMC_D0_PIN: u8 = 40;
/// Active-low SD card power enable. Driven high (off) in deep sleep.
pub const SDMMC_POWER_PIN: u8 = 5;

// --- Shared I2C bus (BM8563 RTC 0x51, CW2017 gauge 0x63, GT911 touch 0x5D) ---

pub const I2C_SDA_PIN: u8 = 39;
pub const I2C_SCL_PIN: u8 = 38;
pub const RTC_I2C_ADDR: u8 = hal_ext::bm8563::DEFAULT_ADDR;
pub const GAUGE_I2C_ADDR: u8 = hal_ext::cw2017::DEFAULT_ADDR;
pub const TOUCH_I2C_ADDR: u8 = hal_ext::gt911::PRIMARY_ADDR;
pub const TOUCH_INT_PIN: u8 = 10;
pub const TOUCH_RST_PIN: u8 = 4;
/// Active-low GT911 touch power enable. Driven high (off) in deep sleep.
pub const TOUCH_POWER_PIN: u8 = 2;

// --- Digital active-low buttons & charger status ---

/// Left button (`Button::Prev`, active-low). GPIO0 is an ESP32-S3 boot strap
/// pin and must never be used as a boot recovery chord.
pub const BTN_LEFT_PIN: u8 = 0;
/// Right button (`Button::Next`, active-low). Recovery chord with Power.
pub const BTN_RIGHT_PIN: u8 = 7;
/// Power button (`Button::Power`, active-low). RTC deep-sleep wake source
/// and recovery chord with Right (neither pin is a boot strap).
pub const BTN_POWER_PIN: u8 = 3;
/// Battery charger status input.
pub const CHARGER_STAT_PIN: u8 = 21;

// --- Dual cool/warm frontlight (active-high 10 kHz 10-bit LEDC PWM) ---

pub const FRONTLIGHT_COOL_PIN: u8 = 8;
pub const FRONTLIGHT_WARM_PIN: u8 = 9;
pub const PWM_FREQ_HZ: u32 = 10_000;
pub const PWM_MAX_DUTY: u16 = 1023;

/// Perception-weighted percent-to-duty curve (gamma 1.6554), ported from
/// FreeInk SDK `FrontlightManager.cpp`: `round(65535 * (pct/100)^1.6554)`.
/// On a 10-bit range (`0..=1023`), 1% maps to 1 LSB and every 1% step is distinct.
const GAMMA_TABLE: [u16; 101] = [
    0, 32, 101, 197, 318, 460, 622, 803, 1001, 1217, 1449, 1697, 1960, 2237, 2529, 2835, 3155,
    3488, 3834, 4193, 4565, 4949, 5345, 5753, 6173, 6604, 7047, 7502, 7967, 8444, 8931, 9429, 9938,
    10457, 10987, 11527, 12077, 12638, 13208, 13789, 14379, 14979, 15588, 16208, 16836, 17474,
    18122, 18779, 19445, 20120, 20804, 21497, 22200, 22911, 23631, 24360, 25097, 25843, 26598,
    27362, 28134, 28914, 29703, 30500, 31306, 32120, 32942, 33772, 34611, 35457, 36312, 37175,
    38045, 38924, 39811, 40705, 41608, 42518, 43436, 44361, 45295, 46236, 47185, 48141, 49105,
    50076, 51055, 52042, 53036, 54037, 55046, 56062, 57086, 58117, 59155, 60200, 61253, 62313,
    63380, 64454, 65535,
];

/// Converts brightness (`0..=100`%) and warmth (`0..=100`%) into 10-bit
/// `(cool_duty, warm_duty)` pairs in `0..=PWM_MAX_DUTY`, converting total
/// brightness to PWM resolution before splitting warm/cool channels so low
/// brightness levels preserve at least 1 LSB total duty.
pub const fn frontlight_duties(brightness_pct: u8, warmth_pct: u8) -> (u16, u16) {
    if brightness_pct == 0 {
        return (0, 0);
    }
    let brightness = if brightness_pct > 100 {
        100
    } else {
        brightness_pct
    } as usize;
    let warmth = if warmth_pct > 100 { 100 } else { warmth_pct } as u32;

    let full = PWM_MAX_DUTY as u32;
    let scaled = (full * GAMMA_TABLE[brightness] as u32 + 32_767) / 65_535;
    let total_duty = if scaled == 0 { 1 } else { scaled };
    let warm_duty = (total_duty * warmth + 50) / 100;
    let cool_duty = total_duty - warm_duty;
    (cool_duty as u16, warm_duty as u16)
}

/// Deep-sleep pad hold contract for X4 Pro rails and EPD `/RST`.
///
/// The panel supply stays powered across deep sleep, so `EPD_RST_PIN` (`14`)
/// must stay HIGH to keep the controller in deep sleep; the active-low
/// `SDMMC_POWER_PIN` (`5`) and `TOUCH_POWER_PIN` (`2`) must stay HIGH so both
/// switchable rails stay powered off across deep sleep.
pub const SLEEP_PAD_SPECS: [PadHoldSpec; 3] = [
    PadHoldSpec::epd_reset(EPD_RST_PIN, false, HoldCapability::RetainedInDeepSleep),
    PadHoldSpec::rail_enable(
        SDMMC_POWER_PIN,
        Polarity::ActiveLow,
        HoldCapability::RetainedInDeepSleep,
    ),
    PadHoldSpec::rail_enable(
        TOUCH_POWER_PIN,
        Polarity::ActiveLow,
        HoldCapability::RetainedInDeepSleep,
    ),
];

/// Releases inherited deep-sleep pad holds once GPIO output drivers are active
/// at safe idle levels.
///
/// Clears RTC pad holds on X4 Pro board/rail pads while leaving native USB
/// Serial/JTAG pads (`GPIO19`/`GPIO20`, `pad19`/`pad20`) untouched and never
/// disabling USB pads.
#[allow(unsafe_code)]
pub fn release_deep_sleep_pad_holds() {
    let rtc_cntl = esp_hal::peripherals::LPWR::regs();
    rtc_cntl.dig_iso().modify(|_, w| {
        w.dg_pad_force_unhold()
            .set_bit()
            .dg_pad_autohold_en()
            .clear_bit()
    });
    // Release holds on X4 Pro RTC GPIOs (0..=18, 21) while explicitly leaving
    // native USB D-/D+ (pad19 / pad20) undisturbed.
    rtc_cntl.pad_hold().modify(|_, w| {
        w.touch_pad0().clear_bit();
        w.touch_pad1().clear_bit();
        w.touch_pad2().clear_bit();
        w.touch_pad3().clear_bit();
        w.touch_pad4().clear_bit();
        w.touch_pad5().clear_bit();
        w.touch_pad6().clear_bit();
        w.touch_pad7().clear_bit();
        w.touch_pad8().clear_bit();
        w.touch_pad9().clear_bit();
        w.touch_pad10().clear_bit();
        w.touch_pad11().clear_bit();
        w.touch_pad12().clear_bit();
        w.touch_pad13().clear_bit();
        w.touch_pad14().clear_bit();
        w.pdac2().clear_bit();
        w.pad21().clear_bit()
    });
    // SAFETY: Clearing digital pad holds releases sleep-retained latches on
    // GPIO22..=48 once active drivers are configured; USB D-/D+ (GPIO19/20)
    // live in `pad_hold` and are not touched here.
    rtc_cntl
        .dig_pad_hold()
        .write(|w| unsafe { w.dig_pad_hold().bits(0) });
}

const _: () = {
    assert!(BOARD_IDENTITY.len() <= 32);

    let mut i = 0;
    while i < SLEEP_PAD_SPECS.len() {
        assert!(matches!(SLEEP_PAD_SPECS[i].sleep_level, PadLevel::High));
        assert!(SLEEP_PAD_SPECS[i].is_retained());
        i += 1;
    }

    let (c0, w0) = frontlight_duties(0, 50);
    assert!(c0 == 0 && w0 == 0);

    let (c1, w1) = frontlight_duties(1, 0);
    assert!(c1 == 1 && w1 == 0);

    let (c2, w2) = frontlight_duties(100, 0);
    assert!(c2 == 1023 && w2 == 0);

    let (c3, w3) = frontlight_duties(100, 100);
    assert!(c3 == 0 && w3 == 1023);
};
