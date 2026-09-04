//! Small pieces shared by the two mutually-exclusive panel models
//! (`panel.rs` for the X4/SSD1677, `panel_uc8253.rs` for the X3/UC8253):
//! command-length validation and the transcript string vocabulary they
//! both push into `history`. Only one panel model compiles into a given
//! build, but keeping their shared format strings and validation in one
//! place avoids the two copies drifting apart from each other.

use display::epd::RefreshMode;

/// What a flush hands back: the mode the plan ran, and the quiet the panel
/// still owes before its RAM is written again.
///
/// Mirrors `fw::display_flush::PanelSettle`. The interval belongs to the
/// caller, so a model that slept through it inside `flush` would model the
/// wrong firmware: the caller passes `settle_ms` to `PanelModel::settle`, and
/// a RAM write taken before that is rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "the flush's trailing settle must be consumed before the next RAM write"]
pub struct FlushOutcome {
    pub effective_mode: RefreshMode,
    pub settle_ms: u16,
}

/// Transcript entry for a settle the caller held.
pub fn settle_history_entry(ms: u16) -> String {
    format!("settle {ms}ms")
}

/// Reject a command whose data doesn't match the controller's fixed
/// argument length for that command.
pub fn expect_len(cmd: u8, data: &[u8], expected: usize) -> Result<(), String> {
    if data.len() == expected {
        Ok(())
    } else {
        Err(format!(
            "command 0x{cmd:02X} expected {expected} data bytes, got {}",
            data.len()
        ))
    }
}

/// Transcript entry for a plain command write.
pub fn cmd_history_entry(cmd: u8, data: &[u8]) -> String {
    format!("cmd 0x{cmd:02X} {:02X?}", data)
}

/// Transcript entry for a completed RAM-plane write.
pub fn ram_history_entry(cmd: u8, width: usize, height: usize) -> String {
    format!("ram 0x{cmd:02X} {width}x{height}")
}

pub const HISTORY_RESET: &str = "reset";
pub const HISTORY_DEEP_SLEEP: &str = "deep_sleep";
