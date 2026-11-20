use crate::board;
use esp_hal::gpio::DriveMode;
use esp_hal::ledc::channel::{self, Channel, ChannelHW, ChannelIFace};
use esp_hal::ledc::timer::{self, Timer, TimerIFace};
use esp_hal::ledc::{LSGlobalClkSource, Ledc, LowSpeed};
use esp_hal::peripherals::{GPIO8, GPIO9, LEDC};
use esp_hal::time::Rate;
use static_cell::StaticCell;

static FRONTLIGHT_TIMER: StaticCell<Timer<'static, LowSpeed>> = StaticCell::new();

pub struct Frontlight {
    cool: Channel<'static, LowSpeed>,
    warm: Channel<'static, LowSpeed>,
}

impl Frontlight {
    pub fn init(
        ledc_periph: LEDC<'static>,
        cool_pin: GPIO8<'static>,
        warm_pin: GPIO9<'static>,
    ) -> Self {
        let mut ledc = Ledc::new(ledc_periph);
        ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

        let mut raw_timer = ledc.timer::<LowSpeed>(timer::Number::Timer0);
        raw_timer
            .configure(timer::config::Config {
                duty: timer::config::Duty::Duty10Bit,
                clock_source: timer::LSClockSource::APBClk,
                frequency: Rate::from_hz(board::PWM_FREQ_HZ),
            })
            .expect("LEDC timer0 config is static and valid");
        let timer = FRONTLIGHT_TIMER.init(raw_timer);

        let mut cool = ledc.channel::<LowSpeed>(channel::Number::Channel0, cool_pin);
        cool.configure(channel::config::Config {
            timer,
            duty_pct: 0,
            drive_mode: DriveMode::PushPull,
        })
        .expect("LEDC cool channel config is static and valid");

        let mut warm = ledc.channel::<LowSpeed>(channel::Number::Channel1, warm_pin);
        warm.configure(channel::config::Config {
            timer,
            duty_pct: 0,
            drive_mode: DriveMode::PushPull,
        })
        .expect("LEDC warm channel config is static and valid");

        let mut frontlight = Self { cool, warm };
        frontlight.off();

        let (cool_duty, warm_duty) = board::frontlight_duties(0, 0);
        esp_println::println!(
            "s3-frontlight: init ok cool_gpio={} warm_gpio={} freq_hz={} bits=10 duty=({}, {})",
            board::FRONTLIGHT_COOL_PIN,
            board::FRONTLIGHT_WARM_PIN,
            board::PWM_FREQ_HZ,
            cool_duty,
            warm_duty,
        );
        frontlight
    }

    pub fn set(&mut self, brightness_pct: u8, warmth_pct: u8) {
        let (cool_duty, warm_duty) = board::frontlight_duties(brightness_pct, warmth_pct);
        self.cool.set_duty_hw(u32::from(cool_duty));
        self.warm.set_duty_hw(u32::from(warm_duty));
    }

    pub fn off(&mut self) {
        self.set(0, 0);
    }
}
