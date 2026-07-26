//! The resistive button ladders, as pure thresholds.
//!
//! Both boards wire the front buttons to one ADC ladder (GPIO1, "nav") and the
//! side buttons to another (GPIO2, "page"), so one calibrated reading in
//! millivolts names at most one button per ladder. The X3 and the X4 share this
//! wiring, so the tables are not device-gated.
//!
//! The tables live here rather than beside the ADC I/O because two callers must
//! agree on them: the input task, which classifies every poll, and the
//! boot-time recovery check, which runs before that task exists. Deriving
//! [`recovery_combo_held`] from the same tables the input loop reads is what
//! keeps a recalibrated band from silently moving the escape hatch off the
//! buttons it documents.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HardwareButton {
    Back,
    Confirm,
    Left,
    Right,
    Up,
    Down,
}

/// One inclusive millivolt window on a ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Band {
    pub min: u16,
    pub max: u16,
    pub button: HardwareButton,
}

/// Front-button ladder on GPIO1. These bands scale Adafruit's current 16-bit
/// CircuitPython X4 thresholds to the 12-bit esp-hal ADC reads.
pub const NAV: &[Band] = &[
    Band {
        min: 2400,
        max: 2700,
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

/// Side-button ladder on GPIO2, scaled from the same thresholds.
pub const PAGE: &[Band] = &[
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

/// The button a calibrated reading names, or `None` between bands (no press).
pub fn classify(value: u16, table: &[Band]) -> Option<HardwareButton> {
    for band in table {
        if value >= band.min && value <= band.max {
            return Some(band.button);
        }
    }
    None
}

/// True when the boot-time recovery combo — `Back` on the front ladder plus
/// `Up` on the side ladder — is held, given one calibrated reading from each.
///
/// The two buttons sit on different ladders, which is the only kind of
/// two-button combo a resistive ladder can report at once: two presses sharing
/// a pin collapse into a single reading that names neither.
pub fn recovery_combo_held(nav_mv: u16, page_mv: u16) -> bool {
    classify(nav_mv, NAV) == Some(HardwareButton::Back)
        && classify(page_mv, PAGE) == Some(HardwareButton::Up)
}

/// The middle of three readings. Rejects a single noisy sample without the
/// latency of averaging across a window.
pub fn median3(a: u16, b: u16, c: u16) -> u16 {
    a.max(b).min(a.min(b).max(c))
}

/// What the confirmer wants the caller to do after a poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComboVerdict {
    /// The combo has been held long enough; act on it.
    Confirmed,
    /// Wait [`ComboConfirmer::POLL_MS`] and poll again.
    KeepPolling,
    /// The budget ran out without a confirmed hold; carry on booting.
    GaveUp,
}

/// Confirms that the recovery combo is genuinely *held*, across a span of time
/// rather than at one instant.
///
/// A single reading can land mid-transition while the ADC settles, and the
/// switch it arms is a reboot into another slot — so a run of consecutive
/// in-band readings is required, and any reading that isn't the combo resets
/// the run. The state machine is kept apart from the ADC so the timing
/// behaviour is host-testable.
///
/// FreeInk's `RecoveryBoot` polls 16 times for 5 consecutive holds at ~6 ms,
/// but most of that budget covers `InputManager`'s debounce state machine
/// warming up, which reading the ADC directly doesn't have. The shorter budget
/// here still spans ~12 ms of continuous hold, and gives up after ~32 ms on an
/// idle boot — the overwhelmingly common case, including every deep-sleep wake,
/// where the added latency is felt.
#[derive(Clone, Copy, Debug, Default)]
pub struct ComboConfirmer {
    consecutive: u8,
    polls: u8,
}

impl ComboConfirmer {
    /// Readings taken before giving up.
    pub const MAX_POLLS: u8 = 8;
    /// Consecutive in-band readings that confirm a hold.
    pub const CONFIRM_POLLS: u8 = 3;
    /// Gap the caller should leave between readings.
    pub const POLL_MS: u32 = 4;

    pub const fn new() -> Self {
        Self {
            consecutive: 0,
            polls: 0,
        }
    }

    /// Feed one reading from each ladder.
    pub fn push(&mut self, nav_mv: u16, page_mv: u16) -> ComboVerdict {
        self.polls = self.polls.saturating_add(1);
        if recovery_combo_held(nav_mv, page_mv) {
            self.consecutive += 1;
            if self.consecutive >= Self::CONFIRM_POLLS {
                return ComboVerdict::Confirmed;
            }
        } else {
            self.consecutive = 0;
        }
        if self.polls >= Self::MAX_POLLS {
            ComboVerdict::GaveUp
        } else {
            ComboVerdict::KeepPolling
        }
    }

    /// How many consecutive holds have been seen, for logging a confirmation.
    pub fn consecutive(&self) -> u8 {
        self.consecutive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mid-band readings for a button, for tests that want a definite press.
    fn center(table: &[Band], button: HardwareButton) -> u16 {
        let band = table
            .iter()
            .find(|b| b.button == button)
            .expect("button is on this ladder");
        band.min + (band.max - band.min) / 2
    }

    #[test]
    fn classify_is_inclusive_at_both_edges() {
        for table in [NAV, PAGE] {
            for band in table {
                assert_eq!(classify(band.min, table), Some(band.button));
                assert_eq!(classify(band.max, table), Some(band.button));
            }
        }
    }

    #[test]
    fn bands_on_a_ladder_never_overlap() {
        for table in [NAV, PAGE] {
            for (i, a) in table.iter().enumerate() {
                for b in &table[i + 1..] {
                    assert!(
                        a.max < b.min || b.max < a.min,
                        "{:?} and {:?} overlap",
                        a.button,
                        b.button
                    );
                }
            }
        }
    }

    #[test]
    fn gaps_between_bands_are_no_press() {
        // 1251..1799 sits between Left and Confirm on the nav ladder.
        assert_eq!(classify(1300, NAV), None);
        // 101..1499 sits between Down and Up on the page ladder.
        assert_eq!(classify(900, PAGE), None);
    }

    #[test]
    fn combo_holds_when_both_ladders_read_their_button() {
        let nav = center(NAV, HardwareButton::Back);
        let page = center(PAGE, HardwareButton::Up);
        assert!(recovery_combo_held(nav, page));
    }

    #[test]
    fn combo_needs_both_halves() {
        let back = center(NAV, HardwareButton::Back);
        let up = center(PAGE, HardwareButton::Up);
        // An idle ladder reads well above every band.
        let idle = 3000;
        assert!(!recovery_combo_held(back, idle));
        assert!(!recovery_combo_held(idle, up));
        assert!(!recovery_combo_held(idle, idle));
    }

    #[test]
    fn other_presses_are_not_the_combo() {
        let up = center(PAGE, HardwareButton::Up);
        let down = center(PAGE, HardwareButton::Down);
        for other in [
            HardwareButton::Confirm,
            HardwareButton::Left,
            HardwareButton::Right,
        ] {
            assert!(!recovery_combo_held(center(NAV, other), up));
        }
        assert!(!recovery_combo_held(
            center(NAV, HardwareButton::Back),
            down
        ));
    }

    /// An idle boot must not trip the hatch. The nav ladder rests high (no
    /// press pulls it down), so sweep everything above the Back band.
    #[test]
    fn idle_nav_readings_never_hold_the_combo() {
        let up = center(PAGE, HardwareButton::Up);
        for nav_mv in 2701..=3300u16 {
            assert!(!recovery_combo_held(nav_mv, up), "nav {nav_mv} tripped");
        }
    }

    #[test]
    fn median3_picks_the_middle_reading() {
        assert_eq!(median3(1, 2, 3), 2);
        assert_eq!(median3(3, 1, 2), 2);
        assert_eq!(median3(2, 3, 1), 2);
        // Ties collapse to the repeated value, not the outlier.
        assert_eq!(median3(5, 5, 1), 5);
        assert_eq!(median3(1, 1, 5), 1);
        assert_eq!(median3(7, 7, 7), 7);
    }

    #[test]
    fn median3_matches_a_sorted_reference() {
        for a in 0..12u16 {
            for b in 0..12u16 {
                for c in 0..12u16 {
                    let mut sorted = [a, b, c];
                    sorted.sort_unstable();
                    assert_eq!(median3(a, b, c), sorted[1], "median3({a},{b},{c})");
                }
            }
        }
    }

    // --- Confirming a held combo -------------------------------------------

    /// A reading pair for a held combo, and one for an idle ladder.
    fn held() -> (u16, u16) {
        (
            center(NAV, HardwareButton::Back),
            center(PAGE, HardwareButton::Up),
        )
    }
    fn idle() -> (u16, u16) {
        (3000, 3000)
    }

    /// Drive the confirmer over a scripted sequence of polls, as the boot loop
    /// would. `true` means the combo reads held on that poll.
    fn run(script: &[bool]) -> ComboVerdict {
        let mut confirmer = ComboConfirmer::new();
        let mut verdict = ComboVerdict::KeepPolling;
        for &is_held in script {
            let (nav, page) = if is_held { held() } else { idle() };
            verdict = confirmer.push(nav, page);
            if verdict != ComboVerdict::KeepPolling {
                break;
            }
        }
        verdict
    }

    #[test]
    fn a_steady_hold_confirms_on_the_third_poll() {
        let mut confirmer = ComboConfirmer::new();
        let (nav, page) = held();
        assert_eq!(confirmer.push(nav, page), ComboVerdict::KeepPolling);
        assert_eq!(confirmer.push(nav, page), ComboVerdict::KeepPolling);
        assert_eq!(confirmer.push(nav, page), ComboVerdict::Confirmed);
        assert_eq!(confirmer.consecutive(), ComboConfirmer::CONFIRM_POLLS);
    }

    #[test]
    fn an_idle_boot_gives_up_within_the_budget() {
        assert_eq!(run(&[false; 8]), ComboVerdict::GaveUp);
    }

    /// The failure the confirm loop exists to fix: the first readings land
    /// mid-transition while the ADC settles, but the button really is held.
    #[test]
    fn a_hold_still_confirms_after_unsettled_first_readings() {
        assert_eq!(
            run(&[false, false, true, true, true]),
            ComboVerdict::Confirmed
        );
    }

    /// The failure it exists to prevent: a transient that never becomes a hold.
    #[test]
    fn a_transient_blip_never_confirms() {
        assert_eq!(
            run(&[true, false, true, false, true, false, true, false]),
            ComboVerdict::GaveUp
        );
    }

    #[test]
    fn an_interrupted_run_starts_the_count_over() {
        // Two holds, a break, then three: the first two don't carry over, so
        // confirmation comes from the second run, on poll 6 rather than poll 5.
        let mut confirmer = ComboConfirmer::new();
        let (nav, page) = held();
        let (idle_nav, idle_page) = idle();
        assert_eq!(confirmer.push(nav, page), ComboVerdict::KeepPolling);
        assert_eq!(confirmer.push(nav, page), ComboVerdict::KeepPolling);
        assert_eq!(
            confirmer.push(idle_nav, idle_page),
            ComboVerdict::KeepPolling
        );
        assert_eq!(confirmer.consecutive(), 0, "the break must reset the run");
        assert_eq!(confirmer.push(nav, page), ComboVerdict::KeepPolling);
        assert_eq!(confirmer.push(nav, page), ComboVerdict::KeepPolling);
        assert_eq!(confirmer.push(nav, page), ComboVerdict::Confirmed);
    }

    #[test]
    fn a_hold_arriving_too_late_is_not_confirmed() {
        // Held only for the last two polls of the budget.
        assert_eq!(
            run(&[false, false, false, false, false, false, true, true]),
            ComboVerdict::GaveUp
        );
    }

    #[test]
    fn confirmation_never_needs_more_than_the_budget() {
        // Whatever the script, the confirmer answers within MAX_POLLS pushes.
        let mut confirmer = ComboConfirmer::new();
        let (nav, page) = idle();
        for i in 1..=ComboConfirmer::MAX_POLLS {
            let verdict = confirmer.push(nav, page);
            if i == ComboConfirmer::MAX_POLLS {
                assert_eq!(verdict, ComboVerdict::GaveUp);
            } else {
                assert_eq!(verdict, ComboVerdict::KeepPolling);
            }
        }
    }
}
