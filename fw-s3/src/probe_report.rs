//! Boot panel-controller probe report written to `READER/PROBE.TXT`.

use crate::epd_probe::ProbeDiag;
use crate::sd_session::{self, SdmmcBus};
use core::fmt::Write as _;
use display::epd::probe::MTP_BYTES;
use embedded_sdmmc::Mode;
use storage::library_sd::{open_or_make_dir, CATALOG_ROOT_DIR};

const REPORT_FILE: &str = "PROBE.TXT";
const REPORT_BYTES: usize = 640;

pub(crate) fn write(bus: &mut SdmmcBus, diag: &ProbeDiag) {
    let mut text = heapless::String::<REPORT_BYTES>::new();
    render(&mut text, diag);
    let written = sd_session::with_root(bus, |root| {
        let shelf = upload_store::library::open_library_root(root).map_err(|_| ())?;
        upload_store::reclaim::recover(root, shelf.as_ref()).map_err(|_| ())?;
        drop(shelf);
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

fn render<const N: usize>(out: &mut heapless::String<N>, diag: &ProbeDiag) {
    let _ = writeln!(out, "CalendulaOS display controller probe");
    let _ = writeln!(
        out,
        "firmware: {} v{}",
        crate::board::BOARD_IDENTITY,
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "result:     {}", diag.verdict.as_str());
    let _ = writeln!(out, "controller: {}", diag.variant.as_str());
    let _ = writeln!(out, "driver:     uc8279-x4");
    let _ = write!(out, "ver:       ");
    for byte in diag.ver {
        let _ = write!(out, " {byte:02X}");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "lut_ver:    {:02X}", diag.lut_ver());
    let _ = writeln!(out, "flg:        {:02X}", diag.flg);
    let _ = writeln!(out);
    if diag.mtp_valid {
        let _ = writeln!(out, "mtp[0x000..0x{:03X}]:", MTP_BYTES - 1);
        for (index, byte) in diag.mtp.iter().enumerate() {
            let separator = if index.is_multiple_of(16) { "" } else { " " };
            let _ = write!(out, "{separator}{byte:02X}");
            if (index + 1).is_multiple_of(16) {
                let _ = writeln!(out);
            }
        }
    } else {
        let _ = writeln!(
            out,
            "mtp: not read (initial status did not confirm idle panel)"
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "This file is rewritten every time the device starts.");
    let _ = writeln!(out, "If your screen looks wrong, send it with your report.");
}
