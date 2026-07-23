//! The storage/display task's command loop, as sequences a host can drive.
//!
//! The task itself owns the SD card, the panel, and a 47 KB reader store, so it
//! cannot run off-device. But almost nothing that has gone wrong in it was about
//! the hardware. The faults were about order: a `Sleep` overtaking work the app
//! had already handed over, a pre-sleep drain swallowing the one command it was
//! not allowed to answer, a book-open transaction announcing a switch whose
//! first write never landed. Every one of those is decidable without touching a
//! card.
//!
//! So the ordering lives here, as sequences the task drives rather than code the
//! task contains. Each `next` names one piece of work; the caller does it and
//! reports back what the hardware said. The firmware answers with a real card;
//! the tests answer with a card model that can be told to fail any individual
//! write, and drive the same state machines the firmware does.
//!
//! RAM (measured): `SleepSequence` is 24 bytes and holds no command — the
//! drained one stays with the caller. `OpenSequence` is 84: it carries the
//! departing 28-byte `PersistedAppState` twice, once as the pending close-out
//! step and once as the base the global pointer record is built from. Both sit
//! on the display task's stack for the length of one command, replacing the
//! `Option<PersistedAppState>` and the resolved chapter/page/resumed locals the
//! open arm carried before, so about 44 bytes more is live across
//! `build_or_load_book_cache` — negligible against that chain's 30-43 KB
//! region, and none of it is added to the deep frames themselves.

use crate::{DisplayCommand, PersistedAppState, StorageCommand, SyncSession};
use display::font::TypeSettings;

/// Which arm of the loop answers a storage command.
///
/// The routing matters because one command is not the storage handler's to
/// apply: `ReceiveUpload` is a request to *become* the upload writer for the
/// rest of the session, which only the loop can do — it is the loop that owns
/// the card and can park on the upload channels. The handler's arm for it is a
/// deliberate no-op, so anything that reaches the handler with an upload in hand
/// has already lost it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopArm {
    /// Hand the card to the upload session until it ends.
    UploadSession,
    /// An upload arrived with no session to serve it. Nothing owns the writer,
    /// so there is nothing to enter; drop it.
    RefusedUpload,
    /// Everything else goes to the storage handler, which applies the session
    /// gate itself — the pre-sleep drain reaches it without passing here.
    Apply,
}

/// Where a selected storage command goes.
pub fn loop_arm(command: &StorageCommand, session: SyncSession) -> LoopArm {
    if !matches!(command, StorageCommand::ReceiveUpload) {
        return LoopArm::Apply;
    }
    if session.admits(command) {
        LoopArm::UploadSession
    } else {
        LoopArm::RefusedUpload
    }
}

/// Why a sleep request was turned down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepRefusal {
    /// An upload request was still queued, and was put back. The loop picks it
    /// up next and `upload_session` does its own sleep handling, re-queueing
    /// this generation once the filesystem is closed.
    UploadQueued,
    /// The declined upload request would not go back on the queue, which can
    /// only mean a producer refilled the slot it came out of. The request
    /// itself is lost, but a full queue is not: the channel now holds a whole
    /// budget's worth of accepted work, so refuse and let the ordinary loop
    /// apply it through the normal routing rather than this drain's restricted
    /// path.
    UploadLost,
    /// The coalesced progress record would not land. Sleeping now would lose it
    /// for good, so stay awake and let the next flush retry.
    ProgressUnwritten,
}

/// What the pre-sleep sequence wants next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepAction {
    /// Take one more command off the storage queue, then report it through
    /// [`SleepSequence::drained`] or [`SleepSequence::queue_empty`].
    TakeQueued,
    /// Write the coalesced progress record, then report through
    /// [`SleepSequence::flushed`].
    FlushProgress,
    /// Do not sleep. Tell the power task, and leave the panel up.
    Refuse(SleepRefusal),
    /// Everything owed has reached the card. Render the sleep frame and put the
    /// panel down.
    Proceed,
}

/// What the drain does with one command it took off the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drained {
    /// Apply it against the card before the panel sleeps.
    Apply,
    /// Not the drain's to answer. Put it back and refuse this sleep.
    RequeueAndRefuse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SleepPhase {
    Draining,
    Flushing,
    Refused(SleepRefusal),
    Ready,
}

/// Everything the task owes the card before the panel may sleep, in order.
///
/// Deep sleep is terminal — waking is a fresh boot — so whatever is still
/// queued when the panel goes down is simply gone. The loop takes display
/// commands ahead of storage ones, so a `Sleep` routinely arrives in front of
/// work handed over a moment earlier, and the app cannot close that itself:
/// once it has passed a command to the channel it has no way to know whether
/// the task has applied it. The guarantee has to live where the card is owned.
///
/// The drain is bounded by the channel's own depth. It is only ever catching up
/// on what was already accepted, never following a producer that keeps writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SleepSequence {
    phase: SleepPhase,
    drained: usize,
    budget: usize,
}

impl SleepSequence {
    /// `drain_budget` is the storage channel's capacity: the most commands that
    /// can be waiting when the sleep is selected.
    pub const fn new(drain_budget: usize) -> Self {
        Self {
            phase: if drain_budget == 0 {
                SleepPhase::Flushing
            } else {
                SleepPhase::Draining
            },
            drained: 0,
            budget: drain_budget,
        }
    }

    pub const fn next(&self) -> SleepAction {
        match self.phase {
            SleepPhase::Draining => SleepAction::TakeQueued,
            SleepPhase::Flushing => SleepAction::FlushProgress,
            SleepPhase::Refused(refusal) => SleepAction::Refuse(refusal),
            SleepPhase::Ready => SleepAction::Proceed,
        }
    }

    /// The verdict for one command taken off the queue. Pure: the caller acts
    /// on it and then reports back through `applied` or `requeued`.
    pub const fn drained(&self, command: &StorageCommand) -> Drained {
        match command {
            StorageCommand::ReceiveUpload => Drained::RequeueAndRefuse,
            _ => Drained::Apply,
        }
    }

    /// The queue had nothing more.
    pub fn queue_empty(&mut self) {
        self.phase = SleepPhase::Flushing;
    }

    /// A drained command was applied against the card.
    pub fn applied(&mut self) {
        self.drained += 1;
        if self.drained >= self.budget {
            self.phase = SleepPhase::Flushing;
        }
    }

    /// Whether the command the drain declined actually made it back onto the
    /// queue.
    ///
    /// Either answer refuses the sleep, but for opposite reasons, and the
    /// difference is why this takes the result rather than assuming it. A
    /// put-back that landed means the loop will answer the request. A put-back
    /// that failed can only mean a producer refilled the slot the command came
    /// out of — so the channel is *full*, not one short. Carrying on would then
    /// be the worst of both: the drain would spend its remaining budget on a
    /// queue that had grown behind it and hand whatever it could not reach to a
    /// terminal sleep.
    pub fn requeued(&mut self, restored: bool) {
        self.phase = SleepPhase::Refused(if restored {
            SleepRefusal::UploadQueued
        } else {
            SleepRefusal::UploadLost
        });
    }

    /// Whether the coalesced progress record reached the card.
    pub fn flushed(&mut self, stored: bool) {
        self.phase = if stored {
            SleepPhase::Ready
        } else {
            SleepPhase::Refused(SleepRefusal::ProgressUnwritten)
        };
    }
}

/// The display command channel's depth, and so the most sleep requests that can
/// be waiting behind one another when the panel goes down.
pub const MAX_HELD_SLEEPS: usize = 4;

/// What the panel-down drain does with one command taken off the display queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelDown {
    /// A frame that will never reach the panel. Answer it as a refresh that
    /// failed — which is the truth — rather than discarding it. `RefreshFailed`
    /// clears the app's render lock and drops its coalesced frame, so a sleep
    /// later abandoned on a button press repaints from scratch. A silent drop
    /// would strand that lock instead, and every later input would queue behind
    /// an acknowledgement that is never coming.
    FailRender,
    /// A sleep request this drain must not consume. The power task waits for a
    /// matching acknowledgement for every `Sleep` it sends, and only the sleep
    /// arm produces one, so it is held and put back once the drain is done.
    HoldSleep,
}

/// What is still queued for a panel that has already gone down.
///
/// Deep sleep is imminent here, but the display queue can still hold a render —
/// most often one the pre-sleep storage drain provoked, since applying a book
/// open emits `Loaded` and the app repaints on it. Processing that render would
/// re-init the panel and paint a page over the sleep image, racing the power
/// task's power cut; whichever wins, the device can end up sleeping on a page.
///
/// Nothing here may `await`. That is what makes putting the held sleeps back
/// infallible: no producer can refill the channel mid-drain, so every slot this
/// drain frees is still free when it hands one back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelDownDrain {
    taken: usize,
    budget: usize,
    held: [u32; MAX_HELD_SLEEPS],
    held_len: usize,
}

impl PanelDownDrain {
    /// `budget` is the display channel's capacity. It is clamped to
    /// [`MAX_HELD_SLEEPS`], which is that same depth: the drain can never need
    /// to hold more sleep requests than the channel is able to contain.
    pub const fn new(budget: usize) -> Self {
        Self {
            taken: 0,
            budget: if budget > MAX_HELD_SLEEPS {
                MAX_HELD_SLEEPS
            } else {
                budget
            },
            held: [0; MAX_HELD_SLEEPS],
            held_len: 0,
        }
    }

    /// Whether to take another command off the queue.
    pub const fn wants_more(&self) -> bool {
        self.taken < self.budget
    }

    /// The verdict for one command taken off the queue.
    pub fn took(&mut self, command: &DisplayCommand) -> PanelDown {
        self.taken += 1;
        match *command {
            DisplayCommand::Sleep { generation } => {
                if self.held_len < MAX_HELD_SLEEPS {
                    self.held[self.held_len] = generation;
                    self.held_len += 1;
                }
                PanelDown::HoldSleep
            }
            DisplayCommand::Render(_) => PanelDown::FailRender,
        }
    }

    /// The sleep generations to put back, in the order they were queued.
    ///
    /// The whole queue is drained before any of these go back, so returning
    /// them to the tail restores their original order rather than reversing it —
    /// and nothing can have overtaken them, because nothing here awaits.
    pub fn held_sleeps(&self) -> impl Iterator<Item = u32> + '_ {
        self.held[..self.held_len].iter().copied()
    }
}

/// What the book-open transaction wants next.
///
/// The order is the design: the departing book's position is written while its
/// catalog entry is still the active one, the book is opened, and only then does
/// the global pointer move — to the position the open actually landed on. Every
/// step that touches the card is a variant here, so the sequence a card sees is
/// a value a test can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenAction {
    /// Write the departing book's position to that book's own file. Must happen
    /// before the catalog slot swaps to the incoming book, or the key this
    /// write needs can no longer be resolved. Report through
    /// [`OpenSequence::departing_stored`].
    CloseOutDeparting(PersistedAppState),
    /// The close-out failed, so the open was never attempted. Announce the
    /// refusal: the app has already left the book that owns that page and will
    /// never reissue it, so a silent return would strand the reader.
    Refuse { book_id: u32 },
    /// Read this book's catalog record into the active-entry slot and adopt the
    /// command's layout. Report through [`OpenSequence::staged`].
    StageBook {
        index: u16,
        type_settings: TypeSettings,
        portrait: bool,
    },
    /// Read this book's own saved position. Report through
    /// [`OpenSequence::saved_position`].
    LoadSavedPosition { index: u16 },
    /// Make `page` resident — from the loaded section window if it covers it,
    /// from the cache or a rebuild otherwise. Report through
    /// [`OpenSequence::section_loaded`].
    LoadSection { index: u16, chapter: u16, page: u16 },
    /// Point the global state file at the book now open. Report through
    /// [`OpenSequence::pointer_stored`].
    StorePointer(PersistedAppState),
    /// Announce the open as one event, carrying the landing position when the
    /// open resolved one. Report through [`OpenSequence::announced`].
    Announce { book_id: u32, position: Option<u32> },
    /// Nothing left to do.
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenPhase {
    CloseOut(PersistedAppState),
    Stage,
    LoadSaved,
    LoadSection,
    StorePointer,
    Announce,
    Refuse,
    Done,
}

/// A book-open (or section-extend) transaction, as the ordered card work it
/// owes.
///
/// The policy is strict on purpose, and the strictness is what removes the need
/// to queue partially finished switches: the reader either completes the move or
/// stays wholly on the book it started from. There is never a half-applied
/// switch for a later command to reconcile, which is why the pending-write state
/// can stay a single latest-value slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenSequence {
    phase: OpenPhase,
    book_id: u32,
    index: u16,
    chapter: u16,
    page: u16,
    type_settings: TypeSettings,
    portrait: bool,
    /// Set only for an open that changes books; also the base record the global
    /// pointer is built from, so device-wide reader settings carry across the
    /// switch instead of resetting to defaults.
    previous: Option<PersistedAppState>,
    /// A bare selection (chapter 0, page 0) of a book, which resumes from that
    /// book's own saved position. Extends never resume.
    resumable: bool,
    resumed: bool,
}

impl OpenSequence {
    /// Begins the transaction `command` describes, or returns `None` when the
    /// request lost to a newer one — a stale open has touched nothing, so there
    /// is no sequence to run and nothing to undo.
    pub fn begin(command: &StorageCommand, latest_request_id: u32) -> Option<Self> {
        let (request_id, book_id, index, chapter, target_pages, type_settings, portrait, previous) =
            match *command {
                StorageCommand::OpenBook {
                    request_id,
                    book_id,
                    index,
                    chapter,
                    target_pages,
                    type_settings,
                    portrait,
                    previous,
                } => (
                    request_id,
                    book_id,
                    index,
                    chapter,
                    target_pages,
                    type_settings,
                    portrait,
                    previous,
                ),
                // An extend stays inside the book already loaded and owes
                // nothing to any other, so it carries no departing state and
                // never resumes.
                StorageCommand::ExtendSection {
                    request_id,
                    book_id,
                    index,
                    chapter,
                    target_pages,
                    type_settings,
                    portrait,
                } => (
                    request_id,
                    book_id,
                    index,
                    chapter,
                    target_pages,
                    type_settings,
                    portrait,
                    None,
                ),
                _ => return None,
            };
        if request_id != latest_request_id {
            return None;
        }
        let resumable =
            matches!(command, StorageCommand::OpenBook { .. }) && chapter == 0 && target_pages == 0;
        Some(Self {
            phase: match previous {
                Some(previous) => OpenPhase::CloseOut(previous),
                None => OpenPhase::Stage,
            },
            book_id,
            index,
            chapter,
            page: target_pages,
            type_settings,
            portrait,
            previous,
            resumable,
            resumed: false,
        })
    }

    pub const fn next(&self) -> OpenAction {
        match self.phase {
            OpenPhase::CloseOut(previous) => OpenAction::CloseOutDeparting(previous),
            OpenPhase::Stage => OpenAction::StageBook {
                index: self.index,
                type_settings: self.type_settings,
                portrait: self.portrait,
            },
            OpenPhase::LoadSaved => OpenAction::LoadSavedPosition { index: self.index },
            OpenPhase::LoadSection => OpenAction::LoadSection {
                index: self.index,
                chapter: self.chapter,
                page: self.page,
            },
            OpenPhase::StorePointer => OpenAction::StorePointer(self.pointer_record()),
            OpenPhase::Announce => OpenAction::Announce {
                book_id: self.book_id,
                position: self.position(),
            },
            OpenPhase::Refuse => OpenAction::Refuse {
                book_id: self.book_id,
            },
            OpenPhase::Done => OpenAction::Done,
        }
    }

    /// The global state record for the book now open: this book, at the position
    /// the open landed on, over the settings the app was already carrying.
    ///
    /// Building it from defaults instead would quietly reset the reader's font
    /// and orientation on every book change, so the departing state is the base
    /// and only the three fields the open actually moved are replaced.
    const fn pointer_record(&self) -> PersistedAppState {
        let base = match self.previous {
            Some(previous) => previous,
            // Unreachable: the pointer only moves for an open that changes
            // books, which is the only case that carries departing state.
            None => PersistedAppState {
                book_id: self.book_id,
                chapter: self.chapter,
                screen: 0,
                shell_orientation: 0,
                reading_orientation: 0,
                refresh_policy: 0,
                font_size: 0,
                line_spacing: 0,
                font_weight: 0,
                font_family: 0,
                front_buttons: 0,
                source_hash: 0,
                source_size: 0,
            },
        };
        PersistedAppState {
            book_id: self.book_id,
            chapter: self.chapter,
            screen: match self.position() {
                Some(page) => page,
                // A book opened at its start is still the active book. The
                // pointer moves at page zero exactly as it does anywhere else.
                None => 0,
            },
            ..base
        }
    }

    /// The page the open resolved, when it resolved one. `None` leaves the app's
    /// own page standing, which is what an explicit page request or an extend
    /// wants.
    const fn position(&self) -> Option<u32> {
        if self.resumed {
            Some(self.page as u32)
        } else {
            None
        }
    }

    /// Whether the departing book's position reached its own file.
    pub fn departing_stored(&mut self, stored: bool) {
        self.phase = if stored {
            OpenPhase::Stage
        } else {
            OpenPhase::Refuse
        };
    }

    /// The catalog entry is staged and the layout adopted.
    pub fn staged(&mut self) {
        self.phase = if self.resumable {
            OpenPhase::LoadSaved
        } else {
            OpenPhase::LoadSection
        };
    }

    /// The book's own saved position, if it had a usable one.
    pub fn saved_position(&mut self, position: Option<(u16, u32)>) {
        if let Some((chapter, screen)) = position {
            // A saved start-of-book is indistinguishable from no saved position
            // and needs no resume: the request already targets chapter 0 page 0.
            if chapter > 0 || screen > 0 {
                self.chapter = chapter;
                self.page = screen.min(u16::MAX as u32) as u16;
                self.resumed = true;
            }
        }
        self.phase = OpenPhase::LoadSection;
    }

    /// The section covering the target page is resident.
    pub fn section_loaded(&mut self) {
        self.phase = if self.previous.is_some() {
            OpenPhase::StorePointer
        } else {
            OpenPhase::Announce
        };
    }

    /// Whether the global pointer reached the card.
    ///
    /// A failed pointer write is recoverable and deliberately not fatal: the
    /// book is open and readable, and only a reboot before the retry would
    /// return to the previous one. So the record is left owed and the open is
    /// announced regardless.
    pub fn pointer_stored(&mut self, _stored: bool) {
        self.phase = OpenPhase::Announce;
    }

    /// The `Loaded` event went out.
    pub fn announced(&mut self) {
        self.phase = OpenPhase::Done;
    }

    /// The refusal event went out.
    pub fn refused(&mut self) {
        self.phase = OpenPhase::Done;
    }

    /// The page this transaction is targeting, after any resume.
    pub const fn target_page(&self) -> u16 {
        self.page
    }

    /// The chapter this transaction is targeting, after any resume.
    pub const fn target_chapter(&self) -> u16 {
        self.chapter
    }

    pub const fn resumed(&self) -> bool {
        self.resumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        book_open_outcome, AppView, BookOpenOutcome, DisplayOrientation, FrontButtons,
        ReaderSource, RefreshPolicy, RenderKind, RenderRequest, SyncStatus,
    };
    use display::font::{FontFamily, FontSize, FontWeight, LineSpacing};

    const SETTINGS: TypeSettings = TypeSettings {
        size: FontSize::Medium,
        spacing: LineSpacing::Normal,
        weight: FontWeight::Normal,
        family: FontFamily::Literata,
    };

    fn persisted(book_id: u32, chapter: u16, screen: u32) -> PersistedAppState {
        PersistedAppState {
            book_id,
            chapter,
            screen,
            shell_orientation: 1,
            reading_orientation: 2,
            refresh_policy: 3,
            font_size: 4,
            line_spacing: 5,
            font_weight: 6,
            font_family: 7,
            front_buttons: 8,
            source_hash: 0xabcd_0000 | book_id,
            source_size: 4096 + book_id,
        }
    }

    fn open(
        book_id: u32,
        chapter: u16,
        page: u16,
        previous: Option<PersistedAppState>,
    ) -> StorageCommand {
        StorageCommand::OpenBook {
            request_id: 7,
            book_id,
            index: ReaderSource::from_book_id(book_id).sd_index().unwrap(),
            chapter,
            target_pages: page,
            type_settings: SETTINGS,
            portrait: false,
            previous,
        }
    }

    fn extend(book_id: u32, chapter: u16, page: u16) -> StorageCommand {
        StorageCommand::ExtendSection {
            request_id: 7,
            book_id,
            index: ReaderSource::from_book_id(book_id).sd_index().unwrap(),
            chapter,
            target_pages: page,
            type_settings: SETTINGS,
            portrait: false,
        }
    }

    /// A card the sequences can be driven against: one position file per book
    /// plus the single global state slot, with any individual write refusable.
    ///
    /// Modelling the two as separate storage is the point. The whole reason a
    /// book's position lives in its own file is that the global slot names one
    /// book at a time, so a stale global record could hand one book's page to
    /// another; a model that collapsed them could not show that.
    #[derive(Clone, Copy, Debug, Default)]
    struct Card {
        positions: [Option<PersistedAppState>; 8],
        global: Option<PersistedAppState>,
        refuse_position_write: bool,
        refuse_global_write: bool,
    }

    impl Card {
        fn slot(book_id: u32) -> usize {
            ReaderSource::from_book_id(book_id).sd_index().unwrap_or(0) as usize % 8
        }

        fn store_position(&mut self, record: PersistedAppState) -> bool {
            if self.refuse_position_write {
                return false;
            }
            self.positions[Self::slot(record.book_id)] = Some(record);
            true
        }

        fn store_global(&mut self, record: PersistedAppState) -> bool {
            if self.refuse_global_write {
                return false;
            }
            self.global = Some(record);
            true
        }

        fn position_of(&self, book_id: u32) -> Option<(u16, u32)> {
            self.positions[Self::slot(book_id)].map(|record| (record.chapter, record.screen))
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Step {
        CloseOut(u32),
        Stage(u16),
        LoadSaved(u16),
        LoadSection { chapter: u16, page: u16 },
        StorePointer { book_id: u32, screen: u32 },
        Announce { book_id: u32, position: Option<u32> },
        Refuse(u32),
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct Trace {
        steps: [Option<Step>; 12],
        len: usize,
    }

    impl Trace {
        /// The bound is also the termination guard. Every action but `Done`
        /// pushes exactly one step, so a phase that failed to advance shows up
        /// here as an overflow rather than as a firmware hang — which is what a
        /// missing advance would be on device, inside the loop that owns the
        /// card.
        fn push(&mut self, step: Step) {
            assert!(self.len < self.steps.len(), "trace overflow: {:?}", step);
            self.steps[self.len] = Some(step);
            self.len += 1;
        }

        fn steps(&self) -> impl Iterator<Item = Step> + '_ {
            self.steps[..self.len].iter().filter_map(|step| *step)
        }

        fn contains(&self, step: Step) -> bool {
            self.steps().any(|seen| seen == step)
        }

        fn position_of(&self, wanted: impl Fn(Step) -> bool) -> Option<usize> {
            self.steps().position(wanted)
        }
    }

    /// Runs a whole book-open transaction against the card model, exactly as the
    /// display task drives it. `ram_hit` stands in for the loaded section window
    /// already covering the target page: it changes how the firmware makes the
    /// page resident, not the transaction around it, so the model just accepts.
    fn run_open(card: &mut Card, command: &StorageCommand, latest_request_id: u32) -> Trace {
        let mut trace = Trace::default();
        let Some(mut sequence) = OpenSequence::begin(command, latest_request_id) else {
            return trace;
        };
        loop {
            match sequence.next() {
                OpenAction::CloseOutDeparting(previous) => {
                    trace.push(Step::CloseOut(previous.book_id));
                    let stored = card.store_position(previous);
                    sequence.departing_stored(stored);
                }
                OpenAction::Refuse { book_id } => {
                    trace.push(Step::Refuse(book_id));
                    sequence.refused();
                }
                OpenAction::StageBook { index, .. } => {
                    trace.push(Step::Stage(index));
                    sequence.staged();
                }
                OpenAction::LoadSavedPosition { index } => {
                    trace.push(Step::LoadSaved(index));
                    let book_id = ReaderSource::sd(index).book_id();
                    sequence.saved_position(card.position_of(book_id));
                }
                OpenAction::LoadSection { chapter, page, .. } => {
                    trace.push(Step::LoadSection { chapter, page });
                    sequence.section_loaded();
                }
                OpenAction::StorePointer(record) => {
                    trace.push(Step::StorePointer {
                        book_id: record.book_id,
                        screen: record.screen,
                    });
                    let stored = card.store_global(record);
                    sequence.pointer_stored(stored);
                }
                OpenAction::Announce { book_id, position } => {
                    trace.push(Step::Announce { book_id, position });
                    sequence.announced();
                }
                OpenAction::Done => return trace,
            }
        }
    }

    #[test]
    fn a_stale_open_touches_nothing() {
        let mut card = Card::default();
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 9);
        assert_eq!(trace.len, 0);
        assert!(card.global.is_none());
        assert!(card.positions.iter().all(Option::is_none));
    }

    #[test]
    fn a_reopen_of_the_active_book_never_moves_the_pointer() {
        let mut card = Card::default();
        let trace = run_open(&mut card, &open(2, 3, 40, None), 7);
        assert!(!trace.contains(Step::CloseOut(2)));
        assert!(trace
            .position_of(|step| matches!(step, Step::StorePointer { .. }))
            .is_none());
        assert!(card.global.is_none());
    }

    // Invariant: switching away preserves the previous book's position.
    #[test]
    fn switching_away_writes_the_departing_book_before_the_slot_moves() {
        let mut card = Card::default();
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 7);
        let close_out = trace
            .position_of(|step| step == Step::CloseOut(2))
            .expect("the departing book is closed out");
        let stage = trace
            .position_of(|step| matches!(step, Step::Stage(_)))
            .expect("the incoming book is staged");
        assert!(
            close_out < stage,
            "the departing write needs its own catalog slot: {:?}",
            trace
        );
        assert_eq!(card.position_of(2), Some((4, 90)));
    }

    // Invariant: a newly opened book becomes active even at page zero.
    #[test]
    fn opening_at_page_zero_still_moves_the_pointer() {
        let mut card = Card::default();
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 7);
        assert!(trace.contains(Step::StorePointer {
            book_id: 3,
            screen: 0
        }));
        let global = card
            .global
            .expect("the pointer names the newly opened book");
        assert_eq!(global.book_id, 3);
        assert_eq!(global.screen, 0);
    }

    #[test]
    fn moving_the_pointer_carries_the_readers_settings_across_the_switch() {
        let mut card = Card::default();
        let departing = persisted(2, 4, 90);
        run_open(&mut card, &open(3, 0, 0, Some(departing)), 7);
        let global = card.global.expect("the pointer moved");
        // Only the book and where it landed change; the device-wide reader
        // settings are the ones the app was already carrying.
        assert_eq!(
            global,
            PersistedAppState {
                book_id: 3,
                chapter: 0,
                screen: 0,
                ..departing
            }
        );
    }

    #[test]
    fn a_bare_selection_resumes_from_the_books_own_position() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 11, 250)));
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 7);
        assert!(trace.contains(Step::LoadSection {
            chapter: 11,
            page: 250
        }));
        assert!(trace.contains(Step::Announce {
            book_id: 3,
            position: Some(250)
        }));
        assert_eq!(
            card.global.map(|record| (record.chapter, record.screen)),
            Some((11, 250))
        );
    }

    #[test]
    fn an_explicit_page_request_is_not_overridden_by_the_saved_position() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 11, 250)));
        let trace = run_open(&mut card, &open(3, 2, 30, Some(persisted(2, 4, 90))), 7);
        assert!(!trace.contains(Step::LoadSaved(1)));
        assert!(trace.contains(Step::LoadSection {
            chapter: 2,
            page: 30
        }));
        // The app asked for this page, so it already has it; the event must not
        // hand it back as a landing position and re-render.
        assert!(trace.contains(Step::Announce {
            book_id: 3,
            position: None
        }));
    }

    #[test]
    fn a_saved_start_of_book_is_not_treated_as_a_resume() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 0, 0)));
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 7);
        assert!(trace.contains(Step::Announce {
            book_id: 3,
            position: None
        }));
    }

    // Invariant: opening produces one final render at the restored page.
    #[test]
    fn an_open_announces_exactly_once() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 11, 250)));
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 7);
        let announcements = trace
            .steps()
            .filter(|step| matches!(step, Step::Announce { .. }))
            .count();
        assert_eq!(announcements, 1, "{:?}", trace);
    }

    // Invariant: interrupted writes never substitute one book's position for
    // another's.
    #[test]
    fn a_refused_departing_write_abandons_the_whole_switch() {
        let mut card = Card {
            refuse_position_write: true,
            ..Card::default()
        };
        assert!(card.store_global(persisted(2, 4, 90)));
        card.refuse_global_write = false;

        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 91))), 7);

        assert!(trace.contains(Step::Refuse(3)));
        assert!(
            trace
                .position_of(|step| matches!(step, Step::Stage(_)))
                .is_none(),
            "nothing may be opened once the departing page is lost: {:?}",
            trace
        );
        assert!(trace
            .position_of(|step| matches!(step, Step::Announce { .. }))
            .is_none());
        // The global slot still names the book the reader is actually in.
        assert_eq!(card.global.map(|record| record.book_id), Some(2));
        assert_eq!(
            book_open_outcome(false, false),
            BookOpenOutcome::KeptBookPositionUnwritten
        );
    }

    #[test]
    fn a_refused_pointer_write_still_opens_and_announces_the_book() {
        let mut card = Card {
            refuse_global_write: true,
            ..Card::default()
        };
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 7);
        assert!(trace.contains(Step::Announce {
            book_id: 3,
            position: None
        }));
        // The departing page is on the card either way, which is what makes the
        // failure recoverable rather than lossy.
        assert_eq!(card.position_of(2), Some((4, 90)));
        assert!(card.global.is_none());
        assert_eq!(
            book_open_outcome(true, false),
            BookOpenOutcome::OpenedPointerOwed
        );
    }

    #[test]
    fn an_extend_owes_nothing_to_any_other_book() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 11, 250)));
        let trace = run_open(&mut card, &extend(3, 2, 60), 7);
        assert!(trace
            .position_of(|step| matches!(step, Step::CloseOut(_)))
            .is_none());
        assert!(trace
            .position_of(|step| matches!(step, Step::LoadSaved(_)))
            .is_none());
        assert!(trace
            .position_of(|step| matches!(step, Step::StorePointer { .. }))
            .is_none());
        assert!(trace.contains(Step::LoadSection {
            chapter: 2,
            page: 60
        }));
        assert!(card.global.is_none());
    }

    #[test]
    fn an_extend_at_chapter_zero_page_zero_does_not_resume() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 11, 250)));
        let trace = run_open(&mut card, &extend(3, 0, 0), 7);
        assert!(trace.contains(Step::LoadSection {
            chapter: 0,
            page: 0
        }));
    }

    #[test]
    fn a_deep_saved_position_is_clamped_to_the_page_field() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 11, u32::MAX)));
        let mut sequence = OpenSequence::begin(&open(3, 0, 0, None), 7).expect("fresh request");
        sequence.staged();
        sequence.saved_position(card.position_of(3));
        assert_eq!(sequence.target_page(), u16::MAX);
        assert_eq!(sequence.target_chapter(), 11);
        assert!(sequence.resumed());
    }

    // The storage queue the pre-sleep drain works against.
    #[derive(Clone, Copy, Debug, Default)]
    struct Queue {
        slots: [Option<StorageCommand>; 4],
        head: usize,
        len: usize,
    }

    impl Queue {
        const CAPACITY: usize = 4;

        fn push(&mut self, command: StorageCommand) -> bool {
            if self.len == Self::CAPACITY {
                return false;
            }
            self.slots[(self.head + self.len) % Self::CAPACITY] = Some(command);
            self.len += 1;
            true
        }

        fn pop(&mut self) -> Option<StorageCommand> {
            let command = self.slots[self.head].take()?;
            self.head = (self.head + 1) % Self::CAPACITY;
            self.len -= 1;
            Some(command)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum SleepOutcome {
        Slept,
        Refused(SleepRefusal),
    }

    fn run_sleep(queue: &mut Queue, progress_lands: bool) -> (SleepOutcome, usize) {
        run_sleep_with(queue, progress_lands, None)
    }

    /// Drives a whole sleep transition the way the display task does, returning
    /// what the panel did and how many queued commands were applied on the way.
    ///
    /// `refill` stands in for a producer enqueuing one more command in the window
    /// between the drain taking a command off the queue and putting it back.
    /// That refill is the *only* thing that can make the put-back fail, so it is
    /// the only honest way to reach the lost-upload path: forcing the failure
    /// without it would model a queue that cannot occur — one short of full —
    /// and would validate accounting rather than behaviour.
    fn run_sleep_with(
        queue: &mut Queue,
        progress_lands: bool,
        mut refill: Option<StorageCommand>,
    ) -> (SleepOutcome, usize) {
        let mut sequence = SleepSequence::new(Queue::CAPACITY);
        let mut applied = 0;
        // A phase that fails to advance would spin here exactly as it would in
        // the task; fail the test instead of hanging it.
        for _ in 0..Queue::CAPACITY * 4 {
            match sequence.next() {
                SleepAction::TakeQueued => match queue.pop() {
                    None => sequence.queue_empty(),
                    Some(command) => match sequence.drained(&command) {
                        Drained::Apply => {
                            applied += 1;
                            sequence.applied();
                        }
                        Drained::RequeueAndRefuse => {
                            if let Some(refill) = refill.take() {
                                assert!(queue.push(refill), "the producer wins the freed slot");
                            }
                            let restored = queue.push(command);
                            sequence.requeued(restored);
                        }
                    },
                },
                SleepAction::FlushProgress => sequence.flushed(progress_lands),
                SleepAction::Refuse(refusal) => return (SleepOutcome::Refused(refusal), applied),
                SleepAction::Proceed => return (SleepOutcome::Slept, applied),
            }
        }
        panic!("the sleep sequence never resolved");
    }

    #[test]
    fn an_empty_queue_sleeps_straight_away() {
        let mut queue = Queue::default();
        assert_eq!(run_sleep(&mut queue, true), (SleepOutcome::Slept, 0));
    }

    #[test]
    fn queued_work_is_applied_before_the_panel_goes_down() {
        let mut queue = Queue::default();
        assert!(queue.push(StorageCommand::StoreProgress(persisted(3, 1, 10))));
        assert!(queue.push(open(4, 0, 0, Some(persisted(3, 1, 10)))));
        let (outcome, applied) = run_sleep(&mut queue, true);
        assert_eq!(outcome, SleepOutcome::Slept);
        assert_eq!(
            applied, 2,
            "deep sleep is terminal; nothing may be left over"
        );
        assert_eq!(queue.len, 0);
    }

    #[test]
    fn a_full_queue_is_drained_within_its_own_depth() {
        let mut queue = Queue::default();
        for page in 0..Queue::CAPACITY as u32 {
            assert!(queue.push(StorageCommand::StoreProgress(persisted(3, 1, page))));
        }
        let (outcome, applied) = run_sleep(&mut queue, true);
        assert_eq!(outcome, SleepOutcome::Slept);
        assert_eq!(applied, Queue::CAPACITY);
        assert_eq!(queue.len, 0);
    }

    #[test]
    fn a_progress_record_that_will_not_land_keeps_the_panel_up() {
        let mut queue = Queue::default();
        assert_eq!(
            run_sleep(&mut queue, false),
            (SleepOutcome::Refused(SleepRefusal::ProgressUnwritten), 0)
        );
    }

    // Regression: the drain must not answer an upload request. Its arm in the
    // storage handler is a no-op, so applying it here would discard the only
    // signal that starts the writer, leaving the browser waiting on a session
    // that never opens with the upload flag still set.
    #[test]
    fn a_queued_upload_is_put_back_and_the_sleep_refused() {
        let mut queue = Queue::default();
        assert!(queue.push(StorageCommand::ReceiveUpload));
        let (outcome, applied) = run_sleep(&mut queue, true);
        assert_eq!(outcome, SleepOutcome::Refused(SleepRefusal::UploadQueued));
        assert_eq!(applied, 0);
        assert_eq!(queue.len, 1, "the request has to survive the refusal");
        assert_eq!(queue.pop(), Some(StorageCommand::ReceiveUpload));
    }

    #[test]
    fn work_queued_ahead_of_an_upload_is_still_applied_before_the_refusal() {
        let mut queue = Queue::default();
        assert!(queue.push(StorageCommand::StoreProgress(persisted(3, 1, 10))));
        assert!(queue.push(StorageCommand::ReceiveUpload));
        let (outcome, applied) = run_sleep(&mut queue, true);
        assert_eq!(outcome, SleepOutcome::Refused(SleepRefusal::UploadQueued));
        assert_eq!(applied, 1);
        assert_eq!(queue.pop(), Some(StorageCommand::ReceiveUpload));
    }

    /// A full queue at the moment of a refill, with the upload at its head.
    fn queue_refilled_behind_an_upload() -> (Queue, StorageCommand) {
        let mut queue = Queue::default();
        assert!(queue.push(StorageCommand::ReceiveUpload));
        for page in 0..Queue::CAPACITY as u32 - 1 {
            assert!(queue.push(StorageCommand::StoreProgress(persisted(3, 1, page))));
        }
        // What the producer slips into the slot the upload vacates. An open is
        // the costly thing to lose this way: it carries the departing book's
        // only close-out position and nothing reissues it.
        (queue, open(4, 0, 0, Some(persisted(3, 1, 40))))
    }

    // Regression: an upload that cannot go back means a producer refilled the
    // queue behind it, so the channel holds a whole budget's worth of accepted
    // work. Draining on would spend the remaining budget on a queue that grew,
    // and hand what it could not reach to a terminal sleep.
    #[test]
    fn an_upload_that_cannot_be_put_back_leaves_the_refilled_queue_intact() {
        let (mut queue, refill) = queue_refilled_behind_an_upload();
        let (outcome, applied) = run_sleep_with(&mut queue, true, Some(refill));
        assert_eq!(outcome, SleepOutcome::Refused(SleepRefusal::UploadLost));
        assert_eq!(applied, 0);
        assert_eq!(
            queue.len,
            Queue::CAPACITY,
            "every accepted command must survive the refusal"
        );
    }

    #[test]
    fn the_retry_after_a_lost_upload_drains_everything_and_sleeps() {
        let (mut queue, refill) = queue_refilled_behind_an_upload();
        assert_eq!(
            run_sleep_with(&mut queue, true, Some(refill)).0,
            SleepOutcome::Refused(SleepRefusal::UploadLost)
        );
        // The power task's idle clock re-requests sleep. No upload is queued
        // this time — it is the thing that was lost — so nothing defers the
        // drain and the whole backlog lands before the panel goes down.
        let (outcome, applied) = run_sleep(&mut queue, true);
        assert_eq!(outcome, SleepOutcome::Slept);
        assert_eq!(applied, Queue::CAPACITY);
        assert_eq!(queue.len, 0);
    }

    #[test]
    fn a_requeued_upload_is_answered_by_the_loop_on_the_next_pass() {
        let mut queue = Queue::default();
        assert!(queue.push(StorageCommand::ReceiveUpload));
        assert_eq!(
            run_sleep(&mut queue, true),
            (SleepOutcome::Refused(SleepRefusal::UploadQueued), 0)
        );
        // The loop takes it next, and the upload arm — not the handler — is
        // what answers it.
        let command = queue.pop().expect("still queued");
        assert_eq!(
            loop_arm(&command, SyncSession::Loaned),
            LoopArm::UploadSession
        );
    }

    #[test]
    fn an_upload_outside_a_session_is_refused_rather_than_entered() {
        assert_eq!(
            loop_arm(&StorageCommand::ReceiveUpload, SyncSession::Idle),
            LoopArm::RefusedUpload
        );
    }

    #[test]
    fn every_other_command_goes_to_the_storage_handler() {
        for command in [
            StorageCommand::LoadCatalogCache,
            StorageCommand::RefreshCatalog,
            StorageCommand::StoreProgress(persisted(3, 1, 10)),
            open(3, 0, 0, None),
            extend(3, 1, 20),
            StorageCommand::ForgetWifiCredentials,
        ] {
            assert_eq!(loop_arm(&command, SyncSession::Idle), LoopArm::Apply);
            assert_eq!(loop_arm(&command, SyncSession::Loaned), LoopArm::Apply);
        }
    }

    /// The display queue the panel-down drain works against, and what came back
    /// out of it.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    struct PanelDownResult {
        failed_renders: usize,
        left_queued: [Option<u32>; MAX_HELD_SLEEPS],
        left_len: usize,
        renders_left: usize,
    }

    /// Drives the panel-down drain the way the display task does: take until the
    /// budget or the queue runs out, answer renders, then hand the held sleeps
    /// back.
    fn run_panel_down(queued: &[DisplayCommand]) -> PanelDownResult {
        let mut inbox = queued.iter().copied();
        let mut drain = PanelDownDrain::new(MAX_HELD_SLEEPS);
        let mut result = PanelDownResult::default();
        while drain.wants_more() {
            let Some(command) = inbox.next() else { break };
            match drain.took(&command) {
                PanelDown::FailRender => result.failed_renders += 1,
                PanelDown::HoldSleep => {}
            }
        }
        for generation in drain.held_sleeps() {
            result.left_queued[result.left_len] = Some(generation);
            result.left_len += 1;
        }
        // Whatever the budget did not reach stays in the channel for the loop.
        result.renders_left = inbox
            .filter(|command| matches!(command, DisplayCommand::Render(_)))
            .count();
        result
    }

    fn render_command() -> DisplayCommand {
        DisplayCommand::Render(RenderRequest {
            kind: RenderKind::Page,
            view: AppView::Reading,
            page: 12,
            page_count: 400,
            chapter: 2,
            selection: 0,
            book_id: 3,
            orientation: DisplayOrientation::LandscapeButtonsBottom,
            front_buttons: FrontButtons::PagesRight,
            reading_sheet: false,
            refresh_policy: RefreshPolicy::FullOnWake,
            font_size: FontSize::Medium,
            line_spacing: LineSpacing::Normal,
            font_weight: FontWeight::Normal,
            font_family: FontFamily::Literata,
            last_button: None,
            aux_raw: 0,
            nav_raw: 0,
            page_raw: 0,
            battery_mv: 0,
            battery_percent: 100,
            library_count: 4,
            sync_status: SyncStatus::NotConfigured,
            wifi_ssid: [0; 32],
            wifi_ssid_len: 0,
            dirty: display::Rect::FULL,
        })
    }

    // Regression: applying a drained book open emits Loaded, the app repaints on
    // it, and that render lands while the sleep frame is still being flushed.
    // Returning to the loop with it queued repaints a page over the sleep image.
    #[test]
    fn a_render_queued_behind_the_sleep_is_answered_not_left_to_repaint() {
        let result = run_panel_down(&[render_command()]);
        assert_eq!(result.failed_renders, 1);
        assert_eq!(result.renders_left, 0);
    }

    // Answered, not discarded: a dropped render never acknowledges, so a sleep
    // abandoned on a button press would leave the app's render lock held and
    // every later input queued behind an acknowledgement that never comes.
    #[test]
    fn every_drained_render_gets_an_acknowledgement() {
        let result = run_panel_down(&[render_command(), render_command(), render_command()]);
        assert_eq!(result.failed_renders, 3);
        assert_eq!(result.renders_left, 0);
    }

    // The power task waits for a matching acknowledgement for every Sleep it
    // sends, and only the sleep arm produces one, so the drain must not eat one.
    #[test]
    fn a_queued_sleep_survives_the_drain() {
        let result = run_panel_down(&[
            render_command(),
            DisplayCommand::Sleep { generation: 9 },
            render_command(),
        ]);
        assert_eq!(result.failed_renders, 2);
        assert_eq!(result.left_len, 1);
        assert_eq!(result.left_queued[0], Some(9));
    }

    #[test]
    fn held_sleeps_go_back_in_the_order_they_were_queued() {
        let result = run_panel_down(&[
            DisplayCommand::Sleep { generation: 7 },
            render_command(),
            DisplayCommand::Sleep { generation: 8 },
        ]);
        assert_eq!(result.failed_renders, 1);
        assert_eq!(result.left_len, 2);
        assert_eq!(result.left_queued[0], Some(7));
        assert_eq!(result.left_queued[1], Some(8));
    }

    #[test]
    fn a_queue_of_nothing_but_sleeps_is_handed_back_whole() {
        let sleeps: [DisplayCommand; MAX_HELD_SLEEPS] =
            core::array::from_fn(|slot| DisplayCommand::Sleep {
                generation: slot as u32,
            });
        let result = run_panel_down(&sleeps);
        assert_eq!(result.failed_renders, 0);
        assert_eq!(result.left_len, MAX_HELD_SLEEPS);
        for slot in 0..MAX_HELD_SLEEPS {
            assert_eq!(result.left_queued[slot], Some(slot as u32));
        }
    }

    #[test]
    fn the_drain_stops_at_the_channels_depth() {
        let mut queued = [render_command(); MAX_HELD_SLEEPS + 2];
        queued[MAX_HELD_SLEEPS] = render_command();
        let result = run_panel_down(&queued);
        assert_eq!(result.failed_renders, MAX_HELD_SLEEPS);
        // Bounded like the storage drain: it catches up on what was accepted,
        // it does not chase a producer. Anything past the depth is the loop's.
        assert_eq!(result.renders_left, 2);
    }
}
