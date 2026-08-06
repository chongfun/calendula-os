//! The abrupt-reset half of the M0S durability campaign (`powercut-selftest`
//! feature only — never a release build).
//!
//! The PRD's hardware power-cut gate wants real power removal mid-write.
//! Without a bench rig that can cut VBUS (and with the battery sealed in),
//! the closest reachable approximation is an RTC-watchdog reset armed by
//! the host and never fed: it fires from RTC hardware at its deadline no
//! matter what the CPU is doing — including inside the blocking SD writes
//! embassy cannot preempt — and validates the store's write *ordering*
//! against a real card and real FAT timing. What it cannot validate is the
//! card's own power-loss physics (the card keeps power and will finish or
//! cleanly abort its in-flight sector), so the true gate stays open in
//! `docs/IMPLEMENTATION_PLAN.md` until a rig exists.
//!
//! The campaign loop lives in `tools/powercut_campaign.py`: arm via
//! `POST /test-powercut?after_ms=N`, start an operation that straddles the
//! deadline, wait out the reboot, verify catalog consistency and replay
//! convergence, repeat. This module contributes the pieces the loop needs
//! from the device: the arm signal (served by the power task, which owns
//! the RTC) and a boot task that starts the wireless session without
//! button presses and keeps the idle-sleep leash pushed out.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};

use crate::{AppView, PowerEvent, POWER_EVENTS, SYNC_COMMANDS};

/// Milliseconds until the armed reset fires. Latest arm wins; the power
/// task translates this into an RTC-watchdog deadline it never feeds.
pub static POWERCUT_ARM: Signal<CriticalSectionRawMutex, u64> = Signal::new();

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
