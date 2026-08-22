//! UC8253 (Xteink X3) protocol model for the emulator.
//!
//! Refresh sequencing comes from `display::epd::uc8253::flush_plan`, the
//! same allocation-free plan executed by the firmware backend. This module
//! only models controller state: command lengths, complete RAM-plane writes,
//! DTM1 data-stop ordering, LUT/CDI readiness, power, refresh, and sleep.

use crate::panel_common::{
    cmd_history_entry, expect_len, ram_history_entry, HISTORY_DEEP_SLEEP, HISTORY_RESET,
};
use display::epd::{
    bank_for, flush_plan, partial_flush_plan, partial_window_data, sleep_plan, FlushStep,
    FrameSource, LutBank, RamPlane, RefreshMode, SleepStep, SpiOp, CDI_INTERVAL,
    CMD_BOOSTER_SOFT_START, CMD_DATA_STOP, CMD_DEEP_SLEEP, CMD_DISPLAY_REFRESH, CMD_DTM1, CMD_DTM2,
    CMD_GATE_SOURCE_START, CMD_LUT_BB, CMD_LUT_BW, CMD_LUT_VCOM, CMD_LUT_WB, CMD_LUT_WW,
    CMD_LV_SELECTION, CMD_PANEL_SETTING, CMD_PARTIAL_IN, CMD_PARTIAL_OUT, CMD_PARTIAL_WINDOW,
    CMD_PLL_CONTROL, CMD_POWER_OFF, CMD_POWER_OFF_SEQ, CMD_POWER_ON, CMD_POWER_SETTING,
    CMD_RESOLUTION, CMD_VCOM_DATA_INTERVAL, CMD_VCOM_DC, DEEP_SLEEP_CHECK, INIT_SEQUENCE, LUT_LEN,
    PARTIAL_PRESTAGE_STEPS, PRESTAGE_STEPS,
};
use display::fb::Framebuffer;
use display::{Rect, BAND_ROWS, FB_BYTES, HEIGHT, ROW_BYTES, WIDTH};

#[derive(Clone, Copy, Debug)]
struct RamWriteState {
    plane: RamPlane,
    written: usize,
}

#[derive(Debug)]
pub struct PanelModel {
    old_ram: [u8; FB_BYTES],
    new_ram: [u8; FB_BYTES],
    displayed: [u8; FB_BYTES],
    deep_sleep: bool,
    initialized: bool,
    powered: bool,
    lut_written: [bool; 5],
    cdi: Option<[u8; 2]>,
    last_refresh: Option<RefreshMode>,
    in_partial_mode: bool,
    partial_window: Option<Rect>,
    ram_write: Option<RamWriteState>,
    plane_complete: [bool; 2],
    old_needs_stop: bool,
    commands: Vec<(u8, usize)>,
    history: Vec<String>,
}

impl PanelModel {
    pub fn new() -> Self {
        Self {
            old_ram: [0xFF; FB_BYTES],
            new_ram: [0xFF; FB_BYTES],
            displayed: [0xFF; FB_BYTES],
            deep_sleep: false,
            initialized: false,
            powered: false,
            lut_written: [false; 5],
            cdi: None,
            last_refresh: None,
            in_partial_mode: false,
            partial_window: None,
            ram_write: None,
            plane_complete: [false; 2],
            old_needs_stop: false,
            commands: Vec::new(),
            history: Vec::new(),
        }
    }

    pub fn init_sequence(&mut self) -> Result<(), String> {
        self.reset();
        for op in INIT_SEQUENCE {
            if let SpiOp::Command { cmd, data } = *op {
                self.command(cmd, data)?;
            }
        }
        self.initialized = true;
        self.write_fill(RamPlane::Old, 0xFF)?;
        self.command(CMD_DATA_STOP, &[])?;
        self.write_fill(RamPlane::New, 0xFF)?;
        self.command(CMD_DATA_STOP, &[])?;
        Ok(())
    }

    pub fn command(&mut self, cmd: u8, data: &[u8]) -> Result<(), String> {
        if self.deep_sleep {
            return Err(format!("command 0x{cmd:02X} while panel is asleep"));
        }
        if self.ram_write.is_some() {
            return Err(format!("command 0x{cmd:02X} during a RAM write"));
        }
        if self.old_needs_stop && cmd != CMD_DATA_STOP {
            return Err(format!(
                "command 0x{cmd:02X} before DATA_STOP completed DTM1"
            ));
        }

        let expected = match cmd {
            CMD_PANEL_SETTING => Some(2),
            CMD_RESOLUTION | CMD_GATE_SOURCE_START | CMD_BOOSTER_SOFT_START => Some(4),
            CMD_POWER_OFF_SEQ | CMD_PLL_CONTROL | CMD_VCOM_DC | CMD_LV_SELECTION => Some(1),
            CMD_POWER_SETTING => Some(5),
            CMD_VCOM_DATA_INTERVAL => Some(2),
            CMD_LUT_VCOM | CMD_LUT_WW | CMD_LUT_BW | CMD_LUT_WB | CMD_LUT_BB => Some(LUT_LEN),
            CMD_PARTIAL_WINDOW => Some(9),
            CMD_POWER_ON | CMD_POWER_OFF | CMD_DISPLAY_REFRESH | CMD_DATA_STOP | CMD_PARTIAL_IN
            | CMD_PARTIAL_OUT => Some(0),
            CMD_DEEP_SLEEP => Some(1),
            _ => None,
        };
        if let Some(expected) = expected {
            expect_len(cmd, data, expected)?;
        }

        match cmd {
            CMD_POWER_ON => {
                if !self.initialized {
                    return Err("PON before init sequence".into());
                }
                self.powered = true;
            }
            CMD_POWER_OFF => self.powered = false,
            CMD_DATA_STOP => self.old_needs_stop = false,
            CMD_PARTIAL_IN => {
                if self.in_partial_mode {
                    return Err("nested PARTIAL_IN without PARTIAL_OUT".into());
                }
                self.in_partial_mode = true;
            }
            CMD_PARTIAL_WINDOW => {
                if !self.in_partial_mode {
                    return Err("PARTIAL_WINDOW before PARTIAL_IN".into());
                }
                if data[8] != 0x01 {
                    return Err(format!("expected pt_scan=0x01, got 0x{:02X}", data[8]));
                }
                let xs = u16::from_be_bytes([data[0], data[1]]);
                let xe = u16::from_be_bytes([data[2], data[3]]);
                let ys = u16::from_be_bytes([data[4], data[5]]);
                let ye = u16::from_be_bytes([data[6], data[7]]);
                if xs > xe || xe >= WIDTH as u16 || ys > ye || ye >= HEIGHT as u16 {
                    return Err(format!(
                        "invalid window coordinates xs={xs}, xe={xe}, ys={ys}, ye={ye}"
                    ));
                }
                if xs % 8 != 0 || (xe + 1) % 8 != 0 {
                    return Err(format!("unaligned X window coordinates xs={xs}, xe={xe}"));
                }
                self.partial_window = Some(Rect {
                    x: xs,
                    y: ys,
                    w: xe - xs + 1,
                    h: ye - ys + 1,
                });
            }
            CMD_PARTIAL_OUT => {
                if !self.in_partial_mode {
                    return Err("PARTIAL_OUT without PARTIAL_IN".into());
                }
                self.in_partial_mode = false;
                self.partial_window = None;
            }
            CMD_VCOM_DATA_INTERVAL => self.cdi = Some([data[0], data[1]]),
            CMD_LUT_VCOM => self.lut_written[0] = true,
            CMD_LUT_WW => self.lut_written[1] = true,
            CMD_LUT_BW => self.lut_written[2] = true,
            CMD_LUT_WB => self.lut_written[3] = true,
            CMD_LUT_BB => self.lut_written[4] = true,
            CMD_DISPLAY_REFRESH => {
                if !self.initialized {
                    return Err("DRF before init sequence".into());
                }
                if !self.powered {
                    return Err("DRF while charge pump is off".into());
                }
                if !self.plane_complete[plane_index(RamPlane::New)] {
                    return Err("DRF before a complete DTM2 write".into());
                }
                if self.cdi.is_none() || !self.lut_written.iter().all(|written| *written) {
                    return Err("DRF before a complete LUT bank and CDI".into());
                }
                if let Some(win) = self.partial_window {
                    let min_bx = (ROW_BYTES - 1) - ((win.x + win.w) / 8 - 1) as usize;
                    let max_bx = (ROW_BYTES - 1) - (win.x / 8) as usize;
                    for y in (win.y as usize)..((win.y + win.h) as usize) {
                        let row_start = y * ROW_BYTES;
                        self.displayed[row_start + min_bx..=row_start + max_bx].copy_from_slice(
                            &self.new_ram[row_start + min_bx..=row_start + max_bx],
                        );
                    }
                } else {
                    self.displayed.copy_from_slice(&self.new_ram);
                }
            }
            CMD_DEEP_SLEEP => {
                if data != [DEEP_SLEEP_CHECK] {
                    return Err("DSLP requires the 0xA5 check byte".into());
                }
                if self.powered {
                    return Err("DSLP while charge pump is on".into());
                }
                self.deep_sleep = true;
            }
            _ => {}
        }
        self.commands.push((cmd, data.len()));
        self.history.push(cmd_history_entry(cmd, data));
        Ok(())
    }

    pub fn flush(
        &mut self,
        current: &Framebuffer,
        previous: &Framebuffer,
        requested_mode: RefreshMode,
        previous_staged: bool,
    ) -> Result<RefreshMode, String> {
        if !self.initialized {
            return Err("flush before init sequence".into());
        }
        let plan = flush_plan(requested_mode, self.powered, previous_staged);
        if plan.effective_mode == RefreshMode::Fast
            && previous_staged
            && !transformed_matches(&self.old_ram, previous)
        {
            return Err("prestaged DTM1 does not match the previous framebuffer".into());
        }
        self.execute_steps(current, previous, plan.steps, None)?;
        self.last_refresh = Some(plan.effective_mode);
        self.history
            .push(format!("refresh {:?}", plan.effective_mode));
        Ok(plan.effective_mode)
    }

    pub fn flush_window(
        &mut self,
        current: &Framebuffer,
        previous: &Framebuffer,
        rect: Rect,
        previous_staged: bool,
    ) -> Result<RefreshMode, String> {
        if !self.initialized {
            return Err("flush before init sequence".into());
        }
        let plan = partial_flush_plan(self.powered, previous_staged);
        if plan.effective_mode == RefreshMode::Fast
            && previous_staged
            && !transformed_matches(&self.old_ram, previous)
        {
            return Err("prestaged DTM1 does not match the previous framebuffer".into());
        }
        self.execute_steps(current, previous, plan.steps, Some(rect))?;
        self.last_refresh = Some(plan.effective_mode);
        self.history.push(format!(
            "refresh {:?} window {:?}",
            plan.effective_mode, rect
        ));
        Ok(plan.effective_mode)
    }

    pub fn prestage_previous(&mut self, fb: &Framebuffer) -> Result<(), String> {
        self.execute_steps(fb, fb, PRESTAGE_STEPS, None)
    }

    pub fn prestage_previous_window(&mut self, fb: &Framebuffer, rect: Rect) -> Result<(), String> {
        self.execute_steps(fb, fb, PARTIAL_PRESTAGE_STEPS, Some(rect))
    }

    pub fn deep_sleep(&mut self) -> Result<(), String> {
        for step in sleep_plan(self.powered) {
            match step {
                SleepStep::PowerOff => self.command(CMD_POWER_OFF, &[])?,
                SleepStep::DeepSleep => self.command(CMD_DEEP_SLEEP, &[DEEP_SLEEP_CHECK])?,
            }
        }
        self.history.push(HISTORY_DEEP_SLEEP.into());
        Ok(())
    }

    pub fn last_refresh(&self) -> Option<RefreshMode> {
        self.last_refresh
    }

    pub fn is_deep_sleep(&self) -> bool {
        self.deep_sleep
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    fn execute_steps(
        &mut self,
        current: &Framebuffer,
        previous: &Framebuffer,
        steps: &[FlushStep],
        window: Option<Rect>,
    ) -> Result<(), String> {
        for step in steps {
            match *step {
                FlushStep::LoadBank(mode) => {
                    let (bank, cdi) = bank_for(mode);
                    self.load_lut(cdi, bank)?;
                }
                FlushStep::WritePlane { plane, source } => match source {
                    FrameSource::White => self.write_fill(plane, 0xFF)?,
                    FrameSource::Current => self.write_framebuffer(plane, current)?,
                    FrameSource::Previous => self.write_framebuffer(plane, previous)?,
                },
                FlushStep::WriteWindowPlane { plane, source } => match source {
                    FrameSource::White => self.write_fill(plane, 0xFF)?,
                    FrameSource::Current => {
                        self.write_window_framebuffer(plane, current, window)?
                    }
                    FrameSource::Previous => {
                        self.write_window_framebuffer(plane, previous, window)?
                    }
                },
                FlushStep::SetPartialWindow => {
                    self.command(CMD_PARTIAL_IN, &[])?;
                    let win_data = partial_window_data(window.unwrap_or(Rect::FULL));
                    self.command(CMD_PARTIAL_WINDOW, &win_data)?;
                }
                FlushStep::PartialOut => {
                    self.command(CMD_PARTIAL_OUT, &[])?;
                }
                FlushStep::DataStop => self.command(CMD_DATA_STOP, &[])?,
                FlushStep::PowerOn => self.command(CMD_POWER_ON, &[])?,
                FlushStep::DisplayRefresh => self.command(CMD_DISPLAY_REFRESH, &[])?,
                FlushStep::DelayMs(ms) => self.history.push(format!("delay {ms}ms")),
            }
        }
        Ok(())
    }

    fn load_lut(&mut self, cdi0: u8, bank: &LutBank) -> Result<(), String> {
        self.lut_written = [false; 5];
        self.command(CMD_VCOM_DATA_INTERVAL, &[cdi0, CDI_INTERVAL])?;
        self.command(CMD_LUT_VCOM, bank.vcom)?;
        self.command(CMD_LUT_WW, bank.ww)?;
        self.command(CMD_LUT_BW, bank.bw)?;
        self.command(CMD_LUT_WB, bank.wb)?;
        self.command(CMD_LUT_BB, bank.bb)
    }

    fn write_window_framebuffer(
        &mut self,
        plane: RamPlane,
        fb: &Framebuffer,
        window: Option<Rect>,
    ) -> Result<(), String> {
        let Some(rect) = window else {
            return self.write_framebuffer(plane, fb);
        };
        if !self.in_partial_mode {
            return Err("windowed plane write while not in partial mode".into());
        }
        let min_bx = (ROW_BYTES - 1) - ((rect.x + rect.w) / 8 - 1) as usize;
        let max_bx = (ROW_BYTES - 1) - (rect.x / 8) as usize;
        let target = match plane {
            RamPlane::Old => &mut self.old_ram,
            RamPlane::New => &mut self.new_ram,
        };
        for y in (rect.y as usize)..((rect.y + rect.h) as usize) {
            let row_start = y * ROW_BYTES;
            target[row_start + min_bx..=row_start + max_bx]
                .copy_from_slice(&fb.bytes()[row_start + min_bx..=row_start + max_bx]);
        }
        self.plane_complete[plane_index(plane)] = true;
        if plane == RamPlane::Old {
            self.old_needs_stop = true;
        }
        let command = match plane {
            RamPlane::Old => CMD_DTM1,
            RamPlane::New => CMD_DTM2,
        };
        let bytes = (rect.w as usize / 8) * rect.h as usize;
        self.commands.push((command, bytes));
        self.history
            .push(ram_history_entry(command, rect.w as usize, rect.h as usize));
        Ok(())
    }

    fn write_framebuffer(&mut self, plane: RamPlane, fb: &Framebuffer) -> Result<(), String> {
        self.begin_ram_write(plane)?;
        let mut y = 0;
        while y < HEIGHT {
            let band = fb.band(y, BAND_ROWS);
            if let Err(err) = self.ram_chunk(band) {
                self.ram_write = None;
                return Err(err);
            }
            y += BAND_ROWS;
        }
        self.end_ram_write()
    }

    fn write_fill(&mut self, plane: RamPlane, value: u8) -> Result<(), String> {
        self.begin_ram_write(plane)?;
        let row = [value; ROW_BYTES];
        for _ in 0..HEIGHT {
            if let Err(err) = self.ram_chunk(&row) {
                self.ram_write = None;
                return Err(err);
            }
        }
        self.end_ram_write()
    }

    fn begin_ram_write(&mut self, plane: RamPlane) -> Result<(), String> {
        if self.deep_sleep {
            return Err("RAM write while panel is asleep".into());
        }
        if !self.initialized {
            return Err("RAM write before init sequence".into());
        }
        if self.ram_write.is_some() {
            return Err("nested RAM write".into());
        }
        if self.old_needs_stop {
            return Err("RAM write before DATA_STOP completed DTM1".into());
        }
        self.plane_complete[plane_index(plane)] = false;
        self.ram_write = Some(RamWriteState { plane, written: 0 });
        Ok(())
    }

    fn ram_chunk(&mut self, data: &[u8]) -> Result<(), String> {
        let Some(state) = self.ram_write else {
            return Err("RAM data without an active write".into());
        };
        let end = state.written.saturating_add(data.len());
        if end > FB_BYTES {
            return Err(format!("RAM write exceeds {FB_BYTES} bytes"));
        }
        let target = match state.plane {
            RamPlane::Old => &mut self.old_ram,
            RamPlane::New => &mut self.new_ram,
        };
        target[state.written..end].copy_from_slice(data);
        self.ram_write = Some(RamWriteState {
            plane: state.plane,
            written: end,
        });
        Ok(())
    }

    fn end_ram_write(&mut self) -> Result<(), String> {
        let Some(state) = self.ram_write.take() else {
            return Err("RAM end without an active write".into());
        };
        if state.written != FB_BYTES {
            return Err(format!(
                "incomplete RAM write: expected {FB_BYTES} bytes, got {}",
                state.written
            ));
        }
        self.plane_complete[plane_index(state.plane)] = true;
        if state.plane == RamPlane::Old {
            self.old_needs_stop = true;
        }
        let command = match state.plane {
            RamPlane::Old => CMD_DTM1,
            RamPlane::New => CMD_DTM2,
        };
        self.commands.push((command, state.written));
        self.history.push(ram_history_entry(command, WIDTH, HEIGHT));
        Ok(())
    }

    fn reset(&mut self) {
        self.deep_sleep = false;
        self.initialized = false;
        self.powered = false;
        self.lut_written = [false; 5];
        self.cdi = None;
        self.in_partial_mode = false;
        self.partial_window = None;
        self.ram_write = None;
        self.plane_complete = [false; 2];
        self.old_needs_stop = false;
        self.history.push(HISTORY_RESET.into());
    }
}

fn plane_index(plane: RamPlane) -> usize {
    match plane {
        RamPlane::Old => 0,
        RamPlane::New => 1,
    }
}

fn transformed_matches(ram: &[u8; FB_BYTES], fb: &Framebuffer) -> bool {
    ram == fb.bytes()
}

impl Default for PanelModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use display::epd::{CDI_ABSOLUTE, CMD_DISPLAY_REFRESH};

    fn frame(x: usize, y: usize) -> Framebuffer {
        let mut fb = Framebuffer::new();
        fb.set_pixel(x, y, false);
        fb
    }

    #[test]
    fn full_refresh_executes_the_shared_two_refresh_plan() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        let target = frame(10, 10);
        let mode = panel
            .flush(&target, &Framebuffer::new(), RefreshMode::Full, false)
            .unwrap();
        assert_eq!(mode, RefreshMode::Full);
        assert!(panel.powered);
        assert!(transformed_matches(&panel.displayed, &target));
        assert_eq!(
            panel
                .commands
                .iter()
                .filter(|(cmd, _)| *cmd == CMD_DISPLAY_REFRESH)
                .count(),
            2
        );
    }

    #[test]
    fn fast_clean_is_one_absolute_refresh_and_leaves_old_plane_untouched() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        let first = frame(1, 2);
        panel
            .flush(&first, &Framebuffer::new(), RefreshMode::Full, false)
            .unwrap();
        let old_before = panel.old_ram;
        let second = frame(100, 200);
        let start = panel.commands.len();
        panel
            .flush(&second, &first, RefreshMode::FastClean, true)
            .unwrap();

        assert_eq!(panel.old_ram, old_before);
        assert_eq!(panel.cdi, Some([CDI_ABSOLUTE, CDI_INTERVAL]));
        assert_eq!(
            panel.commands[start..]
                .iter()
                .filter(|(cmd, _)| *cmd == CMD_DISPLAY_REFRESH)
                .count(),
            1
        );
        assert!(!panel.commands[start..]
            .iter()
            .any(|(cmd, _)| *cmd == CMD_DTM1));
    }

    #[test]
    fn first_fast_after_init_is_promoted_to_clean() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        let target = frame(2, 2);
        let mode = panel
            .flush(&target, &Framebuffer::new(), RefreshMode::Fast, false)
            .unwrap();
        assert_eq!(mode, RefreshMode::FastClean);
        assert!(transformed_matches(&panel.displayed, &target));
    }

    #[test]
    fn x3_geometry_and_init_transcript_are_enforced() {
        assert_eq!(WIDTH, 792);
        assert_eq!(HEIGHT, 528);
        assert_eq!(ROW_BYTES, 99);
        assert_eq!(FB_BYTES, 52272);
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        assert!(panel.initialized);
        assert!(!panel.powered);
    }

    #[test]
    fn prestaged_fast_refresh_skips_writing_the_old_plane() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        let first = frame(12, 12);
        panel
            .flush(&first, &Framebuffer::new(), RefreshMode::Full, false)
            .unwrap();
        panel.prestage_previous(&first).unwrap();

        let second = frame(24, 24);
        let start = panel.commands.len();
        let mode = panel
            .flush(&second, &first, RefreshMode::Fast, true)
            .unwrap();
        assert_eq!(mode, RefreshMode::Fast);
        assert_eq!(
            panel.commands[start..]
                .iter()
                .filter(|(cmd, _)| *cmd == CMD_DISPLAY_REFRESH)
                .count(),
            1
        );
        assert!(!panel.commands[start..]
            .iter()
            .any(|(cmd, _)| *cmd == CMD_DTM1));
    }

    #[test]
    fn stale_prestage_is_rejected_before_a_fast_turn() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        let staged = frame(1, 1);
        panel
            .flush(&staged, &Framebuffer::new(), RefreshMode::Full, false)
            .unwrap();
        panel.prestage_previous(&staged).unwrap();
        let claimed_previous = frame(2, 2);
        let current = frame(3, 3);
        assert!(panel
            .flush(&current, &claimed_previous, RefreshMode::Fast, true)
            .unwrap_err()
            .contains("prestaged DTM1"));
    }

    #[test]
    fn incomplete_ram_write_and_refresh_before_init_are_rejected() {
        let mut panel = PanelModel::new();
        assert!(panel
            .flush(
                &Framebuffer::new(),
                &Framebuffer::new(),
                RefreshMode::Full,
                false
            )
            .is_err());

        panel.init_sequence().unwrap();
        panel.begin_ram_write(RamPlane::New).unwrap();
        panel.ram_chunk(&[0xFF; ROW_BYTES]).unwrap();
        assert!(panel.end_ram_write().unwrap_err().contains("incomplete"));
    }

    #[test]
    fn ram_overrun_and_missing_old_plane_data_stop_are_rejected() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        let row = [0xFF; ROW_BYTES];

        panel.begin_ram_write(RamPlane::New).unwrap();
        for _ in 0..HEIGHT {
            panel.ram_chunk(&row).unwrap();
        }
        assert!(panel.ram_chunk(&[0xFF]).unwrap_err().contains("exceeds"));
        panel.end_ram_write().unwrap();

        panel.begin_ram_write(RamPlane::Old).unwrap();
        for _ in 0..HEIGHT {
            panel.ram_chunk(&row).unwrap();
        }
        panel.end_ram_write().unwrap();
        assert!(panel
            .command(CMD_POWER_ON, &[])
            .unwrap_err()
            .contains("DATA_STOP"));
        panel.command(CMD_DATA_STOP, &[]).unwrap();
        panel.command(CMD_POWER_ON, &[]).unwrap();
    }

    #[test]
    fn refresh_requires_a_complete_lut_bank() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        panel.command(CMD_POWER_ON, &[]).unwrap();
        assert!(panel
            .command(CMD_DISPLAY_REFRESH, &[])
            .unwrap_err()
            .contains("complete LUT"));
    }

    #[test]
    fn sleep_requires_power_off_and_check_byte() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        let fb = Framebuffer::new();
        panel.flush(&fb, &fb, RefreshMode::Fast, false).unwrap();
        panel.deep_sleep().unwrap();
        assert!(panel.is_deep_sleep());
        assert!(panel.prestage_previous(&fb).is_err());
        assert!(panel.history().iter().any(|entry| entry == "deep_sleep"));

        let mut invalid = PanelModel::new();
        invalid.init_sequence().unwrap();
        assert!(invalid.command(CMD_DEEP_SLEEP, &[0x01]).is_err());
    }

    #[test]
    fn partial_window_commands_validate_sequencing_and_bounds() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();

        // PARTIAL_WINDOW without PARTIAL_IN fails
        let win = [0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x07, 0x01];
        assert!(panel
            .command(CMD_PARTIAL_WINDOW, &win)
            .unwrap_err()
            .contains("before PARTIAL_IN"));

        // PARTIAL_OUT without PARTIAL_IN fails
        assert!(panel
            .command(CMD_PARTIAL_OUT, &[])
            .unwrap_err()
            .contains("without PARTIAL_IN"));

        // Enter partial mode
        panel.command(CMD_PARTIAL_IN, &[]).unwrap();

        // Nested PARTIAL_IN fails
        assert!(panel
            .command(CMD_PARTIAL_IN, &[])
            .unwrap_err()
            .contains("nested"));

        // Invalid pt_scan (!= 1) fails
        let mut bad_scan = win;
        bad_scan[8] = 0x00;
        assert!(panel
            .command(CMD_PARTIAL_WINDOW, &bad_scan)
            .unwrap_err()
            .contains("pt_scan"));

        // Unaligned X (xs=1) fails
        let mut unaligned_x = win;
        unaligned_x[1] = 0x01;
        assert!(panel
            .command(CMD_PARTIAL_WINDOW, &unaligned_x)
            .unwrap_err()
            .contains("unaligned"));

        // Out of bounds coordinates fail (xe >= WIDTH)
        let bad_bounds = [0x00, 0x00, 0x03, 0x20, 0x00, 0x00, 0x00, 0x07, 0x01]; // xe = 800 >= 792
        assert!(panel
            .command(CMD_PARTIAL_WINDOW, &bad_bounds)
            .unwrap_err()
            .contains("invalid window"));

        // Valid window succeeds
        panel.command(CMD_PARTIAL_WINDOW, &win).unwrap();

        // Exit partial mode
        panel.command(CMD_PARTIAL_OUT, &[]).unwrap();
    }

    #[test]
    fn stale_dtm1_outside_window_rejects_prestaged_partial_flush() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        let base = frame(10, 20);
        panel
            .flush(&base, &Framebuffer::new(), RefreshMode::Full, false)
            .unwrap();
        panel.prestage_previous(&base).unwrap();

        // Stale claimed previous
        let claimed_prev = frame(100, 100);
        let rect = Rect::new(0, 0, 64, 64);
        let next = frame(10, 20);
        assert!(panel
            .flush_window(&next, &claimed_prev, rect, true)
            .unwrap_err()
            .contains("prestaged DTM1"));
    }

    #[test]
    fn disjoint_partial_refreshes_from_unstaged_state_converge_after_full_prestage() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        let base = frame(10, 20);
        panel
            .flush(&base, &Framebuffer::new(), RefreshMode::Full, false)
            .unwrap();
        // Suppose previous_staged is false (e.g. after loading plate)
        let mut frame_b = Framebuffer::new();
        frame_b.copy_from(&base);
        for y in 10..30 {
            for x in 0..64 {
                frame_b.set_pixel(x, y, false);
            }
        }
        let r1 = frame_b.diff_rect(&base).expect("diff found");
        assert_eq!(r1, Rect::new(0, 10, 64, 20));

        // Partial flush B starting from unstaged (previous_staged = false)
        let mode = panel.flush_window(&frame_b, &base, r1, false).unwrap();
        assert_eq!(mode, RefreshMode::Fast);

        // Per display.rs fix: if not previously staged, run a full prestage after Settled
        panel.prestage_previous(&frame_b).unwrap();
        assert!(transformed_matches(&panel.old_ram, &frame_b));

        // Now mutate a completely disjoint region R2 for frame C
        let mut frame_c = Framebuffer::new();
        frame_c.copy_from(&frame_b);
        for y in 200..250 {
            for x in 128..192 {
                frame_c.set_pixel(x, y, false);
            }
        }
        let r2 = frame_c.diff_rect(&frame_b).expect("diff found");
        assert_eq!(r2, Rect::new(128, 200, 64, 50));

        // Since full prestage ran, previous_staged=true succeeds!
        let mode_c = panel.flush_window(&frame_c, &frame_b, r2, true).unwrap();
        assert_eq!(mode_c, RefreshMode::Fast);

        // Since it was previously staged, partial prestage is sufficient
        panel.prestage_previous_window(&frame_c, r2).unwrap();
        assert!(transformed_matches(&panel.old_ram, &frame_c));
        assert!(transformed_matches(&panel.displayed, &frame_c));
    }

    #[test]
    fn reinit_after_aborted_partial_mode_restores_clean_state() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        let base = frame(10, 20);
        panel
            .flush(&base, &Framebuffer::new(), RefreshMode::Full, false)
            .unwrap();

        // Simulate a prestage entering partial mode and failing before PARTIAL_OUT
        panel.command(CMD_PARTIAL_IN, &[]).unwrap();
        assert!(panel.in_partial_mode);

        // Attempting another PARTIAL_IN without re-init fails as nested
        assert!(panel
            .command(CMD_PARTIAL_IN, &[])
            .unwrap_err()
            .contains("nested"));

        // After failure, display task triggers panel re-initialization
        panel.init_sequence().unwrap();
        assert!(!panel.in_partial_mode);
        assert_eq!(panel.partial_window, None);

        // Subsequent full and partial flushes proceed normally
        let next = frame(30, 40);
        panel.flush(&next, &base, RefreshMode::Full, false).unwrap();
        assert!(transformed_matches(&panel.displayed, &next));
    }
}
