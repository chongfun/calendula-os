//! Boot-time fingerprint of the panel controller: the pins, the timing, and
//! the bit-banged read.
//!
//! Every UC81xx answers a VER (`0x70`) / FLG (`0x71`) read over the display
//! bus, and the controllers this firmware ships for do not. That read is what
//! separates a newer production unit's UltraChip sibling from the part the
//! device build was written for. This module performs it before the SPI
//! peripheral claims the bus; [`display::epd::probe`] decides what the bytes
//! mean, and its module docs carry the rules and the reasoning.
//!
//! # Why bit-bang, and why here
//!
//! The read is half-duplex: the controller drives the board's MOSI line — its
//! own bidirectional SDA — while the host clocks. An SPI peripheral cannot
//! turn its own output pin around mid-transfer, so this has to happen with the
//! pins in GPIO mode, which means before `Spi::new` configures them.
//!
//! # Pins
//!
//! The caller supplies them ([`ProbePins`]), because a probe that hardcodes a
//! pinout is unsafe on any other board: these GPIO numbers mean something else
//! entirely on an ESP32-S3. Today that means `fw::main`, which owns the board's
//! pin map; when a board-profile layer lands, it supplies them instead and
//! nothing in here changes.
//!
//! The probe leaves the bus released and no controller state behind: the panel
//! driver's own init sequence opens with a reset of its own.

use display::epd::probe::{self, PassReading, ProbeVerdict, MTP_BYTES, VER_BYTES};
use esp_hal::delay::Delay;
use esp_hal::gpio::{Flex, InputConfig, OutputConfig, Pull};

/// UC81xx version register: reserved `0x00`, `CHIP_VER`, then 24 bits of
/// `LUT_VER`.
const CMD_VER: u8 = 0x70;
/// UC81xx status register. `BUSY_N` (bit 0) reads 1 when the part is idle.
const CMD_FLG: u8 = 0x71;
/// Bulk MTP read: one dummy byte, then the factory MTP image from offset 0.
const CMD_RMTP: u8 = 0xA2;

/// Reset pulse for a screening pass. Every benched UC81xx answers this one,
/// and it keeps the common case — a default-controller panel that will never
/// answer `0x70` — from adding a tenth of a second to every boot.
const SCREENING_RESET_MS: u32 = 1;
/// Reset pulse the vendor's identification path uses, far beyond the
/// datasheet's 50 us minimum: the ID readback is less forgiving than normal
/// operation. Paid only on a pass that has something to confirm.
const IDENTIFY_RESET_MS: u32 = 50;
/// Flat settle after the reset pulse, sized to cover either controller
/// family's power-up.
///
/// This is where the rule about *not* gating on BUSY lands: which controller
/// is present is exactly the unknown, so its BUSY polarity is unknown too —
/// the two families are opposite (SSD1677 active-high, UC8253 two-phase and
/// idle-high). A flat delay is the only handshake available.
const POST_RESET_SETTLE_MS: u32 = 30;
/// Quiet gap between passes, so the next one starts from a settled bus.
const INTER_PASS_MS: u32 = 2;
/// Half a bit period at ~500 kHz — slow enough to be timing-safe on a bus
/// whose far end has not been identified yet.
const CLOCK_HALF_PERIOD_US: u32 = 1;

/// How hard a missed screening pass tries before the probe gives up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetEscalation {
    /// One [`SCREENING_RESET_MS`] pulse per pass. For a device whose sibling
    /// controller is bench-proven to answer the short pulse.
    Off,
    /// Retry a missed screening pass once at [`IDENTIFY_RESET_MS`] before
    /// concluding "no UC81xx here". Costs about 80 ms on every boot of a
    /// device that does *not* carry the sibling, so it is opt-in per device.
    OnMiss,
}

/// Everything the probe saw, kept so firmware can persist it somewhere a user
/// can reach without a serial cable. A locked unit's owner reporting "my
/// screen looks wrong" cannot be asked for `esp-println` output, and this is
/// the single most useful fact for answering them.
#[derive(Clone, Copy, Debug)]
pub struct ProbeDiag {
    /// The verdict callers dispatch on.
    pub verdict: ProbeVerdict,
    /// VER bytes from the authoritative pass.
    pub ver: [u8; VER_BYTES],
    /// FLG status byte from the first pass.
    pub flg: u8,
    /// Whether [`Self::mtp`] holds a real dump rather than zeros.
    pub mtp_valid: bool,
    /// The first [`MTP_BYTES`] of the controller MTP, read whenever something
    /// was driving the status line.
    pub mtp: [u8; MTP_BYTES],
}

impl ProbeDiag {
    /// `VER` byte 2, the `LUT_VER` field that separates the UltraChip siblings
    /// from each other. See [`display::epd::probe::lut_ver`].
    pub const fn lut_ver(&self) -> u8 {
        probe::lut_ver(&self.ver)
    }
}

/// The five lines the probe drives. `sda` is the controller's bidirectional
/// data pin — the board's MOSI — which the part drives during a read.
pub struct ProbePins<'d> {
    pub sclk: Flex<'d>,
    pub sda: Flex<'d>,
    pub cs: Flex<'d>,
    pub dc: Flex<'d>,
    pub rst: Flex<'d>,
}

/// Fingerprint the controller on the display bus.
///
/// Two independent passes, because a floating bus can produce one plausible
/// answer and not two — [`display::epd::probe`] owns that rule and the rest.
///
/// The reset pulse is **tiered, not a flat 50 ms**, and this table is the
/// normative shape (upstream `ca93e3d`):
///
/// | Pass          | Pulse                              | Condition                                |
/// |---------------|------------------------------------|------------------------------------------|
/// | 1 (screen)    | [`SCREENING_RESET_MS`]             | always                                   |
/// | 1 (escalate)  | [`IDENTIFY_RESET_MS`]              | screen missed **and** [`ResetEscalation::OnMiss`] |
/// | 2 (confirm)   | identify if pass 1 matched, else screen | always                              |
///
/// Two vendor-timing pulses would cost ~166 ms on every unit in the installed
/// base — the SSD1677s and UC8253s that will never answer `0x70` and gain
/// nothing from the longer reset. Tiering makes that case ~68 ms of two cheap
/// passes, and spends the vendor timing only where there is a hit to confirm,
/// which also makes pass 2's VER the authoritative readback.
pub fn probe(pins: ProbePins<'_>, escalation: ResetEscalation) -> ProbeDiag {
    let mut bus = ProbeBus {
        pins,
        delay: Delay::new(),
    };
    bus.configure();

    let mut pass1 = bus.run_pass(SCREENING_RESET_MS);
    if !pass1.matches_uc81xx() && escalation == ResetEscalation::OnMiss {
        // Some boards' sibling answers only the vendor identification timing,
        // so a missed screening pass is retried at that timing before this
        // concludes there is no UC81xx part.
        bus.delay.delay_millis(INTER_PASS_MS);
        pass1 = bus.run_pass(IDENTIFY_RESET_MS);
    }

    // A pass 1 with something to confirm makes pass 2 the doc-timing read, so
    // the VER the report carries was taken under vendor conditions.
    bus.delay.delay_millis(INTER_PASS_MS);
    let reset2 = if pass1.matches_uc81xx() {
        IDENTIFY_RESET_MS
    } else {
        SCREENING_RESET_MS
    };
    let pass2 = bus.run_pass(reset2);

    let mut mtp = [0u8; MTP_BYTES];
    let mtp_valid = probe::should_read_mtp(&pass1, &pass2);
    if mtp_valid {
        // RMTP answers with one dummy byte before the MTP image itself.
        let mut raw = [0u8; MTP_BYTES + 1];
        bus.cmd_read(CMD_RMTP, &mut raw);
        mtp.copy_from_slice(&raw[1..]);
    }

    bus.release();

    ProbeDiag {
        verdict: probe::resolve(&pass1, &pass2, mtp_valid.then_some(&mtp)),
        ver: probe::authoritative_ver(&pass1, &pass2),
        flg: pass1.flg,
        mtp_valid,
        mtp,
    }
}

/// The bit-banged half-duplex 4-wire SPI the probe talks over.
struct ProbeBus<'d> {
    pins: ProbePins<'d>,
    delay: Delay,
}

impl ProbeBus<'_> {
    /// Put every line in a known state. The data line keeps its input buffer
    /// and pull-up on for the whole probe and flips only its output driver, so
    /// a released bus reads a stable `0xFF` instead of an undriven float —
    /// which is what makes the floating-bus tests mean anything.
    fn configure(&mut self) {
        self.pins.cs.apply_output_config(&OutputConfig::default());
        self.pins.cs.set_high();
        self.pins.cs.set_output_enable(true);

        self.pins.sclk.apply_output_config(&OutputConfig::default());
        self.pins.sclk.set_low();
        self.pins.sclk.set_output_enable(true);

        self.pins.dc.apply_output_config(&OutputConfig::default());
        self.pins.dc.set_low();
        self.pins.dc.set_output_enable(true);

        // Pull direction is shared between a pin's input and output config, so
        // this one call sets the read-side pull-up too; nothing re-applies a
        // config to this pin afterwards.
        self.pins
            .sda
            .apply_output_config(&OutputConfig::default().with_pull(Pull::Up));
        self.pins.sda.set_input_enable(true);
        self.pins.sda.set_low();
        self.pins.sda.set_output_enable(true);

        self.pins.rst.apply_output_config(&OutputConfig::default());
        self.pins.rst.set_high();
        self.pins.rst.set_output_enable(true);
    }

    /// Hardware reset, flat settle, then FLG and VER.
    fn run_pass(&mut self, reset_low_ms: u32) -> PassReading {
        self.pins.rst.set_high();
        self.delay.delay_millis(2);
        self.pins.rst.set_low();
        self.delay.delay_millis(reset_low_ms);
        self.pins.rst.set_high();
        self.delay.delay_millis(POST_RESET_SETTLE_MS);

        let mut reading = PassReading::default();
        let mut flg = [0u8; 1];
        self.cmd_read(CMD_FLG, &mut flg);
        self.cmd_read(CMD_VER, &mut reading.ver);
        reading.flg = flg[0];
        reading
    }

    /// One command byte with DC low, then the data line released so the
    /// controller drives `out` while DC is high.
    fn cmd_read(&mut self, cmd: u8, out: &mut [u8]) {
        self.pins.sda.set_output_enable(true);
        self.pins.dc.set_low();
        self.pins.cs.set_low();
        self.clock_delay();
        self.write_byte(cmd);

        self.pins.dc.set_high();
        self.pins.sda.set_output_enable(false);
        self.clock_delay();
        for slot in out.iter_mut() {
            *slot = self.read_byte();
        }

        self.pins.cs.set_high();
        self.pins.sda.set_output_enable(true);
    }

    fn write_byte(&mut self, byte: u8) {
        let mut byte = byte;
        for _ in 0..8 {
            if byte & 0x80 == 0 {
                self.pins.sda.set_low();
            } else {
                self.pins.sda.set_high();
            }
            self.clock_delay();
            self.pins.sclk.set_high();
            self.clock_delay();
            self.pins.sclk.set_low();
            byte <<= 1;
        }
    }

    fn read_byte(&mut self) -> u8 {
        let mut byte = 0u8;
        for _ in 0..8 {
            // The controller shifts the next bit out on the falling clock
            // edge, so sample while the clock is still low and then pulse.
            self.clock_delay();
            byte = (byte << 1) | u8::from(self.pins.sda.is_high());
            self.pins.sclk.set_high();
            self.clock_delay();
            self.pins.sclk.set_low();
        }
        byte
    }

    fn clock_delay(&self) {
        self.delay.delay_micros(CLOCK_HALF_PERIOD_US);
    }

    /// Hand the bus back. Clock, data and DC go high-impedance and chip select
    /// is released to a pull-up so the panel is not left selected — the SPI
    /// peripheral and the panel's own pin drivers claim them next.
    ///
    /// RST is the one line left driven, and deliberately: this board has no
    /// internal pull-up on it, and the caller's very next act is to drive it
    /// high anyway. Releasing it would float the panel's reset for the gap in
    /// between.
    fn release(&mut self) {
        let floating = InputConfig::default();
        for pin in [&mut self.pins.sclk, &mut self.pins.sda, &mut self.pins.dc] {
            pin.set_output_enable(false);
            pin.apply_input_config(&floating);
            pin.set_input_enable(true);
        }
        self.pins.cs.set_output_enable(false);
        self.pins
            .cs
            .apply_input_config(&InputConfig::default().with_pull(Pull::Up));
        self.pins.cs.set_input_enable(true);
    }
}
