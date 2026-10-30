use crate::{
    AppView, DisplayCommand, PowerEvent, SleepResume, SleepTrigger, DISPLAY_COMMANDS, POWER_EVENTS,
    WAKE_PIN_HANDOFF, WAKE_PIN_REQUESTS,
};
use embassy_futures::select::{select3, Either3};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals::{GPIO3, LPWR};
use esp_hal::rtc_cntl::Rtc;

/// Resolves to an armed abrupt-reset deadline (ms) in `powercut-selftest`
/// builds; never resolves otherwise, leaving the select below the plain
/// two-way wait release builds always had.
async fn powercut_arm() -> u64 {
    #[cfg(feature = "powercut-selftest")]
    {
        crate::powercut::POWERCUT_ARM.wait().await
    }
    #[cfg(not(feature = "powercut-selftest"))]
    {
        core::future::pending().await
    }
}

/// Idle time with no input before the device puts itself into deep sleep,
/// tiered by the view the last input left the app in. Reading keeps the
/// long leash for slow readers; the shell views sleep much sooner — the
/// seeded deep-sleep wake is a ~1.5 s one-flicker refresh, so an early
/// sleep costs the user little and saves the 160 MHz / 66 Hz-polling idle
/// tail (order 10–20 mA) on every walk-away. Wireless keeps the long
/// leash too: a running sync session is legitimately button-free.
const fn idle_timeout(view: AppView) -> Duration {
    Duration::from_secs(app_core::idle_timeout_secs(view))
}

/// Milliseconds from `earlier` to `later`, zero if the order is reversed.
#[cfg(feature = "light-sleep")]
fn ms_between(earlier: Instant, later: Instant) -> u64 {
    later.saturating_duration_since(earlier).as_millis()
}

/// The static-page light-sleep policy, live only with the `light-sleep`
/// feature. The power task is the only caller of the sleep primitive; this
/// is the bookkeeping around that call. See `app_core::light_sleep` for the
/// decisions and `docs/ARCHITECTURE.md` ("Idle light sleep") for the
/// contract with the other tasks.
#[cfg(feature = "light-sleep")]
struct IdleSleeper {
    stats: app_core::light_sleep::LightSleepStats,
    /// RTC microseconds at the previous evaluation; the wall-clock delta to
    /// the next one is charged to eligible time when `last_eligible` held.
    last_eval_rtc_us: u64,
    last_eligible: bool,
    /// RTC microseconds of the last telemetry line.
    last_report_rtc_us: u64,
    /// Entries at the last telemetry line, so a quiet device does not
    /// repeat itself.
    last_report_entered: u64,
    last_usb: app_core::light_sleep::UsbHostState,
}

#[cfg(feature = "light-sleep")]
impl IdleSleeper {
    /// Telemetry cadence while the policy is live, in RTC wall-clock.
    const REPORT_EVERY_US: u64 = 60_000_000;

    fn new(rtc: &Rtc<'_>) -> Self {
        let now = hal_ext::rtc::rtc_now_us(rtc);
        Self {
            stats: app_core::light_sleep::LightSleepStats::new(),
            last_eval_rtc_us: now,
            last_eligible: false,
            last_report_rtc_us: now,
            last_report_entered: 0,
            last_usb: app_core::light_sleep::UsbHostState::Unknown,
        }
    }

    /// Evaluate admission once and, when admitted, take one slice. Returns
    /// when the loop should look again; `deadline` is the deep-sleep idle
    /// leash, pulled forward by however much wall-clock the Embassy clock
    /// missed while the chip was away.
    ///
    /// The final check and the sleep share one critical section. An
    /// inhibitor can be taken from the interrupt-priority input executor
    /// between a check and a call that are not atomic; with interrupts off
    /// the count read is the count the sleep begins under. Light sleep is
    /// entered and left with interrupts disabled in ESP-IDF too; the wake is
    /// the RTC controller restarting the clocks, not an interrupt.
    fn service(
        &mut self,
        rtc: &mut Rtc<'_>,
        now: Instant,
        last_activity: Instant,
        deadline: &mut Instant,
    ) -> Instant {
        use crate::power_state;
        use app_core::light_sleep::{
            admit, paused_clock_correction_ms, reevaluate_after_ms, AdmissionInputs, Refusal,
            UsbHostState, SLICE_MS,
        };

        let rtc_now = hal_ext::rtc::rtc_now_us(rtc);
        if self.last_eligible {
            self.stats
                .record_eligible(rtc_now.saturating_sub(self.last_eval_rtc_us) / 1_000);
        }
        self.last_eval_rtc_us = rtc_now;

        let usb = power_state::usb_state();
        if usb != self.last_usb {
            self.last_usb = usb;
            // A host just arrived: it is the only reader this line has, and
            // the untethered run it summarises just ended.
            if usb == UsbHostState::Present {
                self.report(rtc_now, true);
            }
        }
        self.report(rtc_now, false);

        // Idle is measured from the later of the app's last activity event
        // and the input task's last emitted key: the key is published before
        // the app sees it, so a press mid-evaluation is already counted.
        let last_input = Instant::from_millis(power_state::last_input_ms());
        let idle_since = if last_input > last_activity {
            last_input
        } else {
            last_activity
        };
        let idle_ms = ms_between(idle_since, now);
        let inputs = AdmissionInputs {
            supported: power_state::device_supports_light_sleep(),
            idle_ms,
            usb,
            inhibitors: power_state::inhibitor_count(),
            debounce_pending: power_state::is_debounce_pending(),
        };
        let outcome = match admit(inputs) {
            Ok(()) => critical_section::with(|_| {
                // Re-read under interrupts-off; see the method comment.
                if power_state::inhibitor_count() != 0 {
                    return Err(Refusal::Inhibited);
                }
                if power_state::is_debounce_pending() {
                    return Err(Refusal::Debounce);
                }
                let before = Instant::now();
                let slice = hal_ext::rtc::light_sleep_timer(
                    rtc,
                    core::time::Duration::from_millis(SLICE_MS),
                );
                let after = Instant::now();
                Ok((slice, ms_between(before, after)))
            }),
            Err(refusal) => Err(refusal),
        };

        match outcome {
            Ok((slice, embassy_elapsed_ms)) => {
                let rtc_elapsed_ms = slice.rtc_elapsed_ms();
                self.stats.record_slice(SLICE_MS, rtc_elapsed_ms);
                self.last_eligible = true;
                // Pull the idle leash forward by the wall-clock the Embassy
                // clock did not see, so ten minutes of reading-page idle is
                // ten minutes on the RTC rather than ten minutes awake.
                let lost = paused_clock_correction_ms(rtc_elapsed_ms, embassy_elapsed_ms);
                *deadline = deadline
                    .checked_sub(Duration::from_millis(lost))
                    .unwrap_or(Instant::MIN);
                // One poll per slice: the input task's 15 ms wait did not
                // advance while the chip slept.
                power_state::POLL_NUDGE.signal(());
                // Look again at once, after the select has drained events.
                Instant::now()
            }
            Err(refusal) => {
                self.stats.record_refusal(refusal);
                self.last_eligible = refusal.counts_as_eligible();
                let wait_ms = reevaluate_after_ms(refusal, idle_ms);
                now.checked_add(Duration::from_millis(wait_ms))
                    .unwrap_or(Instant::MAX)
            }
        }
    }

    /// One `bench: light_sleep` line, on the report cadence or on demand.
    /// Quiet while nothing has changed.
    fn report(&mut self, rtc_now: u64, force: bool) {
        let due = rtc_now.saturating_sub(self.last_report_rtc_us) >= Self::REPORT_EVERY_US;
        if !force && !due {
            return;
        }
        if !force && self.stats.entered == self.last_report_entered {
            return;
        }
        self.last_report_rtc_us = rtc_now;
        self.last_report_entered = self.stats.entered;
        let s = &self.stats;
        bench_log!(
            "bench: light_sleep entered={} requested_ms={} elapsed_ms={} timer_wakes={} rejected={} refused_inhibited={} refused_usb_unknown={} refused_usb_present={} refused_debounce={} refused_not_idle={} eligible_ms={} residency_permille={} rtc_ms={} t_ms={}",
            s.entered,
            s.requested_ms,
            s.elapsed_ms,
            s.timer_wakes,
            s.rejected,
            s.refused_inhibited,
            s.refused_usb_unknown,
            s.refused_usb_present,
            s.refused_debounce,
            s.refused_not_idle,
            s.eligible_ms,
            s.residency_permille().unwrap_or(0),
            rtc_now / 1_000,
            Instant::now().as_millis(),
        );
    }
}

/// Without the feature: no policy, no sleep, and the loop below waits on
/// the idle leash exactly as it did before the policy existed.
#[cfg(not(feature = "light-sleep"))]
struct IdleSleeper;

#[cfg(not(feature = "light-sleep"))]
impl IdleSleeper {
    fn new(_rtc: &Rtc<'_>) -> Self {
        Self
    }

    fn service(
        &mut self,
        _rtc: &mut Rtc<'_>,
        _now: Instant,
        _last_activity: Instant,
        deadline: &mut Instant,
    ) -> Instant {
        *deadline
    }
}

#[embassy_executor::task]
pub async fn run(lpwr: LPWR<'static>) {
    esp_println::println!("power: started");
    let mut rtc = Rtc::new(lpwr);
    // The Phase 0 proof runs here, before any policy, because this task is
    // the RTC's only owner: the selftest sleeps the chip through the same
    // primitive the policy would use, and hands the handle back for the
    // deep-sleep shutdown the recipe asks for afterwards.
    #[cfg(feature = "light-sleep-selftest")]
    crate::light_sleep_selftest::run(&mut rtc).await;
    // Boot lands on the Home shell, so start on the short leash.
    let mut idle = idle_timeout(AppView::Home);
    let mut deadline = Instant::now() + idle;
    // The last moment the app or the display reported anything happening:
    // the light-sleep idle threshold counts from here (or from the input
    // task's last key, whichever is later).
    let mut last_activity = Instant::now();
    let mut sleeper = IdleSleeper::new(&rtc);
    // Names each sleep request. An Activity press can abandon a handshake
    // while its sleep command is still queued or running; the next
    // handshake must not adopt that sleep's terminal acknowledgement,
    // so every request mints a fresh generation and acknowledgements
    // carry it back.
    let mut sleep_generation: u32 = 0;

    loop {
        // One admission pass. With the policy on this may put the chip to
        // sleep for a slice before returning; either way it says when to
        // look again, and the idle leash is the other bound on the wait.
        let now = Instant::now();
        let reevaluate_at = sleeper.service(&mut rtc, now, last_activity, &mut deadline);
        let wait_until = if reevaluate_at < deadline {
            reevaluate_at
        } else {
            deadline
        };
        match select3(
            POWER_EVENTS.receive(),
            Timer::at(wait_until),
            powercut_arm(),
        )
        .await
        {
            Either3::First(event) => match event {
                // Any input, or a storage answer that changes the view,
                // pushes the idle deadline back out, at the leash of the view
                // the app landed in.
                PowerEvent::Activity(view) => {
                    last_activity = Instant::now();
                    idle = idle_timeout(view);
                    deadline = Instant::now() + idle;
                }
                // Browser traffic pushes the idle deadline out on the Wireless
                // leash and can abort an idle sleep handshake without overriding
                // an explicit power-button sleep.
                PowerEvent::BrowserActivity => {
                    last_activity = Instant::now();
                    idle = idle_timeout(AppView::Wireless);
                    deadline = Instant::now() + idle;
                }
                // Power button: sleep on demand.
                PowerEvent::SleepNow => {
                    // Only reached if a late button press aborted the
                    // handshake; resume on that press's idle tier.
                    sleep_generation = sleep_generation.wrapping_add(1);
                    idle =
                        enter_sleep(&mut rtc, sleep_generation, SleepTrigger::Explicit, idle).await;
                    last_activity = Instant::now();
                    deadline = Instant::now() + idle;
                }
                // A settled paint is the end of a burst of work: the
                // light-sleep threshold restarts so the prestage and
                // chapter tracking behind it run at full speed.
                PowerEvent::DisplaySettled => {
                    last_activity = Instant::now();
                }
                PowerEvent::DisplayAsleep(_)
                | PowerEvent::DisplayRefreshFailed
                | PowerEvent::DisplaySleepFailed(_) => {}
            },
            // The timer: a re-evaluation tick with the leash still ahead, or
            // the idle timeout elapsed with no activity.
            Either3::Second(_) => {
                if Instant::now() < deadline {
                    continue;
                }
                if crate::upload::UPLOAD_IN_FLIGHT.load(portable_atomic::Ordering::SeqCst) {
                    esp_println::println!("power: idle timeout deferred; upload in flight");
                    idle = idle_timeout(AppView::Wireless);
                    deadline = Instant::now() + idle;
                    continue;
                }
                esp_println::println!("power: idle timeout");
                sleep_generation = sleep_generation.wrapping_add(1);
                idle = enter_sleep(&mut rtc, sleep_generation, SleepTrigger::Idle, idle).await;
                last_activity = Instant::now();
                deadline = Instant::now() + idle;
            }
            // Test-only abrupt reset: arm the RTC watchdog and never feed
            // it. The reset fires from RTC hardware at the deadline no
            // matter what is running — including a blocking SD write —
            // which is the entire point of the durability campaign.
            Either3::Third(after_ms) => {
                #[cfg(feature = "powercut-selftest")]
                {
                    rtc.rwdt.enable();
                    rtc.rwdt.set_timeout(
                        esp_hal::rtc_cntl::RwdtStage::Stage0,
                        esp_hal::time::Duration::from_millis(after_ms),
                    );
                    rtc.rwdt.feed();
                    esp_println::println!("powercut: armed {} ms", after_ms);
                }
                #[cfg(not(feature = "powercut-selftest"))]
                let _ = after_ms;
            }
        }
    }
}

/// Renders the sleep screen, lets the display flush in-flight work and any
/// pending reading progress, then powers the SoC down into deep sleep with the
/// Power button as the wake source.
///
/// Deep sleep is terminal — the chip reboots on wake — so this only returns if
/// a button press (or a storage answer that changes the view, which sends the
/// same `Activity`), or renewed browser activity during an idle-initiated
/// sleep, arrives during the display handshake and aborts the transition, and
/// the returned value is the idle tier of the view that cancelled it (or
/// `AppView::Wireless` for browser activity), so the resumed idle clock ticks at
/// that leash. Waiting for `DisplayAsleep` before cutting power guarantees the
/// e-ink panel has settled on its sleep image and progress is safely on the SD card.
async fn enter_sleep(
    rtc: &mut Rtc<'_>,
    generation: u32,
    mut trigger: SleepTrigger,
    fallback_idle: Duration,
) -> Duration {
    let _ = fallback_idle;
    esp_println::println!("power: display sleep");
    DISPLAY_COMMANDS
        .send(DisplayCommand::Sleep {
            generation,
            trigger,
        })
        .await;

    loop {
        match POWER_EVENTS.receive().await {
            PowerEvent::DisplayAsleep(acked) if acked == generation => {
                esp_println::println!("power: deep sleep");
                #[cfg(all(feature = "device-x3", not(feature = "bench-selftest")))]
                {
                    use embassy_futures::select::{select, Either};

                    // Allow the UC8253 booster capacitors and panel electric field to fully
                    // discharge before cutting MCU power, preventing floating GPIOs from inducing
                    // parasitic charge leakage across the sleeping panel.
                    //
                    // The delay is cancellable: if the user interacts during settling (via navigation
                    // activity or pressing the Power button), abort the sleep transition, unpark the
                    // display task, and return to the active view. Other power events do not shorten
                    // the required settling interval.
                    let settle_deadline = Instant::now() + Duration::from_millis(1500);
                    loop {
                        match select(Timer::at(settle_deadline), POWER_EVENTS.receive()).await {
                            Either::First(_) => break,
                            Either::Second(event) => {
                                match app_core::sleep_settle_decision(
                                    event,
                                    trigger,
                                    fallback_idle.as_secs(),
                                ) {
                                    app_core::SleepSettleDecision::Cancel { idle_secs, resume } => {
                                        crate::signal_sleep_cancelled(generation, resume);
                                        return Duration::from_secs(idle_secs);
                                    }
                                    app_core::SleepSettleDecision::Ignore => {}
                                }
                            }
                        }
                    }
                }
                let mut button = take_wake_button().await;
                #[cfg(feature = "device-x3")]
                hold_display_pins_for_deep_sleep();
                // A bench scenario has to come back on its own: nothing can
                // reach a sleeping device but a reset, and a reset is a
                // reboot rather than a wake.
                #[cfg(feature = "bench-selftest")]
                hal_ext::rtc::enter_deep_sleep_button_or_timer(
                    rtc,
                    &mut button,
                    crate::bench_selftest::SLEEP_WAKE_AFTER,
                );
                #[cfg(not(feature = "bench-selftest"))]
                hal_ext::rtc::enter_deep_sleep_button(rtc, &mut button);
            }
            PowerEvent::Activity(view) => {
                // The press that lands here may arrive before, during, or
                // after the display task's sleep sequence. If it has already
                // parked behind its own acknowledgement, only this releases
                // it; if it has not parked yet, the signal latches and is
                // waiting when it does. Naming the generation is what keeps
                // a resume left over from a sleep the display task never
                // parked for out of a later one.
                crate::signal_sleep_cancelled(generation, SleepResume::Activity(view));
                return idle_timeout(view);
            }
            // Renewed browser traffic during an idle-initiated sleep cancels
            // the handshake so an in-flight transfer or portal request is
            // not interrupted. Explicit power-button sleep ignores this below
            // so network traffic cannot override the reader turning the
            // device off.
            PowerEvent::BrowserActivity if trigger == SleepTrigger::Idle => {
                crate::signal_sleep_cancelled(generation, SleepResume::BrowserActivity);
                return idle_timeout(AppView::Wireless);
            }
            PowerEvent::DisplaySleepFailed(acked) if acked == generation => {
                // The display task could not complete the sleep transition
                // (progress flush or panel handshake failed). Cutting power
                // anyway would lose reading position or freeze a mid-refresh
                // panel; stay awake and retry at the shell leash.
                esp_println::println!("power: display sleep failed; staying awake");
                return idle_timeout(AppView::Home);
            }
            // An explicit Power button press arriving while an idle sleep is in
            // flight promotes the trigger to Explicit so subsequent browser
            // keepalives cannot abort the user's intent to sleep.
            PowerEvent::SleepNow => {
                trigger = SleepTrigger::Explicit;
            }
            // Anything else is not this sleep's terminal acknowledgement.
            // A refresh failure belongs to a render queued ahead of our
            // Sleep command; a sleep acknowledgement with another
            // generation is the stale answer of a handshake abandoned on
            // Activity, whose Sleep command was still queued when ours was
            // sent (the four-slot channel permits both in flight).
            // Browser keepalives are ignored here during explicit sleep so
            // an active transfer cannot cancel an intentional sleep.
            // Abandoning the handshake over any of them would let our
            // sleep proceed unobserved: its DisplayAsleep would land in
            // the outer loop, leaving the panel dark with the MCU awake.
            // Keep waiting for the matching acknowledgement — the display
            // task answers every Sleep command with one.
            PowerEvent::DisplayAsleep(_)
            | PowerEvent::DisplaySleepFailed(_)
            | PowerEvent::DisplayRefreshFailed
            | PowerEvent::DisplaySettled
            | PowerEvent::BrowserActivity => {}
        }
    }
}

/// Takes sole ownership of the Power button (GPIO3) and re-materialises it as
/// a deep-sleep wake source.
///
/// The input task owns GPIO3 as an `Input<'static>` and polls it for the whole
/// run, so the pin has to change hands before it can be armed: this asks the
/// input task to stop polling and surrender that handle, waits for it, and
/// drops it. Only then is this task the pin's single owner, which is what
/// makes the steal below sound. The wait costs the terminal sleep path about
/// one 15 ms poll tick, plus any in-progress sample.
///
/// Dropping the `Input` leaves the pad configured as it was — esp-hal's pin
/// drivers have no `Drop` glue — so the button's pull-up survives the gap
/// until the wake source re-enables it.
async fn take_wake_button() -> GPIO3<'static> {
    WAKE_PIN_REQUESTS.send(()).await;
    // The received handle is a temporary: its scope ends at this semicolon,
    // which retires the last `Input` on GPIO3 before the steal below.
    WAKE_PIN_HANDOFF.receive().await;
    steal_wake_button()
}

/// Re-materialises the Power button (GPIO3) once its previous handle is gone.
#[expect(
    unsafe_code,
    reason = "Caller guarantees exclusive GPIO3 ownership via handoff protocol"
)]
fn steal_wake_button() -> GPIO3<'static> {
    // SAFETY: the caller has taken the input task's `Input<'static>` handle on
    // GPIO3 and dropped it, and the input task stops polling the pin before it
    // gives that handle up, so no other handle on GPIO3 is live. Reached only
    // on the terminal deep-sleep path, which never returns the pin to the
    // input task: the chip resets on wake.
    unsafe { GPIO3::steal() }
}

/// Holds EPD control and SPI pins in safe idle states through ESP32-C3 deep sleep on X3.
///
/// On the X3, battery power remains continuously connected to the UC8253 controller.
/// In deep sleep mode (0x07), the UC8253 wakes up on hardware reset (/RST on GPIO5) going low.
/// Holding GPIO5 HIGH via the RTC-domain pad hold (`RTC_CNTL_PAD_HOLD_REG`) ensures the
/// controller stays in deep sleep and does not reset or leak charge onto the panel.
///
/// - RTC Pads (GPIO4, GPIO5): GPIO0..5 reside in the low-power RTC domain, which remains powered
///   throughout deep sleep. Holding GPIO5 in `rtc_cntl.pad_hold()` guarantees the /RST line
///   remains actively driven HIGH across deep sleep and into early boot, preventing controller reset.
///
/// - Digital Pads (GPIO8, 10, 12, 21): Pull resistors are configured in IO_MUX (pull-ups for
///   active-low CS lines GPIO12 and GPIO21; pull-downs for SPI clock/data GPIO8 and GPIO10),
///   and individual digital pad holds are set in `RTC_CNTL_DIG_PAD_HOLD_REG`.
///   Note on deep sleep retention: Per Espressif documentation, digital pads (GPIO6..21) require
///   global digital auto-hold (`dg_pad_autohold_en`) to retain their latched states once the
///   digital core powers down in deep sleep. However, enabling `dg_pad_autohold_en` activates
///   `esp-hal`'s `isolate_digital_gpio()`, which disables input buffers across GPIO0..21 and
///   prevents the active-low Power button (GPIO3) from waking the MCU. Because `dg_pad_autohold_en`
///   must remain disabled, the digital pads are latched during the power-down transition but are
///   not guaranteed to retain active pull/drive states once the digital domain is unpowered.
///   Their deep-sleep behavior is treated as unverified until measured on physical hardware.
#[cfg(feature = "device-x3")]
#[allow(unsafe_code)]
fn hold_display_pins_for_deep_sleep() {
    let rtc_cntl = esp_hal::peripherals::LPWR::regs();
    let io_mux = esp_hal::peripherals::IO_MUX::regs();

    // Enable pull-ups on active-low control lines before sleep.
    // GPIO5: UC8253 /RST (active-low hardware reset)
    // GPIO21: UC8253 /CS (active-low chip select)
    // GPIO12: SD /CS (active-low chip select)
    io_mux
        .gpio(5)
        .modify(|_, w| w.fun_wpu().set_bit().fun_wpd().clear_bit());
    io_mux
        .gpio(21)
        .modify(|_, w| w.fun_wpu().set_bit().fun_wpd().clear_bit());
    io_mux
        .gpio(12)
        .modify(|_, w| w.fun_wpu().set_bit().fun_wpd().clear_bit());

    // Enable pull-downs on SPI lines to avoid floating gates.
    // GPIO8: SCK (Mode 0 idle low)
    // GPIO10: MOSI (idle low)
    io_mux
        .gpio(8)
        .modify(|_, w| w.fun_wpu().clear_bit().fun_wpd().set_bit());
    io_mux
        .gpio(10)
        .modify(|_, w| w.fun_wpu().clear_bit().fun_wpd().set_bit());

    // Hold RTC pads:
    // GPIO5 is UC8253 RST (RTC GPIO 5). Holding it HIGH in the RTC domain prevents spurious
    // hardware reset/wake across deep sleep and into early boot.
    // GPIO4 is UC8253 D/C (RTC GPIO 4).
    rtc_cntl
        .pad_hold()
        .modify(|_, w| w.gpio_pin5_hold().bit(true).gpio_pin4_hold().bit(true));

    // Latch digital pads in dig_pad_hold during the power-down transition:
    // GPIO8 (SCK), GPIO10 (MOSI), GPIO12 (SD /CS), GPIO21 (UC8253 /CS).
    const HOLD_DIG_MASK: u32 = (1 << 8) | (1 << 10) | (1 << 12) | (1 << 21);
    // SAFETY: Setting dig_pad_hold latches the output state and pull resistors of
    // display SPI/CS pins during the power-down transition without enabling digital autohold.
    rtc_cntl
        .dig_pad_hold()
        .modify(|r, w| unsafe { w.bits(r.bits() | HOLD_DIG_MASK) });
}
