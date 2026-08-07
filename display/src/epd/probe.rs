//! Sans-IO core of the UC81xx panel-controller fingerprint.
//!
//! Newer Xteink production runs swap the panel controller for an UltraChip
//! sibling that shares the UC81xx KW-mode command set — the X3's UC8253 for a
//! UC8279d, the X4's SSD1677 for a UC8179 — behind identical glass, pinout and
//! packaging. Nothing outside the device says which one it carries, so the
//! firmware asks the silicon: every UC81xx answers a VER (`0x70`) / FLG
//! (`0x71`) read with a structured version block, which the parts this
//! firmware already drives cannot produce.
//!
//! "Cannot produce" is narrower than "does not answer", and the difference is
//! the whole difficulty. A benched UC8253 answers `0x71` with a real `0x13`
//! status; it is only its VER and MTP that come back blank. Treating any
//! answer as a UC81xx would misidentify it — see rule 3.
//!
//! The bit-banged read lives in `hal_ext::epd_probe`, which drives the pins and
//! the reset timing. Everything that *decides* lives here, where the host can
//! test it — and here that split matters more than usual. No UC8179 or UC8279d
//! hardware exists to try this against, so the confirming path has no bench to
//! prove it and these tests are its only evidence. The negative path does have
//! a bench: a UC8253 X3 answers [`ProbeVerdict::DefaultAssumed`] on twelve
//! consecutive probes, and rule 3 below explains why that was closer to a
//! misidentification than it sounds.
//!
//! # The rules, and why they are not obvious
//!
//! Ported from FreeInk's `libs/hardware/XteinkDetect` (`e52d480`), whose
//! production experience is the reason for three rules a datasheet reading
//! would not produce. A probe missing any of them promotes a driver on a
//! floating bus, which is a device that renders nothing.
//!
//! 1. **Two passes that agree, not one read.** A floating bus can produce one
//!    plausible answer; it cannot produce the same non-trivial answer twice.
//!    A single stray match is [`ProbeVerdict::Inconclusive`].
//! 2. **FLG must be *driven*.** `0x00` and `0xFF` are floating-bus artifacts,
//!    and a real idle status has `BUSY_N` (bit 0) set.
//! 3. **The MTP key is an escalation path.** Field UC8279d units answer VER
//!    with a blank `FF FF FF FF FF`, which rule 2's floating test rejects — and
//!    a pulled-up floating bus reads `FF` too. Recovering them needs positive
//!    evidence, not the absence of negative: the MTP dump must open with the
//!    `0xA5` refresh-enable key, which only a real UC81xx with a programmed MTP
//!    can produce.
//!
//!    **This is not a theoretical guard.** A shipping UC8253 X3, benched
//!    2026-08-07, answers the probe with `VER = FF FF FF FF FF` and
//!    `FLG = 0x13` — byte for byte the field UC8279d signature. Its FLG line is
//!    genuinely driven, so rule 2 passes it; its VER is blank, so the rule 3
//!    condition is met in full. The *only* thing between that unit and a false
//!    sibling confirmation is its MTP dump reading all `FF` instead of opening
//!    with `0xA5`. Relax the key check and this firmware misidentifies the
//!    controller in the installed base it was written for. See
//!    `a_shipping_uc8253_is_only_told_apart_by_its_mtp`.
//!
//!    Rule 3 also constrains *timing*, which is easy to miss because the
//!    condition lives here and the consequence lives in the caller. A blank-VER
//!    part never satisfies [`PassReading::matches_uc81xx`], so a caller that
//!    schedules its confirming pass off a signature match alone gives this path
//!    the short screening reset — and with it the MTP read that decides the
//!    case. [`needs_identify_confirm`] is the predicate that keeps the two in
//!    step; a caller must use it rather than a bare match.
//!
//! A fourth rule is the caller's, because it is about timing rather than
//! bytes: never gate the read on BUSY. Which controller is present is exactly
//! the unknown, so its BUSY polarity is unknown too.

/// Bytes in a VER (`0x70`) response: a reserved `0x00`, `CHIP_VER`, then 24
/// bits of `LUT_VER`.
pub const VER_BYTES: usize = 5;

/// MTP bytes worth keeping for diagnostics. `[0x000]` is the refresh-enable
/// key, `0x001..=0x016` the factory Command Default Setting (the real
/// PSR/TRES/GSST/CDI/TCON), `0x017..=0x019` the product ID, `0x01A..=0x027` the
/// LUT version, and `0x028`+ the temperature boundaries. This is ground truth
/// for what a field module expects, readable even when the panel shows nothing.
pub const MTP_BYTES: usize = 48;

/// MTP byte 0 on any UltraChip part with a programmed MTP. Without it, OTP-mode
/// refreshes would not run at all, so its absence is decisive.
pub const MTP_REFRESH_ENABLE_KEY: u8 = 0xA5;

/// What the probe concluded about the silicon on the display bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// Neither pass saw a UC81xx. The controller this device build was written
    /// for is what is on the bus — the answer every existing unit gives.
    DefaultAssumed,
    /// Two independent passes agreed on a driven UC81xx signature. The only
    /// verdict that may promote a driver.
    Uc81xxConfirmed,
    /// The passes disagreed, or exactly one of them matched. Callers must
    /// treat this as [`Self::DefaultAssumed`]; it stays a separate verdict so
    /// diagnostics can tell a marginal bus from a clean negative.
    Inconclusive,
}

impl ProbeVerdict {
    /// A short, stable token for logs and the on-card report.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DefaultAssumed => "default-assumed",
            Self::Uc81xxConfirmed => "uc81xx-confirmed",
            Self::Inconclusive => "inconclusive",
        }
    }

    /// A one-byte encoding, for callers that keep a verdict somewhere with no
    /// room for a Rust type — firmware caches this in RTC RAM across the
    /// deep-sleep reboot.
    pub const fn code(self) -> u8 {
        match self {
            Self::DefaultAssumed => 0,
            Self::Uc81xxConfirmed => 1,
            Self::Inconclusive => 2,
        }
    }

    /// The inverse of [`Self::code`]. `None` for anything else, so a caller
    /// decoding retained RAM treats a byte this never wrote as no verdict at
    /// all rather than guessing one.
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::DefaultAssumed),
            1 => Some(Self::Uc81xxConfirmed),
            2 => Some(Self::Inconclusive),
            _ => None,
        }
    }
}

/// What one pass read back off the bus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassReading {
    pub ver: [u8; VER_BYTES],
    pub flg: u8,
}

impl PassReading {
    /// A driven idle status: neither floating-bus artifact, and `BUSY_N` set.
    pub const fn flg_is_driven(&self) -> bool {
        self.flg != 0x00 && self.flg != 0xFF && self.flg & 0x01 == 0x01
    }

    /// Five identical bytes is a bus nobody answered; any variation is a real,
    /// driven response.
    ///
    /// No specific `CHIP_VER` is required. A shipping X4 Pro UC8179 was
    /// observed returning `00 00 01 FF FF` — `CHIP_VER` of `0x00` — and an
    /// earlier matcher that insisted on a non-zero `CHIP_VER` wrongly rejected
    /// it.
    pub fn ver_is_floating(&self) -> bool {
        self.ver.iter().all(|byte| *byte == self.ver[0])
    }

    /// The plain UC81xx signature: a driven status *and* a driven VER.
    pub fn matches_uc81xx(&self) -> bool {
        self.flg_is_driven() && !self.ver_is_floating()
    }
}

/// `VER` byte 2 — the `LUT_VER` field that separates the UltraChip siblings
/// from each other: `0x01` is a UC8179, `0x02`/`0x68`/`0x69` a UC8279.
///
/// Not needed to pick a driver while the device build already implies which
/// sibling is possible (X3 to UC8279d, X4 to UC8179), but it is the
/// discriminator if a device ever ships both, and it belongs in diagnostics
/// regardless.
pub const fn lut_ver(ver: &[u8; VER_BYTES]) -> u8 {
    ver[2]
}

/// Rule 1: both passes saw the signature *and* read the same VER back.
pub fn agrees_on_uc81xx(pass1: &PassReading, pass2: &PassReading) -> bool {
    pass1.matches_uc81xx() && pass2.matches_uc81xx() && pass1.ver == pass2.ver
}

/// Whether the MTP dump is worth taking, given two passes.
///
/// Read it whenever *something* was driving the status line: on a confirmed
/// part it is diagnostics, and on the blank-VER path of rule 3 it is the
/// discriminator itself. A part without RMTP (UC8253, the SSD family) floats
/// the line and reads uniform garbage here, which is exactly what the key test
/// rejects.
pub fn should_read_mtp(pass1: &PassReading, pass2: &PassReading) -> bool {
    agrees_on_uc81xx(pass1, pass2) || pass1.flg_is_driven()
}

/// The blank-VER shape rule 3 recovers: a driven status over a VER that read
/// back as an all-`FF` bus.
///
/// Split out because it is not the same question as [`PassReading::matches_uc81xx`]
/// and the difference decides timing. A part in this state has told us
/// something — the status line answered — but not enough, so its MTP dump is
/// the discriminator and that dump has to be worth trusting.
pub fn is_blank_ver_candidate(pass: &PassReading) -> bool {
    pass.flg_is_driven() && pass.ver_is_floating() && pass.ver[0] == 0xFF
}

/// Whether pass 2 must be taken at the vendor's identification timing.
///
/// True when pass 1 produced anything worth confirming: a full signature, or
/// the blank-VER shape above. The blank-VER half is the one that matters and
/// the one that is easy to miss — [`PassReading::matches_uc81xx`] is false for
/// it *by construction*, so keying the confirming pass off a match alone runs
/// pass 2, and the RMTP read that follows it, at the short screening reset.
/// For the very part the escalation exists to serve — a UC8279d that answers
/// only at vendor timing — that produces a floating second pass and an MTP
/// dump with no key, and the probe concludes `DefaultAssumed` on a controller
/// that is really there. See
/// `a_part_that_only_answers_at_vendor_timing_is_still_confirmed`.
pub fn needs_identify_confirm(pass1: &PassReading) -> bool {
    pass1.matches_uc81xx() || is_blank_ver_candidate(pass1)
}

/// Rule 3: the blank-VER recovery, which needs the MTP key as positive
/// evidence before it will overrule the floating-bus test.
///
/// Both passes must show a driven status. Requiring it of pass 1 alone would
/// hand rule 1's two-pass agreement away on exactly this path: a pass 2 that
/// answered nothing reads back an all-`FF` VER, which equals a blank pass 1's
/// VER, so the equality check below would be satisfied by a bus that supplied
/// no evidence at all.
pub fn mtp_key_confirms(
    pass1: &PassReading,
    pass2: &PassReading,
    mtp: Option<&[u8; MTP_BYTES]>,
) -> bool {
    is_blank_ver_candidate(pass1)
        && pass2.flg_is_driven()
        && pass1.ver == pass2.ver
        && mtp.is_some_and(|mtp| mtp[0] == MTP_REFRESH_ENABLE_KEY)
}

/// The whole decision, from two passes and whatever MTP dump was taken.
pub fn resolve(
    pass1: &PassReading,
    pass2: &PassReading,
    mtp: Option<&[u8; MTP_BYTES]>,
) -> ProbeVerdict {
    if agrees_on_uc81xx(pass1, pass2) || mtp_key_confirms(pass1, pass2, mtp) {
        ProbeVerdict::Uc81xxConfirmed
    } else if !pass1.matches_uc81xx() && !pass2.matches_uc81xx() {
        ProbeVerdict::DefaultAssumed
    } else {
        // Exactly one pass matched. Not enough to promote a driver, and not a
        // clean negative either: something on this bus is marginal, and the
        // report should say so.
        ProbeVerdict::Inconclusive
    }
}

/// Which pass's VER belongs in the diagnostics report: the confirming pass
/// when both matched (it was read under the vendor's identification timing),
/// otherwise the first.
pub fn authoritative_ver(pass1: &PassReading, pass2: &PassReading) -> [u8; VER_BYTES] {
    if pass1.matches_uc81xx() && pass2.matches_uc81xx() {
        pass2.ver
    } else {
        pass1.ver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bus nobody drives, read through the data pin's pull-up.
    const FLOATING_HIGH: PassReading = PassReading {
        ver: [0xFF; VER_BYTES],
        flg: 0xFF,
    };
    /// A bus nobody drives and no pull-up reaches — a dead read.
    const FLOATING_LOW: PassReading = PassReading {
        ver: [0x00; VER_BYTES],
        flg: 0x00,
    };
    /// The signature a bench UC8179 actually returned. Note `CHIP_VER` of
    /// `0x00`: requiring a non-zero one here would reject a real part.
    const UC8179: PassReading = PassReading {
        ver: [0x00, 0x00, 0x01, 0xFF, 0xFF],
        flg: 0x13,
    };
    /// A blank VER over a genuinely driven idle status.
    ///
    /// Two different parts produce this: the field UC8279d, which the MTP key
    /// must rescue, and — bench-observed 2026-08-07 — a shipping UC8253 X3,
    /// which it must not. The reading alone does not distinguish them, which is
    /// the whole reason rule 3 demands the key.
    const BLANK_VER_DRIVEN_FLG: PassReading = PassReading {
        ver: [0xFF; VER_BYTES],
        flg: 0x13,
    };

    fn mtp_with_key() -> [u8; MTP_BYTES] {
        let mut mtp = [0u8; MTP_BYTES];
        mtp[0] = MTP_REFRESH_ENABLE_KEY;
        mtp
    }

    /// Bench truth, 2026-08-07, a shipping UC8253 X3: `VER = FF FF FF FF FF`,
    /// `FLG = 0x13`, MTP all `FF`, on twelve consecutive probes.
    ///
    /// This unit clears every gate rule 3 has except the last one. Its status
    /// line is really driven — `0x13` is the datasheet idle default, not a
    /// floating artifact — so rule 2 admits it; its VER is blank and both
    /// passes agree on it, so the blank-VER recovery is fully armed. Only the
    /// missing `0xA5` in its MTP stops the probe from calling a UC8253 a
    /// UC8279d and, once that driver lands, driving the installed base with
    /// the wrong protocol.
    ///
    /// So this test is the one that would catch someone "simplifying" the key
    /// check away, and the reason the MTP read is not optional diagnostics.
    #[test]
    fn a_shipping_uc8253_is_only_told_apart_by_its_mtp() {
        let observed = BLANK_VER_DRIVEN_FLG;
        let mtp_all_ff = [0xFFu8; MTP_BYTES];

        // Every precondition of the recovery holds...
        assert!(observed.flg_is_driven(), "0x13 is a real idle status");
        assert!(observed.ver_is_floating());
        assert!(should_read_mtp(&observed, &observed), "so the MTP is read");

        // ...and the dump is the only thing that refuses it.
        assert_eq!(
            resolve(&observed, &observed, Some(&mtp_all_ff)),
            ProbeVerdict::DefaultAssumed
        );
        // Swap in a programmed MTP and the very same reading confirms, which
        // is exactly how little daylight there is between the two parts.
        assert_eq!(
            resolve(&observed, &observed, Some(&mtp_with_key())),
            ProbeVerdict::Uc81xxConfirmed
        );
    }

    /// The part the X3 escalation exists for: a UC8279d that answers only at
    /// the vendor's identification reset.
    ///
    /// Its blank VER never satisfies the plain signature, so if the confirming
    /// pass is scheduled off `matches_uc81xx` it runs at the short screening
    /// reset — and so does the RMTP read taken on the bus that reset leaves.
    /// The part then answers nothing on pass 2, the MTP has no key, and a real
    /// UC8279d resolves as `DefaultAssumed`. [`needs_identify_confirm`] is what
    /// keeps pass 2 on the timing the part requires.
    #[test]
    fn a_part_that_only_answers_at_vendor_timing_is_still_confirmed() {
        // What the short screening reset gets: nothing.
        let screened = FLOATING_HIGH;
        assert!(
            !needs_identify_confirm(&screened),
            "a bus that answered nothing earns no long confirm"
        );

        // What the escalated 50 ms pass 1 gets, and what pass 2 must repeat.
        let identified = BLANK_VER_DRIVEN_FLG;
        assert!(
            !identified.matches_uc81xx(),
            "the blank VER is why scheduling off a signature match fails"
        );
        assert!(
            needs_identify_confirm(&identified),
            "but it is still worth confirming at vendor timing"
        );
        assert_eq!(
            resolve(&identified, &identified, Some(&mtp_with_key())),
            ProbeVerdict::Uc81xxConfirmed
        );
    }

    /// Rule 1 must survive the blank-VER path. A pass 2 that answered nothing
    /// reads back an all-`FF` VER, which equals a blank pass 1's VER for free —
    /// so VER equality alone is not agreement, and the second pass has to show
    /// a driven status of its own.
    #[test]
    fn a_silent_second_pass_cannot_stand_in_for_agreement() {
        assert_eq!(
            BLANK_VER_DRIVEN_FLG.ver, FLOATING_HIGH.ver,
            "the two are indistinguishable by VER, which is the trap"
        );
        assert_eq!(
            resolve(&BLANK_VER_DRIVEN_FLG, &FLOATING_HIGH, Some(&mtp_with_key())),
            ProbeVerdict::DefaultAssumed,
            "a pass that supplied no evidence must not complete a confirmation"
        );
    }

    /// The only outcome existing hardware can produce, and the one thing this
    /// change must never get wrong: an SSD1677 or UC8253 leaves the read
    /// floating, and a floating read must take the default driver.
    #[test]
    fn a_floating_bus_takes_the_default_controller() {
        for reading in [FLOATING_HIGH, FLOATING_LOW] {
            assert_eq!(
                resolve(&reading, &reading, None),
                ProbeVerdict::DefaultAssumed,
                "floating read {reading:02X?} must not promote a driver"
            );
        }
    }

    #[test]
    fn two_agreeing_passes_confirm_the_sibling() {
        assert_eq!(
            resolve(&UC8179, &UC8179, None),
            ProbeVerdict::Uc81xxConfirmed
        );
    }

    /// Rule 1. A bus that answers once and floats once is not a confirmation,
    /// and it is not a clean negative either.
    #[test]
    fn one_stray_match_is_inconclusive_either_way_round() {
        assert_eq!(
            resolve(&UC8179, &FLOATING_HIGH, None),
            ProbeVerdict::Inconclusive
        );
        assert_eq!(
            resolve(&FLOATING_HIGH, &UC8179, None),
            ProbeVerdict::Inconclusive
        );
    }

    /// Rule 1, the subtler half: both passes can look like a UC81xx and still
    /// disagree on what they read. Two different answers are no answer.
    #[test]
    fn two_matching_passes_that_read_different_ver_are_inconclusive() {
        let other = PassReading {
            ver: [0x00, 0x00, 0x02, 0xFF, 0xFF],
            flg: 0x13,
        };
        assert_eq!(resolve(&UC8179, &other, None), ProbeVerdict::Inconclusive);
    }

    /// Rule 2. Without the driven-FLG test a plausible-looking VER on a bus
    /// with no status at all would confirm a controller that is not there.
    #[test]
    fn a_driven_ver_with_no_status_never_confirms() {
        for flg in [0x00, 0xFF] {
            let reading = PassReading {
                ver: UC8179.ver,
                flg,
            };
            assert!(!reading.flg_is_driven());
            assert_eq!(
                resolve(&reading, &reading, Some(&mtp_with_key())),
                ProbeVerdict::DefaultAssumed
            );
        }
    }

    /// Rule 2's other half: a status byte can be driven and still say busy.
    /// `BUSY_N` clear means the part is mid-operation, not idle and answering.
    #[test]
    fn a_status_without_busy_n_is_not_an_idle_part() {
        let busy = PassReading {
            ver: UC8179.ver,
            flg: 0x12,
        };
        assert!(!busy.flg_is_driven());
        assert_eq!(resolve(&busy, &busy, None), ProbeVerdict::DefaultAssumed);
    }

    /// Rule 3. A blank VER with a driven status is the field UC8279d, and only
    /// the MTP key tells it from a pulled-up floating bus.
    #[test]
    fn a_blank_ver_needs_the_mtp_key_to_confirm() {
        assert_eq!(
            resolve(&BLANK_VER_DRIVEN_FLG, &BLANK_VER_DRIVEN_FLG, None),
            ProbeVerdict::DefaultAssumed,
            "no MTP dump is not evidence"
        );
        assert_eq!(
            resolve(
                &BLANK_VER_DRIVEN_FLG,
                &BLANK_VER_DRIVEN_FLG,
                Some(&[0xFF; MTP_BYTES])
            ),
            ProbeVerdict::DefaultAssumed,
            "a floating MTP read is not evidence"
        );
        assert_eq!(
            resolve(
                &BLANK_VER_DRIVEN_FLG,
                &BLANK_VER_DRIVEN_FLG,
                Some(&mtp_with_key())
            ),
            ProbeVerdict::Uc81xxConfirmed
        );
    }

    /// The blank-VER path still obeys rule 2: an MTP key read off a bus with
    /// no status is a coincidence, not a controller. This is the combination a
    /// probe without the FLG gate would fall for.
    #[test]
    fn the_mtp_key_does_not_override_a_dead_status_line() {
        assert_eq!(
            resolve(&FLOATING_HIGH, &FLOATING_HIGH, Some(&mtp_with_key())),
            ProbeVerdict::DefaultAssumed
        );
    }

    /// And it still obeys rule 1: one blank-VER pass agreeing with itself is
    /// the point, two different blank patterns are not.
    #[test]
    fn the_mtp_key_does_not_override_disagreeing_passes() {
        let other_blank = PassReading {
            ver: [0x00; VER_BYTES],
            flg: 0x13,
        };
        assert_eq!(
            resolve(&BLANK_VER_DRIVEN_FLG, &other_blank, Some(&mtp_with_key())),
            ProbeVerdict::DefaultAssumed,
            "neither pass matches the plain signature, so this is a clean negative"
        );
    }

    /// The MTP dump is worth its bus time whenever anything is driving the
    /// status line — that is what makes the rule 3 recovery reachable at all.
    #[test]
    fn the_mtp_is_read_whenever_the_status_line_is_driven() {
        assert!(should_read_mtp(&UC8179, &UC8179));
        assert!(should_read_mtp(
            &BLANK_VER_DRIVEN_FLG,
            &BLANK_VER_DRIVEN_FLG
        ));
        assert!(!should_read_mtp(&FLOATING_HIGH, &FLOATING_HIGH));
        assert!(!should_read_mtp(&FLOATING_LOW, &FLOATING_LOW));
    }

    /// The report should carry the VER read under the vendor's identification
    /// timing, which is pass 2's whenever pass 1 gave it something to confirm.
    #[test]
    fn the_confirming_pass_supplies_the_reported_ver() {
        let confirming = PassReading {
            ver: [0x00, 0x00, 0x02, 0x11, 0x22],
            flg: 0x13,
        };
        assert_eq!(authoritative_ver(&UC8179, &confirming), confirming.ver);
        assert_eq!(authoritative_ver(&UC8179, &FLOATING_HIGH), UC8179.ver);
        assert_eq!(
            authoritative_ver(&FLOATING_HIGH, &UC8179),
            FLOATING_HIGH.ver
        );
    }

    /// The byte encoding round-trips, and — the part that matters — anything
    /// this never wrote decodes to nothing. Retained RAM after a brownout is
    /// arbitrary bytes, and a decoder that guessed at one would resurrect a
    /// verdict no probe ever reached.
    #[test]
    fn the_verdict_code_round_trips_and_rejects_everything_else() {
        let known = [
            ProbeVerdict::DefaultAssumed,
            ProbeVerdict::Uc81xxConfirmed,
            ProbeVerdict::Inconclusive,
        ];
        for verdict in known {
            assert_eq!(ProbeVerdict::from_code(verdict.code()), Some(verdict));
        }
        let codes: [u8; 3] = [known[0].code(), known[1].code(), known[2].code()];
        for code in 0..=u8::MAX {
            if !codes.contains(&code) {
                assert_eq!(ProbeVerdict::from_code(code), None, "code {code:#04X}");
            }
        }
    }

    #[test]
    fn lut_ver_reads_the_silicon_discriminator() {
        assert_eq!(lut_ver(&UC8179.ver), 0x01);
        assert_eq!(lut_ver(&[0x00, 0x00, 0x68, 0xFF, 0xFF]), 0x68);
    }
}
