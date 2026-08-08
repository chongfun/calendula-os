//! Refuse to run on the board this image was not built for.
//!
//! `proto::ota` has always guarded *updates* by comparing descriptor
//! identities. The initial flash had no such check, and the X3 and X4 are the
//! same ESP32-C3 — so an X4 image esptool'd onto an X3 passes the bootloader,
//! boots, and drives a UC8253 with SSD1677 sequences. The user sees a device
//! that looks dead.
//!
//! [`hal_ext::board_probe`] says which board this is, [`evaluate`] compares it
//! with the one this image names, [`refuse`] runs when they disagree.
//!
//! - **Only a confirmed mismatch refuses.** Inconclusive boots normally;
//!   bricking a good device on a flaky I2C read is worse than the fault.
//! - **It never touches the screen.** The card is the whole message. An
//!   earlier version tried the panel too, on the theory that the wrong
//!   controller might answer. It does not: `init_panel` fails with
//!   `Busy(TimedOut)` after 15 s, the two controllers reading BUSY in opposite
//!   senses, having drawn nothing. Restore only for a board pair sharing a
//!   controller.
//! - **It halts.** A reboot loop looks identical to a dead device and would
//!   rewrite the diagnostic endlessly.

use crate::display_flush::Epd;
use embassy_time::Instant;
use embedded_sdmmc::Mode;
use esp_hal::gpio::Output;
use hal_ext::board_probe::BoardVerdict;

/// What a user needs told about one board. `identity` is the field
/// `proto::ota::parse_identity` cuts out, which is what makes the two questions
/// comparable; the rest is what the answer is worth to someone holding a blank
/// device.
#[derive(Clone, Copy)]
struct BoardFacts {
    /// As spelled in the firmware identity this image stamps.
    identity: &'static str,
    /// What the product is called in the store and in the docs.
    product: &'static str,
    /// The release asset to flash at `0x10000`. The *initial-flash* image, not
    /// the SD updater trigger: whoever reads this flashed the wrong one.
    flash_image: &'static str,
}

const BOARDS: [BoardFacts; 2] = [
    BoardFacts {
        identity: proto::ota::BOARD_X4,
        product: "Xteink X4",
        flash_image: "firmware-x4.bin",
    },
    BoardFacts {
        identity: proto::ota::BOARD_X3,
        product: "Xteink X3",
        flash_image: "firmware-x3.bin",
    },
];

fn facts_for(identity: &str) -> Option<BoardFacts> {
    BOARDS.iter().copied().find(|b| b.identity == identity)
}

/// Where a probe result becomes a board identity. A match rather than a lookup,
/// so adding a board is a compile error rather than a silent `None`.
fn detected_facts(verdict: BoardVerdict) -> Option<BoardFacts> {
    match verdict {
        BoardVerdict::X4Confirmed => facts_for(proto::ota::BOARD_X4),
        BoardVerdict::X3Confirmed => facts_for(proto::ota::BOARD_X3),
        BoardVerdict::Inconclusive => None,
    }
}

/// A board the probe confirmed, and the board this image was built for, when
/// they are not the same.
#[derive(Clone, Copy)]
pub(crate) struct BoardMismatch {
    detected: BoardFacts,
    firmware: BoardFacts,
}

/// Whether this image must refuse to run.
///
/// `None` carries on: no board named, the right board, or a name this build
/// cannot describe. That last case boots rather than halts, because refusing
/// needs a message naming a real download and there would be none to give.
pub(crate) fn evaluate(verdict: BoardVerdict, firmware_identity: &str) -> Option<BoardMismatch> {
    let detected = detected_facts(verdict)?;
    let firmware = facts_for(proto::ota::identity_board(firmware_identity)?)?;
    if detected.identity == firmware.identity {
        return None;
    }
    Some(BoardMismatch { detected, firmware })
}

/// Card root, 8.3 so every FAT tool shows it, named for what it answers rather
/// than for the failure.
const DIAGNOSTIC_FILE: &str = "BOARDID.TXT";

/// The only task a mismatched boot spawns.
///
/// It owns the EPD bus and SD chip select outright, so calling `sd_session`
/// directly keeps the single-writer rule — the display task never starts here.
/// The bus carries only the card; the panel is left alone.
#[embassy_executor::task]
pub async fn refuse(mut epd: Epd, mut sd_cs: Output<'static>, mismatch: BoardMismatch) {
    // Not behind `serial-log`: boot-identity output, and the only channel that
    // works with no card in the slot.
    esp_println::println!(
        "board: REFUSING TO BOOT -- detected {}, firmware built for {} t_ms={}",
        mismatch.detected.product,
        mismatch.firmware.product,
        Instant::now().as_millis()
    );
    esp_println::println!("board: flash {} to fix this", mismatch.detected.flash_image);

    match crate::sd_session::with_root(&mut epd, &mut sd_cs, |root| {
        write_diagnostic(root, mismatch)
    }) {
        Ok(true) => esp_println::println!("board: wrote /{}", DIAGNOSTIC_FILE),
        Ok(false) => esp_println::println!("board: could not write /{}", DIAGNOSTIC_FILE),
        Err(error) => esp_println::println!("board: no SD card for diagnostic: {:?}", error),
    }

    esp_println::println!("board: halted");
    // Nothing else is spawned, so the executor parks the core rather than spins.
    core::future::pending::<()>().await;
}

/// Write the explanation to the card root, replacing any earlier copy. Returns
/// whether the whole file landed.
///
/// Pieces written straight through rather than formatted into a buffer: the
/// variable parts are already `&'static str`, and a formatting buffer in this
/// task's future would be permanent `.bss` for a path almost nothing takes.
fn write_diagnostic(root: &crate::sd_session::SdRoot<'_>, mismatch: BoardMismatch) -> bool {
    let Ok(file) = root.open_file_in_dir(DIAGNOSTIC_FILE, Mode::ReadWriteCreateOrTruncate) else {
        return false;
    };

    // Short lines, plain words, fix before explanation: whoever reads this is
    // holding a device that looks broken. CRLF so Notepad shows lines.
    let parts: [&str; 21] = [
        "CalendulaOS stopped before it could start.\r\n",
        "\r\n",
        "This device is an ",
        mismatch.detected.product,
        ".\r\n",
        "The software on it was built for the ",
        mismatch.firmware.product,
        ".\r\n",
        "\r\n",
        "Nothing is damaged. The screen may stay blank\r\n",
        "or look scrambled until the right software is\r\n",
        "installed.\r\n",
        "\r\n",
        "To fix it, flash this file instead:\r\n",
        "    ",
        mismatch.detected.flash_image,
        "\r\n",
        "\r\n",
        "Downloads:\r\n",
        "    https://github.com/chongfun/calendula-os/releases/latest\r\n",
        "\r\n",
    ];

    let mut wrote = true;
    for part in parts {
        wrote &= file.write(part.as_bytes()).is_ok();
    }
    // Close reports the metadata flush, so a write that never reached the card
    // is not reported as one that did.
    wrote & file.close().is_ok()
}
