//! MCU-neutral channels and cancellation history. Capacities match the C3 runtime.
use crate::upload;
use app_core::*;
use core::cell::RefCell;
use core::sync::atomic::AtomicU32;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;

pub static INPUT_EVENTS: Channel<CriticalSectionRawMutex, InputEvent, 8> = Channel::new();
/// Increments when the S3 panel presents a new frame or enters sleep.
/// A touch carries the frame generation at release, so it cannot select an
/// entry from a prior visible list after a later frame has settled.
pub static DISPLAY_FRAME_EPOCH: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);
/// False until the S3 panel has presented an app frame, and while the sleep
/// image is visible. Taps on a sleeping/blank display are never list picks.
pub static DISPLAY_FRAME_ACTIVE: portable_atomic::AtomicBool =
    portable_atomic::AtomicBool::new(false);
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
/// unmounted, so the session-ending reset cannot race an open writer.
pub static UPLOAD_STOPPED: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
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

#[cfg(test)]
mod tests {
    use super::*;

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
