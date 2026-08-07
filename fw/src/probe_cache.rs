//! The panel-controller probe's result, retained across the deep-sleep reboot.
//!
//! The probe costs 70–150 ms and answers a question about soldered hardware.
//! Nothing short of a soldering iron changes that answer, and nothing reaches
//! the soldering iron without disconnecting the battery — so the honest scope
//! of one probe is *one power cycle*, not one boot. This caches the result in
//! RTC fast RAM so a deep-sleep wake, an OTA reset, or a crash reboot spends
//! that time on the page the user is waiting for instead.
//!
//! RTC fast RAM is the right store here, and the alternatives are worse for
//! the same reason FreeInk refuses to dispatch on the OEM's NVS
//! `hw_calib/screenType`: flash and SD both survive being copied onto another
//! unit, so a cached verdict there can outlive the hardware it describes. This
//! memory cannot. It is zeroed on first power-on, so a battery pull is a full
//! re-probe with no user-visible escape hatch to document — and no way for a
//! stale answer to persist past the moment the panel could have changed.
//!
//! Bench note: a software reset now *reuses* the verdict. Re-running the probe
//! needs a power cycle, and `main` logs which path each boot took.

use core::sync::atomic::Ordering;
use display::epd::probe::{ProbeVerdict, MTP_BYTES, VER_BYTES};
use hal_ext::epd_probe::ProbeDiag;
use portable_atomic::{AtomicU32, AtomicU8};

/// Byte layout of [`PAYLOAD`]: verdict, FLG, the MTP-valid flag, then VER and
/// the MTP dump.
const OFF_VERDICT: usize = 0;
const OFF_FLG: usize = 1;
const OFF_MTP_VALID: usize = 2;
const OFF_VER: usize = 3;
const OFF_MTP: usize = OFF_VER + VER_BYTES;
const PAYLOAD_BYTES: usize = OFF_MTP + MTP_BYTES;

/// Written only by [`store`]. The low byte is a layout generation: **bump it
/// whenever the offsets above change**, so an OTA into firmware that packs
/// this differently re-probes instead of decoding the old shape. Everything
/// else — first-boot zeroing, brownout garbage — misses the magic and reads as
/// no cache.
const CACHE_MAGIC: u32 = 0xC0DE_9A01;

// `persistent`: zeroed once on the first power-on, then left untouched by the
// runtime across deep sleep and every reset. Same retention `sleep_marker`
// relies on, and the same reason it is the only store that expires when the
// hardware could have changed.
//
// Retention across deep sleep is witnessed, not assumed. The linker packs this
// whole section into 64 bytes with `sleep_marker` in the middle of it —
// `VALID` at `0x5000_0000`, `SLEEP_IMAGE` at `0x5000_0004`, `PAYLOAD` at
// `0x5000_0008` — and a deep-sleep wake that renders as one quick flicker
// instead of a multi-flash full refresh proves `SLEEP_IMAGE` came back holding
// the value the sleep handshake wrote. Retention belongs to the region, so
// those four bytes cannot survive while the bytes on either side of them do
// not. Confirmed on an X3, 2026-08-07. Keep the three statics in this section
// together; splitting them would give up the witness.
#[allow(unsafe_code)] // #[ram] expands to the unsafe link_section attribute.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static VALID: AtomicU32 = AtomicU32::new(0);

#[allow(unsafe_code)] // #[ram] expands to the unsafe link_section attribute.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static PAYLOAD: [AtomicU8; PAYLOAD_BYTES] = [const { AtomicU8::new(0) }; PAYLOAD_BYTES];

/// The verdict from earlier in this power cycle, if there is one.
///
/// Two independent gates, because retained RAM after a brownout is arbitrary
/// bytes: the magic must match *and* the verdict byte must be one this
/// firmware writes. A cache that fails either is no cache, and the caller
/// probes.
pub(crate) fn load() -> Option<ProbeDiag> {
    if VALID.load(Ordering::Relaxed) != CACHE_MAGIC {
        return None;
    }
    let verdict = ProbeVerdict::from_code(byte(OFF_VERDICT))?;
    let mut diag = ProbeDiag {
        verdict,
        ver: [0; VER_BYTES],
        flg: byte(OFF_FLG),
        mtp_valid: byte(OFF_MTP_VALID) != 0,
        mtp: [0; MTP_BYTES],
    };
    for (index, slot) in diag.ver.iter_mut().enumerate() {
        *slot = byte(OFF_VER + index);
    }
    for (index, slot) in diag.mtp.iter_mut().enumerate() {
        *slot = byte(OFF_MTP + index);
    }
    Some(diag)
}

/// Record a live probe's result for the rest of this power cycle.
///
/// The magic goes down last, so a reset landing mid-write leaves a cache that
/// reads as absent rather than as half of one.
pub(crate) fn store(diag: &ProbeDiag) {
    set_byte(OFF_VERDICT, diag.verdict.code());
    set_byte(OFF_FLG, diag.flg);
    set_byte(OFF_MTP_VALID, u8::from(diag.mtp_valid));
    for (index, value) in diag.ver.iter().enumerate() {
        set_byte(OFF_VER + index, *value);
    }
    for (index, value) in diag.mtp.iter().enumerate() {
        set_byte(OFF_MTP + index, *value);
    }
    VALID.store(CACHE_MAGIC, Ordering::Relaxed);
}

fn byte(offset: usize) -> u8 {
    PAYLOAD[offset].load(Ordering::Relaxed)
}

fn set_byte(offset: usize, value: u8) {
    PAYLOAD[offset].store(value, Ordering::Relaxed);
}
