//! Device-driven bench scenarios (`bench-selftest` feature only, and kept
//! out of release builds the way `powercut-selftest` is).
//!
//! `tools/bench/bench.py` listens and parses. Its only control over the
//! device is a reset through espflash, so every timing suite has been
//! operator-driven: a human presses Next at a deliberate cadence while the
//! harness records. That puts the operator inside the measurement, and the
//! page-turn statistic has had to be defended against cadence three times
//! (`docs/agents/bench.md`, and the 354 ms baseline it retired).
//!
//! This module presses the buttons instead. `InputEvent::button` builds the
//! same `Sample` the input task produces, so `app_task` cannot tell the two
//! apart and the telemetry bench.py already parses is unchanged. Nothing
//! here writes a new event kind.
//!
//! Cadence comes from the device rather than a host timer: the next press
//! waits for the previous render to settle. That is steadier than a hand
//! can be, and it removes cadence as a variable instead of gating on it
//! after the fact.
//!
//! **What an injected press does not reproduce.** A real press is an ADC
//! sample that clears debounce; an injected one enters at `INPUT_EVENTS`,
//! one stage later. Press-to-settled measured here is therefore a floor,
//! and the offset has to be measured against a hand-pressed run once before
//! any number from this path is quoted beside the operator baselines. C9
//! sizes the boot combo's ADC work at 28 ms for 48 conversions, so the gap
//! is small and it is not zero.
//!
//! The radio stays off, deliberately. `fw::sync_mem` makes the wireless
//! session terminal (it dismantles the EPUB scratch and only a reset gets
//! reading back), so the powercut campaign's HTTP control channel cannot be
//! borrowed for anything that measures the reading path. The scenario is
//! compiled in and runs itself; the host only resets and listens.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer, WithTimeout};
use portable_atomic::{AtomicU8, Ordering};

use crate::{AppView, Button, InputEvent, PowerEvent, INPUT_EVENTS, POWER_EVENTS};

/// Signalled by the app task each time a render settles.
///
/// A `Signal` and not a channel: settles that arrive while nothing is
/// waiting collapse to one, which is what a press-then-wait loop wants. A
/// press that produces two renders is answered by the first.
pub static SETTLED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// The app's view as of its last applied input, so a scenario can drive
/// against what the device actually shows rather than a blind key sequence.
///
/// An atomic rather than a signal: the scenario polls it between presses and
/// a stale read costs one wasted press, while a missed edge would hang it.
static VIEW: AtomicU8 = AtomicU8::new(VIEW_UNKNOWN);

const VIEW_UNKNOWN: u8 = u8::MAX;

/// Called by the app task after each applied input.
pub fn publish_view(view: AppView) {
    VIEW.store(view_code(view), Ordering::Relaxed);
}

/// Called by the app task when a render settles.
pub fn note_settled() {
    SETTLED.signal(());
}

fn view_code(view: AppView) -> u8 {
    match view {
        AppView::Home => 0,
        AppView::Library => 1,
        AppView::Reading => 2,
        AppView::Chapters => 3,
        AppView::Wireless => 4,
        AppView::Settings => 5,
    }
}

fn current_view() -> Option<AppView> {
    match VIEW.load(Ordering::Relaxed) {
        0 => Some(AppView::Home),
        1 => Some(AppView::Library),
        2 => Some(AppView::Reading),
        3 => Some(AppView::Chapters),
        4 => Some(AppView::Wireless),
        5 => Some(AppView::Settings),
        _ => None,
    }
}

fn view_name(view: Option<AppView>) -> &'static str {
    match view {
        Some(AppView::Home) => "home",
        Some(AppView::Library) => "library",
        Some(AppView::Reading) => "reading",
        Some(AppView::Chapters) => "chapters",
        Some(AppView::Wireless) => "wireless",
        Some(AppView::Settings) => "settings",
        None => "unknown",
    }
}

/// How long to let the boot render and the first catalog load finish before
/// the scenario touches anything. The first paint lands around 3.1 s on an
/// X3 with a warm catalog and past 50 s with a cold scan of a large card,
/// so this is a floor on quiet, not a bound on boot: the first press waits
/// on a settle regardless.
const BOOT_QUIET_MS: u64 = 4_000;

/// Ceiling on a menu press. Menu renders are 3 to 8 ms of layout behind a
/// ~405 ms flush, so this is two orders of margin and only a stuck device
/// reaches it.
const NAV_SETTLE_TIMEOUT_MS: u64 = 20_000;

/// Ceiling on a book open, which may build a cache. A full cold build of
/// the 11.7 MB baseline book measured 64 s, and a card with a cold catalog
/// scan in front of it can spend another 48 s, so this is deliberately
/// generous. B4 publishes at the first page rather than the last, so a
/// healthy open settles far inside it.
const OPEN_SETTLE_TIMEOUT_MS: u64 = 240_000;

/// Ceiling on a page turn. Press-to-settled measures ~424 ms; a turn that
/// crosses a section boundary waits on an `ExtendSection` behind it.
const TURN_SETTLE_TIMEOUT_MS: u64 = 60_000;

/// How long the device must go without painting before the timed loop
/// starts. A page turn's own flush is ~405 ms, so a window several times
/// that distinguishes "idle" from "between build slices" without waiting on
/// the whole build.
const QUIET_WINDOW_MS: u64 = 2_500;

/// Ceiling on waiting for quiet. A background build of the baseline book
/// runs tens of seconds and B4 keeps the reader ahead of it, so this is a
/// bound on patience rather than an expectation. Past it the scenario turns
/// pages anyway and says so, since a capture that reports a busy device
/// beats one that silently declines to run.
const QUIET_BUDGET_MS: u64 = 120_000;

/// Presses spent trying to reach Reading before the scenario gives up. Each
/// one is a Confirm, and a card whose first row is a folder costs one per
/// level, so this bounds folder depth rather than retries.
const NAV_ATTEMPTS: u32 = 12;

/// Page turns the scenario performs. The suite default is 50, which is what
/// the operator-driven runs have used.
const TURNS: u32 = 50;

/// The key the scenario turns pages with.
///
/// `Next`, not `PageNext`, though the reducer treats them alike in Reading.
/// bench.py's press accounting counts only `Next` and `Previous` as page
/// inputs (`tools/bench/bench.py`, the `page_input` counter), so a run using
/// the side pair comes home with fifty renders, zero pairings and a
/// suppressed median. The first capture from this module did exactly that.
/// Injecting `Next` also keeps the result comparable with the operator
/// baselines, which were necessarily taken on a key the harness counts.
///
/// The blind spot is real and left alone deliberately: widening that set
/// would change what every stored capture reports, which is a measurement
/// layer decision rather than a side effect of adding a scenario.
const PAGE_TURN_BUTTON: Button = Button::Next;

/// Send a press the app cannot distinguish from the input task's own.
///
/// `send` and not `try_send`: `INPUT_EVENTS` is 8 deep and the input task's
/// overflow policy (drop the oldest, then push) exists for a hand holding a
/// key down. A scenario that presses one at a time and waits should block
/// rather than inherit that, since a silently dropped press is a hang.
///
/// The telemetry line goes out through the input task's own logger. The
/// harness pairs a press to a render through that line, so an injected
/// press that skipped it would leave the capture full of renders and empty
/// of durations, which is what the first run of this module did.
async fn press(button: Button) {
    crate::tasks::input::log_injected_input(button);
    INPUT_EVENTS.send(InputEvent::button(button)).await;
}

/// Press, then wait for the render it caused to settle.
///
/// The signal is reset first so a settle from earlier work cannot answer
/// this press. Returns false on timeout, which the caller reports and
/// treats as the end of the scenario: a capture that carried on past a
/// stuck device would look like a slow run rather than a broken one.
///
/// One case this cannot separate: a render the app started for itself, such
/// as a background build's progress paint, settling between the reset and
/// the press being applied. It would answer this press early and the next
/// one would land during a live refresh, which is the coalescing the whole
/// design exists to avoid. [`await_quiet`] drains those before the timed
/// loop starts, and the harness's own `coalesced` accounting and trust gate
/// catch what gets through, so the failure is a suppressed median rather
/// than a wrong one.
async fn press_and_settle(button: Button, timeout_ms: u64) -> bool {
    SETTLED.reset();
    press(button).await;
    SETTLED
        .wait()
        .with_timeout(Duration::from_millis(timeout_ms))
        .await
        .is_ok()
}

/// Wait until the device stops painting on its own.
///
/// A book open leaves work behind it: B4 publishes at the first page and
/// finishes the build in slices, and each slice that changes the page count
/// can paint. Pressing into that stream is how a run collects coalesced
/// presses it did not need to. Returns false if the device is still going
/// after `budget_ms`, which is a real answer about the device rather than a
/// reason to press anyway.
async fn await_quiet(quiet_ms: u64, budget_ms: u64) -> bool {
    let deadline = embassy_time::Instant::now() + Duration::from_millis(budget_ms);
    loop {
        SETTLED.reset();
        if SETTLED
            .wait()
            .with_timeout(Duration::from_millis(quiet_ms))
            .await
            .is_err()
        {
            // Nothing painted for a whole window, so the device is idle.
            return true;
        }
        if embassy_time::Instant::now() >= deadline {
            return false;
        }
    }
}

/// Walk from wherever boot left the device to Reading.
///
/// Driven by the published view rather than a fixed key sequence, because
/// Home's Confirm lands on Reading or Library depending on whether the
/// current book is an SD one, and Library's Confirm picks a row the card
/// decides the meaning of. Both are answered by looking at where the press
/// actually landed.
async fn reach_reading() -> bool {
    for attempt in 0..NAV_ATTEMPTS {
        match current_view() {
            Some(AppView::Reading) => return true,
            Some(AppView::Library) => {
                // A row is a book or a folder, and the card decides which.
                // An open may build a cache, so this wears the open ceiling.
                if !press_and_settle(Button::Confirm, OPEN_SETTLE_TIMEOUT_MS).await {
                    bench_log!(
                        "bench-selftest: nav timed out in library attempt={}",
                        attempt
                    );
                    return false;
                }
            }
            Some(AppView::Home) => {
                if !press_and_settle(Button::Confirm, OPEN_SETTLE_TIMEOUT_MS).await {
                    bench_log!("bench-selftest: nav timed out at home attempt={}", attempt);
                    return false;
                }
            }
            // Chapters, Wireless, Settings, or a view not yet published.
            // Back is the one key every one of them answers.
            _ => {
                if !press_and_settle(Button::Back, NAV_SETTLE_TIMEOUT_MS).await {
                    bench_log!(
                        "bench-selftest: nav timed out backing out of {} attempt={}",
                        view_name(current_view()),
                        attempt
                    );
                    return false;
                }
            }
        }
    }
    bench_log!(
        "bench-selftest: gave up reaching reading after {} presses, view={}",
        NAV_ATTEMPTS,
        view_name(current_view())
    );
    false
}

/// The page-turn suite, driven from the device.
///
/// Waits out the boot paint, navigates to Reading, then turns pages one
/// settled render at a time. Every timing line bench.py reads is emitted by
/// the paths this drives, so the report, the trust gating and `--strict`
/// all work on the result unchanged.
#[embassy_executor::task]
pub async fn page_turn() {
    Timer::after(Duration::from_millis(BOOT_QUIET_MS)).await;
    bench_log!(
        "bench-selftest: scenario=page-turn turns={} view={}",
        TURNS,
        view_name(current_view())
    );

    if !reach_reading().await {
        bench_log!("bench-selftest: scenario=page-turn result=nav-failed");
        return;
    }
    // The open that got here may still be building. Let it finish rather
    // than timing page turns that are queued behind build slices.
    if !await_quiet(QUIET_WINDOW_MS, QUIET_BUDGET_MS).await {
        bench_log!(
            "bench-selftest: still painting after {} ms, turning anyway",
            QUIET_BUDGET_MS
        );
    }
    bench_log!("bench-selftest: reached reading, turning {} pages", TURNS);

    let mut turned = 0u32;
    for turn in 0..TURNS {
        // The idle leash is 10 minutes in Reading and every press pushes it
        // out, so a settled cadence cannot sleep the device. This covers the
        // one gap: a single open or turn that runs long enough to matter.
        let _ = POWER_EVENTS.try_send(PowerEvent::Activity(AppView::Reading));
        if !press_and_settle(PAGE_TURN_BUTTON, TURN_SETTLE_TIMEOUT_MS).await {
            bench_log!(
                "bench-selftest: turn {} did not settle within {} ms",
                turn,
                TURN_SETTLE_TIMEOUT_MS
            );
            break;
        }
        turned += 1;
    }

    bench_log!(
        "bench-selftest: scenario=page-turn result=done turns={} of {}",
        turned,
        TURNS
    );
}
