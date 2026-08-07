//! The boot panel-controller probe's verdict, written where a user can read it.
//!
//! A locked unit has no serial console, so "my screen looks wrong" cannot be
//! answered by asking its owner for `esp-println` output — and the probe's
//! verdict is the single most useful fact for answering it. One small text
//! file on the card, rewritten every boot, makes that failure diagnosable from
//! a photo or a copy-paste.
//!
//! Plain text on purpose: the reader is a person with the card in a laptop,
//! not a tool.

use crate::display_flush::{self, Epd};
use crate::library_sd::{open_or_make_dir, CATALOG_ROOT_DIR};
use crate::sd_session;
use core::fmt::Write as _;
use display::epd::probe::MTP_BYTES;
use embedded_sdmmc::Mode;
use esp_hal::gpio::Output;
use hal_ext::epd_probe::ProbeDiag;

const REPORT_FILE: &str = "PROBE.TXT";

/// Sized against the longest report this can produce — the fixed template plus
/// the MTP dump, three characters per byte, wrapped 16 to a line — with room
/// to spare. Overflow truncates rather than failing (see `render`).
const REPORT_BYTES: usize = 640;

/// Write `XTEINK/PROBE.TXT` from this boot's probe.
///
/// Best effort: the report is diagnostics, so a card that is absent, full, or
/// read-only costs a log line and nothing else.
pub(crate) fn write(epd: &mut Epd, sd_cs: &mut Output<'static>, diag: &ProbeDiag) {
    let mut text = heapless::String::<REPORT_BYTES>::new();
    render(&mut text, diag);
    let written = sd_session::with_root(epd, sd_cs, |root| {
        let dir = open_or_make_dir(root, CATALOG_ROOT_DIR)?;
        let file = dir
            .open_file_in_dir(REPORT_FILE, Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| ())?;
        file.write(text.as_bytes()).map_err(|_| ())
    });
    match written {
        Ok(Ok(())) => esp_println::println!("probe: wrote {}/{}", CATALOG_ROOT_DIR, REPORT_FILE),
        Ok(Err(())) => esp_println::println!("probe: report write failed"),
        Err(error) => esp_println::println!("probe: report skipped: {:?}", error),
    }
}

/// Build the report body. Separate from the SD work so the text has one shape
/// and no filesystem in it.
fn render<const N: usize>(out: &mut heapless::String<N>, diag: &ProbeDiag) {
    // Each `write!` fails only on a full buffer, which REPORT_BYTES is sized
    // to prevent; a truncated diagnostics file still beats a boot that trips
    // over one.
    let _ = writeln!(out, "CalendulaOS display controller probe");
    let _ = writeln!(
        out,
        "firmware: {} v{}",
        crate::PROJECT_NAME,
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "result:  {}", diag.verdict.as_str());
    let _ = writeln!(
        out,
        "driver:  {}",
        display_flush::detected_controller().name()
    );
    let _ = write!(out, "ver:    ");
    for byte in diag.ver {
        let _ = write!(out, " {byte:02X}");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "lut_ver: {:02X}", diag.lut_ver());
    let _ = writeln!(out, "flg:     {:02X}", diag.flg);
    let _ = writeln!(out);
    if diag.mtp_valid {
        let _ = writeln!(out, "mtp[0x000..0x{:03X}]:", MTP_BYTES - 1);
        for (index, byte) in diag.mtp.iter().enumerate() {
            let separator = if index % 16 == 0 { "" } else { " " };
            let _ = write!(out, "{separator}{byte:02X}");
            if index % 16 == 15 {
                let _ = writeln!(out);
            }
        }
    } else {
        let _ = writeln!(out, "mtp: not read (nothing drove the status line)");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "This file is rewritten every time the device starts.");
    let _ = writeln!(out, "If your screen looks wrong, send it with your report.");
}
