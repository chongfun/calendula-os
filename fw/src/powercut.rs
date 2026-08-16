//! The abrupt-reset half of the install durability gate
//! (`powercut-selftest` feature only — never a release build).
//!
//! The plan's hardware power-cut gate wants real power removal mid-write.
//! Without a bench rig that can cut VBUS (and with the battery sealed in),
//! the closest reachable approximation is an RTC-watchdog reset armed by
//! the host and never fed: it fires from RTC hardware at its deadline no
//! matter what the CPU is doing — including inside the blocking SD writes
//! embassy cannot preempt — and validates the installer's write *ordering*
//! against a real card and real FAT timing. What it cannot validate is the
//! card's own power-loss physics (the card keeps power and will finish or
//! cleanly abort its in-flight sector), so the true gate stays open in
//! `docs/IMPLEMENTATION_PLAN.md` until a rig exists.
//!
//! The campaign loop lives in `tools/powercut_campaign.py`: arm via
//! `POST /test-powercut?after_ms=N`, start an upload or delete that
//! straddles the deadline, wait out the reboot, verify the shelf converged
//! and the journal cleared, repeat. This module contributes the pieces the
//! loop needs from the device: the arm signal (served by the power task,
//! which owns the RTC) and a boot task that starts the wireless session
//! without button presses and keeps the idle-sleep leash pushed out.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use portable_atomic::{AtomicU32, Ordering};

use crate::{AppView, PowerEvent, POWER_EVENTS, SYNC_COMMANDS};

/// Milliseconds until the armed reset fires. Latest arm wins; the power
/// task translates this into an RTC-watchdog deadline it never feeds.
pub static POWERCUT_ARM: Signal<CriticalSectionRawMutex, u64> = Signal::new();

/// Bounds on an armed deadline. Long enough that the response still leaves
/// the socket, short enough that a typo cannot arm a reset weeks out with
/// the watchdog live the whole time.
pub const MIN_ARM_MS: u64 = 10;
pub const MAX_ARM_MS: u64 = 600_000;

/// `after_ms` from the arm request's query string, if it is present and in
/// range. The query splitting it rests on is `proto::upload::query_param`,
/// which is tested on the host; `fw`'s own tests never run (`check.sh
/// test-host` excludes this crate), so nothing subtle belongs here.
pub fn parse_after_ms(path: &[u8]) -> Option<u64> {
    let value = proto::upload::query_param(path, b"after_ms")?;
    let ms = core::str::from_utf8(value).ok()?.parse::<u64>().ok()?;
    (MIN_ARM_MS..=MAX_ARM_MS).contains(&ms).then_some(ms)
}

/// A deadline to arm at the start of the next install, or 0 for none.
///
/// Timing a cut into the install from the host does not work: the install is
/// the last few percent of an upload and the write path's throughput varies
/// by tens of percent between runs, so an aimed deadline lands in the body
/// transfer or past the whole thing. Measured over 36 cycles on an X3
/// (2026-08-15), that window was never once hit. The device knows exactly
/// when the install starts, so it arms for itself.
pub static CUT_AT_INSTALL_MS: AtomicU32 = AtomicU32::new(0);

/// Bounds on an install-timed deadline. The install is a short sequence of
/// directory rewrites, so the useful range is small; the floor leaves the
/// arm time to take effect at all.
pub const MIN_INSTALL_ARM_MS: u32 = 4;
pub const MAX_INSTALL_ARM_MS: u32 = 2_000;

/// `at_install_ms` from the arm request's query string, if in range.
pub fn parse_at_install_ms(path: &[u8]) -> Option<u32> {
    let value = proto::upload::query_param(path, b"at_install_ms")?;
    let ms = core::str::from_utf8(value).ok()?.parse::<u32>().ok()?;
    (MIN_INSTALL_ARM_MS..=MAX_INSTALL_ARM_MS)
        .contains(&ms)
        .then_some(ms)
}

/// A decimal `u32` query parameter, if present and parsable.
pub fn parse_u32(path: &[u8], name: &[u8]) -> Option<u32> {
    let value = proto::upload::query_param(path, name)?;
    core::str::from_utf8(value).ok()?.parse::<u32>().ok()
}

/// The `seed` parameter: the running digest, as 16 hex digits.
pub fn parse_seed(path: &[u8]) -> Option<u64> {
    let value = proto::upload::query_param(path, b"seed")?;
    u64::from_str_radix(core::str::from_utf8(value).ok()?, 16).ok()
}

/// How long to let the executor run before returning, so the power task can
/// act on the signal. The install that follows is a blocking SD sequence
/// embassy cannot preempt, so an arm that has not taken effect by then would
/// not take effect until after the thing it is meant to interrupt.
const ARM_SETTLE_MS: u64 = 5;

/// Arms a reset timed to land inside the install that is about to start, if
/// one was requested. Consumes the request: one arm, one install.
///
/// The deadline runs from when the power task arms the watchdog, so the cut
/// lands roughly `ms - ARM_SETTLE_MS` into the install.
pub async fn arm_for_install() {
    let ms = CUT_AT_INSTALL_MS.swap(0, Ordering::SeqCst);
    if ms == 0 {
        return;
    }
    esp_println::println!("powercut: arming {} ms at install", ms);
    POWERCUT_ARM.signal(u64::from(ms));
    Timer::after(Duration::from_millis(ARM_SETTLE_MS)).await;
}

/// A request to read part of a book off the card and digest what is there.
///
/// Ranged, because the answer travels back over a socket with a 30-second
/// idle timeout and a whole book is minutes of blocking SD reads: a request
/// that outlives the timeout is killed with its reply half-made. The host
/// walks the file in bounded pieces, carrying the running hash, so no single
/// request holds a connection open long. Measured on an X3 at ~800 kB/s, a
/// 4 MB piece answers in about five seconds.
pub struct DigestRequest {
    pub name: crate::upload::UploadName,
    pub in_books: bool,
    pub from: u32,
    pub len: u32,
    pub seed: u64,
}

/// `(file length, bytes read, digest so far)`, or `None` if the book could
/// not be opened or the read failed.
pub type DigestReply = Option<(u32, u32, u64)>;

pub static DIGEST_REQUESTS: Channel<CriticalSectionRawMutex, DigestRequest, 1> = Channel::new();
pub static DIGEST_RESULTS: Channel<CriticalSectionRawMutex, DigestReply, 1> = Channel::new();

/// FNV-1a over the file's bytes. Not a cryptographic claim — the campaign
/// compares against a body it generated itself, so this only has to catch
/// bytes that differ, not bytes chosen to collide. Cheap enough that the SD
/// read stays the cost.
pub const DIGEST_SEED: u64 = 0xcbf2_9ce4_8422_2325;

pub fn digest_chunk(hash: u64, bytes: &[u8]) -> u64 {
    let mut hash = hash;
    for &byte in bytes {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// Reports what mount-time recovery found, for the campaign to read.
///
/// The shipping log says "finished an interrupted install" only when
/// recovery *moves* a file, which is the visible half of its job. A cut
/// between the last move and the record being cleared leaves recovery
/// planning `Done`: it still reads the record, still clears it, and still
/// decides the transaction's fate — but moves nothing and prints nothing.
/// That is a journal replay, and a campaign that cannot see it reads a run
/// exercising the window as a run that missed it.
pub fn report_recovery(outcome: &upload_store::install::InstallRecovery) {
    if !outcome.had_intent {
        return;
    }
    esp_println::println!(
        "powercut: recovery replayed a record: moved={} swept={} complete={}",
        outcome.touched_shelf,
        outcome.swept,
        outcome.complete
    );
}

/// Enters the wireless session on boot and keeps the device awake, so a
/// cut/reboot/verify cycle needs no hands on the device. The app shell's
/// own screen state is deliberately not driven — the wifi task only needs
/// `SyncCommand::Start`, and a campaign device showing Home while serving
/// is a cosmetic non-goal.
#[embassy_executor::task]
pub async fn autostart() {
    // The loan request serializes behind the boot scan on the storage
    // queue anyway; this delay just keeps the first render undisturbed.
    Timer::after(Duration::from_secs(3)).await;
    esp_println::println!("powercut: auto-starting wireless session");
    SYNC_COMMANDS.send(app_core::SyncCommand::Start).await;
    // A campaign pause must not tip the device into idle deep sleep;
    // stand in for the button presses a human session would produce.
    loop {
        Timer::after(Duration::from_secs(60)).await;
        POWER_EVENTS
            .send(PowerEvent::Activity(AppView::Wireless))
            .await;
    }
}
