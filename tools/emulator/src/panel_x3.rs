use display::epd::{
    fill_transformed_band, lut_for_mode, LutBank, RefreshMode, SpiOp, CMD_DATA_STOP,
    CMD_DEEP_SLEEP, CMD_DISPLAY_REFRESH, CMD_GATE_SOURCE_START, CMD_LUT_BB, CMD_LUT_BW,
    CMD_LUT_VCOM, CMD_LUT_WB, CMD_LUT_WW, CMD_PANEL_SETTING, CMD_PLL, CMD_POWER_OFF,
    CMD_POWER_OFF_SEQUENCE, CMD_POWER_ON, CMD_POWER_SETTING, CMD_RESOLUTION, CMD_SOURCE_LV_SELECT,
    CMD_VCOM_DATA_INTERVAL, CMD_VCOM_DC, CMD_WRITE_RAM_NEW, CMD_WRITE_RAM_OLD, DEEP_SLEEP_CHECK,
    INIT_SEQUENCE, LUT_FAST,
};
use display::fb::Framebuffer;
use display::{BAND_BYTES, BAND_ROWS, FB_BYTES, HEIGHT, ROW_BYTES, WIDTH};

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
            commands: Vec::new(),
            history: Vec::new(),
        }
    }

    pub fn init_sequence(&mut self) -> Result<(), String> {
        for op in INIT_SEQUENCE {
            match *op {
                SpiOp::Reset => self.reset(),
                SpiOp::WaitBusy => self.history.push("wait_busy_active_low".into()),
                SpiOp::Command { cmd, data } => self.command(cmd, data)?,
            }
        }
        self.write_fill(CMD_WRITE_RAM_OLD, 0xFF)?;
        self.command(CMD_DATA_STOP, &[])?;
        self.write_fill(CMD_WRITE_RAM_NEW, 0xFF)?;
        self.command(CMD_DATA_STOP, &[])?;
        self.initialized = true;
        Ok(())
    }

    pub fn command(&mut self, cmd: u8, data: &[u8]) -> Result<(), String> {
        if self.deep_sleep {
            return Err(format!("command 0x{cmd:02X} while panel is asleep"));
        }
        let expected = match cmd {
            CMD_PANEL_SETTING => Some(2),
            CMD_RESOLUTION | CMD_GATE_SOURCE_START => Some(4),
            CMD_POWER_OFF_SEQUENCE | CMD_PLL | CMD_VCOM_DC | CMD_SOURCE_LV_SELECT => Some(1),
            CMD_POWER_SETTING => Some(5),
            CMD_VCOM_DATA_INTERVAL => Some(2),
            CMD_LUT_VCOM | CMD_LUT_WW | CMD_LUT_BW | CMD_LUT_WB | CMD_LUT_BB => Some(42),
            CMD_POWER_ON | CMD_POWER_OFF | CMD_DISPLAY_REFRESH | CMD_DATA_STOP => Some(0),
            CMD_DEEP_SLEEP => Some(1),
            _ => None,
        };
        if let Some(expected) = expected {
            if data.len() != expected {
                return Err(format!(
                    "command 0x{cmd:02X} expected {expected} data bytes, got {}",
                    data.len()
                ));
            }
        }

        match cmd {
            CMD_POWER_ON => self.powered = true,
            CMD_POWER_OFF => self.powered = false,
            CMD_VCOM_DATA_INTERVAL => self.cdi = Some([data[0], data[1]]),
            CMD_LUT_VCOM => self.lut_written[0] = true,
            CMD_LUT_WW => self.lut_written[1] = true,
            CMD_LUT_BW => self.lut_written[2] = true,
            CMD_LUT_WB => self.lut_written[3] = true,
            CMD_LUT_BB => self.lut_written[4] = true,
            CMD_DISPLAY_REFRESH => {
                if !self.powered {
                    return Err("DRF while charge pump is off".into());
                }
                if self.cdi.is_none() || !self.lut_written.iter().all(|written| *written) {
                    return Err("DRF before a complete LUT bank and CDI".into());
                }
                self.displayed.copy_from_slice(&self.new_ram);
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
        self.history.push(format!("cmd 0x{cmd:02X} {:02X?}", data));
        Ok(())
    }

    pub fn write_framebuffer_bw(&mut self, fb: &Framebuffer) -> Result<(), String> {
        if self.deep_sleep {
            return Err("RAM write while panel is asleep".into());
        }
        if !self.initialized {
            return Err("RAM write before init sequence".into());
        }
        let mut band = [0u8; BAND_BYTES];
        let mut y = 0;
        while y < HEIGHT {
            let len = fill_transformed_band(fb, y, &mut band);
            let start = y * ROW_BYTES;
            self.new_ram[start..start + len].copy_from_slice(&band[..len]);
            y += BAND_ROWS;
        }
        self.commands.push((CMD_WRITE_RAM_NEW, FB_BYTES));
        self.history
            .push(format!("ram 0x{CMD_WRITE_RAM_NEW:02X} {WIDTH}x{HEIGHT}"));
        Ok(())
    }

    pub fn refresh(&mut self, mode: RefreshMode) -> Result<(), String> {
        let bank = lut_for_mode(mode);
        self.load_lut(bank)?;
        if mode != RefreshMode::Fast {
            self.write_fill(CMD_WRITE_RAM_OLD, 0xFF)?;
        }
        if !self.powered || mode != RefreshMode::Fast {
            self.command(CMD_POWER_ON, &[])?;
        }
        self.command(CMD_DISPLAY_REFRESH, &[])?;

        if mode != RefreshMode::Fast {
            self.old_ram.copy_from_slice(&self.new_ram);
            self.commands.push((CMD_WRITE_RAM_OLD, FB_BYTES));
            self.load_lut(&LUT_FAST)?;
            self.commands.push((CMD_WRITE_RAM_NEW, FB_BYTES));
            self.command(CMD_DISPLAY_REFRESH, &[])?;
        }

        self.last_refresh = Some(mode);
        self.history.push(format!("refresh {mode:?}"));
        Ok(())
    }

    pub fn deep_sleep(&mut self) -> Result<(), String> {
        self.command(CMD_POWER_OFF, &[])?;
        self.command(CMD_DEEP_SLEEP, &[DEEP_SLEEP_CHECK])
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

    fn load_lut(&mut self, bank: &LutBank) -> Result<(), String> {
        self.lut_written = [false; 5];
        self.command(CMD_VCOM_DATA_INTERVAL, &bank.cdi)?;
        self.command(CMD_LUT_VCOM, &bank.vcom)?;
        self.command(CMD_LUT_WW, &bank.ww)?;
        self.command(CMD_LUT_BW, &bank.bw)?;
        self.command(CMD_LUT_WB, &bank.wb)?;
        self.command(CMD_LUT_BB, &bank.bb)
    }

    fn write_fill(&mut self, command: u8, value: u8) -> Result<(), String> {
        let target = match command {
            CMD_WRITE_RAM_OLD => &mut self.old_ram,
            CMD_WRITE_RAM_NEW => &mut self.new_ram,
            _ => return Err(format!("unsupported RAM command 0x{command:02X}")),
        };
        target.fill(value);
        self.commands.push((command, FB_BYTES));
        self.history
            .push(format!("ram 0x{command:02X} {WIDTH}x{HEIGHT}"));
        Ok(())
    }

    fn reset(&mut self) {
        self.deep_sleep = false;
        self.initialized = false;
        self.powered = false;
        self.lut_written = [false; 5];
        self.cdi = None;
        self.history.push("reset".into());
    }
}

impl Default for PanelModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x3_geometry_and_init_transcript_are_enforced() {
        assert_eq!((WIDTH, HEIGHT, FB_BYTES), (792, 528, 52_272));
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        assert_eq!(panel.commands[0], (CMD_PANEL_SETTING, 2));
        assert!(panel.commands.contains(&(CMD_RESOLUTION, 4)));
        assert!(panel.commands.contains(&(CMD_WRITE_RAM_OLD, FB_BYTES)));
        assert!(panel.commands.contains(&(CMD_WRITE_RAM_NEW, FB_BYTES)));
    }

    #[test]
    fn full_refresh_uses_uc8253_planes_luts_and_post_fast_settle() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        panel.write_framebuffer_bw(&Framebuffer::new()).unwrap();
        panel.refresh(RefreshMode::Full).unwrap();

        assert_eq!(panel.last_refresh(), Some(RefreshMode::Full));
        assert_eq!(
            panel
                .commands
                .iter()
                .filter(|(cmd, _)| *cmd == CMD_DISPLAY_REFRESH)
                .count(),
            2
        );
        for command in [CMD_LUT_VCOM, CMD_LUT_WW, CMD_LUT_BW, CMD_LUT_WB, CMD_LUT_BB] {
            assert!(panel.commands.contains(&(command, 42)));
        }
        assert!(panel.commands.contains(&(CMD_POWER_ON, 0)));
    }

    #[test]
    fn fast_refresh_requires_power_and_uses_one_drf() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        panel.write_framebuffer_bw(&Framebuffer::new()).unwrap();
        panel.refresh(RefreshMode::Fast).unwrap();
        assert_eq!(panel.last_refresh(), Some(RefreshMode::Fast));
        assert_eq!(
            panel
                .commands
                .iter()
                .filter(|(cmd, _)| *cmd == CMD_DISPLAY_REFRESH)
                .count(),
            1
        );
    }

    #[test]
    fn sleep_requires_power_off_and_check_byte() {
        let mut panel = PanelModel::new();
        panel.init_sequence().unwrap();
        panel.write_framebuffer_bw(&Framebuffer::new()).unwrap();
        panel.refresh(RefreshMode::Fast).unwrap();
        panel.deep_sleep().unwrap();
        assert!(panel.is_deep_sleep());
        assert!(panel.write_framebuffer_bw(&Framebuffer::new()).is_err());

        let mut invalid = PanelModel::new();
        invalid.init_sequence().unwrap();
        assert!(invalid.command(CMD_DEEP_SLEEP, &[0x01]).is_err());
    }
}
