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

use crate::{
    AppView, Button, DisplayOrientation, InputEvent, PowerEvent, INPUT_EVENTS, POWER_EVENTS,
};

/// Signalled by the app task each time a render settles.
///
/// A `Signal` and not a channel: settles that arrive while nothing is
/// waiting collapse to one, which is what a press-then-wait loop wants. A
/// press that produces two renders is answered by the first.
pub static SETTLED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// The view the app is about to paint, so a scenario can drive against what
/// the device actually shows rather than a blind key sequence.
///
/// Published from `send_render`, the one place a render leaves the app, and
/// not from the input arm. A Library pick is answered by the card, so the
/// move to Reading arrives as a storage event that touches no input: a
/// scenario reading the input side would still see "library" after the book
/// opened and press Confirm into the hold Library keeps while a pick is in
/// flight. That cost a storage-cache run twelve Confirms and one open.
///
/// An atomic rather than a signal: the scenario polls it between presses and
/// a stale read costs one wasted press, while a missed edge would hang it.
static VIEW: AtomicU8 = AtomicU8::new(VIEW_UNKNOWN);

const VIEW_UNKNOWN: u8 = u8::MAX;

/// The reader's saved controls, so an injected key can be the one that
/// reaches the action a scenario means.
///
/// `apply_input` maps a raw key through the front-pair swap and the
/// orientation before the reducer sees it. A scenario sending raw `Next` to
/// turn a page turns it on the default settings and opens the chapter list
/// on `PagesLeft`, so a deterministic selftest has to know what the card
/// last saved.
static ORIENTATION: AtomicU8 = AtomicU8::new(0);
static FRONT_BUTTONS: AtomicU8 = AtomicU8::new(0);

/// Called by the app task as each render is frozen and sent.
pub fn publish_view(view: AppView, orientation: DisplayOrientation, front_pages_left: bool) {
    VIEW.store(view_code(view), Ordering::Relaxed);
    ORIENTATION.store(orientation_code(orientation), Ordering::Relaxed);
    FRONT_BUTTONS.store(u8::from(front_pages_left), Ordering::Relaxed);
}

fn orientation_code(orientation: DisplayOrientation) -> u8 {
    match orientation {
        DisplayOrientation::LandscapeButtonsBottom => 0,
        DisplayOrientation::LandscapeButtonsTop => 1,
        DisplayOrientation::PortraitButtonsLeft => 2,
        DisplayOrientation::PortraitButtonsRight => 3,
    }
}

fn current_orientation() -> DisplayOrientation {
    match ORIENTATION.load(Ordering::Relaxed) {
        1 => DisplayOrientation::LandscapeButtonsTop,
        2 => DisplayOrientation::PortraitButtonsLeft,
        3 => DisplayOrientation::PortraitButtonsRight,
        _ => DisplayOrientation::LandscapeButtonsBottom,
    }
}

fn current_front_buttons() -> app_core::FrontButtons {
    if FRONT_BUTTONS.load(Ordering::Relaxed) == 0 {
        app_core::FrontButtons::PagesRight
    } else {
        app_core::FrontButtons::PagesLeft
    }
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

/// How often to re-read the published view while waiting on one.
const VIEW_POLL_MS: u64 = 100;

/// How long to wait for a view to arrive before pressing again. Long enough
/// for the second render an entry can paint, short enough that a wrong guess
/// costs a press rather than the capture.
const VIEW_SETTLE_MS: u64 = 3_000;

/// Presses spent trying to reach Reading before the scenario gives up. Each
/// one is a Confirm, and a card whose first row is a folder costs one per
/// level, so this bounds folder depth rather than retries.
const NAV_ATTEMPTS: u32 = 12;

/// Page turns the scenario performs. The suite default is 50, which is what
/// the operator-driven runs have used.
const TURNS: u32 = 50;

/// Open, read, back out, open again. Two is the smallest number that shows
/// both a first open and a repeat one in the same capture, which is what
/// `storage-cache --cold --warm` checks for; three leaves margin if the
/// first open was already warm from a previous run.
const STORAGE_CYCLES: u32 = 3;

/// Turns per storage cycle. Enough to cross a section boundary on a normal
/// book, since the extend is half of what this suite exists to time.
const STORAGE_TURNS_PER_CYCLE: u32 = 12;

/// Folder entries to collect, matching the suite's own default of 20.
///
/// A target, not a press count. bench.py stops folder-nav on `folder_enter`
/// telemetry, and a row that turns out to be a book produces none of it, so
/// counting picks instead of entries put the two sides of the capture on
/// different contracts: twenty picks on a card holding one book could not
/// reach twenty entries, and on a flat card reached none.
const FOLDER_ENTRIES: u32 = 20;

/// Picks allowed while collecting those entries. Books consume attempts
/// without producing entries, so this is what stops a mostly-flat card from
/// walking forever.
const FOLDER_ATTEMPT_CEILING: u32 = 40;

/// Picks that may come back a book before the scenario calls the card flat
/// and stops. Opening a book costs seconds, so a card with no folders should
/// say so early rather than spend the whole capture proving it.
const FOLDER_FLAT_PROBE: u32 = 5;

/// Cursor steps before each entry, so the walk crosses page boundaries
/// rather than entering the same row twenty times.
const FOLDER_SCROLL_STEPS: u32 = 3;

/// Turns per soak pass, on each side of the chapter jump.
const SOAK_TURNS: u32 = 10;

/// Turns before a sleep. The suite's own words are "several fast page
/// turns", and the sleep is the part under test.
const SLEEP_TURNS: u32 = 6;

/// How long to give the sleep transition before deciding it was refused.
/// The panel has a sleep image to draw and progress to write first, and the
/// whole entry measured about 4.1 s.
const SLEEP_SETTLE_MS: u64 = 15_000;

/// Sleep requests before giving up. The app refuses one while it is holding
/// storage work, and says so, so a retry costs a wait and answers it.
const SLEEP_ATTEMPTS: u32 = 3;

/// How long the RTC timer waits before waking a sleeping device.
///
/// Long enough that the sleep is a real one the capture can see, short
/// enough that a ten-cycle suite finishes. Deep sleep is terminal, so this
/// is a reboot interval rather than a nap.
///
/// `core::time::Duration`, which is what the RTC wake source takes;
/// everything else in this module is on embassy's clock.
pub const SLEEP_WAKE_AFTER: core::time::Duration = core::time::Duration::from_secs(20);

/// How long a sleeping scenario waits at boot before it starts.
///
/// This is a reflash window, not a measurement. It costs one wait per cycle
/// and buys back the ability to interrupt a device that is otherwise only
/// reachable in whatever gap its own wake timer leaves.
const SLEEP_SCENARIO_HOLD_MS: u64 = 25_000;

/// Whether a scenario ends by sleeping, and so leaves the device cycling.
fn scenario_sleeps(scenario: &str) -> bool {
    matches!(scenario, "sleep-sync" | "reader-soak")
}

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
async fn press(action: Button) {
    // `action` is what the scenario means; the key that reaches it depends
    // on the orientation and front pair the reader last saved. Sending the
    // action itself made the selftest do whatever those settings said: on
    // `PagesLeft` a page turn arrives as Confirm and opens the chapter list
    // instead. Falling back to the action when no key reaches it keeps a
    // scenario running rather than hanging on a mapping that stopped being a
    // permutation.
    let key = app_core::physical_key_for(current_orientation(), current_front_buttons(), action)
        .unwrap_or(action);
    crate::tasks::input::log_injected_input(key);
    INPUT_EVENTS.send(InputEvent::button(key)).await;
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

/// Wait for the app to be showing `target`, or give up.
///
/// A press is not always answered by one render. Entering Library paints the
/// view and then paints again when the card hands over the rows, and a
/// scenario that decided after the first render pressed once more into a
/// view it had already reached. Polling the published view instead of
/// counting renders makes that harmless.
async fn wait_for_view(target: AppView, timeout_ms: u64) -> bool {
    let deadline = embassy_time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if current_view() == Some(target) {
            return true;
        }
        if embassy_time::Instant::now() >= deadline {
            return false;
        }
        Timer::after(Duration::from_millis(VIEW_POLL_MS)).await;
    }
}

/// Press a named key in Reading, allowing for the portrait key sheet.
///
/// Portrait reading is full-bleed, so the first Confirm or Back summons the
/// key sheet and acts on nothing; the second press acts on the label it
/// revealed. Landscape maps directly and acts on the first press. Page
/// turns are exempt in both, so `turn_pages` needs none of this.
///
/// So this presses, and presses again only if the view has not moved. That
/// is right in both orientations: in landscape the first press already
/// left Reading and the second is not sent, and in portrait the first press
/// only opened the sheet. A scenario that sent one press and waited was
/// reading a sheet as a refusal, which is what reported `jumped=false` on a
/// book whose chapter list was fine.
async fn act_in_reading(button: Button, timeout_ms: u64) -> bool {
    if !press_and_settle(button, timeout_ms).await {
        return false;
    }
    if current_view() != Some(AppView::Reading) {
        return true;
    }
    press_and_settle(button, timeout_ms).await
}

/// Pick the selected row and report where it left the app.
///
/// One helper because getting this wrong is the same mistake three times.
/// A pick is a storage command the card answers, so two things have to
/// happen before the answer means anything. The queue has to be free, or
/// the app refuses the command outright and says "storage rejected choose
/// row" while staying exactly where it was. And the move to Reading can land
/// on a later render than the one that answered the press, so a scenario
/// that reads the view as soon as the press settles sees the shelf it was
/// already on and calls a book a folder.
///
/// Returns the view the pick landed in: `Reading` for a book, `Library` for
/// a folder now entered, `None` if the press itself did not settle.
async fn pick_row() -> Option<AppView> {
    let _ = await_quiet(QUIET_WINDOW_MS, QUIET_BUDGET_MS).await;
    if !press_and_settle(Button::Confirm, OPEN_SETTLE_TIMEOUT_MS).await {
        return None;
    }
    if wait_for_view(AppView::Reading, VIEW_SETTLE_MS).await {
        Some(AppView::Reading)
    } else {
        current_view()
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
            Some(AppView::Library) => match pick_row().await {
                Some(AppView::Reading) => return true,
                // A folder: the walk is one level deeper and the next pick
                // takes a row inside it.
                Some(_) => {}
                None => {
                    bench_log!(
                        "bench-selftest: nav timed out in library attempt={}",
                        attempt
                    );
                    return false;
                }
            },
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

/// Back out to Library, whichever view the scenario is standing in.
///
/// Back is the one key every view answers, and Home's Back is Files, so
/// repeating it reaches the shelf from anywhere. Bounded, because a view
/// that answers Back with itself would otherwise spin.
async fn back_to_library() -> bool {
    for _ in 0..NAV_ATTEMPTS {
        if wait_for_view(AppView::Library, VIEW_SETTLE_MS).await {
            return true;
        }
        // Reading owes the portrait two-step; every other view acts on one
        // press, and act_in_reading sends only one from those because the
        // view has already moved.
        if !act_in_reading(Button::Back, NAV_SETTLE_TIMEOUT_MS).await {
            return false;
        }
    }
    false
}

/// Turn `count` pages, one settled render at a time. Returns how many
/// landed, so a caller can report a short run rather than hide it.
async fn turn_pages(count: u32) -> u32 {
    let mut turned = 0;
    for turn in 0..count {
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
    turned
}

/// Open a book and let whatever it started finish.
async fn open_and_quiesce() -> bool {
    if !reach_reading().await {
        return false;
    }
    // The open may still be building. B4 publishes at the first page and
    // finishes in slices, and pressing into that stream collects coalesced
    // presses the run did not need.
    if !await_quiet(QUIET_WINDOW_MS, QUIET_BUDGET_MS).await {
        bench_log!(
            "bench-selftest: still painting after {} ms, continuing anyway",
            QUIET_BUDGET_MS
        );
    }
    true
}

/// Ask for sleep, and say whether the app took it.
///
/// The app refuses a Power press while it is holding storage work and says
/// so on the wire; the retry is for that. Waking is a fresh boot, so on
/// success this call does not come back and the scenario resumes from the
/// top on the other side.
async fn request_sleep() -> bool {
    // Measured: the first request of every sleep-sync cycle came back
    // "sleep deferred until book open settles", because a background build
    // was still holding storage work six page turns after the open. Waiting
    // for the device to go quiet first turns that into a request that takes.
    let _ = await_quiet(QUIET_WINDOW_MS, QUIET_BUDGET_MS).await;
    for attempt in 0..SLEEP_ATTEMPTS {
        press(Button::Power).await;
        // Nothing settles for a sleep, so this waits on the transition
        // rather than a render: the panel has its sleep image to draw and
        // progress to write before the SoC goes down.
        Timer::after(Duration::from_millis(SLEEP_SETTLE_MS)).await;
        bench_log!("bench-selftest: sleep request {} did not take", attempt);
    }
    false
}

/// `page-turn`: the suite the operator cadence has cost the most.
async fn scenario_page_turn() {
    if !open_and_quiesce().await {
        bench_log!("bench-selftest: scenario=page-turn result=nav-failed");
        return;
    }
    bench_log!("bench-selftest: reached reading, turning {} pages", TURNS);
    let turned = turn_pages(TURNS).await;
    bench_log!(
        "bench-selftest: scenario=page-turn result=done turns={} of {}",
        turned,
        TURNS
    );
}

/// `storage-cache`: cold and warm opens, section extends, progress writes.
///
/// The shape puts both modes in one capture. The first open
/// is whatever the card offers, cold if its cache has to be built. Backing
/// out to Library and opening again is served from the built cache or the
/// RAM window, which is the warm evidence `--warm` checks for. Page turns
/// in between produce the progress writes and, at a section boundary, the
/// extends.
async fn scenario_storage_cache() {
    let mut opens = 0u32;
    for cycle in 0..STORAGE_CYCLES {
        if !open_and_quiesce().await {
            bench_log!(
                "bench-selftest: scenario=storage-cache result=nav-failed cycle={}",
                cycle
            );
            break;
        }
        opens += 1;
        let turned = turn_pages(STORAGE_TURNS_PER_CYCLE).await;
        bench_log!("bench-selftest: storage cycle={} turns={}", cycle, turned);
        if !back_to_library().await {
            bench_log!(
                "bench-selftest: could not reach library after cycle {}",
                cycle
            );
            break;
        }
    }
    bench_log!(
        "bench-selftest: scenario=storage-cache result=done opens={} of {}",
        opens,
        STORAGE_CYCLES
    );
}

/// `folder-nav`: entering and leaving rows, and paging the list.
///
/// A row is a book or a folder and the scenario cannot tell which before
/// pressing, so it presses and then reads where it landed: Reading means a
/// book, and still Library means a folder was entered. Both are backed out
/// of, so the walk keeps moving instead of settling into one book. The
/// `Next` presses before each Confirm are what carries the cursor over a
/// page boundary, which is the other half of what this suite times.
async fn scenario_folder_nav() {
    if !back_to_library().await {
        bench_log!("bench-selftest: scenario=folder-nav result=no-library");
        return;
    }
    let mut entered = 0u32;
    let mut books = 0u32;
    let mut attempts = 0u32;
    while entered < FOLDER_ENTRIES && attempts < FOLDER_ATTEMPT_CEILING {
        attempts += 1;
        for _ in 0..FOLDER_SCROLL_STEPS {
            if !press_and_settle(Button::Next, NAV_SETTLE_TIMEOUT_MS).await {
                bench_log!("bench-selftest: scroll stalled at attempt {}", attempts);
                break;
            }
        }
        match pick_row().await {
            Some(AppView::Reading) => books += 1,
            Some(AppView::Library) => entered += 1,
            Some(_) => {}
            None => {
                bench_log!("bench-selftest: attempt {} did not settle", attempts);
                break;
            }
        }
        if !back_to_library().await {
            bench_log!("bench-selftest: lost the library at attempt {}", attempts);
            break;
        }
        if entered == 0 && books >= FOLDER_FLAT_PROBE {
            bench_log!(
                "bench-selftest: scenario=folder-nav result=no-folders books={} \
                 (this card has nothing to enter; the suite wants a folder tree)",
                books
            );
            return;
        }
    }
    bench_log!(
        "bench-selftest: scenario=folder-nav result=done folders={} of {} books={} attempts={}",
        entered,
        FOLDER_ENTRIES,
        books,
        attempts
    );
}

/// `reader-soak`: the whole reading workflow, including a sleep and a wake.
///
/// One pass per boot. Deep sleep is terminal, so the wake at the end is a
/// fresh boot that runs this again, and the capture accumulates cycles
/// across reboots the way an operator's session does. `--strict` wants a
/// completed sleep with a wake later in the same run, which is exactly the
/// shape this produces.
async fn scenario_reader_soak() {
    if !open_and_quiesce().await {
        bench_log!("bench-selftest: scenario=reader-soak result=nav-failed");
        return;
    }
    let turned = turn_pages(SOAK_TURNS).await;

    // Chapter jump: Confirm opens the list, a step moves the cursor off the
    // current chapter, Confirm takes it. This is the navigation a page-turn
    // run does not exercise.
    // Reading suppresses a Confirm for POST_OPEN_CONFIRM_BLOCK_MS after an
    // open, so a reader's Confirm cannot be eaten by the open that just
    // landed. Every page turn re-arms it, so a Confirm pressed the instant
    // the last turn settles is inside the window and comes back as another
    // Reading render. Measured: the jump reported false and the list did
    // not open. Waiting for quiet clears it.
    let _ = await_quiet(QUIET_WINDOW_MS, QUIET_BUDGET_MS).await;
    let mut jumped = false;
    if act_in_reading(Button::Confirm, NAV_SETTLE_TIMEOUT_MS).await
        // Poll rather than read once: a press is not always answered by one
        // render.
        && wait_for_view(AppView::Chapters, VIEW_SETTLE_MS).await
        && press_and_settle(Button::Next, NAV_SETTLE_TIMEOUT_MS).await
    {
        jumped = press_and_settle(Button::Confirm, OPEN_SETTLE_TIMEOUT_MS).await
            && wait_for_view(AppView::Reading, VIEW_SETTLE_MS).await;
    }

    // Home and Library returns, then back into the book.
    let returned = back_to_library().await && open_and_quiesce().await;
    let turned_after = if returned {
        turn_pages(SOAK_TURNS).await
    } else {
        0
    };

    bench_log!(
        "bench-selftest: scenario=reader-soak turns={}+{} jumped={} returned={}",
        turned,
        turned_after,
        jumped,
        returned
    );
    // A soak that skipped its chapter navigation is not the workflow this
    // suite advertises, and --strict cannot tell: it asks for input and
    // render telemetry plus a completed sleep and a later wake, all of which
    // a jumpless pass still produces. So say so loudly enough that a reader
    // of the capture cannot miss it.
    if !jumped {
        bench_log!("bench-selftest: scenario=reader-soak INVALID: chapter navigation did not run");
    }

    // Last, because it does not return.
    request_sleep().await;
    bench_log!("bench-selftest: scenario=reader-soak result=sleep-refused");
}

/// `sleep-sync`: fast turns, then sleep, then wake and repeat.
///
/// One cycle per boot, for the same reason `reader-soak` is: the timer wake
/// reboots the chip and this runs again. bench.py counts `sleep_complete`
/// until it has the cycles it asked for.
async fn scenario_sleep_sync() {
    if !open_and_quiesce().await {
        bench_log!("bench-selftest: scenario=sleep-sync result=nav-failed");
        return;
    }
    let turned = turn_pages(SLEEP_TURNS).await;
    bench_log!(
        "bench-selftest: scenario=sleep-sync turns={}, sleeping",
        turned
    );
    request_sleep().await;
    bench_log!("bench-selftest: scenario=sleep-sync result=sleep-refused");
}

/// Which scenario this build runs, from `BENCH_SCENARIO` at compile time.
///
/// Compile time and not run time because there is nothing to ask: the radio
/// is off by design (see the module header) and the firmware reads no
/// serial. Every scenario is compiled into every bench build, so the choice
/// costs a rebuild and a flash rather than a code path that only some builds
/// type-check. `fw/build.rs` reruns on the variable, so changing it actually
/// changes the image.
fn scenario_name() -> &'static str {
    option_env!("BENCH_SCENARIO").unwrap_or("page-turn")
}

/// The scenario driver.
///
/// Every timing line bench.py reads is emitted by the paths these drive, so
/// the report, the trust gating and `--strict` all work on the result
/// unchanged. Nothing here writes a new event kind.
#[embassy_executor::task]
pub async fn run() {
    Timer::after(Duration::from_millis(BOOT_QUIET_MS)).await;
    let scenario = scenario_name();
    bench_log!(
        "bench-selftest: scenario={} view={}",
        scenario,
        view_name(current_view())
    );

    // A scenario that sleeps leaves the device cycling: it wakes on the RTC
    // timer, runs again, and sleeps again, forever. Deep sleep takes the USB
    // Serial/JTAG peripheral down with it, so the only way to reflash is to
    // catch an awake window, and without this hold those windows are short
    // enough to need a polling loop. Measured: twelve espflash attempts in a
    // row missed. So a sleeping scenario sits still first and gives the host
    // a window it can rely on.
    if scenario_sleeps(scenario) {
        bench_log!(
            "bench-selftest: holding {} ms before a sleeping scenario, reflash now if you meant to",
            SLEEP_SCENARIO_HOLD_MS
        );
        Timer::after(Duration::from_millis(SLEEP_SCENARIO_HOLD_MS)).await;
    }

    match scenario {
        "storage-cache" => scenario_storage_cache().await,
        "folder-nav" => scenario_folder_nav().await,
        "reader-soak" => scenario_reader_soak().await,
        "sleep-sync" => scenario_sleep_sync().await,
        "page-turn" => scenario_page_turn().await,
        other => {
            // Naming a scenario that does not exist should say so rather
            // than quietly running the default, which would report a
            // page-turn capture under whatever name was asked for.
            bench_log!(
                "bench-selftest: unknown scenario {}, doing nothing. \
                 Valid: page-turn storage-cache folder-nav reader-soak sleep-sync",
                other
            );
        }
    }
}
