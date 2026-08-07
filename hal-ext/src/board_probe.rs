//! Which board is this: Xteink X3 or X4?
//!
//! Same ESP32-C3, different panel geometry, so the image header cannot tell
//! them apart. What differs is population: the X3 carries three I2C parts the
//! X4 does not, on the pins the X3 build already drives its gauge over
//! (SCL GPIO0, SDA GPIO20, 400 kHz). So: a read-only address sweep, run twice,
//! because one ACK on a bus idle a microsecond ago is not evidence.
//!
//! Everything uncertain resolves toward booting. A guard that bricks a
//! correctly flashed device is worse than the problem it solves.

/// One X3-only peripheral. A list of addresses because the IMU answers at
/// either, depending on its AD0 strap; any ACK among them counts once.
struct ProbeTarget {
    name: &'static str,
    addresses: &'static [u8],
}

/// The X3's I2C population, in [`PassResult::found`] bit order. The X4 has none
/// of these — it reads battery voltage off an ADC divider on GPIO0.
const X3_ONLY: &[ProbeTarget] = &[
    // TI BQ27220 fuel gauge, the part `crate::bq27220` drives.
    ProbeTarget {
        name: "BQ27220",
        addresses: &[0x55],
    },
    // Maxim DS3231 RTC.
    ProbeTarget {
        name: "DS3231",
        addresses: &[0x68],
    },
    // QST QMI8658 IMU; address depends on the AD0 strap.
    ProbeTarget {
        name: "QMI8658",
        addresses: &[0x6B, 0x6A],
    },
];

/// How many peripherals [`PassResult::found`] has bits for.
pub const TARGET_COUNT: usize = X3_ONLY.len();

/// Name behind bit `index` of [`PassResult::found`], so a caller can report
/// what answered without a second copy of the table.
pub fn target_name(index: usize) -> &'static str {
    match X3_ONLY.get(index) {
        Some(target) => target.name,
        None => "?",
    }
}

/// How many must answer *in both passes* to read as an X3. Two, not three: one
/// ACK is noise, but demanding all three would fail a real X3 over one absent
/// part.
const X3_CONFIRM_HITS: u32 = 2;

/// What one address sweep saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PassResult {
    /// Bit *i* set means `X3_ONLY[i]` acknowledged at one of its addresses.
    pub found: u8,
    /// A probe failed for a reason other than "nothing is there" — timeout,
    /// lost arbitration, incomplete command. Such a pass is evidence of a bad
    /// bus, not of absence, so it can never confirm an X4.
    pub faulted: bool,
}

impl PassResult {
    pub const fn new(found: u8, faulted: bool) -> Self {
        Self { found, faulted }
    }
}

/// Both passes, kept separate so the caller can log each. The verdict is
/// derived rather than stored, so a log line and a decision cannot disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub first: PassResult,
    pub second: PassResult,
}

/// Which board the fingerprint proves, if any.
///
/// Deliberately not a name: a copy of the spelling here could drift out of step
/// with the one the rest of the firmware compares against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoardVerdict {
    X3Confirmed,
    X4Confirmed,
    Inconclusive,
}

impl Fingerprint {
    /// A probe that could not run: no bus, no evidence. The fault flags keep it
    /// distinct from "nothing answered", which is what an X4 looks like.
    pub const fn unavailable() -> Self {
        Self {
            first: PassResult::new(0, true),
            second: PassResult::new(0, true),
        }
    }

    /// [`X3_CONFIRM_HITS`] of the *same* peripherals in both passes confirms an
    /// X3; a clean, unfaulted nothing in both confirms an X4.
    ///
    /// Intersection, not two independent counts: separate counts would let two
    /// different pairs of spurious ACKs (`0b011` then `0b110`) add up to a
    /// confident X3 and halt a good X4. Costs a real X3 nothing — soldered
    /// parts do not come and go between sweeps milliseconds apart.
    pub const fn verdict(self) -> BoardVerdict {
        let persistent = (self.first.found & self.second.found).count_ones();
        if persistent >= X3_CONFIRM_HITS {
            BoardVerdict::X3Confirmed
        } else if self.first.found == 0
            && self.second.found == 0
            && !self.first.faulted
            && !self.second.faulted
        {
            BoardVerdict::X4Confirmed
        } else {
            BoardVerdict::Inconclusive
        }
    }
}

// The truth table, pinned at compile time: `hal-ext` cannot be built for a
// host, so this rule would otherwise be checked nowhere.
const _: () = {
    const fn verdict(a: PassResult, b: PassResult) -> BoardVerdict {
        Fingerprint {
            first: a,
            second: b,
        }
        .verdict()
    }
    const CLEAN: bool = false;
    const FAULT: bool = true;

    // The same two peripherals answering in both passes: an X3.
    assert!(matches!(
        verdict(PassResult::new(0b011, CLEAN), PassResult::new(0b011, CLEAN)),
        BoardVerdict::X3Confirmed
    ));
    // One pass may see more, so long as two persist across both.
    assert!(matches!(
        verdict(PassResult::new(0b011, CLEAN), PassResult::new(0b111, CLEAN)),
        BoardVerdict::X3Confirmed
    ));
    // Two hits each, one in common: different pairs of spurious ACKs must not
    // add up to a board. This is what would otherwise halt a good X4.
    assert!(matches!(
        verdict(PassResult::new(0b011, CLEAN), PassResult::new(0b110, CLEAN)),
        BoardVerdict::Inconclusive
    ));
    // Nothing in common at all.
    assert!(matches!(
        verdict(PassResult::new(0b011, CLEAN), PassResult::new(0b100, CLEAN)),
        BoardVerdict::Inconclusive
    ));
    // A fault alongside real hits does not undo them: the X3 case rests on
    // what answered, not on what didn't.
    assert!(matches!(
        verdict(PassResult::new(0b111, CLEAN), PassResult::new(0b111, FAULT)),
        BoardVerdict::X3Confirmed
    ));
    // A clean nothing, twice: an X4.
    assert!(matches!(
        verdict(PassResult::new(0, CLEAN), PassResult::new(0, CLEAN)),
        BoardVerdict::X4Confirmed
    ));
    // Nothing found, but the bus faulted: no evidence of absence.
    assert!(matches!(
        verdict(PassResult::new(0, FAULT), PassResult::new(0, CLEAN)),
        BoardVerdict::Inconclusive
    ));
    assert!(matches!(
        verdict(PassResult::new(0, CLEAN), PassResult::new(0, FAULT)),
        BoardVerdict::Inconclusive
    ));
    // One stray ACK is not an X3, and it is no longer an X4 either.
    assert!(matches!(
        verdict(PassResult::new(0b001, CLEAN), PassResult::new(0, CLEAN)),
        BoardVerdict::Inconclusive
    ));
    assert!(matches!(
        verdict(PassResult::new(0b001, CLEAN), PassResult::new(0b001, CLEAN)),
        BoardVerdict::Inconclusive
    ));
    // Passes that disagree.
    assert!(matches!(
        verdict(PassResult::new(0b111, CLEAN), PassResult::new(0, CLEAN)),
        BoardVerdict::Inconclusive
    ));
    // A probe that never ran.
    assert!(matches!(
        Fingerprint::unavailable().verdict(),
        BoardVerdict::Inconclusive
    ));
};

// Compiled per-target rather than guarded at runtime, because of the pins: on
// the C3 they are the battery divider (GPIO0) and U0RXD (GPIO20), safe to drive
// briefly; on an S3 the same numbers are native USB D+ and a strapping pin,
// which are not. The code that touches them simply does not exist there.
// `Fingerprint` and the verdict rule stay available either way.
//
// Driving U0RXD costs no console: the C3's U0TXD is GPIO21, which both boards
// repurpose as EPD chip select, so serial here is always USB Serial/JTAG.
#[cfg(target_arch = "riscv32")]
mod c3 {
    use super::{Fingerprint, PassResult, X3_ONLY};
    use esp_hal::delay::Delay;
    use esp_hal::gpio::{AnyPin, Input, InputConfig};
    use esp_hal::i2c::master::{BusTimeout, Config, Error, I2c};
    use esp_hal::peripherals::I2C0;
    use esp_hal::time::Rate;

    /// Gap between passes: long enough that a transient does not land the same
    /// way twice, short enough to stay invisible against first paint.
    const PASS_GAP_MS: u32 = 5;

    /// The rate the X3 build already runs these pins at.
    const PROBE_KHZ: u32 = 400;

    /// SCL timeout in bus cycles. The BQ27220 clock-stretches for milliseconds,
    /// so esp-hal's default of 10 cycles (25 us) would abort every read and
    /// report the gauge absent on the board that has one. Matches what `main.rs`
    /// gives the gauge driver, and bounds a wedged bus at ~40 ms overall.
    const PROBE_BUS_CYCLES: u32 = 2000;

    /// Fingerprint over the X3's gauge pins, then hand both back as inputs.
    ///
    /// Takes reborrowed handles because the same I2C0 and pins are claimed
    /// straight afterward — by the gauge driver on the X3, by the battery ADC
    /// on the X4.
    ///
    /// Safe before any other bring-up: two pins, one peripheral, no register
    /// written, bounded by [`PROBE_BUS_CYCLES`].
    pub fn fingerprint<'d>(
        i2c: I2C0<'d>,
        sda: impl Into<AnyPin<'d>>,
        scl: impl Into<AnyPin<'d>>,
    ) -> Fingerprint {
        let mut sda = sda.into();
        let mut scl = scl.into();

        let config = Config::default()
            .with_frequency(Rate::from_khz(PROBE_KHZ))
            .with_timeout(BusTimeout::BusCycles(PROBE_BUS_CYCLES));
        let fingerprint = match I2c::new(i2c, config) {
            Ok(bus) => {
                let mut bus = bus.with_sda(sda.reborrow()).with_scl(scl.reborrow());
                let first = probe_pass(&mut bus);
                Delay::new().delay_millis(PASS_GAP_MS);
                let second = probe_pass(&mut bus);
                // Drop releases the bus: the pin guards disconnect I2C0.
                drop(bus);
                Fingerprint { first, second }
            }
            Err(_) => Fingerprint::unavailable(),
        };

        // Those guards leave the pads as the driver had them: open-drain,
        // output enabled, pulled up. The next owner expects plain inputs.
        release(sda);
        release(scl);
        fingerprint
    }

    /// `Input::new` does it all — clears output enable, enables input, applies
    /// `Pull::None` — and carries no `Drop`, so the pad keeps that once the
    /// binding goes out of scope.
    fn release(pin: AnyPin<'_>) {
        let _restored = Input::new(pin, InputConfig::default());
    }

    /// One sweep of the X3-only address table.
    fn probe_pass(bus: &mut I2c<'_, esp_hal::Blocking>) -> PassResult {
        let mut found = 0u8;
        let mut faulted = false;
        for (index, target) in X3_ONLY.iter().enumerate() {
            for &address in target.addresses {
                match probe_address(bus, address) {
                    Probe::Present => {
                        // The alternate strap cannot also be populated.
                        found |= 1 << index;
                        break;
                    }
                    Probe::Absent => {}
                    Probe::Faulted => faulted = true,
                }
            }
        }
        PassResult { found, faulted }
    }

    enum Probe {
        Present,
        Absent,
        Faulted,
    }

    /// A one-byte read: the smallest transaction carrying an address phase, and
    /// a read rather than a write so a populated gauge, RTC or IMU sees nothing
    /// it must act on. The byte is discarded; only the acknowledgment matters.
    fn probe_address(bus: &mut I2c<'_, esp_hal::Blocking>, address: u8) -> Probe {
        let mut discard = [0u8; 1];
        match bus.read(address, &mut discard) {
            Ok(()) => Probe::Present,
            // The clean negative the X4 verdict is built on, whichever phase
            // esp-hal attributes it to.
            Err(Error::AcknowledgeCheckFailed(_)) => Probe::Absent,
            // Anything else says the bus did not work, not that the board is
            // bare.
            Err(_) => Probe::Faulted,
        }
    }
}

#[cfg(target_arch = "riscv32")]
pub use c3::fingerprint;
