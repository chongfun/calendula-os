//! UC8279 panel controller — Xteink X4 Pro (800x480).
//!
//! Sans-IO protocol ported from FreeInk's `Uc8279X4Driver` (`Uc8279X4Driver.h`
//! / `Uc8279X4Driver.cpp`). Like the UC8253, this is a UC81xx KW-mode
//! controller with two RAM planes (`DTM1` old, `DTM2` new), but it uses
//! controller-intrinsic OTP waveforms (`PSR` bit 5 cleared before `DRF`),
//! a single-byte `CDI`, a 600-gate scan whose 480 visible rows begin at gate
//! offset `120`, and an explicit `PTIN`/`PTL`/`PTOUT` partial window for fast
//! updates.
//!
//! Geometry and protocol constants here belong to the UC8279 X4-family panel
//! itself and are intentionally independent of `crate::WIDTH`/`crate::HEIGHT`
//! so `display` compiles cleanly under both default X4 and `--features
//! device-x3`.

use super::{RefreshMode, SpiOp};

// --- UC8279 X4-family command set ---
pub const CMD_PANEL_SETTING: u8 = 0x00;
pub const CMD_POWER_OFF: u8 = 0x02;
pub const CMD_PFS: u8 = 0x03;
pub const CMD_POWER_ON: u8 = 0x04;
pub const CMD_DEEP_SLEEP: u8 = 0x07;
pub const CMD_DTM1: u8 = 0x10;
pub const CMD_DISPLAY_REFRESH: u8 = 0x12;
pub const CMD_DTM2: u8 = 0x13;
pub const CMD_PLL: u8 = 0x30;
pub const CMD_VCOM_DATA_INTERVAL: u8 = 0x50;
pub const CMD_RESOLUTION: u8 = 0x61;
pub const CMD_GATE_SOURCE_START: u8 = 0x65;
pub const CMD_PARTIAL_WINDOW: u8 = 0x90;
pub const CMD_PARTIAL_IN: u8 = 0x91;
pub const CMD_PARTIAL_OUT: u8 = 0x92;
pub const CMD_CCSET: u8 = 0xE0;
pub const CMD_GATE_SCAN: u8 = 0xE1;
pub const CMD_TSSET: u8 = 0xE5;

/// Argument to `CMD_DEEP_SLEEP` (check code the controller requires).
pub const DEEP_SLEEP_CHECK: u8 = 0xA5;

// --- Panel geometry and register configuration ---
pub const WIDTH: u16 = 800;
pub const VISIBLE_HEIGHT: u16 = 480;
pub const TRES_HEIGHT: u16 = 600;
pub const GATE_OFFSET: u16 = 120;
pub const ROW_BYTES: usize = 100;
pub const FB_BYTES: usize = 48_000;
pub const SPI_HZ: u32 = 16_000_000;

/// Panel setting at init (external-LUT bit set).
pub const PSR_INIT: [u8; 2] = [0x37, 0x4D];
/// Panel setting rewritten after `PON` and before `DRF` (`PSR_INIT[0] & 0xDF`)
/// to select the controller's built-in OTP waveform.
pub const PSR_OTP: [u8; 2] = [0x17, 0x4D];
pub const PFS: u8 = 0x20;
pub const PLL: u8 = 0x0E;
pub const GATE_SCAN: u8 = 0x02;
pub const CCSET: u8 = 0x02;
pub const TSSET_FULL: u8 = 0x1E;
pub const TSSET_FAST: u8 = 0x5A;
pub const CDI_FULL: u8 = 0x97;
pub const CDI_FAST: u8 = 0xD7;

const _: () = assert!((WIDTH as usize).is_multiple_of(8));
const _: () = assert!(WIDTH as usize / 8 == ROW_BYTES);
const _: () = assert!(ROW_BYTES * VISIBLE_HEIGHT as usize == FB_BYTES);
const _: () = assert!(GATE_OFFSET + VISIBLE_HEIGHT <= TRES_HEIGHT);
const _: () = assert!(PSR_OTP[0] == (PSR_INIT[0] & 0xDF));

/// Controller initialization byte sequence after hardware reset
/// (`Uc8279X4Driver::initController`).
pub static INIT_SEQUENCE: &[SpiOp] = &[
    SpiOp::Command {
        cmd: CMD_PANEL_SETTING,
        data: &PSR_INIT,
    },
    SpiOp::Command {
        cmd: CMD_RESOLUTION,
        data: &[
            (WIDTH >> 8) as u8,
            WIDTH as u8,
            (TRES_HEIGHT >> 8) as u8,
            TRES_HEIGHT as u8,
        ],
    },
    SpiOp::Command {
        cmd: CMD_GATE_SOURCE_START,
        data: &[0x00, 0x00, 0x00, 0x00],
    },
    SpiOp::Command {
        cmd: CMD_PFS,
        data: &[PFS],
    },
    SpiOp::Command {
        cmd: CMD_PLL,
        data: &[PLL],
    },
    SpiOp::Command {
        cmd: CMD_GATE_SCAN,
        data: &[GATE_SCAN],
    },
];

const X_END: u16 = WIDTH - 1;
const Y_START: u16 = GATE_OFFSET;
const Y_END: u16 = GATE_OFFSET + VISIBLE_HEIGHT - 1;

/// Full-screen `CMD_PARTIAL_WINDOW` (`0x90`) payload covering X `[0..=799]`
/// (byte-aligned end `| 0x07`) and Y `[120..=599]` with `PT_SCAN = 0x01`.
pub const PARTIAL_WINDOW_DATA: [u8; 9] = [
    0x00,
    0x00,
    (X_END >> 8) as u8,
    (X_END | 0x07) as u8,
    (Y_START >> 8) as u8,
    Y_START as u8,
    (Y_END >> 8) as u8,
    Y_END as u8,
    0x01,
];

/// Mapping of one controller gate index (`0..TRES_HEIGHT`) into panel RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateRow {
    /// Non-visible gate (`0..GATE_OFFSET`, or `GATE_OFFSET + VISIBLE_HEIGHT..TRES_HEIGHT`)
    /// padded with `0xFF` white bytes.
    WhitePadding,
    /// Visible framebuffer row index (`0..VISIBLE_HEIGHT`).
    Visible(u16),
}

/// Map a controller gate coordinate (`0..TRES_HEIGHT`) to padding or a visible
/// framebuffer row, returning `None` for gates outside `0..TRES_HEIGHT`.
pub const fn gate_row(gate: u16) -> Option<GateRow> {
    if gate >= TRES_HEIGHT {
        None
    } else if gate < GATE_OFFSET || gate >= GATE_OFFSET + VISIBLE_HEIGHT {
        Some(GateRow::WhitePadding)
    } else {
        Some(GateRow::Visible(gate - GATE_OFFSET))
    }
}

/// Populate one `ROW_BYTES` gate row from a 1-bpp framebuffer slice.
///
/// Gates `0..120`, gates `>= 600`, and out-of-range slices fill `out` with
/// `0xFF` (white). Visible gates (`120..600`) copy row `gate - 120`,
/// bitwise-inverting each byte when `invert` is `true`.
pub fn fill_gate_row(fb: &[u8], gate: u16, invert: bool, out: &mut [u8; ROW_BYTES]) {
    let Some(GateRow::Visible(row)) = gate_row(gate) else {
        out.fill(0xFF);
        return;
    };
    let Some(start) = (row as usize).checked_mul(ROW_BYTES) else {
        out.fill(0xFF);
        return;
    };
    let Some(src) = start
        .checked_add(ROW_BYTES)
        .and_then(|end| fb.get(start..end))
    else {
        out.fill(0xFF);
        return;
    };
    if invert {
        for (dst, &byte) in out.iter_mut().zip(src.iter()) {
            *dst = !byte;
        }
    } else {
        out.copy_from_slice(src);
    }
}

/// Populate one `ROW_BYTES` UC8279 gate row from Calendula's `Framebuffer`.
///
/// X4 [`Framebuffer`](crate::fb::Framebuffer) renders upright orientations
/// (`Portrait`, `Landscape`, `LandscapeFlipped`) directly into the panel's
/// native row-major buffer (matching CrossPoint's `GfxRenderer` `frameBuffer`),
/// so UC8279 X4 (`PSR0 = 0x37` / `0x17`, `SHL = 1`) streams visible rows
/// `0..VISIBLE_HEIGHT` forward into gates `120..600` with unmodified bytes and
/// bits, matching FreeInk's `Uc8279X4Driver::streamPlane`. White padding gates
/// (`0..120`), out-of-range gates (`>= 600`), and mismatched framebuffer
/// geometries (such as X3 `792x528`) fill `out` with `0xFF` without inversion.
pub fn fill_gate_row_from_framebuffer(
    fb: &crate::fb::Framebuffer,
    gate: u16,
    invert: bool,
    out: &mut [u8; ROW_BYTES],
) {
    let fb_bytes = fb.bytes();
    if fb_bytes.len() != FB_BYTES {
        out.fill(0xFF);
        return;
    }
    fill_gate_row(fb_bytes, gate, invert, out);
}

/// Controller RAM plane selected by a flush write step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RamPlane {
    Old,
    New,
}

impl RamPlane {
    /// SPI command selecting this RAM plane (`0x10` for Old, `0x13` for New).
    pub const fn cmd(self) -> u8 {
        match self {
            Self::Old => CMD_DTM1,
            Self::New => CMD_DTM2,
        }
    }
}

/// Byte source streamed across `TRES_HEIGHT` gates for a RAM-plane write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameSource {
    Current,
    InvertedCurrent,
    White,
}

/// One controller-level operation in a UC8279 X4-family refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushStep {
    WritePlane {
        plane: RamPlane,
        source: FrameSource,
    },
    Command {
        cmd: u8,
        data: &'static [u8],
    },
    WaitBusy,
}

/// Allocation-free step plan for a single UC8279 X4-family flush.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlushPlan {
    pub requested_mode: RefreshMode,
    pub effective_mode: RefreshMode,
    pub steps: &'static [FlushStep],
}

const MAX_FLUSH_STEPS: usize = 18;

const CDI_FULL_DATA: [u8; 1] = [CDI_FULL];
const CDI_FAST_DATA: [u8; 1] = [CDI_FAST];
const CCSET_DATA: [u8; 1] = [CCSET];
const TSSET_FULL_DATA: [u8; 1] = [TSSET_FULL];
const TSSET_FAST_DATA: [u8; 1] = [TSSET_FAST];
const PFS_DATA: [u8; 1] = [PFS];
const GATE_SCAN_DATA: [u8; 1] = [GATE_SCAN];

struct StepBuilder {
    steps: [FlushStep; MAX_FLUSH_STEPS],
    len: usize,
}

impl StepBuilder {
    const fn new() -> Self {
        Self {
            steps: [FlushStep::WaitBusy; MAX_FLUSH_STEPS],
            len: 0,
        }
    }

    const fn push(&mut self, step: FlushStep) {
        assert!(self.len < MAX_FLUSH_STEPS);
        self.steps[self.len] = step;
        self.len += 1;
    }

    const fn cmd(&mut self, cmd: u8, data: &'static [u8]) {
        self.push(FlushStep::Command { cmd, data });
    }
}

const fn build_flush_steps(
    fast: bool,
    dark_background: bool,
    screen_powered: bool,
    turn_off: bool,
) -> ([FlushStep; MAX_FLUSH_STEPS], usize) {
    let mut b = StepBuilder::new();
    b.push(FlushStep::WritePlane {
        plane: RamPlane::New,
        source: FrameSource::Current,
    });
    if !fast {
        b.push(FlushStep::WritePlane {
            plane: RamPlane::Old,
            source: FrameSource::White,
        });
    } else if dark_background {
        b.push(FlushStep::WritePlane {
            plane: RamPlane::Old,
            source: FrameSource::InvertedCurrent,
        });
    }
    b.cmd(
        CMD_VCOM_DATA_INTERVAL,
        if fast { &CDI_FAST_DATA } else { &CDI_FULL_DATA },
    );
    b.cmd(CMD_CCSET, &CCSET_DATA);
    b.cmd(
        CMD_TSSET,
        if fast {
            &TSSET_FAST_DATA
        } else {
            &TSSET_FULL_DATA
        },
    );
    if fast {
        b.cmd(CMD_PFS, &PFS_DATA);
        b.cmd(CMD_GATE_SCAN, &GATE_SCAN_DATA);
    }
    if !screen_powered {
        b.cmd(CMD_POWER_ON, &[]);
        b.push(FlushStep::WaitBusy);
    }
    if fast {
        b.cmd(CMD_PARTIAL_IN, &[]);
        b.cmd(CMD_PARTIAL_WINDOW, &PARTIAL_WINDOW_DATA);
    }
    b.cmd(CMD_PANEL_SETTING, &PSR_OTP);
    b.cmd(CMD_DISPLAY_REFRESH, &[]);
    b.push(FlushStep::WaitBusy);
    if fast {
        b.cmd(CMD_PARTIAL_OUT, &[]);
    }
    b.push(FlushStep::WritePlane {
        plane: RamPlane::Old,
        source: FrameSource::Current,
    });
    if turn_off {
        b.cmd(CMD_POWER_OFF, &[]);
        b.push(FlushStep::WaitBusy);
    }
    (b.steps, b.len)
}

macro_rules! step_table {
    ($fast:expr, $dark:expr, $powered:expr, $turn_off:expr) => {{
        const RAW: ([FlushStep; MAX_FLUSH_STEPS], usize) =
            build_flush_steps($fast, $dark, $powered, $turn_off);
        RAW.0.split_at(RAW.1).0
    }};
}

const FULL_POWER_ON_KEEP_ON: &[FlushStep] = step_table!(false, false, false, false);
const FULL_POWER_ON_TURN_OFF: &[FlushStep] = step_table!(false, false, false, true);
const FULL_POWERED_KEEP_ON: &[FlushStep] = step_table!(false, false, true, false);
const FULL_POWERED_TURN_OFF: &[FlushStep] = step_table!(false, false, true, true);

const FAST_LIGHT_POWER_ON_KEEP_ON: &[FlushStep] = step_table!(true, false, false, false);
const FAST_LIGHT_POWER_ON_TURN_OFF: &[FlushStep] = step_table!(true, false, false, true);
const FAST_LIGHT_POWERED_KEEP_ON: &[FlushStep] = step_table!(true, false, true, false);
const FAST_LIGHT_POWERED_TURN_OFF: &[FlushStep] = step_table!(true, false, true, true);

const FAST_DARK_POWER_ON_KEEP_ON: &[FlushStep] = step_table!(true, true, false, false);
const FAST_DARK_POWER_ON_TURN_OFF: &[FlushStep] = step_table!(true, true, false, true);
const FAST_DARK_POWERED_KEEP_ON: &[FlushStep] = step_table!(true, true, true, false);
const FAST_DARK_POWERED_TURN_OFF: &[FlushStep] = step_table!(true, true, true, true);

/// Select the controller operation stream for one UC8279 X4-family refresh.
///
/// `Fast` and `FastClean` require a valid `DTM1` baseline (`old_plane_valid`);
/// when `!old_plane_valid`, both promote to `RefreshMode::Full`.
pub const fn flush_plan(
    requested_mode: RefreshMode,
    screen_powered: bool,
    old_plane_valid: bool,
    dark_background: bool,
    turn_off: bool,
) -> FlushPlan {
    let effective_mode = match requested_mode {
        RefreshMode::Fast | RefreshMode::FastClean if !old_plane_valid => RefreshMode::Full,
        other => other,
    };
    let steps = match effective_mode {
        RefreshMode::Full | RefreshMode::PowerDown => match (screen_powered, turn_off) {
            (false, false) => FULL_POWER_ON_KEEP_ON,
            (false, true) => FULL_POWER_ON_TURN_OFF,
            (true, false) => FULL_POWERED_KEEP_ON,
            (true, true) => FULL_POWERED_TURN_OFF,
        },
        RefreshMode::Fast | RefreshMode::FastClean => {
            match (dark_background, screen_powered, turn_off) {
                (false, false, false) => FAST_LIGHT_POWER_ON_KEEP_ON,
                (false, false, true) => FAST_LIGHT_POWER_ON_TURN_OFF,
                (false, true, false) => FAST_LIGHT_POWERED_KEEP_ON,
                (false, true, true) => FAST_LIGHT_POWERED_TURN_OFF,
                (true, false, false) => FAST_DARK_POWER_ON_KEEP_ON,
                (true, false, true) => FAST_DARK_POWER_ON_TURN_OFF,
                (true, true, false) => FAST_DARK_POWERED_KEEP_ON,
                (true, true, true) => FAST_DARK_POWERED_TURN_OFF,
            }
        }
    };
    FlushPlan {
        requested_mode,
        effective_mode,
        steps,
    }
}

/// One controller-level operation in a UC8279 deep-sleep transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepStep {
    PowerOff,
    DeepSleep,
}

const SLEEP_POWERED_STEPS: &[SleepStep] = &[SleepStep::PowerOff, SleepStep::DeepSleep];
const SLEEP_UNPOWERED_STEPS: &[SleepStep] = &[SleepStep::DeepSleep];

/// Select the deep-sleep operation stream for the current charge-pump state.
pub const fn sleep_plan(screen_powered: bool) -> &'static [SleepStep] {
    if screen_powered {
        SLEEP_POWERED_STEPS
    } else {
        SLEEP_UNPOWERED_STEPS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd_step(cmd: u8, data: &'static [u8]) -> FlushStep {
        FlushStep::Command { cmd, data }
    }

    #[test]
    fn init_sequence_matches_vendor_reference() {
        assert_eq!(
            INIT_SEQUENCE,
            &[
                SpiOp::Command {
                    cmd: CMD_PANEL_SETTING,
                    data: &[0x37, 0x4D],
                },
                SpiOp::Command {
                    cmd: CMD_RESOLUTION,
                    data: &[0x03, 0x20, 0x02, 0x58],
                },
                SpiOp::Command {
                    cmd: CMD_GATE_SOURCE_START,
                    data: &[0x00, 0x00, 0x00, 0x00],
                },
                SpiOp::Command {
                    cmd: CMD_PFS,
                    data: &[0x20],
                },
                SpiOp::Command {
                    cmd: CMD_PLL,
                    data: &[0x0E],
                },
                SpiOp::Command {
                    cmd: CMD_GATE_SCAN,
                    data: &[0x02],
                },
            ]
        );
    }

    #[test]
    fn partial_window_data_matches_800x480_with_120_gate_offset() {
        assert_eq!(
            PARTIAL_WINDOW_DATA,
            [0x00, 0x00, 0x03, 0x1F, 0x00, 0x78, 0x02, 0x57, 0x01]
        );
    }

    #[test]
    fn gate_row_and_fill_gate_row_handle_padding_visible_invert_and_bounds() {
        assert_eq!(gate_row(0), Some(GateRow::WhitePadding));
        assert_eq!(gate_row(119), Some(GateRow::WhitePadding));
        assert_eq!(gate_row(120), Some(GateRow::Visible(0)));
        assert_eq!(gate_row(599), Some(GateRow::Visible(479)));
        assert_eq!(gate_row(600), None);
        assert_eq!(gate_row(u16::MAX), None);

        let mut fb = [0u8; FB_BYTES];
        for (i, byte) in fb[..ROW_BYTES].iter_mut().enumerate() {
            *byte = i as u8;
        }
        let last_row_start = 479 * ROW_BYTES;
        for (i, byte) in fb[last_row_start..].iter_mut().enumerate() {
            *byte = (0xA0u8).wrapping_add(i as u8);
        }

        let mut out = [0x11u8; ROW_BYTES];
        for pad_gate in [0u16, 119] {
            fill_gate_row(&fb, pad_gate, false, &mut out);
            assert_eq!(out, [0xFF; ROW_BYTES]);
            out.fill(0x00);
            fill_gate_row(&fb, pad_gate, true, &mut out);
            assert_eq!(
                out, [0xFF; ROW_BYTES],
                "white padding must not invert at gate {pad_gate}"
            );
        }

        fill_gate_row(&fb, 120, false, &mut out);
        assert_eq!(out, fb[..ROW_BYTES]);
        fill_gate_row(&fb, 120, true, &mut out);
        for (i, &byte) in out.iter().enumerate() {
            assert_eq!(byte, !(i as u8));
        }

        fill_gate_row(&fb, 599, false, &mut out);
        assert_eq!(out, fb[last_row_start..]);
        fill_gate_row(&fb, 599, true, &mut out);
        for (i, &byte) in out.iter().enumerate() {
            assert_eq!(byte, !(0xA0u8.wrapping_add(i as u8)));
        }

        for oob_gate in [600u16, u16::MAX] {
            out.fill(0x22);
            fill_gate_row(&fb, oob_gate, false, &mut out);
            assert_eq!(out, [0xFF; ROW_BYTES]);
            out.fill(0x22);
            fill_gate_row(&fb, oob_gate, true, &mut out);
            assert_eq!(out, [0xFF; ROW_BYTES]);
        }

        let short_fb = [0x55u8; ROW_BYTES + 10];
        fill_gate_row(&short_fb, 120, false, &mut out);
        assert_eq!(out, [0x55; ROW_BYTES]);
        fill_gate_row(&short_fb, 121, false, &mut out);
        assert_eq!(out, [0xFF; ROW_BYTES]);
        fill_gate_row(&[], 120, true, &mut out);
        assert_eq!(out, [0xFF; ROW_BYTES]);
    }

    #[cfg(not(feature = "device-x3"))]
    #[test]
    fn fill_gate_row_from_framebuffer_streams_native_rows_forward() {
        let mut out = [0x11u8; ROW_BYTES];

        // Portrait (boot orientation): Framebuffer::map already folds the panel
        // scan/column order into fb.bytes(), so gates 120..600 forward match
        // Uc8279X4Driver::streamPlane.
        let mut portrait_fb = crate::fb::Framebuffer::new();
        portrait_fb.set_frame(crate::fb::FbFrame::Portrait);
        portrait_fb.clear(true);
        portrait_fb.set_pixel(0, 0, false);
        portrait_fb.set_pixel(479, 799, false);

        // White padding gates must always remain 0xFF regardless of `invert`.
        for pad_gate in [0u16, 119, 600, u16::MAX] {
            out.fill(0x22);
            fill_gate_row_from_framebuffer(&portrait_fb, pad_gate, false, &mut out);
            assert_eq!(out, [0xFF; ROW_BYTES], "gate {pad_gate} invert=false");

            out.fill(0x33);
            fill_gate_row_from_framebuffer(&portrait_fb, pad_gate, true, &mut out);
            assert_eq!(out, [0xFF; ROW_BYTES], "gate {pad_gate} invert=true");
        }

        // Portrait top-left (0, 0) -> native (x=799, y=479) -> gate 599, byte 0, mask 0x80.
        fill_gate_row_from_framebuffer(&portrait_fb, 599, false, &mut out);
        assert_eq!(out[0], 0x7F);
        assert!(out[1..].iter().all(|&b| b == 0xFF));

        fill_gate_row_from_framebuffer(&portrait_fb, 599, true, &mut out);
        assert_eq!(out[0], 0x80);
        assert!(out[1..].iter().all(|&b| b == 0x00));

        // Portrait bottom-right (479, 799) -> native (x=0, y=0) -> gate 120, byte 99, mask 0x01.
        fill_gate_row_from_framebuffer(&portrait_fb, 120, false, &mut out);
        assert_eq!(out[99], 0xFE);
        assert!(out[..99].iter().all(|&b| b == 0xFF));

        fill_gate_row_from_framebuffer(&portrait_fb, 120, true, &mut out);
        assert_eq!(out[99], 0x01);
        assert!(out[..99].iter().all(|&b| b == 0x00));

        // Landscape: top-left (0, 0) -> native (0, 479) -> gate 599, byte 99, mask 0x01.
        let mut landscape_fb = crate::fb::Framebuffer::new();
        landscape_fb.set_frame(crate::fb::FbFrame::Landscape);
        landscape_fb.clear(true);
        landscape_fb.set_pixel(0, 0, false);
        landscape_fb.set_pixel(799, 479, false);
        landscape_fb.set_pixel(123, 200, false);

        fill_gate_row_from_framebuffer(&landscape_fb, 599, false, &mut out);
        assert_eq!(out[99], 0xFE);
        assert!(out[..99].iter().all(|&b| b == 0xFF));

        // Landscape bottom-right (799, 479) -> native (799, 0) -> gate 120, byte 0, mask 0x80.
        fill_gate_row_from_framebuffer(&landscape_fb, 120, false, &mut out);
        assert_eq!(out[0], 0x7F);
        assert!(out[1..].iter().all(|&b| b == 0xFF));

        // Landscape (123, 200) -> native (123, 279) -> gate 399, col 84, bit 3 (0x08).
        fill_gate_row_from_framebuffer(&landscape_fb, 399, false, &mut out);
        assert_eq!(out[84], 0xF7);
        for (idx, &b) in out.iter().enumerate() {
            if idx != 84 {
                assert_eq!(b, 0xFF, "unexpected non-white byte at col {idx}");
            }
        }

        fill_gate_row_from_framebuffer(&landscape_fb, 399, true, &mut out);
        assert_eq!(out[84], 0x08);
        for (idx, &b) in out.iter().enumerate() {
            if idx != 84 {
                assert_eq!(b, 0x00, "unexpected non-black inverted byte at col {idx}");
            }
        }
    }

    #[cfg(feature = "device-x3")]
    #[test]
    fn fill_gate_row_from_framebuffer_pads_white_on_mismatched_x3_geometry() {
        let mut fb = crate::fb::Framebuffer::new();
        fb.clear(false);
        let mut out = [0x00u8; ROW_BYTES];
        fill_gate_row_from_framebuffer(&fb, 120, true, &mut out);
        assert_eq!(out, [0xFF; ROW_BYTES]);
    }

    #[test]
    fn full_plan_follows_display_start_and_finish_sequence() {
        let unpowered_keep_on = flush_plan(RefreshMode::Full, false, false, false, false);
        assert_eq!(unpowered_keep_on.requested_mode, RefreshMode::Full);
        assert_eq!(unpowered_keep_on.effective_mode, RefreshMode::Full);
        assert_eq!(
            unpowered_keep_on.steps,
            &[
                FlushStep::WritePlane {
                    plane: RamPlane::New,
                    source: FrameSource::Current,
                },
                FlushStep::WritePlane {
                    plane: RamPlane::Old,
                    source: FrameSource::White,
                },
                cmd_step(CMD_VCOM_DATA_INTERVAL, &[CDI_FULL]),
                cmd_step(CMD_CCSET, &[CCSET]),
                cmd_step(CMD_TSSET, &[TSSET_FULL]),
                cmd_step(CMD_POWER_ON, &[]),
                FlushStep::WaitBusy,
                cmd_step(CMD_PANEL_SETTING, &PSR_OTP),
                cmd_step(CMD_DISPLAY_REFRESH, &[]),
                FlushStep::WaitBusy,
                FlushStep::WritePlane {
                    plane: RamPlane::Old,
                    source: FrameSource::Current,
                },
            ]
        );

        let powered_turn_off = flush_plan(RefreshMode::Full, true, true, true, true);
        assert_eq!(powered_turn_off.effective_mode, RefreshMode::Full);
        assert_eq!(
            powered_turn_off.steps,
            &[
                FlushStep::WritePlane {
                    plane: RamPlane::New,
                    source: FrameSource::Current,
                },
                FlushStep::WritePlane {
                    plane: RamPlane::Old,
                    source: FrameSource::White,
                },
                cmd_step(CMD_VCOM_DATA_INTERVAL, &[CDI_FULL]),
                cmd_step(CMD_CCSET, &[CCSET]),
                cmd_step(CMD_TSSET, &[TSSET_FULL]),
                cmd_step(CMD_PANEL_SETTING, &PSR_OTP),
                cmd_step(CMD_DISPLAY_REFRESH, &[]),
                FlushStep::WaitBusy,
                FlushStep::WritePlane {
                    plane: RamPlane::Old,
                    source: FrameSource::Current,
                },
                cmd_step(CMD_POWER_OFF, &[]),
                FlushStep::WaitBusy,
            ]
        );
    }

    #[test]
    fn fast_and_fast_clean_promote_to_full_when_old_plane_invalid() {
        for mode in [RefreshMode::Fast, RefreshMode::FastClean] {
            for screen_powered in [false, true] {
                for dark_background in [false, true] {
                    for turn_off in [false, true] {
                        let promoted =
                            flush_plan(mode, screen_powered, false, dark_background, turn_off);
                        let full = flush_plan(
                            RefreshMode::Full,
                            screen_powered,
                            false,
                            dark_background,
                            turn_off,
                        );
                        assert_eq!(promoted.requested_mode, mode);
                        assert_eq!(promoted.effective_mode, RefreshMode::Full);
                        assert_eq!(promoted.steps, full.steps);
                    }
                }
            }
        }
    }

    #[test]
    fn fast_plan_with_valid_old_plane_covers_dark_background_power_and_turn_off() {
        let light_unpowered = flush_plan(RefreshMode::Fast, false, true, false, false);
        assert_eq!(light_unpowered.requested_mode, RefreshMode::Fast);
        assert_eq!(light_unpowered.effective_mode, RefreshMode::Fast);
        assert_eq!(
            light_unpowered.steps,
            &[
                FlushStep::WritePlane {
                    plane: RamPlane::New,
                    source: FrameSource::Current,
                },
                cmd_step(CMD_VCOM_DATA_INTERVAL, &[CDI_FAST]),
                cmd_step(CMD_CCSET, &[CCSET]),
                cmd_step(CMD_TSSET, &[TSSET_FAST]),
                cmd_step(CMD_PFS, &[PFS]),
                cmd_step(CMD_GATE_SCAN, &[GATE_SCAN]),
                cmd_step(CMD_POWER_ON, &[]),
                FlushStep::WaitBusy,
                cmd_step(CMD_PARTIAL_IN, &[]),
                cmd_step(CMD_PARTIAL_WINDOW, &PARTIAL_WINDOW_DATA),
                cmd_step(CMD_PANEL_SETTING, &PSR_OTP),
                cmd_step(CMD_DISPLAY_REFRESH, &[]),
                FlushStep::WaitBusy,
                cmd_step(CMD_PARTIAL_OUT, &[]),
                FlushStep::WritePlane {
                    plane: RamPlane::Old,
                    source: FrameSource::Current,
                },
            ]
        );

        let dark_powered_off = flush_plan(RefreshMode::Fast, true, true, true, true);
        assert_eq!(dark_powered_off.effective_mode, RefreshMode::Fast);
        assert_eq!(
            dark_powered_off.steps,
            &[
                FlushStep::WritePlane {
                    plane: RamPlane::New,
                    source: FrameSource::Current,
                },
                FlushStep::WritePlane {
                    plane: RamPlane::Old,
                    source: FrameSource::InvertedCurrent,
                },
                cmd_step(CMD_VCOM_DATA_INTERVAL, &[CDI_FAST]),
                cmd_step(CMD_CCSET, &[CCSET]),
                cmd_step(CMD_TSSET, &[TSSET_FAST]),
                cmd_step(CMD_PFS, &[PFS]),
                cmd_step(CMD_GATE_SCAN, &[GATE_SCAN]),
                cmd_step(CMD_PARTIAL_IN, &[]),
                cmd_step(CMD_PARTIAL_WINDOW, &PARTIAL_WINDOW_DATA),
                cmd_step(CMD_PANEL_SETTING, &PSR_OTP),
                cmd_step(CMD_DISPLAY_REFRESH, &[]),
                FlushStep::WaitBusy,
                cmd_step(CMD_PARTIAL_OUT, &[]),
                FlushStep::WritePlane {
                    plane: RamPlane::Old,
                    source: FrameSource::Current,
                },
                cmd_step(CMD_POWER_OFF, &[]),
                FlushStep::WaitBusy,
            ]
        );

        let clean_valid = flush_plan(RefreshMode::FastClean, true, true, true, true);
        assert_eq!(clean_valid.requested_mode, RefreshMode::FastClean);
        assert_eq!(clean_valid.effective_mode, RefreshMode::FastClean);
        assert_eq!(clean_valid.steps, dark_powered_off.steps);
    }

    #[test]
    fn power_down_plan_matches_full_sequence_and_charge_pump_rules() {
        for screen_powered in [false, true] {
            for old_plane_valid in [false, true] {
                for dark_background in [false, true] {
                    for turn_off in [false, true] {
                        let pd = flush_plan(
                            RefreshMode::PowerDown,
                            screen_powered,
                            old_plane_valid,
                            dark_background,
                            turn_off,
                        );
                        let full = flush_plan(
                            RefreshMode::Full,
                            screen_powered,
                            old_plane_valid,
                            dark_background,
                            turn_off,
                        );
                        assert_eq!(pd.requested_mode, RefreshMode::PowerDown);
                        assert_eq!(pd.effective_mode, RefreshMode::PowerDown);
                        assert_eq!(pd.steps, full.steps);
                    }
                }
            }
        }
    }

    #[test]
    fn exhaustive_plan_invariants_hold_across_all_modes_and_flags() {
        assert_eq!(RamPlane::Old.cmd(), CMD_DTM1);
        assert_eq!(RamPlane::New.cmd(), CMD_DTM2);

        for mode in [
            RefreshMode::Full,
            RefreshMode::Fast,
            RefreshMode::FastClean,
            RefreshMode::PowerDown,
        ] {
            for screen_powered in [false, true] {
                for old_plane_valid in [false, true] {
                    for dark_background in [false, true] {
                        for turn_off in [false, true] {
                            let plan = flush_plan(
                                mode,
                                screen_powered,
                                old_plane_valid,
                                dark_background,
                                turn_off,
                            );
                            let fast = matches!(
                                plan.effective_mode,
                                RefreshMode::Fast | RefreshMode::FastClean
                            );

                            assert_eq!(
                                plan.steps.first(),
                                Some(&FlushStep::WritePlane {
                                    plane: RamPlane::New,
                                    source: FrameSource::Current,
                                })
                            );

                            let pon = cmd_step(CMD_POWER_ON, &[]);
                            let drf = cmd_step(CMD_DISPLAY_REFRESH, &[]);
                            let pof = cmd_step(CMD_POWER_OFF, &[]);
                            let ptout = cmd_step(CMD_PARTIAL_OUT, &[]);
                            let psr = cmd_step(CMD_PANEL_SETTING, &PSR_OTP);
                            let sync = FlushStep::WritePlane {
                                plane: RamPlane::Old,
                                source: FrameSource::Current,
                            };

                            assert_eq!(plan.steps.contains(&pon), !screen_powered);
                            assert_eq!(plan.steps.contains(&pof), turn_off);
                            assert_eq!(plan.steps.contains(&ptout), fast);

                            let psr_pos = plan.steps.iter().position(|s| *s == psr).unwrap();
                            let drf_pos = plan.steps.iter().position(|s| *s == drf).unwrap();
                            assert!(psr_pos < drf_pos);
                            assert_eq!(plan.steps.get(drf_pos + 1), Some(&FlushStep::WaitBusy));

                            if let Some(pon_pos) = plan.steps.iter().position(|s| *s == pon) {
                                assert!(pon_pos < psr_pos);
                                assert_eq!(plan.steps.get(pon_pos + 1), Some(&FlushStep::WaitBusy));
                            }

                            let sync_pos = plan.steps.iter().rposition(|s| *s == sync).unwrap();
                            assert!(sync_pos > drf_pos + 1);
                            if fast {
                                let ptout_pos =
                                    plan.steps.iter().position(|s| *s == ptout).unwrap();
                                assert_eq!(ptout_pos, drf_pos + 2);
                                assert_eq!(sync_pos, ptout_pos + 1);
                            } else {
                                assert_eq!(sync_pos, drf_pos + 2);
                            }

                            if turn_off {
                                let pof_pos = plan.steps.iter().position(|s| *s == pof).unwrap();
                                assert_eq!(pof_pos, sync_pos + 1);
                                assert_eq!(plan.steps.get(pof_pos + 1), Some(&FlushStep::WaitBusy));
                                assert_eq!(plan.steps.len(), pof_pos + 2);
                            } else {
                                assert_eq!(plan.steps.len(), sync_pos + 1);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn sleep_plan_covers_powered_and_unpowered_panel() {
        assert_eq!(
            sleep_plan(true),
            &[SleepStep::PowerOff, SleepStep::DeepSleep]
        );
        assert_eq!(sleep_plan(false), &[SleepStep::DeepSleep]);
    }
}
