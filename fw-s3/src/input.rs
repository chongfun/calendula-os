use app_core::buttons::PowerEdge;
use app_core::{Button, InputEvent};
use display::epd::uc8279_x4;
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Delay, Timer};
use esp_hal::gpio::{Flex, Input, InputConfig, Output, OutputConfig, Pull};
use esp_hal::i2c::master::I2c;
use esp_hal::Async;
use hal_ext::bm8563::Bm8563;
use hal_ext::cw2017::{Cw2017, X4_PRO_BATINFO};
use hal_ext::gt911::{self, Gt911, TouchAction, TouchPoint, TouchTransform};

use crate::board;

pub struct WakePins {
    pub power: Input<'static>,
    pub touch_power: Output<'static>,
}

pub struct InputHardware {
    pub i2c: I2c<'static, Async>,
    pub touch_power: Output<'static>,
    pub touch_rst: Output<'static>,
    pub touch_int: Flex<'static>,
    pub btn_left: Input<'static>,
    pub btn_right: Input<'static>,
    pub btn_power: Input<'static>,
    pub charger_stat: Input<'static>,
}

static WAKE_PIN_REQUESTS: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
static WAKE_PIN_HANDOFF: Channel<CriticalSectionRawMutex, WakePins, 1> = Channel::new();

pub async fn take_wake_pins() -> WakePins {
    WAKE_PIN_REQUESTS.send(()).await;
    WAKE_PIN_HANDOFF.receive().await
}

const POLL_PERIOD_MS: u64 = 15;
const GAUGE_POLL_TICKS: u16 = 200;
const GAUGE_RETRY_TICKS: u16 = 667;
const FALLBACK_BATTERY_MV: u16 = 0;
const FALLBACK_BATTERY_SOC: u8 = 100;

struct DigitalButton {
    debounce_ticks: u8,
    repeat_cooldown: u8,
}

impl DigitalButton {
    const DEBOUNCE_TICKS: u8 = 2;
    const REPEAT_TICKS: u8 = 32;

    const fn new() -> Self {
        Self {
            debounce_ticks: 0,
            repeat_cooldown: 0,
        }
    }

    fn poll(&mut self, raw_pressed: bool) -> bool {
        if !raw_pressed {
            self.debounce_ticks = 0;
            self.repeat_cooldown = 0;
            return false;
        }
        if self.debounce_ticks < Self::DEBOUNCE_TICKS {
            self.debounce_ticks += 1;
            if self.debounce_ticks == Self::DEBOUNCE_TICKS {
                self.repeat_cooldown = Self::REPEAT_TICKS;
                return true;
            }
            return false;
        }
        if self.repeat_cooldown > 0 {
            self.repeat_cooldown -= 1;
            if self.repeat_cooldown == 0 {
                self.repeat_cooldown = Self::REPEAT_TICKS;
                return true;
            }
        }
        false
    }
}

async fn reset_gt911(
    touch_rst: &mut Output<'static>,
    touch_int: &mut Flex<'static>,
    int_high: bool,
) {
    touch_rst.set_low();
    touch_int.apply_output_config(&OutputConfig::default());
    touch_int.set_input_enable(false);
    if int_high {
        touch_int.set_high();
    } else {
        touch_int.set_low();
    }
    touch_int.set_output_enable(true);
    Timer::after_millis(10).await;

    touch_rst.set_high();
    Timer::after_millis(10).await;
    Timer::after_millis(50).await;

    touch_int.set_output_enable(false);
    touch_int.apply_input_config(&InputConfig::default().with_pull(Pull::None));
    touch_int.set_input_enable(true);
    Timer::after_millis(50).await;
}

async fn probe_gt911_addrs(i2c: &mut I2c<'static, Async>, addrs: [u8; 2]) -> Option<u8> {
    for addr in addrs {
        let mut probe = Gt911::with_addr(&mut *i2c, addr, TouchTransform::X4_PRO_LANDSCAPE);
        if matches!(probe.probe().await, Ok(true)) {
            return Some(addr);
        }
    }
    None
}

async fn read_gauge_metrics(
    gauge: &mut Cw2017<&mut I2c<'static, Async>>,
) -> (Option<u8>, Option<u16>) {
    let soc = gauge.read_soc().await.ok().flatten();
    let mv = gauge.read_millivolts().await.ok();
    (soc, mv)
}

fn emit_sample(button: Option<Button>, battery_mv: u16, battery_percent: u8) {
    if button.is_some() {
        fw_common::note_button_press();
    }
    let event = InputEvent::Sample {
        button,
        aux_raw: battery_mv,
        nav_raw: 0,
        page_raw: 0,
        battery_mv,
        battery_percent,
    };
    if fw_common::INPUT_EVENTS.try_send(event).is_ok() {
        return;
    }
    let _ = fw_common::INPUT_EVENTS.try_receive();
    if fw_common::INPUT_EVENTS.try_send(event).is_err() {
        esp_println::println!("s3-input: event queue full");
    }
}

fn emit_button(button: Button, last_battery: Option<(u16, u8)>) {
    let (mv, soc) = last_battery.unwrap_or((FALLBACK_BATTERY_MV, FALLBACK_BATTERY_SOC));
    emit_sample(Some(button), mv, soc);
}

#[embassy_executor::task]
pub async fn run(mut hw: InputHardware) {
    reset_gt911(&mut hw.touch_rst, &mut hw.touch_int, false).await;

    let rtc_init = {
        let mut rtc = Bm8563::with_addr(&mut hw.i2c, board::RTC_I2C_ADDR);
        let init = rtc.init().await;
        let time = rtc.read_time().await;
        (init, time)
    };
    esp_println::println!("s3-rtc: bm8563 init={:?} time={:?}", rtc_init.0, rtc_init.1,);

    let mut delay = Delay;
    let ready = {
        let mut gauge = Cw2017::with_addr(&mut hw.i2c, board::GAUGE_I2C_ADDR);
        gauge.ensure_profile(&mut delay, &X4_PRO_BATINFO).await
    };
    let mut gauge_ready = matches!(ready, Ok(true));
    let (soc_opt, mv_opt) = if gauge_ready {
        let mut gauge = Cw2017::with_addr(&mut hw.i2c, board::GAUGE_I2C_ADDR);
        read_gauge_metrics(&mut gauge).await
    } else {
        (None, None)
    };
    esp_println::println!(
        "s3-gauge: cw2017 ready={:?} soc={:?} mv={:?}",
        ready,
        soc_opt,
        mv_opt,
    );

    let mut last_battery: Option<(u16, u8)> = None;
    if let (Some(mv), Some(soc)) = (mv_opt, soc_opt) {
        last_battery = Some((mv, soc));
        emit_sample(None, mv, soc);
    }

    let mut touch_addr =
        probe_gt911_addrs(&mut hw.i2c, [board::TOUCH_I2C_ADDR, gt911::ALT_ADDR]).await;
    if touch_addr.is_none() {
        reset_gt911(&mut hw.touch_rst, &mut hw.touch_int, true).await;
        touch_addr = probe_gt911_addrs(&mut hw.i2c, [gt911::ALT_ADDR, board::TOUCH_I2C_ADDR]).await;
    }
    esp_println::println!(
        "s3-touch: gt911 addr=0x{:02X} detected={}",
        touch_addr.unwrap_or(0),
        touch_addr.is_some(),
    );

    let mut power_edge = PowerEdge::new();
    let mut left_btn = DigitalButton::new();
    let mut right_btn = DigitalButton::new();
    let mut last_charging: Option<bool> = None;
    let mut home_key_down = false;
    let mut down_pt: Option<TouchPoint> = None;
    let mut last_pt: Option<TouchPoint> = None;
    let mut gauge_ticks: u16 = 0;

    loop {
        if let Either::First(()) = select(
            WAKE_PIN_REQUESTS.receive(),
            Timer::after_millis(POLL_PERIOD_MS),
        )
        .await
        {
            WAKE_PIN_HANDOFF
                .send(WakePins {
                    power: hw.btn_power,
                    touch_power: hw.touch_power,
                })
                .await;
            return;
        }

        if power_edge.sample(hw.btn_power.is_low()) {
            emit_button(Button::Power, last_battery);
        }
        if left_btn.poll(hw.btn_left.is_low()) {
            emit_button(Button::Previous, last_battery);
        }
        if right_btn.poll(hw.btn_right.is_low()) {
            emit_button(Button::Next, last_battery);
        }

        let charging = hw.charger_stat.is_low();
        if last_charging != Some(charging) {
            last_charging = Some(charging);
            esp_println::println!("s3-power: charger_active={}", charging);
        }

        if let Some(addr) = touch_addr {
            let poll_res = {
                let mut touch =
                    Gt911::with_addr(&mut hw.i2c, addr, TouchTransform::X4_PRO_LANDSCAPE);
                touch.poll().await
            };
            if let Ok(Some(sample)) = poll_res {
                if sample.home_key {
                    home_key_down = true;
                } else if home_key_down {
                    home_key_down = false;
                    emit_button(Button::Back, last_battery);
                }

                if let Some(pt) = sample.point {
                    if down_pt.is_none() {
                        down_pt = Some(pt);
                    }
                    last_pt = Some(pt);
                } else if let (Some(start), Some(end)) = (down_pt.take(), last_pt.take()) {
                    let button = match gt911::classify_tap_or_swipe(start, end, uc8279_x4::WIDTH) {
                        TouchAction::Prev => Button::Previous,
                        TouchAction::Next => Button::Next,
                        TouchAction::Confirm => Button::Confirm,
                        TouchAction::Back | TouchAction::Home => Button::Back,
                    };
                    emit_button(button, last_battery);
                }
            }
        }

        gauge_ticks = gauge_ticks.wrapping_add(1);
        if gauge_ready {
            if gauge_ticks >= GAUGE_POLL_TICKS {
                gauge_ticks = 0;
                let sample = {
                    let mut gauge = Cw2017::with_addr(&mut hw.i2c, board::GAUGE_I2C_ADDR);
                    gauge.sample().await
                };
                if let Ok(Some((mv, soc))) = sample {
                    if last_battery.is_none_or(|(_, prev_soc)| prev_soc.abs_diff(soc) >= 2) {
                        last_battery = Some((mv, soc));
                        emit_sample(None, mv, soc);
                    }
                }
            }
        } else if gauge_ticks >= GAUGE_RETRY_TICKS {
            gauge_ticks = 0;
            let retry_ready = {
                let mut gauge = Cw2017::with_addr(&mut hw.i2c, board::GAUGE_I2C_ADDR);
                gauge.ensure_profile(&mut delay, &X4_PRO_BATINFO).await
            };
            if matches!(retry_ready, Ok(true)) {
                gauge_ready = true;
                let sample = {
                    let mut gauge = Cw2017::with_addr(&mut hw.i2c, board::GAUGE_I2C_ADDR);
                    gauge.sample().await
                };
                if let Ok(Some((mv, soc))) = sample {
                    last_battery = Some((mv, soc));
                    emit_sample(None, mv, soc);
                }
            }
        }
    }
}
