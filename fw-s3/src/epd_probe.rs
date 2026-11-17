//! Boot-time fingerprint of the X4 Pro panel controller: the bit-banged read
//! (`0x70` VER, `0x71` FLG, `0xA2` RMTP) and variant resolution.
//!
//! Adapted from `fw_c3::epd_probe`: every UC81xx answers VER/FLG over the
//! half-duplex display bus before SPI claims the pins, while SSD1677 floats.
//! [`display::epd::probe`] decides the verdict and resolves the X4-family
//! controller variant (`ssd1677`, `uc8179`, or `uc8279-x4`).

use display::epd::probe::{
    self, PassReading, ProbeVerdict, X4ControllerVariant, MTP_BYTES, VER_BYTES,
};
use embedded_hal::digital::OutputPin as _;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Flex, InputConfig, OutputConfig, Pull};
use fw_common::pad::ReconciledOutput;

/// UC81xx version register: reserved `0x00`, `CHIP_VER`, then 24 bits of `LUT_VER`.
const CMD_VER: u8 = 0x70;
/// UC81xx status register. `BUSY_N` (bit 0) reads 1 when the part is idle.
const CMD_FLG: u8 = 0x71;
/// Bulk MTP read: one dummy byte, then the factory MTP image from offset 0.
const CMD_RMTP: u8 = 0xA2;

/// Short screening reset pulse for pass 1. Both X4-family UltraChip siblings
/// answer this pulse, keeping default SSD1677 boots fast.
const SCREENING_RESET_MS: u32 = 1;
/// Vendor identification reset pulse for confirming pass 2 when pass 1 saw a
/// candidate response.
const IDENTIFY_RESET_MS: u32 = 50;
/// Flat settle after the reset pulse; never gate on BUSY while the controller
/// family (and therefore its BUSY polarity) is still unknown.
const POST_RESET_SETTLE_MS: u32 = 30;
/// `RST_N` idle high settle before pulsing low so the falling edge is clean.
const PRE_RESET_HIGH_MS: u32 = 2;
/// Quiet gap between passes.
const INTER_PASS_MS: u32 = 2;
/// Half a bit period at ~500 kHz.
const CLOCK_HALF_PERIOD_US: u32 = 1;

/// Boot probe outcome and diagnostics for the X4 Pro display bus.
#[derive(Clone, Copy, Debug)]
pub struct ProbeDiag {
    /// Two-pass verdict from [`display::epd::probe::resolve`].
    pub verdict: ProbeVerdict,
    /// Resolved X4-family panel controller variant.
    pub variant: X4ControllerVariant,
    /// `VER` bytes from the authoritative pass.
    pub ver: [u8; VER_BYTES],
    /// `FLG` status byte from the first pass.
    pub flg: u8,
    /// Whether [`Self::mtp`] holds a real dump rather than zeros.
    pub mtp_valid: bool,
    /// First [`MTP_BYTES`] of controller MTP when the status line was driven.
    pub mtp: [u8; MTP_BYTES],
}

impl ProbeDiag {
    /// `VER` byte 2, the `LUT_VER` field separating UC8179 (`0x01`) from
    /// UC8279 (`0x02`/`0x68`/`0x69`).
    pub const fn lut_ver(&self) -> u8 {
        probe::lut_ver(&self.ver)
    }
}

/// The five lines the probe drives. `sda` is the controller's bidirectional
/// data pin (board MOSI). `rst` is wrapped in [`ReconciledOutput`] so any
/// inherited deep-sleep pad hold is released once on first drive while all bus
/// lines are actively driven at safe idle levels, before pulsing reset.
pub struct ProbePins<'d, R> {
    pub sclk: Flex<'d>,
    pub sda: Flex<'d>,
    pub cs: Flex<'d>,
    pub dc: Flex<'d>,
    pub rst: ReconciledOutput<Flex<'d>, R>,
}

/// Fingerprint the panel controller on the X4 Pro display bus.
pub fn probe<R: FnMut()>(pins: ProbePins<'_, R>) -> ProbeDiag {
    let mut bus = ProbeBus {
        pins,
        delay: Delay::new(),
    };
    bus.configure();

    let pass1 = bus.run_pass(SCREENING_RESET_MS);
    bus.delay.delay_millis(INTER_PASS_MS);
    let reset2 = if probe::needs_identify_confirm(&pass1) {
        IDENTIFY_RESET_MS
    } else {
        SCREENING_RESET_MS
    };
    let pass2 = bus.run_pass(reset2);

    let mut mtp = [0u8; MTP_BYTES];
    let mtp_valid = probe::should_read_mtp(&pass1);
    if mtp_valid {
        let mut raw = [0u8; MTP_BYTES + 1];
        bus.cmd_read(CMD_RMTP, &mut raw);
        mtp.copy_from_slice(&raw[1..]);
    }

    let mut mtp_repeat = [0u8; MTP_BYTES];
    let repeated = mtp_valid && probe::needs_mtp_repeat(&pass1, &pass2, &mtp);
    if repeated {
        let mut raw = [0u8; MTP_BYTES + 1];
        bus.cmd_read(CMD_RMTP, &mut raw);
        mtp_repeat.copy_from_slice(&raw[1..]);
    }

    bus.release();

    let verdict = probe::resolve(
        &pass1,
        &pass2,
        mtp_valid.then_some(&mtp),
        repeated.then_some(&mtp_repeat),
    );
    let ver = probe::authoritative_ver(&pass1, &pass2);
    let variant = probe::resolve_x4_controller(verdict, &ver);

    ProbeDiag {
        verdict,
        variant,
        ver,
        flg: pass1.flg,
        mtp_valid,
        mtp,
    }
}

struct ProbeBus<'d, R> {
    pins: ProbePins<'d, R>,
    delay: Delay,
}

impl<R: FnMut()> ProbeBus<'_, R> {
    /// Drive every bus line to a safe idle state before reconciling pad holds
    /// on the first `rst` transition.
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

        self.pins
            .sda
            .apply_output_config(&OutputConfig::default().with_pull(Pull::Up));
        self.pins.sda.set_input_enable(true);
        self.pins.sda.set_low();
        self.pins.sda.set_output_enable(true);

        let rst = self.pins.rst.inner_mut();
        rst.apply_output_config(&OutputConfig::default());
        rst.set_high();
        rst.set_output_enable(true);
    }

    fn run_pass(&mut self, reset_low_ms: u32) -> PassReading {
        // First drive through `ReconciledOutput` lifts any inherited deep-sleep
        // pad holds once while every line above is driven at a safe idle level.
        let _ = self.pins.rst.set_high();
        self.delay.delay_millis(PRE_RESET_HIGH_MS);
        let _ = self.pins.rst.set_low();
        self.delay.delay_millis(reset_low_ms);
        let _ = self.pins.rst.set_high();
        self.delay.delay_millis(POST_RESET_SETTLE_MS);

        let mut reading = PassReading::default();
        let mut flg = [0u8; 1];
        self.cmd_read(CMD_FLG, &mut flg);
        self.cmd_read(CMD_VER, &mut reading.ver);
        reading.flg = flg[0];
        reading
    }

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

    /// Release clock, data, DC, and chip select (with pull-up) while leaving
    /// `rst` actively driven high until the panel driver claims it.
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
