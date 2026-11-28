//! MCU-neutral channels and cancellation history. Capacities match the C3 runtime.
use crate::upload;
use app_core::storage_loop::storage_may_run;
use app_core::*;
use core::cell::{Cell, RefCell};
use core::sync::atomic::AtomicU32;
use core::task::{Context, Poll, Waker};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;

pub static INPUT_EVENTS: Channel<CriticalSectionRawMutex, InputEvent, 8> = Channel::new();
pub static LATEST_READER_REQUEST_ID: AtomicU32 = AtomicU32::new(0);
/// Moves on every button press queued on `INPUT_EVENTS` (battery samples
/// carry none), so work running on the display task can see a press the
/// app task has not yet been able to receive and give way to it.
pub static BUTTON_PRESSES: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);

/// Count a button press for `BUTTON_PRESSES`; every producer of button
/// events calls this as it queues one. Readers only compare against an
/// earlier value, so two writers racing still move the count.
pub fn note_button_press() {
    BUTTON_PRESSES.fetch_add(1, portable_atomic::Ordering::Relaxed);
}
/// The Library move the app is waiting on: a row pick or a folder leave, by
/// its request id, or 0 for none. The app task writes it as soon as its state
/// changes and before it next yields, and the storage task reads it before a
/// pick's rescan, to skip the scan for a pick the reader has walked away from
/// (Back) while the "updating the library" note was painting.
pub static LIBRARY_BROWSE_REQUEST_ID: AtomicU32 = AtomicU32::new(0);
pub static DISPLAY_COMMANDS: Channel<CriticalSectionRawMutex, DisplayCommand, 4> = Channel::new();

/// A sleep interrupted an upload while the display task was already the
/// consumer of DISPLAY_COMMANDS. Keep its handoff separate, and only take it
/// after previously queued display commands have been processed. In
/// particular, never evict a Render or wait on a full queue from its consumer.
pub fn take_deferred_display_sleep(
    queue_empty: bool,
    pending: &mut Option<DisplayCommand>,
) -> Option<DisplayCommand> {
    if queue_empty {
        pending.take()
    } else {
        None
    }
}

// 8 slots (270 B each) is enough for the cache-build burst case, and the
// ~2.1 KB of .bss saved by not going wider widens the main stack region. A
// full queue is not a loss: the queue is left alone and the acknowledgement
// waits in DisplayEventHolder until the app drains a slot (see
// send_required_display_event), so a shorter queue costs only that wait.
pub static DISPLAY_EVENTS: Channel<
    CriticalSectionRawMutex,
    DisplayEvent,
    { app_core::DISPLAY_EVENT_SLOTS },
> = Channel::new();
// Sized from app_core so the eviction walk that makes room in this channel
// (see send_required_library_event) cannot disagree with it about the ring.
pub static LIBRARY_EVENTS: Channel<
    CriticalSectionRawMutex,
    LibraryEvent,
    { app_core::LIBRARY_EVENT_SLOTS },
> = Channel::new();
pub static STORAGE_COMMANDS: Channel<CriticalSectionRawMutex, StorageCommand, 4> = Channel::new();
pub static POWER_EVENTS: Channel<CriticalSectionRawMutex, PowerEvent, 4> = Channel::new();

/// Latest frontlight request survives the one-way Wi-Fi memory loan. During an
/// upload the display task owns the SD session and cannot drain ordinary
/// STORAGE_COMMANDS; it writes this sidecar before acknowledging session exit.
const FRONTLIGHT_CHANGE_PRESENT: u32 = 1 << 16;
static FRONTLIGHT_CHANGE: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);

pub fn note_frontlight_change(brightness: u8, warmth: u8) {
    FRONTLIGHT_CHANGE.store(
        FRONTLIGHT_CHANGE_PRESENT | u32::from(brightness) | (u32::from(warmth) << 8),
        portable_atomic::Ordering::Release,
    );
}

pub fn latest_frontlight_change() -> Option<(u8, u8)> {
    let packed = FRONTLIGHT_CHANGE.load(portable_atomic::Ordering::Acquire);
    if packed & FRONTLIGHT_CHANGE_PRESENT == 0 {
        return None;
    }
    Some((packed as u8, (packed >> 8) as u8))
}

/// A frontlight command already buffered before a newer edit can survive an
/// upload session and run after teardown wrote the newest LIGHTA/B values.
/// Replace only frontlight writes with the latest producer-published value,
/// never letting an old queued command roll that value backwards.
pub fn current_frontlight_command(
    command: StorageCommand,
    latest: Option<(u8, u8)>,
) -> StorageCommand {
    match (command, latest) {
        (StorageCommand::StoreFrontlight { .. }, Some((brightness, warmth))) => {
            StorageCommand::StoreFrontlight { brightness, warmth }
        }
        _ => command,
    }
}

/// Latest activity survives POWER_EVENTS queue saturation so an input event
/// cannot be lost merely because its notification had no free channel slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerActivitySnapshot {
    ui_seq: u32,
    browser_seq: u32,
    ui_view: AppView,
}

impl PowerActivitySnapshot {
    const fn new() -> Self {
        Self {
            ui_seq: 0,
            browser_seq: 0,
            ui_view: AppView::Home,
        }
    }

    /// Snapshot carried by an explicit Power request, before it can wait in
    /// POWER_EVENTS behind older notifications. Only the generations matter
    /// for deciding whether *newer* activity cancels that request.
    pub const fn for_sleep_request(ui_seq: u32, browser_seq: u32) -> Self {
        Self {
            ui_seq,
            browser_seq,
            ui_view: AppView::Home,
        }
    }

    pub const fn sequences(self) -> (u32, u32) {
        (self.ui_seq, self.browser_seq)
    }

    pub const fn ui_view(self) -> AppView {
        self.ui_view
    }

    pub fn with_ui_activity(mut self, view: AppView) -> Self {
        self.ui_seq = self.ui_seq.wrapping_add(1);
        self.ui_view = view;
        self
    }

    pub fn with_browser_activity(mut self) -> Self {
        self.browser_seq = self.browser_seq.wrapping_add(1);
        self
    }

    /// Apply only the generation actually carried by this queued activity,
    /// not the potentially newer global snapshot. A stale queued event must
    /// not double-count after a canceled sleep or hide a dropped newer one.
    pub fn observe_ui(mut self, seq: u32, view: AppView) -> Self {
        let delta = seq.wrapping_sub(self.ui_seq);
        if delta != 0 && delta < (1 << 31) {
            self.ui_seq = seq;
            self.ui_view = view;
        }
        self
    }

    pub fn observe_browser(mut self, seq: u32) -> Self {
        let delta = seq.wrapping_sub(self.browser_seq);
        if delta != 0 && delta < (1 << 31) {
            self.browser_seq = seq;
        }
        self
    }

    /// Observe only a new queued notification. Returning None means the
    /// power task must leave its idle leash/deadline unchanged.
    pub fn accept_ui(self, seq: u32, view: AppView) -> Option<Self> {
        let next = self.observe_ui(seq, view);
        (next != self).then_some(next)
    }

    pub fn accept_browser(self, seq: u32) -> Option<Self> {
        let next = self.observe_browser(seq);
        (next != self).then_some(next)
    }

    pub fn cancellation_since(self, latest: Self, trigger: SleepTrigger) -> Option<SleepResume> {
        if self.ui_seq != latest.ui_seq {
            Some(SleepResume::Activity(latest.ui_view))
        } else if trigger == SleepTrigger::Idle && self.browser_seq != latest.browser_seq {
            Some(SleepResume::BrowserActivity)
        } else {
            None
        }
    }
}

static POWER_ACTIVITY: Mutex<CriticalSectionRawMutex, Cell<PowerActivitySnapshot>> =
    Mutex::new(Cell::new(PowerActivitySnapshot::new()));

pub fn power_activity_snapshot() -> PowerActivitySnapshot {
    POWER_ACTIVITY.lock(Cell::get)
}

pub fn note_power_activity(view: AppView) -> u32 {
    POWER_ACTIVITY.lock(|cell| {
        let updated = cell.get().with_ui_activity(view);
        cell.set(updated);
        updated.ui_seq
    })
}

/// Record a valid physical input before the app has a chance to handle it.
/// Sleep cancellation must see input even while the app task is blocked.
pub fn note_physical_input_activity(view: AppView) {
    let _ = note_power_activity(view);
}

pub fn note_browser_power_activity() -> u32 {
    POWER_ACTIVITY.lock(|cell| {
        let updated = cell.get().with_browser_activity();
        cell.set(updated);
        updated.browser_seq
    })
}

/// Synthetic restoration is owned by the display task, not the app render
/// lock. An aborted second sleep must not discard the pending restoration.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeldSleepRestore {
    pending: Option<RenderRequest>,
    failures: u8,
}

impl HeldSleepRestore {
    pub const fn new() -> Self {
        Self {
            pending: None,
            failures: 0,
        }
    }

    pub const fn pending(&self) -> Option<RenderRequest> {
        self.pending
    }

    pub fn take_for_render(&mut self) -> Option<RenderRequest> {
        self.pending.take()
    }

    pub fn hold(&mut self, request: RenderRequest) {
        self.pending = Some(request);
        self.failures = 0;
    }

    /// Only call when a real Render supersedes the restore or a new sleep
    /// commits to painting; not from pre-flush cancellation/refusal paths.
    pub fn supersede(&mut self) -> bool {
        self.failures = 0;
        self.pending.take().is_some()
    }

    pub fn update_frontlight(&mut self, brightness: u8, warmth: u8) {
        if let Some(request) = self.pending.as_mut() {
            request.frontlight_brightness = brightness;
            request.frontlight_warmth = warmth;
        }
    }

    /// Retry two transient panel errors, then let the app request a new frame.
    pub fn retry_failed(&mut self, request: RenderRequest) -> bool {
        if self.failures >= 2 {
            self.failures = 0;
            false
        } else {
            self.failures += 1;
            self.pending = Some(request);
            true
        }
    }

    pub fn completed(&mut self) {
        self.failures = 0;
    }
}

/// The generation of a sleep handshake the power task gave up on.
///
/// Once the display task has put the panel down it parks until deep sleep
/// takes the chip or this names its own generation. Parking is what keeps a
/// render from repainting over the sleep image: a task that is not running
/// cannot touch the panel, and `enter_deep_sleep_button` never returns, so on
/// the ordinary path the park simply never ends.
///
/// A ring buffer of recent sleep cancellations preserved by generation.
/// Storing cancellations by generation ensures that an older sleep command
/// checking for cancellation never discards or overwrites a newer generation's
/// cancellation.
const CANCELLED_GENERATIONS_CAP: usize = 4;

struct CancelledSleepHistory {
    entries: [Option<(u32, SleepResume)>; CANCELLED_GENERATIONS_CAP],
    next: usize,
}

impl CancelledSleepHistory {
    const fn new() -> Self {
        Self {
            entries: [None; CANCELLED_GENERATIONS_CAP],
            next: 0,
        }
    }

    fn record(&mut self, generation: u32, reason: SleepResume) {
        for (gen, r) in self.entries.iter_mut().flatten() {
            if *gen == generation {
                *r = reason;
                return;
            }
        }
        self.entries[self.next] = Some((generation, reason));
        self.next = (self.next + 1) % CANCELLED_GENERATIONS_CAP;
    }

    fn check(&self, generation: u32) -> Option<SleepResume> {
        for (gen, reason) in self.entries.iter().flatten() {
            if *gen == generation {
                return Some(*reason);
            }
        }
        None
    }
}

static CANCELLED_SLEEPS: Mutex<CriticalSectionRawMutex, RefCell<CancelledSleepHistory>> =
    Mutex::new(RefCell::new(CancelledSleepHistory::new()));

/// Wakeup notification for tasks waiting on sleep cancellation.
pub static DISPLAY_RESUME: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Notifies wifi_task of upload session readiness or teardown.
pub static UPLOAD_SESSION_READY: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Record a sleep cancellation for `generation` and notify any waiting tasks.
pub fn signal_sleep_cancelled(generation: u32, reason: SleepResume) {
    CANCELLED_SLEEPS.lock(|cell| {
        cell.borrow_mut().record(generation, reason);
    });
    DISPLAY_RESUME.signal(());
}

/// Non-blocking check whether the specified sleep generation was cancelled by
/// the power task (e.g. via button press or browser activity).
pub fn sleep_is_cancelled(generation: u32) -> Option<SleepResume> {
    CANCELLED_SLEEPS.lock(|cell| cell.borrow().check(generation))
}
pub static SYNC_COMMANDS: Channel<CriticalSectionRawMutex, SyncCommand, 2> = Channel::new();
pub static SYNC_EVENTS: Channel<CriticalSectionRawMutex, SyncEvent, 4> = Channel::new();
pub static UPLOAD_BEGINS: Channel<CriticalSectionRawMutex, upload::UploadBegin, 1> = Channel::new();
pub static UPLOAD_CHUNKS: Channel<CriticalSectionRawMutex, upload::UploadChunk, 2> = Channel::new();
pub static UPLOAD_RETURNS: Channel<CriticalSectionRawMutex, &'static mut [u8], 2> = Channel::new();
pub static UPLOAD_RESULTS: Channel<CriticalSectionRawMutex, bool, 1> = Channel::new();
/// Wireless Exit asks the board I/O task to abort and close the upload
/// session (any in-flight book is aborted and its clusters reclaimed).
pub static UPLOAD_STOP_REQUESTS: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
/// Acknowledges the stop: every FAT handle is closed and the volume is
/// unmounted. The boolean additionally confirms that S3 frontlight settings
/// were saved before Wi-Fi may reset the board (C3 always sends true).
pub static UPLOAD_STOPPED: Channel<CriticalSectionRawMutex, bool, 1> = Channel::new();
/// A failed S3 upload-teardown frontlight write must not be bypassed by a
/// subsequent Exit that observes an already-inactive upload session. Keep
/// this latched until a later teardown successfully retries the latest value.
pub static UPLOAD_FRONTLIGHT_SAVE_FAILED: portable_atomic::AtomicBool =
    portable_atomic::AtomicBool::new(false);
/// A Sleep exit closed the upload session while the book server may be
/// mid-request. The server consumes this to fail the interrupted request,
/// reclaim the loaned buffers, and restart the session on the next request.
/// Only observable when sleep turns out non-terminal (a late activity event
/// cancels the handshake, or display shutdown fails); a completed deep
/// sleep resets before anything reads it. Wireless Exit doesn't raise it —
/// that path ends in a reset behind the UPLOAD_STOPPED handshake.
pub static UPLOAD_INTERRUPTS: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Whether a StoreWifiCredentials write landed AND read back identically
/// through the boot-time read path; the portal waits on this before it
/// may show the success page.
pub static WIFI_STORAGE_RESULTS: Channel<CriticalSectionRawMutex, bool, 1> = Channel::new();

/// The settling event with nowhere to go, and the gates it closes while it
/// waits. [`LibraryEventHolder`] owns both, and is host-tested; the display
/// task asks it rather than re-deciding at each site that must agree.
pub static HELD_LIBRARY_EVENT: Mutex<CriticalSectionRawMutex, Cell<LibraryEventHolder>> =
    Mutex::new(Cell::new(LibraryEventHolder::new()));

/// Reads the library-event holder. `Copy`, so this is a snapshot — fine for the
/// gates, which only ever narrow: nothing but the display task fills the holder,
/// and the one thing that empties it is that task's own placing branch.
pub fn library_event_holder() -> LibraryEventHolder {
    HELD_LIBRARY_EVENT.lock(Cell::get)
}

fn with_library_event_holder<T>(update: impl FnOnce(&mut LibraryEventHolder) -> T) -> T {
    HELD_LIBRARY_EVENT.lock(|cell| {
        let mut holder = cell.get();
        let outcome = update(&mut holder);
        cell.set(holder);
        outcome
    })
}

/// Sends a library event, routing settling events through
/// [`send_required_library_event`] so a full channel cannot drop work the app
/// is waiting on.
pub fn send_library_event(event: &LibraryEvent) {
    if event.must_be_delivered() {
        send_required_library_event(event);
        return;
    }
    if LIBRARY_EVENTS.try_send(*event).is_err() {
        crate::runtime_log!("display: library event queue full");
    }
}

/// Queue an event the app is waiting on, making room if the channel is full.
///
/// Walks the ring via [`EvictionWalk`] to spend only refresh events rather than
/// destroying another settling event; if every slot holds a settling event,
/// holds `event` in [`HELD_LIBRARY_EVENT`] for [`place_held_library_event`].
pub fn send_required_library_event(event: &LibraryEvent) {
    if LIBRARY_EVENTS.try_send(*event).is_ok() {
        return;
    }
    let mut walk = EvictionWalk::new(app_core::LIBRARY_EVENT_SLOTS);
    while !walk.exhausted() {
        let Ok(head) = LIBRARY_EVENTS.try_receive() else {
            break;
        };
        match walk.inspect(&head) {
            EvictionStep::Discard => {
                // To preserve FIFO order among all surviving events, rotate
                // the remaining uninspected events that were originally behind
                // the discarded element.
                for _ in 0..walk.remaining() {
                    if let Ok(item) = LIBRARY_EVENTS.try_receive() {
                        let _ = LIBRARY_EVENTS.try_send(item);
                    }
                }
                // The refresh is spent; its slot is this event's. Nothing
                // else writes this channel, so the send cannot lose the race.
                if LIBRARY_EVENTS.try_send(*event).is_ok() {
                    return;
                }
                break;
            }
            EvictionStep::Requeue => {
                // One slot is free (the head came off), so this always lands.
                let _ = LIBRARY_EVENTS.try_send(head);
            }
        }
    }
    hold_library_event(event);
}

/// Keeps `event` until the channel has room, or says why it could not.
fn hold_library_event(event: &LibraryEvent) {
    let outcome = with_library_event_holder(|holder| holder.hold(event));
    let refusal = match outcome {
        HoldOutcome::Held => return,
        HoldOutcome::NotSettling => "refresh routed to the holder",
        HoldOutcome::Occupied => "holder occupied",
    };
    if LIBRARY_EVENTS.try_send(*event).is_err() {
        crate::runtime_log!("display: {}, dropped {:?}", refusal, event);
    }
}

/// Places the settling event [`send_required_library_event`] could not, once
/// the channel has room. Pending forever when nothing is held, so it can sit
/// in the display loop's select as a branch that only fires when it has work.
pub async fn place_held_library_event() {
    let Some(event) = library_event_holder().pending() else {
        return core::future::pending::<()>().await;
    };
    LIBRARY_EVENTS.send(event).await;
    // Nothing awaits between the send completing and this, so the holder
    // cannot be observed empty with the event still unsent, or cleared for a
    // send that a cancellation abandoned.
    let _ = with_library_event_holder(LibraryEventHolder::placed);
}

/// The next storage command, but only while nothing is held and no rescan is
/// owed.
pub async fn storage_command_while_free(rescan_owed: bool) -> StorageCommand {
    if !storage_may_run(library_event_holder().storage_may_run(), rescan_owed) {
        return core::future::pending::<StorageCommand>().await;
    }
    STORAGE_COMMANDS.receive().await
}

/// The acknowledgement that had nowhere to go, waiting for room. Gates
/// nothing: see [`DisplayEventHolder`] for why refusing renders while one is
/// held would deadlock the very task that empties it.
pub static HELD_DISPLAY_EVENT: Mutex<CriticalSectionRawMutex, Cell<DisplayEventHolder>> =
    Mutex::new(Cell::new(DisplayEventHolder::new()));

/// Reads the display-event holder snapshot.
pub fn display_event_holder() -> DisplayEventHolder {
    HELD_DISPLAY_EVENT.lock(Cell::get)
}

fn with_display_event_holder<T>(update: impl FnOnce(&mut DisplayEventHolder) -> T) -> T {
    HELD_DISPLAY_EVENT.lock(|cell| {
        let mut holder = cell.get();
        let outcome = update(&mut holder);
        cell.set(holder);
        outcome
    })
}

/// Sends a display event, by the rule the event itself carries.
pub fn send_display_event(event: &DisplayEvent) {
    if event.must_be_delivered() {
        send_required_display_event(event);
        return;
    }
    if DISPLAY_EVENTS.try_send(*event).is_err() {
        crate::runtime_log!("display: display event queue full, dropped {:?}", event);
    }
}

/// Queues an acknowledgement the app is waiting on, holding it in
/// [`HELD_DISPLAY_EVENT`] if the channel is full.
pub fn send_required_display_event(event: &DisplayEvent) {
    if DISPLAY_EVENTS.try_send(*event).is_ok() {
        return;
    }
    let outcome = with_display_event_holder(|holder| holder.hold(event));
    let refusal = match outcome {
        DisplayHoldOutcome::Held => return,
        DisplayHoldOutcome::NotRequired => "refresh routed to the acknowledgement holder",
        // Both end the render cycle and the app clears its lock on either, so
        // the one already waiting answers for this one too.
        DisplayHoldOutcome::Occupied => "acknowledgement holder occupied",
    };
    if DISPLAY_EVENTS.try_send(*event).is_err() {
        crate::runtime_log!("display: {}, dropped {:?}", refusal, event);
    }
}

/// Places the acknowledgement [`send_required_display_event`] could not, once
/// the channel has room. Pending forever when nothing is held, so it can sit
/// in the display loop's select as a branch that only fires when it has work.
pub async fn place_held_display_event() {
    let Some(event) = display_event_holder().pending() else {
        return core::future::pending::<()>().await;
    };
    DISPLAY_EVENTS.send(event).await;
    // Nothing awaits between the send completing and this, so the holder
    // cannot be observed empty with the event still unsent, or cleared for a
    // send that a cancellation abandoned.
    let _ = with_display_event_holder(DisplayEventHolder::placed);
}

/// The waker of the task awaiting this, without yielding.
pub async fn own_waker() -> Waker {
    core::future::poll_fn(|cx| Poll::Ready(cx.waker().clone())).await
}

/// Drive `future` to completion from synchronous code that holds the
/// executor anyway: the scan, which runs for seconds without yielding.
///
/// Polled with this task's own waker, since embassy-time files its timers
/// under the task a waker names and refuses any other. The spin lasts one
/// flush and replaces no work: nothing else can run until the scan returns.
/// The wakes it collects only poll the task once more after the scan.
pub fn poll_to_completion<F: core::future::Future>(future: F, waker: &Waker) -> F::Output {
    let mut future = core::pin::pin!(future);
    let mut cx = Context::from_waker(waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        core::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_to_completion_drives_multi_poll_future() {
        let mut remaining = 3u8;
        let output = poll_to_completion(
            core::future::poll_fn(|_| {
                if remaining == 0 {
                    Poll::Ready(42u32)
                } else {
                    remaining -= 1;
                    Poll::Pending
                }
            }),
            Waker::noop(),
        );
        assert_eq!(output, 42);
    }

    #[test]
    fn full_display_queue_does_not_evict_a_render_for_deferred_sleep() {
        use embassy_sync::blocking_mutex::raw::NoopRawMutex;

        let queue: Channel<NoopRawMutex, DisplayCommand, 4> = Channel::new();
        let render = DisplayCommand::Render(ReaderState::boot().render_request(RenderKind::Page));
        queue.try_send(render).unwrap();
        for level in [10, 20, 30] {
            queue
                .try_send(DisplayCommand::SetFrontlight {
                    brightness: level,
                    warmth: 0,
                })
                .unwrap();
        }
        assert!(queue.is_full());

        let sleep = DisplayCommand::Sleep {
            generation: 7,
            trigger: SleepTrigger::Explicit,
        };
        let mut pending = Some(sleep);
        assert_eq!(
            take_deferred_display_sleep(queue.is_empty(), &mut pending),
            None
        );
        assert_eq!(queue.try_receive(), Ok(render));
        for level in [10, 20, 30] {
            assert_eq!(
                queue.try_receive(),
                Ok(DisplayCommand::SetFrontlight {
                    brightness: level,
                    warmth: 0,
                })
            );
        }
        assert_eq!(
            take_deferred_display_sleep(queue.is_empty(), &mut pending),
            Some(sleep)
        );
        assert_eq!(pending, None);
    }

    #[test]
    fn buffered_light_write_does_not_undo_newer_teardown_save() {
        let older = StorageCommand::StoreFrontlight {
            brightness: 25,
            warmth: 0,
        };
        let newest = StorageCommand::StoreFrontlight {
            brightness: 75,
            warmth: 50,
        };
        assert_eq!(current_frontlight_command(older, Some((75, 50))), newest);
        assert_eq!(current_frontlight_command(older, None), older);
        let non_light = StorageCommand::RefreshCatalog;
        assert_eq!(
            current_frontlight_command(non_light, Some((75, 50))),
            non_light
        );
    }

    #[test]
    fn upload_teardown_uses_latest_settings_not_earlier_toggle() {
        // A Home-key toggle is followed by brightness/warmth edits in
        // Settings before the upload session terminates and writes LIGHTA/B.
        note_frontlight_change(25, 0);
        assert_eq!(latest_frontlight_change(), Some((25, 0)));
        note_frontlight_change(75, 50);
        assert_eq!(latest_frontlight_change(), Some((75, 50)));
    }

    #[test]
    fn power_activity_survives_full_notification_queue() {
        let before = PowerActivitySnapshot::new();
        let ui = before.with_ui_activity(AppView::Reading);
        assert_eq!(
            before.cancellation_since(ui, SleepTrigger::Explicit),
            Some(SleepResume::Activity(AppView::Reading))
        );
        assert_eq!(
            before.cancellation_since(ui, SleepTrigger::Idle),
            Some(SleepResume::Activity(AppView::Reading))
        );
        let browser = before.with_browser_activity();
        assert_eq!(
            before.cancellation_since(browser, SleepTrigger::Idle),
            Some(SleepResume::BrowserActivity)
        );
        assert_eq!(
            before.cancellation_since(browser, SleepTrigger::Explicit),
            None
        );
        assert_eq!(ui.cancellation_since(ui, SleepTrigger::Idle), None);
        // The power task must not sample global state while consuming an
        // older event: a newer interaction might already have been dropped
        // behind a full queue, and still needs to cancel queued SleepNow.
        let newer = ui.with_ui_activity(AppView::Library);
        let consumed_old_event = before.with_ui_activity(AppView::Reading);
        assert_eq!(
            consumed_old_event.cancellation_since(newer, SleepTrigger::Explicit),
            Some(SleepResume::Activity(AppView::Library))
        );
        // A and B arrived during sleep and the task canceled after A. An
        // already-snapshotted B must not increment again when dequeued.
        assert_eq!(newer.observe_ui(ui.ui_seq, AppView::Reading), newer);
        assert_eq!(newer.observe_ui(newer.ui_seq, AppView::Library), newer);
        assert_eq!(before.observe_ui(ui.ui_seq, AppView::Reading), ui);
        assert_eq!(ui.observe_ui(newer.ui_seq, AppView::Library), newer);

        let browser_one = before.with_browser_activity();
        let browser_two = browser_one.with_browser_activity();
        assert_eq!(
            browser_two.observe_browser(browser_one.browser_seq),
            browser_two
        );
        assert_eq!(before.observe_browser(browser_two.browser_seq), browser_two);

        // A stale queued Library notification following a cancellation
        // cannot shorten the Reading idle timeout; it is not accepted.
        assert_eq!(newer.accept_ui(ui.ui_seq, AppView::Reading), None);
        assert_eq!(ui.accept_ui(newer.ui_seq, AppView::Library), Some(newer));
        assert_eq!(browser_two.accept_browser(browser_one.browser_seq), None);

        // Explicit sleep uses the generations captured when the Power
        // request was issued, even if older UI notifications were dropped.
        let issued =
            PowerActivitySnapshot::for_sleep_request(newer.ui_seq, browser_two.browser_seq);
        assert_eq!(
            issued.cancellation_since(newer.with_browser_activity(), SleepTrigger::Explicit),
            None
        );
        assert_eq!(
            issued.cancellation_since(
                newer.with_ui_activity(AppView::Reading),
                SleepTrigger::Explicit
            ),
            Some(SleepResume::Activity(AppView::Reading))
        );
        assert_eq!(issued.sequences(), (newer.ui_seq, browser_two.browser_seq));
    }

    #[test]
    fn post_ack_input_cancels_sleep_during_final_wake_pin_handoff() {
        // The sleep image is settled. Nothing new is visible at the first
        // check, but Home is pressed while Power is still being released.
        let issued = PowerActivitySnapshot::new().with_ui_activity(AppView::Reading);
        assert_eq!(
            issued.cancellation_since(issued, SleepTrigger::Explicit),
            None
        );
        let during_handoff = issued.with_ui_activity(AppView::Reading);
        assert_eq!(
            issued.cancellation_since(during_handoff, SleepTrigger::Explicit),
            Some(SleepResume::Activity(AppView::Reading))
        );

        // A transfer is allowed to keep an idle sleep awake, but a browser
        // keepalive must not override an explicit Power press.
        let browser = issued.with_browser_activity();
        assert_eq!(
            issued.cancellation_since(browser, SleepTrigger::Idle),
            Some(SleepResume::BrowserActivity)
        );
        assert_eq!(
            issued.cancellation_since(browser, SleepTrigger::Explicit),
            None
        );
    }

    #[test]
    fn failed_synthetic_repaint_has_bounded_retries_and_sleep_preserves_restore() {
        let request = ReaderState::boot().render_request(RenderKind::Page);
        let mut held = HeldSleepRestore::new();
        held.hold(request);
        // A cancelled or refused Sleep does not call supersede.
        assert_eq!(held.pending(), Some(request));
        held.update_frontlight(25, 75);
        let updated = held.pending().unwrap();
        assert_eq!(updated.frontlight_brightness, 25);
        assert_eq!(updated.frontlight_warmth, 75);
        assert_eq!(held.take_for_render(), Some(updated));
        assert!(held.retry_failed(updated));
        assert_eq!(held.take_for_render(), Some(updated));
        assert!(held.retry_failed(updated));
        assert_eq!(held.take_for_render(), Some(updated));
        assert!(!held.retry_failed(updated));
        assert_eq!(held.pending(), None);
        held.hold(request);
        assert!(held.supersede());
        assert_eq!(held.pending(), None);
    }

    #[test]
    fn cancellation_updates_one_generation_without_consuming_another() {
        let mut history = CancelledSleepHistory::new();
        history.record(41, SleepResume::PowerButton);
        history.record(42, SleepResume::BrowserActivity);
        history.record(41, SleepResume::Activity(AppView::Reading));
        assert_eq!(
            history.check(41),
            Some(SleepResume::Activity(AppView::Reading))
        );
        assert_eq!(history.check(42), Some(SleepResume::BrowserActivity));
        assert_eq!(history.check(43), None);
        // Repeated reads by an old queued command never clear the newer one.
        assert_eq!(history.check(42), Some(SleepResume::BrowserActivity));
        history.record(43, SleepResume::PowerButton);
        history.record(44, SleepResume::PowerButton);
        assert!(history.check(41).is_some());
        history.record(45, SleepResume::PowerButton);
        assert_eq!(history.check(41), None);
        assert!(history.check(42).is_some());
    }
}
