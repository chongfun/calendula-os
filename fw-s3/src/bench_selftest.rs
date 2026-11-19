//! Device-driven bench scenarios for `fw-s3` (`bench-selftest` feature only,
//! kept out of production releases).
//!
//! Mirrors `fw_c3::bench_selftest` using `fw_common` channels/hooks and
//! `crate::input::log_injected_input` so `tools/bench/bench.py` parses S3
//! captures with the same state-machine and timing contracts as C3.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, WithTimeout};
use fw_common::{
    note_button_press, AppView, Button, DisplayOrientation, InputEvent, PowerEvent, INPUT_EVENTS,
    POWER_EVENTS,
};
use portable_atomic::{AtomicU16, AtomicU32, AtomicU8, Ordering};

pub static SETTLED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

static VIEW: AtomicU8 = AtomicU8::new(VIEW_UNKNOWN);
const VIEW_UNKNOWN: u8 = u8::MAX;

static ORIENTATION: AtomicU8 = AtomicU8::new(2);
static FRONT_BUTTONS: AtomicU8 = AtomicU8::new(0);
static LIBRARY_DEPTH: AtomicU8 = AtomicU8::new(0);
static PAGE: AtomicU32 = AtomicU32::new(u32::MAX);
static SELECTION: AtomicU16 = AtomicU16::new(u16::MAX);
static SCENARIO: AtomicU8 = AtomicU8::new(u8::MAX);

const SCENARIOS: [&str; 5] = [
    "page-turn",
    "storage-cache",
    "folder-nav",
    "reader-soak",
    "sleep-sync",
];

fn set_scenario(name: &str) {
    let code = SCENARIOS
        .iter()
        .position(|s| *s == name)
        .map_or(u8::MAX, |i| i as u8);
    SCENARIO.store(code, Ordering::Relaxed);
}

fn current_scenario() -> &'static str {
    SCENARIOS
        .get(usize::from(SCENARIO.load(Ordering::Relaxed)))
        .copied()
        .unwrap_or("unknown")
}

fn current_page() -> u32 {
    PAGE.load(Ordering::Relaxed)
}

pub fn publish_view(
    view: AppView,
    orientation: DisplayOrientation,
    front_pages_left: bool,
    library_depth: u8,
    page: u32,
    selection: u16,
) {
    SELECTION.store(selection, Ordering::Relaxed);
    PAGE.store(page, Ordering::Relaxed);
    VIEW.store(view_code(view), Ordering::Relaxed);
    ORIENTATION.store(orientation_code(orientation), Ordering::Relaxed);
    FRONT_BUTTONS.store(u8::from(front_pages_left), Ordering::Relaxed);
    LIBRARY_DEPTH.store(library_depth, Ordering::Relaxed);
}

fn current_depth() -> u8 {
    LIBRARY_DEPTH.load(Ordering::Relaxed)
}

fn current_selection() -> u16 {
    SELECTION.load(Ordering::Relaxed)
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
        0 => DisplayOrientation::LandscapeButtonsBottom,
        1 => DisplayOrientation::LandscapeButtonsTop,
        3 => DisplayOrientation::PortraitButtonsRight,
        _ => DisplayOrientation::PortraitButtonsLeft,
    }
}

fn current_front_buttons() -> app_core::FrontButtons {
    if FRONT_BUTTONS.load(Ordering::Relaxed) == 0 {
        app_core::FrontButtons::PagesRight
    } else {
        app_core::FrontButtons::PagesLeft
    }
}

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

const BOOT_QUIET_MS: u64 = 4_000;
const NAV_SETTLE_TIMEOUT_MS: u64 = 20_000;
const OPEN_SETTLE_TIMEOUT_MS: u64 = 240_000;
const TURN_QUIET_MS: u64 = 1_500;
const TURN_QUIET_BUDGET_MS: u64 = 10_000;
const TURN_SETTLE_TIMEOUT_MS: u64 = 60_000;
const QUIET_WINDOW_MS: u64 = 2_500;
const QUIET_BUDGET_MS: u64 = 120_000;
const VIEW_POLL_MS: u64 = 100;
const LEAVE_QUIET_MS: u64 = 1_200;
const LEAVE_ANSWER_MS: u64 = 30_000;
const VIEW_SETTLE_MS: u64 = 3_000;
const NAV_ATTEMPTS: u32 = 12;
const TURNS: u32 = 50;
const STORAGE_CYCLES: u32 = 3;
const STORAGE_TURNS_PER_CYCLE: u32 = 12;
const FOLDER_ENTRIES: u32 = 20;
const FOLDER_ATTEMPT_CEILING: u32 = 40;
const FOLDER_FLAT_PROBE: u32 = 5;
const FOLDER_SCROLL_STEPS: u32 = 3;
const SOAK_TURNS: u32 = 10;
const SLEEP_TURNS: u32 = 6;
const SLEEP_SETTLE_MS: u64 = 15_000;
const SLEEP_ATTEMPTS: u32 = 3;
pub const SLEEP_WAKE_AFTER: core::time::Duration = core::time::Duration::from_secs(20);
const SLEEP_SCENARIO_HOLD_MS: u64 = 25_000;

fn scenario_sleeps(scenario: &str) -> bool {
    matches!(scenario, "sleep-sync" | "reader-soak")
}

const PAGE_TURN_BUTTON: Button = Button::Next;

async fn press(action: Button) {
    let view = current_view().unwrap_or(AppView::Library);
    let key =
        app_core::physical_key_for(view, current_orientation(), current_front_buttons(), action)
            .unwrap_or(action);
    crate::input::log_injected_input(key, action);
    note_button_press();
    INPUT_EVENTS.send(InputEvent::button(key)).await;
}

async fn press_and_settle(button: Button, timeout_ms: u64) -> bool {
    SETTLED.reset();
    press(button).await;
    SETTLED
        .wait()
        .with_timeout(Duration::from_millis(timeout_ms))
        .await
        .is_ok()
}

async fn await_quiet(quiet_ms: u64, budget_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(budget_ms);
    loop {
        if Instant::now() + Duration::from_millis(quiet_ms) > deadline {
            return false;
        }
        SETTLED.reset();
        if SETTLED
            .wait()
            .with_timeout(Duration::from_millis(quiet_ms))
            .await
            .is_err()
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

async fn wait_for_view(target: AppView, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if current_view() == Some(target) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        Timer::after(Duration::from_millis(VIEW_POLL_MS)).await;
    }
}

async fn act_in_reading(button: Button, timeout_ms: u64) -> bool {
    if !press_and_settle(button, timeout_ms).await {
        return false;
    }
    if current_view() != Some(AppView::Reading) {
        return true;
    }
    press_and_settle(button, timeout_ms).await
}

async fn wait_for_depth_below(from: u8, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if current_depth() < from {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        Timer::after(Duration::from_millis(VIEW_POLL_MS)).await;
    }
}

async fn leave_to_depth(depth: u8) -> bool {
    for _ in 0..NAV_ATTEMPTS {
        let from = current_depth();
        if from <= depth {
            return true;
        }
        if !press_and_settle(Button::Back, NAV_SETTLE_TIMEOUT_MS).await {
            return false;
        }
        if !wait_for_depth_below(from, LEAVE_ANSWER_MS).await {
            bench_log!(
                "bench-selftest: leave from depth {} was not answered in {} ms",
                from,
                LEAVE_ANSWER_MS
            );
            return false;
        }
        if !await_quiet(LEAVE_QUIET_MS, LEAVE_ANSWER_MS).await {
            bench_log!("bench-selftest: the shelf kept painting after the leave");
            return false;
        }
    }
    current_depth() <= depth
}

async fn pick_row() -> Option<AppView> {
    if !await_quiet(QUIET_WINDOW_MS, QUIET_BUDGET_MS).await {
        report_invalid_current("not-quiescent");
    }
    let depth_before = current_depth();
    if !press_and_settle(Button::Confirm, OPEN_SETTLE_TIMEOUT_MS).await {
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(VIEW_SETTLE_MS);
    let landed = loop {
        if current_view() == Some(AppView::Reading) {
            break AppView::Reading;
        }
        if current_view() == Some(AppView::Library) && current_depth() > depth_before {
            break AppView::Library;
        }
        if Instant::now() >= deadline {
            return None;
        }
        Timer::after(Duration::from_millis(VIEW_POLL_MS)).await;
    };
    if !await_quiet(LEAVE_QUIET_MS, QUIET_BUDGET_MS).await {
        bench_log!("bench-selftest: the pick's answer kept painting");
        return None;
    }
    Some(landed)
}

const START_CHAPTER: u16 = 3;

async fn seek_to_chapter(target: u16) -> bool {
    if !await_quiet(QUIET_WINDOW_MS, QUIET_BUDGET_MS).await {
        report_invalid_current("not-quiescent");
    }
    if !act_in_reading(Button::Confirm, NAV_SETTLE_TIMEOUT_MS).await
        || !wait_for_view(AppView::Chapters, VIEW_SETTLE_MS).await
    {
        return leave_chapters(false).await;
    }
    let first = current_selection();
    while current_selection() != target {
        let before = current_selection();
        if !press_and_settle(Button::Next, NAV_SETTLE_TIMEOUT_MS).await {
            return leave_chapters(false).await;
        }
        let now = current_selection();
        if now == before {
            bench_log!(
                "bench-selftest: {} chapter cursor stuck at {}",
                current_scenario(),
                now
            );
            return leave_chapters(false).await;
        }
        if now == first {
            bench_log!(
                "bench-selftest: {} has no chapter {}",
                current_scenario(),
                target
            );
            return leave_chapters(false).await;
        }
    }
    if !press_and_settle(Button::Confirm, OPEN_SETTLE_TIMEOUT_MS).await {
        return leave_chapters(false).await;
    }
    if !wait_for_view(AppView::Reading, VIEW_SETTLE_MS).await {
        return leave_chapters(false).await;
    }
    true
}

async fn leave_chapters(outcome: bool) -> bool {
    if current_view() == Some(AppView::Chapters)
        && press_and_settle(Button::Back, NAV_SETTLE_TIMEOUT_MS).await
    {
        let _ = wait_for_view(AppView::Reading, VIEW_SETTLE_MS).await;
    }
    outcome
}

async fn place_at_start_chapter() -> bool {
    if seek_to_chapter(START_CHAPTER).await {
        bench_log!(
            "bench-selftest: {} placed at chapter {} page {}",
            current_scenario(),
            START_CHAPTER,
            current_page()
        );
        return true;
    }
    report_invalid_current("chapter-seek");
    if wait_for_view(AppView::Reading, VIEW_SETTLE_MS).await {
        if !await_quiet(QUIET_WINDOW_MS, QUIET_BUDGET_MS).await {
            report_invalid_current("not-quiescent");
        }
        return true;
    }
    bench_log!(
        "bench-selftest: {} could not get back to reading after a failed seek",
        current_scenario()
    );
    false
}

async fn reach_reading() -> bool {
    for attempt in 0..NAV_ATTEMPTS {
        match current_view() {
            Some(AppView::Reading) => return true,
            Some(AppView::Library) => match pick_row().await {
                Some(AppView::Reading) => return true,
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

async fn back_to_library() -> bool {
    for _ in 0..NAV_ATTEMPTS {
        if wait_for_view(AppView::Library, VIEW_SETTLE_MS).await {
            return leave_to_depth(0).await;
        }
        if !act_in_reading(Button::Back, NAV_SETTLE_TIMEOUT_MS).await {
            return false;
        }
    }
    false
}

async fn turn_pages(count: u32) -> u32 {
    let mut turned = 0;
    for turn in 0..count {
        let _ = POWER_EVENTS.try_send(PowerEvent::Activity(AppView::Reading));
        let before = current_page();
        if !press_and_settle(PAGE_TURN_BUTTON, TURN_SETTLE_TIMEOUT_MS).await {
            bench_log!(
                "bench-selftest: turn {} did not settle within {} ms",
                turn,
                TURN_SETTLE_TIMEOUT_MS
            );
            break;
        }
        if current_page() == before {
            bench_log!(
                "bench-selftest: turn {} redrew page {} without moving; end of book",
                turn,
                before
            );
            report_invalid_current("end-of-book");
            break;
        }
        turned += 1;
        if !await_quiet(TURN_QUIET_MS, TURN_QUIET_BUDGET_MS).await {
            bench_log!(
                "bench-selftest: display still changing {} ms after turn {}",
                TURN_QUIET_BUDGET_MS,
                turn
            );
            report_invalid_current("not-quiescent");
        }
        report_completed("page_turn", turned);
    }
    turned
}

async fn open_and_quiesce() -> bool {
    if !reach_reading().await {
        return false;
    }
    if !await_quiet(QUIET_WINDOW_MS, QUIET_BUDGET_MS).await {
        bench_log!(
            "bench-selftest: still painting after {} ms, continuing anyway",
            QUIET_BUDGET_MS
        );
        report_invalid_current("not-quiescent");
    }
    true
}

async fn request_sleep() -> bool {
    if !await_quiet(QUIET_WINDOW_MS, QUIET_BUDGET_MS).await {
        report_invalid_current("not-quiescent");
    }
    for attempt in 0..SLEEP_ATTEMPTS {
        press(Button::Power).await;
        Timer::after(Duration::from_millis(SLEEP_SETTLE_MS)).await;
        bench_log!("bench-selftest: sleep request {} did not take", attempt);
    }
    false
}

type Result_ = &'static str;
const DONE: Result_ = "done";

fn report_invalid(scenario: &str, reason: &str) {
    bench_log!("bench-selftest: scenario={} invalid={}", scenario, reason);
}

fn report_completed(kind: &str, n: u32) {
    bench_log!(
        "bench-selftest: scenario={} completed={} count={}",
        current_scenario(),
        kind,
        n
    );
}

fn report_invalid_current(reason: &str) {
    report_invalid(current_scenario(), reason);
}

async fn scenario_page_turn() -> Result_ {
    if !open_and_quiesce().await {
        return "nav-failed";
    }
    if !place_at_start_chapter().await {
        return "nav-failed";
    }
    bench_log!("bench-selftest: reached reading, turning {} pages", TURNS);
    let turned = turn_pages(TURNS).await;
    bench_log!("bench-selftest: page-turn turns={} of {}", turned, TURNS);
    if turned < TURNS {
        return "short-turns";
    }
    DONE
}

async fn scenario_storage_cache() -> Result_ {
    let mut opens = 0u32;
    let mut short = false;
    for cycle in 0..STORAGE_CYCLES {
        if !open_and_quiesce().await {
            bench_log!("bench-selftest: storage cycle={} could not open", cycle);
            return "nav-failed";
        }
        opens += 1;
        if cycle == 0 && !place_at_start_chapter().await {
            return "nav-failed";
        }
        let turned = turn_pages(STORAGE_TURNS_PER_CYCLE).await;
        bench_log!("bench-selftest: storage cycle={} turns={}", cycle, turned);
        short |= turned < STORAGE_TURNS_PER_CYCLE;
        if !back_to_library().await {
            bench_log!(
                "bench-selftest: could not reach library after cycle {}",
                cycle
            );
            return "lost-library";
        }
    }
    bench_log!(
        "bench-selftest: storage opens={} of {}",
        opens,
        STORAGE_CYCLES
    );
    if short {
        report_invalid("storage-cache", "short-turns");
    }
    DONE
}

async fn scenario_folder_nav() -> Result_ {
    if !back_to_library().await {
        return "no-library";
    }
    let mut entered = 0u32;
    let mut books = 0u32;
    let mut attempts = 0u32;
    let mut scroll_stalled = false;
    while entered < FOLDER_ENTRIES && attempts < FOLDER_ATTEMPT_CEILING {
        attempts += 1;
        // Scan one row at a time until a folder is found so a fixed stride
        // cannot cycle over small shelves without hitting the folder row.
        let steps = if entered == 0 { 1 } else { FOLDER_SCROLL_STEPS };
        for _ in 0..steps {
            if !press_and_settle(Button::Next, NAV_SETTLE_TIMEOUT_MS).await {
                bench_log!("bench-selftest: scroll stalled at attempt {}", attempts);
                if !scroll_stalled {
                    report_invalid("folder-nav", "scroll-stalled");
                    scroll_stalled = true;
                }
                break;
            }
        }
        let depth_before = current_depth();
        match pick_row().await {
            Some(AppView::Reading) => {
                books += 1;
                if !back_to_library().await {
                    bench_log!("bench-selftest: lost the library at attempt {}", attempts);
                    return "lost-library";
                }
            }
            Some(AppView::Library) => {
                entered += 1;
                if !leave_to_depth(depth_before).await {
                    bench_log!(
                        "bench-selftest: stuck at depth {} after attempt {}",
                        current_depth(),
                        attempts
                    );
                    return "leave-failed";
                }
                report_completed("folder_leave", entered);
            }
            Some(_) => {}
            None => {
                bench_log!("bench-selftest: attempt {} did not settle", attempts);
                return "pick-stalled";
            }
        }
        if entered == 0 && books >= FOLDER_FLAT_PROBE {
            bench_log!(
                "bench-selftest: folder-nav found no folders in {} picks; this card \
                 has nothing to enter and the suite wants a folder tree",
                books
            );
            return "no-folders";
        }
    }
    bench_log!(
        "bench-selftest: folder-nav folders={} of {} books={} attempts={}",
        entered,
        FOLDER_ENTRIES,
        books,
        attempts
    );
    if entered < FOLDER_ENTRIES {
        return "short-entries";
    }
    DONE
}

async fn scenario_reader_soak() -> Result_ {
    if !open_and_quiesce().await {
        return "nav-failed";
    }
    if !place_at_start_chapter().await {
        return "nav-failed";
    }
    let turned = turn_pages(SOAK_TURNS).await;
    if !await_quiet(QUIET_WINDOW_MS, QUIET_BUDGET_MS).await {
        report_invalid_current("not-quiescent");
    }
    let mut jumped = false;
    if act_in_reading(Button::Confirm, NAV_SETTLE_TIMEOUT_MS).await
        && wait_for_view(AppView::Chapters, VIEW_SETTLE_MS).await
        && press_and_settle(Button::Next, NAV_SETTLE_TIMEOUT_MS).await
    {
        jumped = press_and_settle(Button::Confirm, OPEN_SETTLE_TIMEOUT_MS).await
            && wait_for_view(AppView::Reading, VIEW_SETTLE_MS).await;
    }

    let returned = back_to_library().await && open_and_quiesce().await;
    let turned_after = if returned {
        turn_pages(SOAK_TURNS).await
    } else {
        0
    };

    bench_log!(
        "bench-selftest: reader-soak turns={}+{} jumped={} returned={}",
        turned,
        turned_after,
        jumped,
        returned
    );
    if !jumped {
        report_invalid("reader-soak", "chapter-navigation");
    }
    if !returned {
        report_invalid("reader-soak", "library-return");
    }
    if turned < SOAK_TURNS || (returned && turned_after < SOAK_TURNS) {
        report_invalid("reader-soak", "short-turns");
    }

    request_sleep().await;
    "sleep-refused"
}

async fn scenario_sleep_sync() -> Result_ {
    if !open_and_quiesce().await {
        return "nav-failed";
    }
    let turned = turn_pages(SLEEP_TURNS).await;
    bench_log!("bench-selftest: sleep-sync turns={}, sleeping", turned);
    if turned < SLEEP_TURNS {
        report_invalid("sleep-sync", "short-turns");
    }
    request_sleep().await;
    "sleep-refused"
}

fn scenario_name() -> &'static str {
    option_env!("BENCH_SCENARIO").unwrap_or("page-turn")
}

#[embassy_executor::task]
pub async fn run() {
    Timer::after(Duration::from_millis(BOOT_QUIET_MS)).await;
    let scenario = scenario_name();
    set_scenario(scenario);
    bench_log!(
        "bench-selftest: scenario={} view={}",
        scenario,
        view_name(current_view())
    );

    if scenario_sleeps(scenario) {
        bench_log!(
            "bench-selftest: holding {} ms before a sleeping scenario, reflash now if you meant to",
            SLEEP_SCENARIO_HOLD_MS
        );
        Timer::after(Duration::from_millis(SLEEP_SCENARIO_HOLD_MS)).await;
    }

    let result = match scenario {
        "storage-cache" => scenario_storage_cache().await,
        "folder-nav" => scenario_folder_nav().await,
        "reader-soak" => scenario_reader_soak().await,
        "sleep-sync" => scenario_sleep_sync().await,
        "page-turn" => scenario_page_turn().await,
        other => {
            bench_log!(
                "bench-selftest: unknown scenario {}, doing nothing. \
                 Valid: page-turn storage-cache folder-nav reader-soak sleep-sync",
                other
            );
            "unknown-scenario"
        }
    };

    bench_log!("bench-selftest: scenario={} result={}", scenario, result);
}
