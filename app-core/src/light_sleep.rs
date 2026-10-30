//! The static-page light-sleep policy, as decisions a host test can drive.
//!
//! `fw::power_state` owns the atomics and the register reads; `fw::tasks::power`
//! owns the RTC and is the only caller of the sleep primitive. Everything
//! that decides, counts, or detects lives here so it can be tested without a
//! chip: the inhibitor count's saturation and underflow rules, the tri-state
//! USB host detector fed from the USB-Serial-JTAG SOF frame index, the
//! admission predicate with its refusal reasons, the residency accounting,
//! and the arithmetic that keeps an Embassy deadline honest across a sleep
//! the Embassy clock does not see.
//!
//! None of this is clock-aware beyond the millisecond counts it is handed:
//! callers pass in the time, and the tests pass in whatever time they like.

/// How long the device must sit with no input before a slice is considered.
pub const IDLE_THRESHOLD_MS: u64 = 1_000;

/// One timer-only light-sleep slice. Short, so a button poll is at most
/// this far away while the chip is asleep: the ADC ladders cannot wake
/// the chip, so the slice length is the worst-case press-to-sample latency.
pub const SLICE_MS: u64 = 50;

/// Two SOF samples must be at least this far apart to be compared. The host
/// sends a start-of-frame every millisecond while the bus is enumerated;
/// samples inside one frame compare equal and would read as no host.
pub const SOF_SAMPLE_MS: u64 = 10;

/// Unchanged comparisons before the detector reports `Absent`. The frame
/// index is an 11-bit counter at 1 kHz, so two samples taken 2,048 ms apart
/// compare equal with a host attached; one aliasing read must not flip a
/// live link to `Absent` and let a slice kill it.
pub const ABSENT_CONFIRM_SAMPLES: u8 = 2;

/// Whether a USB host is driving the USB-Serial-JTAG bus.
///
/// Light sleep stops the clock that peripheral runs on, so an enumerated
/// host sees the device vanish and the serial link drops. `Unknown` is the
/// boot state until two samples far enough apart have been compared, and it
/// forbids sleep the same way `Present` does.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UsbHostState {
    Unknown,
    Absent,
    Present,
}

impl UsbHostState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Absent => "absent",
            Self::Present => "present",
        }
    }

    /// The wire form for an atomic byte; `from_code` reverses it.
    pub const fn code(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Absent => 1,
            Self::Present => 2,
        }
    }

    pub const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Absent,
            2 => Self::Present,
            _ => Self::Unknown,
        }
    }
}

/// SOF-based host-link detector.
///
/// Feed it the `USB_DEVICE.FRAM_NUM.SOF_FRAME_INDEX` field and a millisecond
/// clock; it compares samples at least [`SOF_SAMPLE_MS`] apart. A moved
/// index is a live host and reports `Present` at once; an unchanged index
/// has to repeat [`ABSENT_CONFIRM_SAMPLES`] times before `Absent`, so the
/// 11-bit counter's 2,048 ms aliasing cannot flap a live link.
#[derive(Clone, Debug)]
pub struct UsbSofDetector {
    last: Option<(u16, u64)>,
    unchanged: u8,
    state: UsbHostState,
}

impl Default for UsbSofDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl UsbSofDetector {
    pub const fn new() -> Self {
        Self {
            last: None,
            unchanged: 0,
            state: UsbHostState::Unknown,
        }
    }

    pub const fn state(&self) -> UsbHostState {
        self.state
    }

    /// Feed one sample. Returns the new state when it changed, so a caller
    /// can log transitions without diffing.
    pub fn sample(&mut self, sof_frame_index: u16, now_ms: u64) -> Option<UsbHostState> {
        let Some((last_index, last_ms)) = self.last else {
            self.last = Some((sof_frame_index, now_ms));
            return None;
        };
        if now_ms.saturating_sub(last_ms) < SOF_SAMPLE_MS {
            // Too close to the sample it would be compared against; keep the
            // earlier one so the next comparison spans a full window.
            return None;
        }
        self.last = Some((sof_frame_index, now_ms));
        let next = if sof_frame_index != last_index {
            self.unchanged = 0;
            UsbHostState::Present
        } else {
            self.unchanged = self.unchanged.saturating_add(1);
            if self.unchanged >= ABSENT_CONFIRM_SAMPLES {
                UsbHostState::Absent
            } else {
                // Not enough evidence either way; keep what we had.
                self.state
            }
        };
        if next == self.state {
            return None;
        }
        self.state = next;
        Some(next)
    }
}

/// One more holder of the sleep inhibitor. Saturates rather than wrapping:
/// a count that wrapped to zero would admit a sleep under a live guard,
/// which is the one failure this counter exists to prevent.
pub const fn inhibit_acquire(count: u32) -> u32 {
    count.saturating_add(1)
}

/// One holder released. A release with nothing held is a bug in a caller,
/// but the safe answer is to stay at zero, not to wrap to `u32::MAX` and
/// pin the chip awake for the rest of the boot.
pub const fn inhibit_release(count: u32) -> u32 {
    count.saturating_sub(1)
}

/// Why a slice was not admitted. Each is counted separately so a telemetry
/// line can say what kept the chip awake.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The build has no light-sleep policy for this board.
    Unsupported,
    /// Less than [`IDLE_THRESHOLD_MS`] since the last input or paint.
    NotIdle,
    /// The USB detector has not settled since boot.
    UsbUnknown,
    /// A host is on the bus; sleeping would drop the serial link.
    UsbPresent,
    /// A `SleepInhibitGuard` is live (SD, display DMA, Wi-Fi, a sample).
    Inhibited,
    /// A button change is still inside its debounce window.
    Debounce,
}

impl Refusal {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::NotIdle => "not_idle",
            Self::UsbUnknown => "usb_unknown",
            Self::UsbPresent => "usb_present",
            Self::Inhibited => "inhibited",
            Self::Debounce => "debounce",
        }
    }

    /// Whether this refusal is temporary bookkeeping inside an otherwise
    /// eligible idle window. Residency is measured against eligible time,
    /// so a guard held for a background build counts against it while a
    /// user actively pressing buttons does not.
    pub const fn counts_as_eligible(self) -> bool {
        matches!(self, Self::Inhibited | Self::Debounce)
    }
}

/// Everything the admission predicate looks at, read at one instant.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AdmissionInputs {
    pub supported: bool,
    pub idle_ms: u64,
    pub usb: UsbHostState,
    pub inhibitors: u32,
    pub debounce_pending: bool,
}

/// Whether a slice may begin. The checks are ordered so the first refusal
/// names the most structural reason: a device that is not idle is refused
/// as such whatever the USB state, and the USB verdict comes before the
/// transient guard and debounce checks.
pub const fn admit(inputs: AdmissionInputs) -> Result<(), Refusal> {
    if !inputs.supported {
        return Err(Refusal::Unsupported);
    }
    if inputs.idle_ms < IDLE_THRESHOLD_MS {
        return Err(Refusal::NotIdle);
    }
    match inputs.usb {
        UsbHostState::Unknown => return Err(Refusal::UsbUnknown),
        UsbHostState::Present => return Err(Refusal::UsbPresent),
        UsbHostState::Absent => {}
    }
    if inputs.inhibitors != 0 {
        return Err(Refusal::Inhibited);
    }
    if inputs.debounce_pending {
        return Err(Refusal::Debounce);
    }
    Ok(())
}

/// How long to wait before re-evaluating after a refusal, in milliseconds.
/// A device that is not idle is re-checked when it would become idle; a
/// pending debounce is re-checked on the input task's own cadence; a USB
/// host is a slow-moving fact. A live guard is re-checked soon: the common
/// one after a slice is the input task's own sample, under a millisecond,
/// and waiting a whole slice for it would halve residency.
pub const fn reevaluate_after_ms(refusal: Refusal, idle_ms: u64) -> u64 {
    match refusal {
        Refusal::NotIdle => IDLE_THRESHOLD_MS.saturating_sub(idle_ms),
        Refusal::Debounce => 20,
        Refusal::Inhibited => 5,
        Refusal::UsbUnknown | Refusal::UsbPresent => 250,
        // Never re-evaluated: the policy is compiled out.
        Refusal::Unsupported => u64::MAX,
    }
}

/// A slice that came back in under half its request did not sleep: the
/// hardware rejected it (a pending wake source) or the clock was off. It is
/// counted, not trusted.
pub const fn slice_was_rejected(requested_ms: u64, elapsed_ms: u64) -> bool {
    elapsed_ms * 2 < requested_ms
}

/// The Embassy clock runs on SYSTIMER, which stops with the crystal in light
/// sleep, so a deadline armed before a slice is now `rtc - embassy` ms late
/// in wall-clock terms. Pulling it forward by that much keeps the idle leash
/// a wall-clock promise. Zero when the clock kept running.
pub const fn paused_clock_correction_ms(rtc_elapsed_ms: u64, embassy_elapsed_ms: u64) -> u64 {
    rtc_elapsed_ms.saturating_sub(embassy_elapsed_ms)
}

/// Sleep-residency counters. All saturating; a counter that wrapped would
/// report a residency that did not happen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LightSleepStats {
    /// Slices entered, rejected ones included.
    pub entered: u64,
    /// Sum of the requested slice lengths.
    pub requested_ms: u64,
    /// Sum of the RTC-measured time the chip was actually away.
    pub elapsed_ms: u64,
    /// Slices that came back within the requested length (the timer woke it).
    pub timer_wakes: u64,
    /// Slices that came back in under half their request: see
    /// [`slice_was_rejected`].
    pub rejected: u64,
    pub refused_inhibited: u64,
    pub refused_usb_unknown: u64,
    pub refused_usb_present: u64,
    pub refused_debounce: u64,
    pub refused_not_idle: u64,
    /// Wall-clock time spent idle past the threshold with no USB host, the
    /// denominator of residency. Slept time is part of it.
    pub eligible_ms: u64,
}

impl LightSleepStats {
    pub const fn new() -> Self {
        Self {
            entered: 0,
            requested_ms: 0,
            elapsed_ms: 0,
            timer_wakes: 0,
            rejected: 0,
            refused_inhibited: 0,
            refused_usb_unknown: 0,
            refused_usb_present: 0,
            refused_debounce: 0,
            refused_not_idle: 0,
            eligible_ms: 0,
        }
    }

    pub fn record_refusal(&mut self, refusal: Refusal) {
        let counter = match refusal {
            Refusal::Inhibited => &mut self.refused_inhibited,
            Refusal::UsbUnknown => &mut self.refused_usb_unknown,
            Refusal::UsbPresent => &mut self.refused_usb_present,
            Refusal::Debounce => &mut self.refused_debounce,
            Refusal::NotIdle => &mut self.refused_not_idle,
            Refusal::Unsupported => return,
        };
        *counter = counter.saturating_add(1);
    }

    /// One slice came back. `elapsed_ms` is RTC wall-clock, the only clock
    /// that ran while the chip was asleep.
    pub fn record_slice(&mut self, requested_ms: u64, elapsed_ms: u64) {
        self.entered = self.entered.saturating_add(1);
        self.requested_ms = self.requested_ms.saturating_add(requested_ms);
        self.elapsed_ms = self.elapsed_ms.saturating_add(elapsed_ms);
        if slice_was_rejected(requested_ms, elapsed_ms) {
            self.rejected = self.rejected.saturating_add(1);
        } else {
            self.timer_wakes = self.timer_wakes.saturating_add(1);
        }
    }

    /// Wall-clock time that passed while the device was eligible to sleep,
    /// whether it slept or a transient refusal kept it awake.
    pub fn record_eligible(&mut self, ms: u64) {
        self.eligible_ms = self.eligible_ms.saturating_add(ms);
    }

    /// `elapsed_sleep / eligible_idle`, in parts per thousand. `None` before
    /// any eligible time has been seen. Clamped to 1,000: slept time inside
    /// the eligible window cannot exceed the window, but the two are
    /// recorded at different instants and rounding can put it over.
    pub fn residency_permille(&self) -> Option<u32> {
        if self.eligible_ms == 0 {
            return None;
        }
        // Widened so a saturated counter still divides to a true ratio.
        let permille = u128::from(self.elapsed_ms) * 1_000 / u128::from(self.eligible_ms);
        Some(permille.min(1_000) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inhibit_count_saturates_and_does_not_underflow() {
        assert_eq!(inhibit_acquire(0), 1);
        assert_eq!(inhibit_acquire(u32::MAX), u32::MAX);
        assert_eq!(inhibit_release(1), 0);
        // A stray release stays at zero rather than wrapping to u32::MAX,
        // which would pin the chip awake for the rest of the boot.
        assert_eq!(inhibit_release(0), 0);
        // And the pair is a no-op, so an RAII guard leaves no residue.
        assert_eq!(inhibit_release(inhibit_acquire(7)), 7);
    }

    #[test]
    fn usb_state_codes_round_trip() {
        for state in [
            UsbHostState::Unknown,
            UsbHostState::Absent,
            UsbHostState::Present,
        ] {
            assert_eq!(UsbHostState::from_code(state.code()), state);
        }
        assert_eq!(UsbHostState::from_code(200), UsbHostState::Unknown);
    }

    #[test]
    fn detector_boots_unknown_and_needs_a_spaced_pair() {
        let mut d = UsbSofDetector::new();
        assert_eq!(d.state(), UsbHostState::Unknown);
        assert_eq!(d.sample(10, 0), None);
        // Inside the sample window: ignored, state stays Unknown.
        assert_eq!(d.sample(15, 5), None);
        assert_eq!(d.state(), UsbHostState::Unknown);
        // A moved index a full window later is a host.
        assert_eq!(d.sample(25, 15), Some(UsbHostState::Present));
    }

    #[test]
    fn detector_boot_detached_needs_two_unchanged_windows() {
        let mut d = UsbSofDetector::new();
        assert_eq!(d.sample(0, 0), None);
        assert_eq!(d.sample(0, 15), None);
        assert_eq!(d.state(), UsbHostState::Unknown);
        assert_eq!(d.sample(0, 30), Some(UsbHostState::Absent));
        // Steady: no further transitions reported.
        assert_eq!(d.sample(0, 45), None);
        assert_eq!(d.state(), UsbHostState::Absent);
    }

    #[test]
    fn detector_hot_plug_and_unplug() {
        let mut d = UsbSofDetector::new();
        d.sample(0, 0);
        d.sample(0, 15);
        assert_eq!(d.sample(0, 30), Some(UsbHostState::Absent));
        // Plug: the index starts moving; reported on the first changed
        // sample, no confirmation needed.
        assert_eq!(d.sample(7, 45), Some(UsbHostState::Present));
        assert_eq!(d.sample(22, 60), None);
        // Unplug: the index freezes; one unchanged sample is not enough.
        assert_eq!(d.sample(22, 75), None);
        assert_eq!(d.state(), UsbHostState::Present);
        assert_eq!(d.sample(22, 90), Some(UsbHostState::Absent));
    }

    #[test]
    fn detector_counter_rollover_reads_as_present() {
        let mut d = UsbSofDetector::new();
        d.sample(2040, 0);
        // 11-bit counter wraps: 2047 -> 0 is a change like any other.
        assert_eq!(d.sample(2047, 15), Some(UsbHostState::Present));
        assert_eq!(d.sample(3, 30), None);
        assert_eq!(d.state(), UsbHostState::Present);
    }

    #[test]
    fn detector_aliased_sample_does_not_flap_a_live_link() {
        let mut d = UsbSofDetector::new();
        d.sample(100, 0);
        assert_eq!(d.sample(115, 15), Some(UsbHostState::Present));
        // Exactly 2,048 frames later the index reads the same with a host
        // still attached. One such read must not report Absent.
        assert_eq!(d.sample(115, 2_063), None);
        assert_eq!(d.state(), UsbHostState::Present);
        // The next real sample moved again: still Present, no transition.
        assert_eq!(d.sample(130, 2_078), None);
        assert_eq!(d.unchanged, 0);
    }

    #[test]
    fn detector_time_going_backwards_is_treated_as_too_close() {
        let mut d = UsbSofDetector::new();
        d.sample(1, 100);
        // A clock that did not advance (or stepped back) is inside the
        // window; the sample is ignored, not compared.
        assert_eq!(d.sample(9, 50), None);
        assert_eq!(d.state(), UsbHostState::Unknown);
    }

    fn eligible_inputs() -> AdmissionInputs {
        AdmissionInputs {
            supported: true,
            idle_ms: IDLE_THRESHOLD_MS,
            usb: UsbHostState::Absent,
            inhibitors: 0,
            debounce_pending: false,
        }
    }

    #[test]
    fn admission_admits_only_when_everything_holds() {
        assert_eq!(admit(eligible_inputs()), Ok(()));
        assert_eq!(
            admit(AdmissionInputs {
                supported: false,
                ..eligible_inputs()
            }),
            Err(Refusal::Unsupported)
        );
        assert_eq!(
            admit(AdmissionInputs {
                idle_ms: IDLE_THRESHOLD_MS - 1,
                ..eligible_inputs()
            }),
            Err(Refusal::NotIdle)
        );
        assert_eq!(
            admit(AdmissionInputs {
                usb: UsbHostState::Unknown,
                ..eligible_inputs()
            }),
            Err(Refusal::UsbUnknown)
        );
        assert_eq!(
            admit(AdmissionInputs {
                usb: UsbHostState::Present,
                ..eligible_inputs()
            }),
            Err(Refusal::UsbPresent)
        );
        assert_eq!(
            admit(AdmissionInputs {
                inhibitors: 1,
                ..eligible_inputs()
            }),
            Err(Refusal::Inhibited)
        );
        assert_eq!(
            admit(AdmissionInputs {
                debounce_pending: true,
                ..eligible_inputs()
            }),
            Err(Refusal::Debounce)
        );
    }

    #[test]
    fn admission_names_the_structural_reason_first() {
        // Not idle beats everything but an unsupported build, and the USB
        // verdict beats a transient guard.
        assert_eq!(
            admit(AdmissionInputs {
                idle_ms: 0,
                usb: UsbHostState::Present,
                inhibitors: 3,
                debounce_pending: true,
                ..eligible_inputs()
            }),
            Err(Refusal::NotIdle)
        );
        assert_eq!(
            admit(AdmissionInputs {
                usb: UsbHostState::Present,
                inhibitors: 3,
                ..eligible_inputs()
            }),
            Err(Refusal::UsbPresent)
        );
    }

    #[test]
    fn reevaluation_waits_out_the_idle_threshold() {
        assert_eq!(reevaluate_after_ms(Refusal::NotIdle, 400), 600);
        assert_eq!(reevaluate_after_ms(Refusal::NotIdle, 5_000), 0);
        assert!(reevaluate_after_ms(Refusal::Inhibited, 0) < SLICE_MS / 4);
        assert_eq!(reevaluate_after_ms(Refusal::Unsupported, 0), u64::MAX);
    }

    #[test]
    fn eligibility_follows_the_refusal() {
        assert!(Refusal::Inhibited.counts_as_eligible());
        assert!(Refusal::Debounce.counts_as_eligible());
        assert!(!Refusal::NotIdle.counts_as_eligible());
        assert!(!Refusal::UsbPresent.counts_as_eligible());
        assert!(!Refusal::UsbUnknown.counts_as_eligible());
        assert!(!Refusal::Unsupported.counts_as_eligible());
    }

    #[test]
    fn rejected_slices_are_counted_apart() {
        let mut stats = LightSleepStats::new();
        stats.record_slice(50, 49);
        stats.record_slice(50, 24);
        stats.record_slice(50, 0);
        assert_eq!(stats.entered, 3);
        assert_eq!(stats.timer_wakes, 1);
        assert_eq!(stats.rejected, 2);
        assert_eq!(stats.requested_ms, 150);
        assert_eq!(stats.elapsed_ms, 73);
        assert!(!slice_was_rejected(50, 25));
        assert!(slice_was_rejected(50, 24));
    }

    #[test]
    fn residency_is_slept_over_eligible() {
        let mut stats = LightSleepStats::new();
        assert_eq!(stats.residency_permille(), None);
        stats.record_eligible(1_000);
        stats.record_slice(50, 50);
        stats.record_slice(50, 50);
        assert_eq!(stats.residency_permille(), Some(100));
        // Rounding can put slept time over the window; clamp, do not lie.
        stats.record_slice(50, 1_000);
        assert_eq!(stats.residency_permille(), Some(1_000));
    }

    #[test]
    fn refusals_are_counted_by_reason_and_unsupported_is_not() {
        let mut stats = LightSleepStats::new();
        for r in [
            Refusal::Inhibited,
            Refusal::Inhibited,
            Refusal::UsbUnknown,
            Refusal::UsbPresent,
            Refusal::Debounce,
            Refusal::NotIdle,
            Refusal::Unsupported,
        ] {
            stats.record_refusal(r);
        }
        assert_eq!(stats.refused_inhibited, 2);
        assert_eq!(stats.refused_usb_unknown, 1);
        assert_eq!(stats.refused_usb_present, 1);
        assert_eq!(stats.refused_debounce, 1);
        assert_eq!(stats.refused_not_idle, 1);
        assert_eq!(stats.entered, 0);
    }

    #[test]
    fn counters_saturate() {
        let mut stats = LightSleepStats {
            entered: u64::MAX,
            elapsed_ms: u64::MAX,
            ..LightSleepStats::new()
        };
        stats.record_slice(50, 50);
        assert_eq!(stats.entered, u64::MAX);
        assert_eq!(stats.elapsed_ms, u64::MAX);
        stats.record_eligible(u64::MAX);
        assert_eq!(stats.residency_permille(), Some(1_000));
    }

    #[test]
    fn paused_clock_correction_is_the_unseen_time() {
        // The chip was away 50 ms; Embassy saw 1 ms of it.
        assert_eq!(paused_clock_correction_ms(50, 1), 49);
        // A clock that kept running needs no correction, and one that
        // somehow ran ahead gets none rather than a wrapped value.
        assert_eq!(paused_clock_correction_ms(50, 50), 0);
        assert_eq!(paused_clock_correction_ms(50, 60), 0);
    }
}
