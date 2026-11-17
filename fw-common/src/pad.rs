//! MCU-neutral pad sleep-level policy and deep-sleep hold reconciliation.
//!
//! Keeps board-agnostic pad-level rules (`Polarity`, gated vs. un-gated EPD
//! reset levels, and per-pad hold capability contracts) host-testable while
//! ensuring firmware lifts inherited deep-sleep pad holds on the first output
//! transition.

use embedded_hal::digital::{ErrorType, OutputPin, PinState};

/// Logic level of a GPIO pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadLevel {
    Low,
    High,
}

impl PadLevel {
    /// Returns the opposite logic level.
    pub const fn inverted(self) -> Self {
        match self {
            Self::Low => Self::High,
            Self::High => Self::Low,
        }
    }

    /// Converts this level into an `embedded_hal` [`PinState`].
    pub const fn pin_state(self) -> PinState {
        match self {
            Self::Low => PinState::Low,
            Self::High => PinState::High,
        }
    }
}

impl From<PadLevel> for PinState {
    fn from(level: PadLevel) -> Self {
        level.pin_state()
    }
}

impl From<PinState> for PadLevel {
    fn from(state: PinState) -> Self {
        match state {
            PinState::Low => Self::Low,
            PinState::High => Self::High,
        }
    }
}

/// Signal or power-rail enable polarity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    ActiveHigh,
    ActiveLow,
}

impl Polarity {
    /// Level that enables or asserts the signal.
    pub const fn active_level(self) -> PadLevel {
        match self {
            Self::ActiveHigh => PadLevel::High,
            Self::ActiveLow => PadLevel::Low,
        }
    }

    /// Level that disables or deasserts the signal.
    pub const fn inactive_level(self) -> PadLevel {
        self.active_level().inverted()
    }
}

/// Deep-sleep level for an e-paper controller's active-low `/RST` pad.
///
/// - When `panel_rail_gated` is `true` (e.g., Sticky, whose panel supply is
///   switched off during deep sleep), `/RST` must be driven low so the MCU pin
///   does not back-power the unpowered controller through its ESD diode.
/// - When `panel_rail_gated` is `false` (e.g., C3 X3 and S3 X4 Pro, whose
///   panel supply remains powered in sleep), `/RST` must stay high so
///   UC8253/UC8179/UC8279 remains in controller deep sleep.
pub const fn epd_reset_sleep_level(panel_rail_gated: bool) -> PadLevel {
    if panel_rail_gated {
        PadLevel::Low
    } else {
        PadLevel::High
    }
}

/// Whether a GPIO pad can safely hold its state across MCU deep sleep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldCapability {
    /// The pad domain stays powered and retains its held level across sleep.
    RetainedInDeepSleep,
    /// Deep-sleep retention is unavailable or breaks required wake paths.
    Unsupported { reason: &'static str },
}

/// Deep-sleep hold contract for a single board GPIO pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PadHoldSpec {
    pub pad: u8,
    pub sleep_level: PadLevel,
    pub capability: HoldCapability,
}

impl PadHoldSpec {
    /// Constructs a pad hold specification with an explicit sleep level.
    pub const fn new(pad: u8, sleep_level: PadLevel, capability: HoldCapability) -> Self {
        Self {
            pad,
            sleep_level,
            capability,
        }
    }

    /// Hold specification for a switchable rail enable driven inactive in sleep.
    pub const fn rail_enable(pad: u8, polarity: Polarity, capability: HoldCapability) -> Self {
        Self::new(pad, polarity.inactive_level(), capability)
    }

    /// Hold specification for a latch or rail enable kept active across sleep.
    pub const fn retained_active(pad: u8, polarity: Polarity, capability: HoldCapability) -> Self {
        Self::new(pad, polarity.active_level(), capability)
    }

    /// Hold specification for an e-paper `/RST` pad.
    pub const fn epd_reset(pad: u8, panel_rail_gated: bool, capability: HoldCapability) -> Self {
        Self::new(pad, epd_reset_sleep_level(panel_rail_gated), capability)
    }

    /// Returns `true` when this pad reliably retains its level in deep sleep.
    pub const fn is_retained(self) -> bool {
        matches!(self.capability, HoldCapability::RetainedInDeepSleep)
    }
}

/// Wraps an [`OutputPin`] so an inherited deep-sleep pad hold is released once
/// before or with the first output drive.
///
/// ### Contract and ordering
/// 1. Construct the underlying pin driver `P` (and any sibling pins covered by
///    the same hold-release domain) at safe idle output levels first.
/// 2. Wrap `P` with [`ReconciledOutput::with_idle`] (when an explicit idle
///    pre-drive should be reinforced immediately before lifting the hold) or
///    [`ReconciledOutput::new`] (when `P` is already latched at its idle level).
/// 3. On the first call to [`reconcile`](Self::reconcile),
///    [`reconcile_at`](Self::reconcile_at), or any [`OutputPin`] write
///    (`set_low`, `set_high`, `set_state`), `ReconciledOutput`:
///    - drives the idle level onto `P` (if provided),
///    - invokes `release()` **once** to lift the hardware pad hold while `P` is
///      actively driving its safe idle state (zero glitch), and
///    - drives the requested target state onto `P` (guaranteeing that the first
///      pulse, such as `rst.set_low()`, physically moves the now-unheld pad).
/// 4. If driving the idle pre-drive fails, `release()` is not called and
///    `reconciled` stays `false` so a caller can retry. Once `release()` has
///    executed, `reconciled` becomes `true` and subsequent drives pass directly
///    to `P` without invoking `release()` again.
pub struct ReconciledOutput<P, R> {
    pin: P,
    idle: Option<PinState>,
    release: R,
    reconciled: bool,
}

impl<P, R> ReconciledOutput<P, R> {
    /// Wraps `pin`, assuming it was already initialized at its safe idle level.
    pub const fn new(pin: P, release: R) -> Self {
        Self {
            pin,
            idle: None,
            release,
            reconciled: false,
        }
    }

    /// Wraps `pin` with an explicit `idle` level written immediately before
    /// `release` runs on first reconciliation.
    pub const fn with_idle(pin: P, idle: PadLevel, release: R) -> Self {
        Self {
            pin,
            idle: Some(idle.pin_state()),
            release,
            reconciled: false,
        }
    }

    /// Returns `true` once the hold-release hook has run.
    pub const fn is_reconciled(&self) -> bool {
        self.reconciled
    }

    /// Borrows the wrapped pin without altering reconciliation state.
    pub const fn inner(&self) -> &P {
        &self.pin
    }

    /// Mutably borrows the wrapped pin.
    ///
    /// Call [`reconcile`](Self::reconcile) first if bypassing [`OutputPin`]
    /// methods while a pad hold may still be active.
    pub fn inner_mut(&mut self) -> &mut P {
        &mut self.pin
    }

    /// Unwraps the inner pin.
    pub fn into_inner(self) -> P {
        self.pin
    }
}

impl<P, R> ReconciledOutput<P, R>
where
    P: OutputPin,
    R: FnMut(),
{
    fn reconcile_with(&mut self, idle: Option<PinState>) -> Result<(), P::Error> {
        if self.reconciled {
            return Ok(());
        }
        if let Some(level) = idle {
            self.pin.set_state(level)?;
        }
        (self.release)();
        self.reconciled = true;
        Ok(())
    }

    /// Lifts any inherited pad hold once (pre-driving the configured idle level
    /// first if constructed with [`Self::with_idle`]).
    pub fn reconcile(&mut self) -> Result<(), P::Error> {
        self.reconcile_with(self.idle)
    }

    /// Drives `idle` onto the pin and lifts any inherited pad hold once.
    pub fn reconcile_at(&mut self, idle: PadLevel) -> Result<(), P::Error> {
        self.reconcile_with(Some(idle.pin_state()))
    }

    /// Drives `level` onto the pin, reconciling any inherited hold on the first
    /// transition.
    pub fn set_level(&mut self, level: PadLevel) -> Result<(), P::Error> {
        self.set_state(level.pin_state())
    }
}

impl<P, R> ErrorType for ReconciledOutput<P, R>
where
    P: ErrorType,
{
    type Error = P::Error;
}

impl<P, R> OutputPin for ReconciledOutput<P, R>
where
    P: OutputPin,
    R: FnMut(),
{
    fn set_low(&mut self) -> Result<(), Self::Error> {
        self.set_state(PinState::Low)
    }

    fn set_high(&mut self) -> Result<(), Self::Error> {
        self.set_state(PinState::High)
    }

    fn set_state(&mut self, state: PinState) -> Result<(), Self::Error> {
        self.reconcile()?;
        self.pin.set_state(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use std::rc::Rc;
    use std::vec::Vec;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Event {
        Write(PadLevel),
        ReleaseHold,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct MockError;

    impl embedded_hal::digital::Error for MockError {
        fn kind(&self) -> embedded_hal::digital::ErrorKind {
            embedded_hal::digital::ErrorKind::Other
        }
    }

    struct MockPin {
        events: Rc<RefCell<Vec<Event>>>,
        held: Rc<RefCell<bool>>,
        fail_next_writes: Rc<RefCell<usize>>,
        physical_level: Rc<RefCell<PadLevel>>,
    }

    struct MockBus {
        events: Rc<RefCell<Vec<Event>>>,
        held: Rc<RefCell<bool>>,
        fail_next_writes: Rc<RefCell<usize>>,
        physical_level: Rc<RefCell<PadLevel>>,
    }

    impl MockBus {
        fn new_held(initial_level: PadLevel) -> (Self, MockPin) {
            let bus = Self {
                events: Rc::new(RefCell::new(Vec::new())),
                held: Rc::new(RefCell::new(true)),
                fail_next_writes: Rc::new(RefCell::new(0)),
                physical_level: Rc::new(RefCell::new(initial_level)),
            };
            let pin = MockPin {
                events: bus.events.clone(),
                held: bus.held.clone(),
                fail_next_writes: bus.fail_next_writes.clone(),
                physical_level: bus.physical_level.clone(),
            };
            (bus, pin)
        }

        fn release_hook(&self) -> impl FnMut() + 'static {
            let held = self.held.clone();
            let events = self.events.clone();
            move || {
                *held.borrow_mut() = false;
                events.borrow_mut().push(Event::ReleaseHold);
            }
        }

        fn is_held(&self) -> bool {
            *self.held.borrow()
        }

        fn physical_level(&self) -> PadLevel {
            *self.physical_level.borrow()
        }
    }

    impl ErrorType for MockPin {
        type Error = MockError;
    }

    impl OutputPin for MockPin {
        fn set_low(&mut self) -> Result<(), Self::Error> {
            let mut fails = self.fail_next_writes.borrow_mut();
            if *fails > 0 {
                *fails -= 1;
                return Err(MockError);
            }
            self.events.borrow_mut().push(Event::Write(PadLevel::Low));
            if !*self.held.borrow() {
                *self.physical_level.borrow_mut() = PadLevel::Low;
            }
            Ok(())
        }

        fn set_high(&mut self) -> Result<(), Self::Error> {
            let mut fails = self.fail_next_writes.borrow_mut();
            if *fails > 0 {
                *fails -= 1;
                return Err(MockError);
            }
            self.events.borrow_mut().push(Event::Write(PadLevel::High));
            if !*self.held.borrow() {
                *self.physical_level.borrow_mut() = PadLevel::High;
            }
            Ok(())
        }
    }

    #[test]
    fn sticky_pad_state_contract() {
        // Sticky gates the EPD power rail in deep sleep: /RST must sleep low
        // so the MCU pin does not back-power the panel through ESD diodes.
        assert_eq!(epd_reset_sleep_level(true), PadLevel::Low);
        let epd_rst = PadHoldSpec::epd_reset(14, true, HoldCapability::RetainedInDeepSleep);
        assert_eq!(epd_rst.sleep_level, PadLevel::Low);
        assert!(epd_rst.is_retained());

        // Active-high rail enables drop low in sleep; PWR_HOLD/PWR_LOCK latch
        // stays asserted high across sleep.
        assert_eq!(Polarity::ActiveHigh.active_level(), PadLevel::High);
        assert_eq!(Polarity::ActiveHigh.inactive_level(), PadLevel::Low);
        let gated_rail =
            PadHoldSpec::rail_enable(1, Polarity::ActiveHigh, HoldCapability::RetainedInDeepSleep);
        assert_eq!(gated_rail.sleep_level, PadLevel::Low);
        let pwr_hold = PadHoldSpec::retained_active(
            42,
            Polarity::ActiveHigh,
            HoldCapability::RetainedInDeepSleep,
        );
        assert_eq!(pwr_hold.sleep_level, PadLevel::High);

        // Inherited hold on an active pad is lifted on the first drive.
        let (bus, pin) = MockBus::new_held(PadLevel::Low);
        let mut out = ReconciledOutput::with_idle(pin, PadLevel::Low, bus.release_hook());
        assert!(bus.is_held());
        out.set_high().unwrap();
        assert!(!bus.is_held());
        assert_eq!(
            *bus.events.borrow(),
            std::vec![
                Event::Write(PadLevel::Low),
                Event::ReleaseHold,
                Event::Write(PadLevel::High),
            ]
        );
    }

    #[test]
    fn x4_pro_pad_state_contract() {
        // X4 Pro panel rail stays powered in sleep: GPIO14 (/RST) must stay high.
        assert_eq!(epd_reset_sleep_level(false), PadLevel::High);
        let epd_rst = PadHoldSpec::epd_reset(14, false, HoldCapability::RetainedInDeepSleep);
        assert_eq!(epd_rst.pad, 14);
        assert_eq!(epd_rst.sleep_level, PadLevel::High);
        assert!(epd_rst.is_retained());

        // Active-low SD enable (GPIO5) and touch enable (GPIO2) must sleep high.
        assert_eq!(Polarity::ActiveLow.active_level(), PadLevel::Low);
        assert_eq!(Polarity::ActiveLow.inactive_level(), PadLevel::High);
        let sd_en =
            PadHoldSpec::rail_enable(5, Polarity::ActiveLow, HoldCapability::RetainedInDeepSleep);
        let touch_en =
            PadHoldSpec::rail_enable(2, Polarity::ActiveLow, HoldCapability::RetainedInDeepSleep);
        assert_eq!(sd_en.pad, 5);
        assert_eq!(sd_en.sleep_level, PadLevel::High);
        assert_eq!(touch_en.pad, 2);
        assert_eq!(touch_en.sleep_level, PadLevel::High);

        // Peripheral enable on GPIO1 is active-high.
        let periph_en = PadHoldSpec::retained_active(
            1,
            Polarity::ActiveHigh,
            HoldCapability::RetainedInDeepSleep,
        );
        assert_eq!(periph_en.pad, 1);
        assert_eq!(periph_en.sleep_level, PadLevel::High);
        assert_eq!(Polarity::ActiveHigh.inactive_level(), PadLevel::Low);
    }

    #[test]
    fn c3_x3_x4_pad_state_contract() {
        // RTC pads GPIO4 (D/C) and GPIO5 (/RST) retain hold state in deep sleep.
        let dc = PadHoldSpec::new(4, PadLevel::High, HoldCapability::RetainedInDeepSleep);
        let rst = PadHoldSpec::epd_reset(5, false, HoldCapability::RetainedInDeepSleep);
        assert!(dc.is_retained());
        assert_eq!(rst.pad, 5);
        assert_eq!(rst.sleep_level, PadLevel::High);
        assert!(rst.is_retained());

        // Digital pads (GPIO8, 10, 12, 21) cannot safely retain sleep holds
        // because enabling dg_pad_autohold_en isolates GPIO0..21 and breaks
        // active-low GPIO3 power-button wake.
        let reason = "dg_pad_autohold_en disables GPIO3 wake input buffer";
        for (pad, level) in [
            (8, PadLevel::Low),
            (10, PadLevel::Low),
            (12, PadLevel::High),
            (21, PadLevel::High),
        ] {
            let spec = PadHoldSpec::new(pad, level, HoldCapability::Unsupported { reason });
            assert!(!spec.is_retained());
            assert_eq!(spec.capability, HoldCapability::Unsupported { reason });
        }
    }

    #[test]
    fn reconciled_output_with_idle_releases_once_before_first_pulse() {
        let (bus, pin) = MockBus::new_held(PadLevel::High);
        let mut rst = ReconciledOutput::with_idle(pin, PadLevel::High, bus.release_hook());
        assert!(!rst.is_reconciled());
        assert!(bus.is_held());

        // First drive pulses /RST low: pre-drives idle High, lifts hold, then drives Low.
        rst.set_low().unwrap();
        assert!(rst.is_reconciled());
        assert!(!bus.is_held());
        assert_eq!(bus.physical_level(), PadLevel::Low);

        // Subsequent drives never re-invoke release.
        rst.set_high().unwrap();
        rst.set_state(PinState::Low).unwrap();
        rst.set_level(PadLevel::High).unwrap();
        rst.reconcile().unwrap();

        assert_eq!(
            *bus.events.borrow(),
            std::vec![
                Event::Write(PadLevel::High),
                Event::ReleaseHold,
                Event::Write(PadLevel::Low),
                Event::Write(PadLevel::High),
                Event::Write(PadLevel::Low),
                Event::Write(PadLevel::High),
            ]
        );
        let release_count = bus
            .events
            .borrow()
            .iter()
            .filter(|ev| **ev == Event::ReleaseHold)
            .count();
        assert_eq!(release_count, 1);
    }

    #[test]
    fn reconciled_output_new_and_explicit_reconcile_paths() {
        // `new` assumes pin is already constructed at safe idle level.
        let (bus, pin) = MockBus::new_held(PadLevel::High);
        let mut out = ReconciledOutput::new(pin, bus.release_hook());
        out.set_low().unwrap();
        out.set_high().unwrap();
        assert!(!bus.is_held());
        assert_eq!(bus.physical_level(), PadLevel::High);
        assert_eq!(
            *bus.events.borrow(),
            std::vec![
                Event::ReleaseHold,
                Event::Write(PadLevel::Low),
                Event::Write(PadLevel::High),
            ]
        );

        // Explicit `reconcile_at` lifts hold before any transition.
        let (bus2, pin2) = MockBus::new_held(PadLevel::High);
        let mut out2 = ReconciledOutput::new(pin2, bus2.release_hook());
        out2.reconcile_at(PadLevel::High).unwrap();
        assert!(out2.is_reconciled());
        assert!(!bus2.is_held());
        out2.set_low().unwrap();
        assert_eq!(
            *bus2.events.borrow(),
            std::vec![
                Event::Write(PadLevel::High),
                Event::ReleaseHold,
                Event::Write(PadLevel::Low),
            ]
        );
    }

    #[test]
    fn reconciled_output_preserves_invariant_across_write_errors() {
        let (bus, pin) = MockBus::new_held(PadLevel::High);
        *bus.fail_next_writes.borrow_mut() = 1;
        let mut out = ReconciledOutput::with_idle(pin, PadLevel::High, bus.release_hook());

        // Idle pre-drive fails -> hold must NOT be released and reconciled stays false.
        assert_eq!(out.set_low(), Err(MockError));
        assert!(!out.is_reconciled());
        assert!(bus.is_held());
        assert!(bus.events.borrow().is_empty());

        // Next drive succeeds -> idle pre-drive, release once, target write.
        out.set_low().unwrap();
        assert!(out.is_reconciled());
        assert!(!bus.is_held());
        assert_eq!(
            *bus.events.borrow(),
            std::vec![
                Event::Write(PadLevel::High),
                Event::ReleaseHold,
                Event::Write(PadLevel::Low),
            ]
        );

        // Make target write fail on a second pin after idle+release succeed.
        let (bus2, pin2) = MockBus::new_held(PadLevel::High);
        let fail2 = bus2.fail_next_writes.clone();
        let mut base_release = bus2.release_hook();
        let mut out2 = ReconciledOutput::with_idle(pin2, PadLevel::High, move || {
            base_release();
            // Cause the immediately following target write to fail.
            *fail2.borrow_mut() = 1;
        });

        assert_eq!(out2.set_low(), Err(MockError));
        assert!(out2.is_reconciled());
        assert!(!bus2.is_held());

        // Retrying target write must NOT re-invoke release.
        out2.set_low().unwrap();
        assert_eq!(
            *bus2.events.borrow(),
            std::vec![
                Event::Write(PadLevel::High),
                Event::ReleaseHold,
                Event::Write(PadLevel::Low),
            ]
        );
    }
}
