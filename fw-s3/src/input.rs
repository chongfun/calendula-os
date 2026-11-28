use core::sync::atomic::{AtomicU32, Ordering};

use app_core::buttons::PowerEdge;
use app_core::touch::UiAction;
use app_core::{
    physical_key_for, AppView, Button, DisplayOrientation, FrontButtons, InputEvent, RenderRequest,
};
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Delay, Instant, Timer};
use esp_hal::gpio::{Flex, Input, InputConfig, Output, OutputConfig, Pull};
use esp_hal::i2c::master::I2c;
use esp_hal::{Async, Blocking};
use fw_common::touch_input::TouchPollController;
use hal_ext::bm8563::Bm8563;
use hal_ext::cw2017::{Cw2017, X4_PRO_BATINFO};
use hal_ext::gt911::{self, Gt911, HomeKeyEvent, HomeKeyTracker, TouchTransform};
use static_cell::ConstStaticCell;

use crate::board;

#[derive(Clone, Copy)]
struct PhysicalButtonContext {
    view: AppView,
    orientation: DisplayOrientation,
    front_buttons: FrontButtons,
}

const DEFAULT_PHYSICAL_CONTEXT: u32 = pack_physical_context(PhysicalButtonContext {
    view: AppView::Home,
    orientation: DisplayOrientation::PortraitButtonsLeft,
    front_buttons: FrontButtons::PagesRight,
});
static PHYSICAL_BUTTON_CONTEXT: AtomicU32 = AtomicU32::new(DEFAULT_PHYSICAL_CONTEXT);

const fn pack_physical_context(ctx: PhysicalButtonContext) -> u32 {
    let view_bits = match ctx.view {
        AppView::Home => 0,
        AppView::Library => 1,
        AppView::Reading => 2,
        AppView::Chapters => 3,
        AppView::Wireless => 4,
        AppView::Settings => 5,
    };
    let orient_bits = match ctx.orientation {
        DisplayOrientation::LandscapeButtonsBottom => 0,
        DisplayOrientation::LandscapeButtonsTop => 1,
        DisplayOrientation::PortraitButtonsLeft => 2,
        DisplayOrientation::PortraitButtonsRight => 3,
    };
    let front_bits = match ctx.front_buttons {
        FrontButtons::PagesRight => 0,
        FrontButtons::PagesLeft => 1,
    };
    view_bits | (orient_bits << 4) | (front_bits << 8)
}

fn unpack_physical_context(bits: u32) -> PhysicalButtonContext {
    let view = match bits & 0xF {
        1 => AppView::Library,
        2 => AppView::Reading,
        3 => AppView::Chapters,
        4 => AppView::Wireless,
        5 => AppView::Settings,
        _ => AppView::Home,
    };
    let orientation = match (bits >> 4) & 0xF {
        0 => DisplayOrientation::LandscapeButtonsBottom,
        1 => DisplayOrientation::LandscapeButtonsTop,
        3 => DisplayOrientation::PortraitButtonsRight,
        _ => DisplayOrientation::PortraitButtonsLeft,
    };
    let front_buttons = if ((bits >> 8) & 1) == 1 {
        FrontButtons::PagesLeft
    } else {
        FrontButtons::PagesRight
    };
    PhysicalButtonContext {
        view,
        orientation,
        front_buttons,
    }
}

pub fn set_physical_button_context(request: &RenderRequest) {
    PHYSICAL_BUTTON_CONTEXT.store(
        pack_physical_context(PhysicalButtonContext {
            view: request.view,
            orientation: request.orientation,
            front_buttons: request.front_buttons,
        }),
        Ordering::Relaxed,
    );
}

const fn transform_for_orientation(orientation: DisplayOrientation) -> TouchTransform {
    match orientation {
        DisplayOrientation::LandscapeButtonsBottom => TouchTransform::X4_PRO_LANDSCAPE_BOTTOM,
        DisplayOrientation::LandscapeButtonsTop => TouchTransform::X4_PRO_LANDSCAPE,
        DisplayOrientation::PortraitButtonsLeft | DisplayOrientation::PortraitButtonsRight => {
            TouchTransform::X4_PRO_PORTRAIT
        }
    }
}

pub struct WakePins {
    pub power: Input<'static>,
    pub touch_power: Output<'static>,
}

pub struct InputHardware {
    pub i2c: I2c<'static, Blocking>,
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
static WAKE_PIN_RETURN: Channel<CriticalSectionRawMutex, WakePins, 1> = Channel::new();
static SLEEP_INPUT_FENCE_REQUESTS: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
static SLEEP_INPUT_FENCE_ACKS: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();

pub async fn take_wake_pins() -> WakePins {
    WAKE_PIN_REQUESTS.send(()).await;
    WAKE_PIN_HANDOFF.receive().await
}

/// Give ownership back after a canceled sleep handshake. The input task stays
/// alive while the pins are on loan so other buttons and the GT911 still
/// contribute producer-side activity before the final sleep decision.
pub async fn return_wake_pins(pins: WakePins) {
    WAKE_PIN_RETURN.send(pins).await;
}

/// Require a fresh input-task poll before the power task's final generation
/// check. Unlike a fixed delay, this acknowledgement is issued only after
/// Home/touch polling and a raw (pre-debounce) page-button check complete.
pub async fn fence_input_before_sleep() {
    SLEEP_INPUT_FENCE_REQUESTS.send(()).await;
    SLEEP_INPUT_FENCE_ACKS.receive().await;
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
    // GT911 datasheet reset timing: hold INT low for >=50 ms after address
    // latch before releasing it to floating input.
    touch_int.set_low();
    Timer::after_millis(50).await;

    touch_int.set_output_enable(false);
    touch_int.apply_input_config(&InputConfig::default().with_pull(Pull::None));
    touch_int.set_input_enable(true);
    Timer::after_millis(50).await;
}

async fn probe_gt911_addrs(i2c: &mut I2c<'static, Async>, addrs: [u8; 2]) -> Option<u8> {
    for addr in addrs {
        let mut probe = Gt911::with_addr(&mut *i2c, addr, TouchTransform::X4_PRO_PORTRAIT);
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

fn send_input_event(event: InputEvent) {
    if fw_common::INPUT_EVENTS.try_send(event).is_ok() {
        return;
    }
    // Never evict a pending button or touch event just to publish battery telemetry.
    if matches!(event, InputEvent::Sample { button: None, .. }) {
        return;
    }
    let _ = fw_common::INPUT_EVENTS.try_receive();
    if fw_common::INPUT_EVENTS.try_send(event).is_err() {
        esp_println::println!("s3-input: event queue full");
    }
}

#[cfg(feature = "bench-selftest")]
pub(crate) fn log_injected_input(button: Button, action: Button) {
    bench_log!(
        "bench: input button={:?} action={:?} aux={} nav={} page_raw={} t_ms={}",
        Some(button),
        action,
        2000,
        0,
        0,
        embassy_time::Instant::now().as_millis(),
    );
}

fn emit_sample(
    button: Option<Button>,
    battery_mv: u16,
    battery_percent: u8,
    record_activity: bool,
) {
    if button.is_some() {
        bench_log!(
            "bench: input button={:?} aux={} nav={} page_raw={} t_ms={}",
            button,
            battery_mv,
            0,
            0,
            embassy_time::Instant::now().as_millis(),
        );
        fw_common::note_button_press();
        if record_activity && button != Some(Button::Power) {
            // Power requests sleep; page/navigation input cancels it.
            // Home is counted on raw press, before its delayed classifier.
            let view =
                unpack_physical_context(PHYSICAL_BUTTON_CONTEXT.load(Ordering::Relaxed)).view;
            fw_common::note_physical_input_activity(view);
        }
    }
    send_input_event(InputEvent::Sample {
        button,
        aux_raw: battery_mv,
        nav_raw: 0,
        page_raw: 0,
        battery_mv,
        battery_percent,
    });
}

fn emit_button(button: Button, last_battery: Option<(u16, u8)>) {
    let (mv, soc) = last_battery.unwrap_or((FALLBACK_BATTERY_MV, FALLBACK_BATTERY_SOC));
    emit_sample(Some(button), mv, soc, true);
}

fn emit_home_single_tap(ctx: PhysicalButtonContext, last_battery: Option<(u16, u8)>) {
    if let Some(button) =
        physical_key_for(ctx.view, ctx.orientation, ctx.front_buttons, Button::Back)
    {
        esp_println::println!(
            "s3-touch: home_key view={:?} orient={:?} btn={:?}",
            ctx.view,
            ctx.orientation,
            button
        );
        let (mv, soc) = last_battery.unwrap_or((FALLBACK_BATTERY_MV, FALLBACK_BATTERY_SOC));
        // The Home press itself was already recorded by the GT911 poll.
        emit_sample(Some(button), mv, soc, false);
    }
}

fn emit_toggle_frontlight(last_battery: Option<(u16, u8)>) {
    fw_common::note_button_press();
    // Both Home presses were recorded before classification. Do not create
    // a new activity generation when the delayed double-tap resolves.
    let (mv, soc) = last_battery.unwrap_or((FALLBACK_BATTERY_MV, FALLBACK_BATTERY_SOC));
    esp_println::println!("s3-touch: home_key double_tap toggle_frontlight");
    send_input_event(InputEvent::ToggleFrontlight {
        battery_mv: mv,
        battery_percent: soc,
    });
}

fn emit_touch_action(view: AppView, action: UiAction, frame_epoch: u32) {
    let meta = fw_common::published_touch_frame_meta();
    if !meta.active || meta.overflowed || meta.epoch != frame_epoch {
        return;
    }
    fw_common::note_button_press();
    fw_common::note_physical_input_activity(view);
    send_input_event(InputEvent::Touch {
        action,
        frame_epoch,
    });
}

#[embassy_executor::task]
pub async fn run(mut hw: InputHardware) {
    // Convert I2C0 into async mode inside the interrupt-executor task so the
    // `I2c<'static, Blocking>` handle can be sent across `SendSpawner::spawn`
    // and the I2C0 interrupt handler binds on the executing core.
    let mut i2c = hw.i2c.into_async();
    reset_gt911(&mut hw.touch_rst, &mut hw.touch_int, false).await;

    let rtc_init = {
        let mut rtc = Bm8563::with_addr(&mut i2c, board::RTC_I2C_ADDR);
        let init = rtc.init().await;
        let time = rtc.read_time().await;
        (init, time)
    };
    esp_println::println!("s3-rtc: bm8563 init={:?} time={:?}", rtc_init.0, rtc_init.1,);

    let mut delay = Delay;
    let ready = {
        let mut gauge = Cw2017::with_addr(&mut i2c, board::GAUGE_I2C_ADDR);
        gauge.ensure_profile(&mut delay, &X4_PRO_BATINFO).await
    };
    let mut gauge_ready = matches!(ready, Ok(true));
    let (soc_opt, mv_opt) = if gauge_ready {
        let mut gauge = Cw2017::with_addr(&mut i2c, board::GAUGE_I2C_ADDR);
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
        emit_sample(None, mv, soc, false);
    }

    let mut touch_addr =
        probe_gt911_addrs(&mut i2c, [board::TOUCH_I2C_ADDR, gt911::ALT_ADDR]).await;
    if touch_addr.is_none() {
        reset_gt911(&mut hw.touch_rst, &mut hw.touch_int, true).await;
        touch_addr = probe_gt911_addrs(&mut i2c, [gt911::ALT_ADDR, board::TOUCH_I2C_ADDR]).await;
    }
    esp_println::println!(
        "s3-touch: gt911 addr=0x{:02X} detected={}",
        touch_addr.unwrap_or(0),
        touch_addr.is_some(),
    );

    // Keep the wake-capable pins separate so input polling can continue
    // while the power task temporarily owns them.
    let mut wake_pins = Some(WakePins {
        power: hw.btn_power,
        touch_power: hw.touch_power,
    });
    let mut power_edge = PowerEdge::new();
    let mut left_btn = DigitalButton::new();
    let mut right_btn = DigitalButton::new();
    let mut last_charging: Option<bool> = None;
    let mut home_key = HomeKeyTracker::new();
    static TOUCH_CONTROLLER: ConstStaticCell<TouchPollController> =
        ConstStaticCell::new(TouchPollController::new());
    let touch_ctrl = TOUCH_CONTROLLER.take();
    const TOUCH_RETRY_TICKS: u16 = 500;
    let mut touch_retry_ticks: u16 = 0;
    let mut gauge_ticks: u16 = 0;

    loop {
        let mut fence_requested = false;
        if wake_pins.is_some() {
            if let Either::First(()) = select(
                WAKE_PIN_REQUESTS.receive(),
                Timer::after_millis(POLL_PERIOD_MS),
            )
            .await
            {
                // A pending Home press was already counted at touch-down.
                // Keep its classification state: a canceled handoff must
                // resume polling without inventing or losing a Home action.
                WAKE_PIN_HANDOFF
                    .send(wake_pins.take().expect("wake pins owned"))
                    .await;
                continue;
            }
        } else {
            match select3(
                WAKE_PIN_RETURN.receive(),
                SLEEP_INPUT_FENCE_REQUESTS.receive(),
                Timer::after_millis(POLL_PERIOD_MS),
            )
            .await
            {
                Either3::First(pins) => {
                    wake_pins = Some(pins);
                    // Re-arm edge tracking after the power task owned the pin.
                    power_edge = PowerEdge::new();
                    continue;
                }
                Either3::Second(()) => fence_requested = true,
                Either3::Third(()) => {}
            }
        }

        let phys_ctx = unpack_physical_context(PHYSICAL_BUTTON_CONTEXT.load(Ordering::Relaxed));
        let poll_now_ms = Instant::now().as_millis() as u32;
        if wake_pins
            .as_ref()
            .is_some_and(|pins| power_edge.sample(pins.power.is_low()))
        {
            touch_ctrl.cancel_for_physical_input(true, poll_now_ms);
            fw_common::set_touch_contact_veto(false);
            if home_key.flush_pending() {
                emit_home_single_tap(phys_ctx, last_battery);
            }
            emit_button(Button::Power, last_battery);
        }
        if left_btn.poll(hw.btn_left.is_low()) {
            touch_ctrl.cancel_for_physical_input(false, poll_now_ms);
            fw_common::set_touch_contact_veto(touch_ctrl.contact_veto());
            if home_key.flush_pending() {
                emit_home_single_tap(phys_ctx, last_battery);
            }
            emit_button(Button::PagePrevious, last_battery);
        }
        if right_btn.poll(hw.btn_right.is_low()) {
            touch_ctrl.cancel_for_physical_input(false, poll_now_ms);
            fw_common::set_touch_contact_veto(touch_ctrl.contact_veto());
            if home_key.flush_pending() {
                emit_home_single_tap(phys_ctx, last_battery);
            }
            emit_button(Button::PageNext, last_battery);
        }

        let charging = hw.charger_stat.is_low();
        if last_charging != Some(charging) {
            last_charging = Some(charging);
            esp_println::println!("s3-power: charger_active={}", charging);
        }

        if let Some(addr) = touch_addr {
            let pre_meta = fw_common::published_touch_frame_meta();
            let poll_res = {
                let mut touch = Gt911::with_addr(&mut i2c, addr, TouchTransform::X4_PRO_LANDSCAPE);
                touch.poll().await
            };
            let now_ms = Instant::now().as_millis() as u32;
            let post_meta = fw_common::published_touch_frame_meta();
            let post_phys_ctx =
                unpack_physical_context(PHYSICAL_BUTTON_CONTEXT.load(Ordering::Relaxed));
            match poll_res {
                Ok(Some(sample)) => {
                    if home_key.begins_press(sample.home_key) {
                        // Record physical interaction at press down, not when
                        // SingleTap/DoubleTap resolves 350ms later, and cancel
                        // any conflicting active touchscreen gesture.
                        touch_ctrl.cancel_for_physical_input(false, now_ms);
                        fw_common::set_touch_contact_veto(touch_ctrl.contact_veto());
                        fw_common::note_physical_input_activity(post_phys_ctx.view);
                    }
                    match home_key.update(sample.home_key, now_ms) {
                        HomeKeyEvent::SingleTap => {
                            emit_home_single_tap(post_phys_ctx, last_battery)
                        }
                        HomeKeyEvent::DoubleTap => emit_toggle_frontlight(last_battery),
                        HomeKeyEvent::None => {}
                    }

                    let outcome = touch_ctrl.on_sample(
                        sample
                            .raw_point
                            .map(|pt| app_core::touch::TouchPoint::new(pt.x, pt.y)),
                        now_ms,
                        pre_meta,
                        post_meta,
                        fw_common::snapshot_touch_frame,
                        |ori, raw| {
                            let mapped = transform_for_orientation(ori)
                                .map_raw(hal_ext::gt911::TouchPoint { x: raw.x, y: raw.y });
                            app_core::touch::TouchPoint::new(mapped.x, mapped.y)
                        },
                    );
                    fw_common::set_touch_contact_veto(touch_ctrl.contact_veto());
                    let touch_view = outcome.activity_view.unwrap_or(post_phys_ctx.view);
                    if outcome.record_physical_activity {
                        fw_common::note_physical_input_activity(touch_view);
                    }
                    if let Some((action, frame_epoch)) = outcome.action {
                        bench_log!("s3-touch: action={:?} epoch={}", action, frame_epoch,);
                        if home_key.flush_pending() {
                            emit_home_single_tap(post_phys_ctx, last_battery);
                        }
                        emit_touch_action(touch_view, action, frame_epoch);
                    }
                }
                Ok(None) => {
                    let action = touch_ctrl.on_no_report(now_ms, pre_meta, post_meta);
                    fw_common::set_touch_contact_veto(touch_ctrl.contact_veto());
                    if let Some((action, frame_epoch)) = action {
                        bench_log!("s3-touch: hold action={:?} epoch={}", action, frame_epoch,);
                        if home_key.flush_pending() {
                            emit_home_single_tap(post_phys_ctx, last_battery);
                        }
                        emit_touch_action(touch_ctrl.active_view(), action, frame_epoch);
                    }
                    if home_key.tick(now_ms) == HomeKeyEvent::SingleTap {
                        emit_home_single_tap(post_phys_ctx, last_battery);
                    }
                }
                Err(_) => {
                    touch_ctrl.on_transport_error();
                    fw_common::set_touch_contact_veto(touch_ctrl.contact_veto());
                    if home_key.tick(now_ms) == HomeKeyEvent::SingleTap {
                        emit_home_single_tap(post_phys_ctx, last_battery);
                    }
                }
            }
        } else {
            touch_retry_ticks = touch_retry_ticks.wrapping_add(1);
            if touch_retry_ticks >= TOUCH_RETRY_TICKS {
                touch_retry_ticks = 0;
                touch_addr =
                    probe_gt911_addrs(&mut i2c, [board::TOUCH_I2C_ADDR, gt911::ALT_ADDR]).await;
                if let Some(addr) = touch_addr {
                    esp_println::println!("s3-touch: gt911 recovered addr=0x{:02X}", addr);
                }
            }
        }

        gauge_ticks = gauge_ticks.wrapping_add(1);
        if gauge_ready {
            if gauge_ticks >= GAUGE_POLL_TICKS {
                gauge_ticks = 0;
                let sample = {
                    let mut gauge = Cw2017::with_addr(&mut i2c, board::GAUGE_I2C_ADDR);
                    gauge.sample().await
                };
                if let Ok(Some((mv, soc))) = sample {
                    if last_battery.is_none_or(|(_, prev_soc)| prev_soc.abs_diff(soc) >= 2) {
                        last_battery = Some((mv, soc));
                        emit_sample(None, mv, soc, false);
                    }
                }
            }
        } else if gauge_ticks >= GAUGE_RETRY_TICKS {
            gauge_ticks = 0;
            let retry_ready = {
                let mut gauge = Cw2017::with_addr(&mut i2c, board::GAUGE_I2C_ADDR);
                gauge.ensure_profile(&mut delay, &X4_PRO_BATINFO).await
            };
            if matches!(retry_ready, Ok(true)) {
                gauge_ready = true;
                let sample = {
                    let mut gauge = Cw2017::with_addr(&mut i2c, board::GAUGE_I2C_ADDR);
                    gauge.sample().await
                };
                if let Ok(Some((mv, soc))) = sample {
                    last_battery = Some((mv, soc));
                    emit_sample(None, mv, soc, false);
                }
            }
        }

        if fence_requested {
            // A freshly pressed page button may not yet have reached the
            // second 15ms debounce sample. Check the raw levels immediately
            // before acknowledging the poll instead of waiting for an event.
            if app_core::sleep_fence_page_pressed(hw.btn_left.is_low(), hw.btn_right.is_low()) {
                fw_common::note_physical_input_activity(phys_ctx.view);
            }
            SLEEP_INPUT_FENCE_ACKS.send(()).await;
        }
    }
}
