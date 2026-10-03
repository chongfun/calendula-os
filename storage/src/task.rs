//! The storage task: the app's storage commands, the background walk, and
//! the state between them.
//!
//! Everything it needs from the firmware beyond the card goes through
//! [`Host`]: event channels, the scratch in statics, the newest reader
//! request, and the sync memory loan.

use crate::book_build::{self, ReaderCacheScratch};
use crate::card::Card;
use app_core::storage_loop::{OpenAction, OpenSequence};
use app_core::{
    book_open_outcome, AppView, LibraryEvent, PersistedAppState, ReaderSource, RenderRequest,
    StorageCommand, SyncSession, WifiSsid,
};
use embassy_time::Instant;
use proto::nvm::AppStateRecord;
use reader_cache::store::{BookLoadStatus, LibraryScanStatus, ReaderStore};

/// What the storage task needs from the firmware, beyond the card.
pub trait Host {
    /// Deliver an event the app may drop when its queue is full.
    fn send(&mut self, event: &LibraryEvent);
    /// Deliver an event that settles something the app is waiting on.
    fn send_required(&mut self, event: &LibraryEvent);
    /// Deliver the event that ends an open or announces new pages.
    fn send_loaded(&mut self, event: &LibraryEvent);
    /// The newest reader request the app has issued.
    fn latest_reader_request_id(&self) -> u32;
    /// Whether the app is still waiting on the Library pick `request_id`.
    /// Read between the note painted for a pick's rescan and the scan, where
    /// the app has had its turn and nothing lets it run again before the
    /// scan ends.
    fn waiting_on_pick(&self, request_id: u32) -> bool;
    /// Put a command back on the storage queue for a later pass.
    fn requeue(&mut self, command: StorageCommand);
    /// The reader's scratch, built on first use.
    fn ensure_scratch<'s>(
        &mut self,
        slot: &'s mut Option<&'static mut ReaderCacheScratch<'static>>,
    ) -> &'s mut ReaderCacheScratch<'static>;
    /// The card names a saved network.
    fn network_saved(&mut self, ssid: WifiSsid);
    /// Whether a credential write was confirmed by reading it back.
    fn wifi_storage_result(&mut self, confirmed: bool);
    /// Hand the retired scratch to the sync session, with the card's saved
    /// network and catalog listing.
    fn grant_sync_loan(
        &mut self,
        card: &mut impl Card,
        scratch: &'static mut ReaderCacheScratch<'static>,
    );
    /// Tell the sync session its loan was refused.
    fn refuse_sync_loan(&mut self);
}

/// A pick of a book the catalog does not know yet, announced and waiting on
/// its rescan. The scan holds the card, and the bus the panel shares, for
/// about 12 s on a 1,100-book X3 card, so the caller paints the note first
/// and then runs the scan as work its loop owes, behind any frame the reader
/// asked for meanwhile. A sleep refuses the pick instead, through
/// [`StorageTask::abandon_rescan`].
///
/// Empty: the pick itself waits in [`StorageTask`], out of the caller's poll
/// frame. It exists so that dropping it is a compile warning.
#[must_use = "a pick waits on this rescan; run it with StorageTask::rescan or refuse it with abandon_rescan"]
pub struct OwedRescan(());

/// The pick an [`OwedRescan`] stands for.
pub struct PendingRescan {
    request_id: u32,
    at: proto::library_path::BookRoot,
    locator: proto::library_path::LibraryPath,
    size: u32,
}

/// The storage task's own state, between commands and background slices.
#[derive(Default)]
pub struct StorageTask {
    pub epub_scratch: Option<&'static mut ReaderCacheScratch<'static>>,
    pub pending_progress: Option<AppStateRecord>,
    pub last_progress_write: Option<Instant>,
    pub state_restored: StateRestore,
    pub background_build: Option<BackgroundBuild>,
    pub pending_place: Option<PendingPlace>,
    pub evidence_settled: Option<book_build::EvidencePlace>,
    pub pending_evidence: Option<book_build::SourceEvidenceJob>,
    pub catalog_refresh: CatalogRefresh,
    pending_rescan: Option<PendingRescan>,
}

/// A catalog refresh that waited on a refused position write. A background
/// slice requeues it after the slice's backoff, so a card that keeps refusing
/// is retried on a timer rather than straight away. Once the retry gets past
/// the write it is an ordinary refresh: a scan that then fails is not retried,
/// as no refresh's is. Any other scan that lands settles it first.
#[derive(Clone, Copy, Debug, Default)]
pub struct CatalogRefresh {
    pub owed: bool,
    pub refusals: u8,
}

impl StorageTask {
    /// Run one storage command.
    ///
    /// Every entry point here is out of line, so the arms' multi-KB scratch
    /// stays out of the task loop's poll frame.
    ///
    /// A pick that needs a rescan comes back unfinished, so the caller can
    /// paint before the scan holds the card. Pass it to [`Self::rescan`], or
    /// to [`Self::abandon_rescan`] when the panel is about to sleep. Finish or
    /// abandon that pick before handling another storage command.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    #[must_use = "a pick waits on this rescan; run it with StorageTask::rescan or refuse it with abandon_rescan"]
    pub fn handle(
        &mut self,
        command: StorageCommand,
        card: &mut impl Card,
        host: &mut impl Host,
        sd_library: &mut ReaderStore,
        font_metrics: &mut crate::custom_font::MetricCache,
        sync_session: &mut SyncSession,
        portrait: bool,
    ) -> Option<OwedRescan> {
        handle_storage_command(
            command,
            card,
            host,
            sd_library,
            font_metrics,
            &mut self.epub_scratch,
            sync_session,
            &mut self.pending_progress,
            &mut self.last_progress_write,
            &mut self.state_restored,
            &mut self.background_build,
            &mut self.pending_place,
            &mut self.catalog_refresh,
            portrait,
            &mut self.pending_rescan,
        );
        self.pending_rescan.as_ref().map(|_| OwedRescan(()))
    }

    /// Finish a pick that [`Self::handle`] left owing a rescan: attempt the
    /// scan, then answer the pick from the available catalog.
    ///
    /// Sends `RowFailed` without scanning if the app no longer waits on the
    /// pick, by [`Host::waiting_on_pick`], or pending progress cannot be saved.
    /// After a scan attempt, sends `Scanned` even if the old catalog was retained.
    /// A missing or unreadable row sends `RowFailed` and relists the root; a found
    /// row sends `RowIsBook` and relists the current folder. `portrait` selects
    /// the listing's layout.
    #[inline(never)]
    pub fn rescan(
        &mut self,
        _owed: OwedRescan,
        card: &mut impl Card,
        host: &mut impl Host,
        sd_library: &mut ReaderStore,
        portrait: bool,
    ) {
        let Some(PendingRescan {
            request_id,
            at,
            locator,
            size,
        }) = self.pending_rescan.take()
        else {
            return;
        };
        // The note's refresh let the app run, and Back is the one press it
        // takes while a pick waits. A pick walked away from gets no scan: the
        // app may since have asked for its own book by the row it holds in
        // this catalog, and a scan would renumber that row under it. Answered
        // anyway, so a wait this host misjudged still ends.
        if !host.waiting_on_pick(request_id) {
            slog!(
                "sd: pick request={} walked away from before its rescan",
                request_id
            );
            host.send_required(&LibraryEvent::RowFailed { request_id });
            return;
        }
        if !scan_books_after_flush(
            card,
            sd_library,
            &mut self.pending_progress,
            &mut self.last_progress_write,
            &mut self.pending_place,
            &mut self.catalog_refresh,
        ) {
            // Nothing moved, and the page is still resident for the next
            // pick to write.
            host.send_required(&LibraryEvent::RowFailed { request_id });
            return;
        }
        restore_saved_state(card, host, sd_library, &mut self.state_restored, true);
        host.send(&LibraryEvent::Scanned {
            count: sd_library.catalog_count_u16(),
            catalog_epoch: sd_library.catalog_epoch(),
        });
        // Find the picked book by its place in the new catalog. The answer
        // carries `Scanned`'s epoch, so it goes after.
        match crate::library_sd::find_index_by_locator(card, at, locator.as_str(), size) {
            crate::library_sd::CatalogRow::Found(index) => {
                host.send_required(&LibraryEvent::RowIsBook {
                    request_id,
                    index,
                    catalog_epoch: sd_library.catalog_epoch(),
                });
                // The scan changed only the catalog, so browsing stays in
                // this folder, on the book picked. Relist it after the answer
                // to replace the catalog total `Scanned` showed.
                relist_library_folder_here(card, host, sd_library, portrait, at, &locator);
            }
            // Still not in the catalog, or the card would not answer: back to
            // the root, as after any rescan.
            crate::library_sd::CatalogRow::Rebuild | crate::library_sd::CatalogRow::Unreadable => {
                relist_library_folder(card, host, sd_library, portrait);
                host.send_required(&LibraryEvent::RowFailed { request_id });
            }
        }
    }

    /// Refuse the pick [`Self::handle`] left owing a rescan, without scanning.
    ///
    /// For a sleep, which is terminal: a 12 s scan now would only hold the
    /// note on the panel ahead of the sleep image. The refusal is required,
    /// so a sleep a late press abandons finds the Library with nothing
    /// waiting. The catalog is untouched, and `handle` already wrote the
    /// position.
    #[inline(never)]
    pub fn abandon_rescan(&mut self, _owed: OwedRescan, host: &mut impl Host) {
        if let Some(PendingRescan { request_id, .. }) = self.pending_rescan.take() {
            slog!("sd: sleeping instead of rescanning for a pick; refusing it");
            host.send_required(&LibraryEvent::RowFailed { request_id });
        }
    }

    /// Put a coalesced position on the card, as a sleep or a loan must first.
    #[inline(never)]
    pub fn flush_pending_progress(
        &mut self,
        card: &mut impl Card,
        sd_library: &mut ReaderStore,
    ) -> bool {
        flush_pending_progress(
            card,
            sd_library,
            &mut self.pending_progress,
            &mut self.last_progress_write,
            &mut self.pending_place,
        )
    }

    /// Whether this task owes itself a background slice: a walk, a waiting
    /// place, or reading the open book's bytes.
    pub fn background_owed(&self, sd_library: &ReaderStore) -> bool {
        self.catalog_refresh.owed
            || self.background_build.is_some()
            || self
                .pending_place
                .as_ref()
                .is_some_and(|waiting| !waiting.stopped)
            || self.pending_evidence.is_some()
            || book_build::evidence_place(sd_library)
                .is_some_and(|place| self.evidence_settled.as_ref() != Some(&place))
    }

    /// How far `book_id`'s background build has got, for the reading footer:
    /// the walk's own count, so it lives and dies with the walk.
    pub fn build_progress(&self, book_id: u32) -> Option<proto::progress::JobProgress> {
        self.background_build
            .filter(|pending| pending.book_id == book_id)
            .and(self.epub_scratch.as_deref())
            .and_then(ReaderCacheScratch::build_progress)
    }

    /// Consecutive refusals the next slice backs off for.
    pub fn background_attempts(&self) -> u8 {
        self.background_build
            .map_or(0, |pending| pending.attempts)
            .max(
                self.pending_place
                    .as_ref()
                    .filter(|waiting| !waiting.stopped)
                    .map_or(0, |waiting| waiting.refusals),
            )
            .max(if self.catalog_refresh.owed {
                self.catalog_refresh.refusals
            } else {
                0
            })
    }

    /// Run one background slice. `last_request` is the last render, which
    /// says which page of which book the reader is on.
    #[inline(never)]
    pub fn background_step(
        &mut self,
        card: &mut impl Card,
        host: &mut impl Host,
        sd_library: &mut ReaderStore,
        font_metrics: &mut crate::custom_font::MetricCache,
        last_request: Option<RenderRequest>,
    ) {
        if self.catalog_refresh.owed {
            self.catalog_refresh.owed = false;
            host.requeue(StorageCommand::RefreshCatalog);
            return;
        }
        let Some(pending) = self.background_build else {
            // A place waiting on a card that refused a read, with no
            // walk to carry it. Its retry rides this slice instead:
            // a complete cache and a finished walk both leave nothing
            // building, and a place kept with nothing coming back for
            // it is a place quietly abandoned.
            if let Some(waiting) = self.pending_place.as_ref() {
                let book_id = waiting.hold.book_id();
                let resolved = resolve_pending_place(
                    card,
                    host,
                    sd_library,
                    &mut self.pending_place,
                    book_id,
                    reader_page_of(last_request, book_id),
                    false,
                    &mut self.epub_scratch,
                    font_metrics,
                    &mut self.background_build,
                );
                match resolved {
                    PlaceOutcome::Moved(target) => {
                        slog!(
                            "restore: the place resolved on a later look, page {}",
                            target
                        );
                        host.send_loaded(&LibraryEvent::Loaded {
                            book_id,
                            pages: sd_library.advertised_page_count(),
                            chapters: sd_library.chapter_count_for_ui(),
                            current_chapter: sd_library.current_chapter(),
                            chapter_pages: reader_cache::store::chapter_pages_for_event(sd_library),
                            position: Some(target),
                            text_replaced: true,
                        });
                        return;
                    }
                    // The book took its own text with it and gave
                    // nothing back. A `Loaded` here would say it has a
                    // page, which is the one thing it does not have.
                    PlaceOutcome::Unreadable => {
                        host.send_loaded(&LibraryEvent::BookOpenUnreadable { book_id });
                        return;
                    }
                    PlaceOutcome::Waiting => {}
                }
            }
            // No walk owed, so the slice goes to reading the open
            // book's bytes, which is the other thing this task owes
            // itself and the one that has to wait for the reader to
            // have a page before it starts.
            let open_place = book_build::evidence_place(sd_library);
            // A job follows the book that is open. The reader
            // moving on takes this one with them, half read: the
            // copy they moved to is the one whose bytes are worth
            // having, and the one they left is read again whenever
            // it is opened again.
            if self
                .pending_evidence
                .as_ref()
                .is_some_and(|job| open_place.as_ref() != Some(job.place()))
            {
                self.pending_evidence = None;
            }
            if self.pending_evidence.is_none() {
                if let Some(place) = &open_place {
                    if self.evidence_settled.as_ref() != Some(place) {
                        self.pending_evidence = book_build::evidence_job(sd_library);
                        if self.pending_evidence.is_none() {
                            self.evidence_settled = Some(place.clone());
                        }
                    }
                }
            }
            if let Some(job) = &mut self.pending_evidence {
                let place = job.place().clone();
                match book_build::continue_source_evidence(card, job) {
                    book_build::EvidenceStep::Continued => {}
                    // Settled either way: a copy the card would not
                    // give up is not asked for again in this
                    // session, and a book reopened after one is
                    // asked about afresh.
                    book_build::EvidenceStep::Finished | book_build::EvidenceStep::Abandoned => {
                        self.pending_evidence = None;
                        self.evidence_settled = Some(place);
                    }
                }
            }
            return;
        };
        // Deliberately not gated on the latest reader request id.
        // Reading normally through a background build issues extends
        // and bumps that id constantly, and every one of them has
        // already passed through `apply_build_outcome`, which is what
        // decides whether the walk survived. The step itself re-checks
        // the catalog row it is building against the card.
        let advertised_before = sd_library.advertised_page_count();
        // Where the reader is now, which is the page each step must
        // leave resident. Only a Reading render carries a global page;
        // from anywhere else fall back to the book's start, which any
        // later page turn extends from.
        let reader_page = last_request
            .filter(|request| {
                request.book_id == pending.book_id && request.view == AppView::Reading
            })
            .map_or(0, |request| request.page);
        let scratch = host.ensure_scratch(&mut self.epub_scratch);
        let step =
            book_build::continue_book_build(card, sd_library, reader_page, scratch, font_metrics);
        let finished = step == book_build::BackgroundStep::Finished;
        match step {
            book_build::BackgroundStep::Continued => {
                // A step that ran clears the budget: it is consecutive
                // failures to begin that mean the card is gone, not a
                // single one somewhere in a minute of building.
                self.background_build = Some(BackgroundBuild {
                    attempts: 0,
                    ..pending
                });
            }
            // Nothing was touched and the walk is re-armed, so it is
            // simply kept — for as long as this book stays open, however
            // long the card is away. A reader at the frontier has no
            // page turn that would provoke a rebuild, which leaves this
            // walk as the only thing that can still raise their page
            // count; there is nothing to hand the job over to. The wait
            // before the next attempt is what makes holding on
            // affordable, and the walk still dies the moment the book
            // changes or its cache is cleared.
            book_build::BackgroundStep::Retry => {
                let attempts = pending.attempts.saturating_add(1);
                slog!(
                    "storage: background build retry {} in {} ms book_id={}",
                    attempts,
                    app_core::storage_loop::background_retry_delay_ms(attempts),
                    pending.book_id
                );
                self.background_build = Some(BackgroundBuild {
                    attempts,
                    ..pending
                });
            }
            _ => self.background_build = None,
        }
        if finished {
            slog!(
                "storage: background build done book_id={} pages={}",
                pending.book_id,
                sd_library.advertised_page_count()
            );
            bench_log!(
                "bench: storage_background_build book_id={} pages={} elapsed_ms={}",
                pending.book_id,
                sd_library.advertised_page_count(),
                pending.started.elapsed().as_millis(),
            );
        }
        // Announcing forces a full repaint, so an abandoned step may
        // never do it: its store may be mid-move and the arena may
        // still hold whatever the builder touched last rather than the
        // page on screen. Silence leaves the panel showing the frame it
        // already has, and the next page turn issues an ordinary extend
        // that reloads properly.
        let announce = match step {
            book_build::BackgroundStep::Abandoned => {
                slog!(
                    "storage: background build abandoned book_id={}",
                    pending.book_id
                );
                false
            }
            // The walk is over, but it grew the book before it broke and
            // left the store whole. Those pages are on the card and the
            // resident index reaches them; only the app's page count is
            // behind, and at the frontier that count is what makes the
            // next-page button do nothing.
            book_build::BackgroundStep::Stopped => {
                slog!(
                    "storage: background build stopped book_id={} pages={}",
                    pending.book_id,
                    sd_library.advertised_page_count()
                );
                app_core::storage_loop::stopped_announce(
                    advertised_before,
                    sd_library.advertised_page_count(),
                    reader_page,
                )
            }
            // Not one page was built, so there is nothing to say and a
            // repaint would only redraw the frontier the reader is
            // already looking at. The walk being kept is the answer
            // here, not the announcement.
            book_build::BackgroundStep::Retry => false,
            book_build::BackgroundStep::Continued | book_build::BackgroundStep::Finished => {
                app_core::storage_loop::background_announce(
                    finished,
                    reader_page,
                    advertised_before,
                )
            }
        };
        // The walk just came further, which is the only thing that
        // can make a waiting place resolvable. Asked here rather than
        // inside the open, because the walk advances one sliced step
        // at a time and the open cannot hold the executor for it.
        let resolved_place = resolve_pending_place(
            card,
            host,
            sd_library,
            &mut self.pending_place,
            pending.book_id,
            reader_page_of(last_request, pending.book_id),
            matches!(
                step,
                book_build::BackgroundStep::Continued | book_build::BackgroundStep::Retry
            ),
            &mut self.epub_scratch,
            font_metrics,
            &mut self.background_build,
        );
        match resolved_place {
            PlaceOutcome::Moved(target) => {
                slog!(
                    "restore: the place resolved once the book reached it, page {}",
                    target
                );
                host.send_loaded(&LibraryEvent::Loaded {
                    book_id: pending.book_id,
                    pages: sd_library.advertised_page_count(),
                    chapters: sd_library.chapter_count_for_ui(),
                    current_chapter: sd_library.current_chapter(),
                    chapter_pages: reader_cache::store::chapter_pages_for_event(sd_library),
                    position: Some(target),
                    text_replaced: true,
                });
                return;
            }
            // Nothing of this book is resident any more, so the
            // announcement the walk was about to make would report an
            // empty store as a book with a page in it.
            PlaceOutcome::Unreadable => {
                host.send_loaded(&LibraryEvent::BookOpenUnreadable {
                    book_id: pending.book_id,
                });
                return;
            }
            PlaceOutcome::Waiting => {}
        }
        if announce {
            // `position: None` — the book grew, the reader did not
            // move, and adopting a page here would yank them.
            host.send_loaded(&LibraryEvent::Loaded {
                book_id: pending.book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                current_chapter: sd_library.current_chapter(),
                chapter_pages: reader_cache::store::chapter_pages_for_event(sd_library),
                position: None,
                // The step reloaded the reader's section on its way
                // out, and this announce is only reached when the
                // repaint is the point of it (`background_announce`).
                text_replaced: true,
            });
        }
    }
}

/// The page the reader is on in `book_id`, or `None` when no Reading render
/// says they are in that book at all.
///
/// Kept apart from a page number on purpose. A place waiting on a book the
/// reader has left must be dropped rather than matched against the page it
/// happened to land on, and the provisional landing is page 0.
pub fn reader_page_of(last_request: Option<RenderRequest>, book_id: u32) -> Option<u32> {
    last_request
        .filter(|request| request.book_id == book_id && request.view == AppView::Reading)
        .map(|request| request.page)
}

/// Same-book page-turn progress is coalesced: at most one durable state write
/// per this interval, with a guaranteed flush before display sleep. A
/// battery pull can lose at most this many seconds of reading position.
pub const PROGRESS_WRITE_MIN_SECS: u64 = 15;

/// Try a waiting place against the pagination the last slice left behind.
///
/// `Some(page)` once the place resolves and the reader is still standing
/// where the open put them: they asked to resume, so moving them to the place
/// they asked for is finishing that, and a reader who has turned a page has
/// chosen somewhere else.
///
/// Liveness comes from the walk's own step rather than from the page count: a
/// spine item that renders nothing advances the cursor and adds no pages, so
/// counting pages would drop the place one empty item short of its target.
#[expect(clippy::too_many_arguments)] // The slice's whole world: the card, the store, the place, and the walk it waits on
fn resolve_pending_place(
    card: &mut impl Card,
    host: &mut impl Host,
    sd_library: &mut ReaderStore,
    pending_place: &mut Option<PendingPlace>,
    book_id: u32,
    reader_page: Option<u32>,
    walk_alive: bool,
    epub_scratch: &mut Option<&'static mut ReaderCacheScratch<'static>>,
    font_metrics: &mut crate::custom_font::MetricCache,
    background_build: &mut Option<BackgroundBuild>,
) -> PlaceOutcome {
    let Some(waiting) = pending_place.as_ref() else {
        return PlaceOutcome::Waiting;
    };
    if waiting.hold.book_id() != book_id {
        // A place left over from a book the reader has moved on from. Its
        // walk belongs to that open, and this one will not carry it.
        *pending_place = None;
        return PlaceOutcome::Waiting;
    }
    // `None` is nobody reading this book, which is not the same as somebody
    // reading its first page. Collapsing the two lets a place fire into a
    // book the reader has left, because the provisional landing is page 0.
    let Some(reader_page) = reader_page else {
        *pending_place = None;
        return PlaceOutcome::Waiting;
    };
    if reader_page != waiting.hold.landed() {
        // The reader moved. Whatever they are reading now is a better answer
        // than where they left off last time.
        *pending_place = None;
        return PlaceOutcome::Waiting;
    }
    if waiting.stopped {
        // Done asking. Still held, so the save cannot overwrite the place
        // this open failed to reach.
        return PlaceOutcome::Waiting;
    }
    // The row is about to be dereferenced, so it has to still be the book this
    // place was read from.
    let row_holds_it = sd_library
        .catalog_entry(waiting.index as usize)
        .is_some_and(|entry| (entry.source_hash, entry.byte_size) == waiting.source_identity);
    if !row_holds_it {
        slog!("restore: the row this place was waiting on holds another book");
        *pending_place = None;
        return PlaceOutcome::Waiting;
    }
    let index = waiting.index;
    let landed = waiting.hold.landed();
    let place = waiting.place;
    match book_build::resolve_place(card, sd_library, index as usize, place) {
        book_build::PlaceTarget::Page(target) => {
            if load_target_page(
                card,
                host,
                sd_library,
                index,
                target,
                book_id,
                epub_scratch,
                font_metrics,
                background_build,
            ) {
                *pending_place = None;
                return PlaceOutcome::Moved(target);
            }
            // The place resolved and its text would not come off the card,
            // which is a worse position rather than a different book. The
            // same ladder the open walks: back to where the reader is, then
            // the start of the book. The attempt cleared the store on its way
            // in, so something has to be put back either way.
            slog!("restore: the place resolved and its section would not load");
            let settled = settle_on_a_readable_page(
                card,
                host,
                sd_library,
                index,
                landed,
                book_id,
                epub_scratch,
                font_metrics,
                background_build,
            );
            // Counted with the refused reads, and for the same reason: a
            // target that resolves and will not load is a card saying no to a
            // particular section, and retrying it forever means a cache build
            // every settle interval for as long as the book stays open.
            if let Some(waiting) = pending_place.as_mut() {
                waiting.refusals = waiting.refusals.saturating_add(1);
                if waiting.refusals >= PLACE_READ_REFUSALS {
                    slog!("restore: the place's section kept refusing; leaving the reader put");
                    waiting.stopped = true;
                }
            }
            match settled {
                // Back where the reader already was, so there is nothing to
                // tell the app, and the place keeps its turn: the target it
                // wants may load on a later slice.
                Some(page) if page == landed => PlaceOutcome::Waiting,
                // The ladder went past the reader's own page to the start of
                // the book, which is the last rung and a decision rather than
                // a wait. Done asking, so a retry cannot reach for the same
                // target again and move a reader who has already been moved.
                // Held rather than dropped, and standing on the page it just
                // put them on: a page nobody chose is no reason to overwrite
                // the place on the card, and the reader turning away from it
                // is.
                Some(page) => {
                    if let Some(waiting) = pending_place.as_mut() {
                        waiting.hold.settled_on(page);
                        waiting.stopped = true;
                    }
                    PlaceOutcome::Moved(page)
                }
                None => {
                    *pending_place = None;
                    PlaceOutcome::Unreadable
                }
            }
        }
        // Still short of it. Worth waiting only while a walk is coming: once
        // it has finished or stopped, no later slice will reach the place.
        book_build::PlaceTarget::Extend(_) if walk_alive => PlaceOutcome::Waiting,
        // The card refused a read, which says nothing about the place, so the
        // remedy is to ask again. Counted, because a card that keeps refusing
        // would otherwise be asked forever; giving up costs this session's
        // resume and nothing else, since the place itself is on the card and
        // the next open reads it again.
        book_build::PlaceTarget::Unavailable => {
            if let Some(waiting) = pending_place.as_mut() {
                waiting.refusals = waiting.refusals.saturating_add(1);
                if waiting.refusals >= PLACE_READ_REFUSALS {
                    slog!("restore: the card kept refusing the place; leaving the reader put");
                    waiting.stopped = true;
                }
            }
            PlaceOutcome::Waiting
        }
        _ => {
            *pending_place = None;
            PlaceOutcome::Waiting
        }
    }
}

/// Bring a resolved page's text into the store, and say whether it arrived.
///
/// The answer is the store's own, not the build's: a build can report it did
/// what it could and leave the page short of resident, and the only thing
/// worth acting on is whether the page can be drawn now.
#[expect(clippy::too_many_arguments)] // The load's whole world: the card, the store, the page, and the walk it may start
fn load_target_page(
    card: &mut impl Card,
    host: &mut impl Host,
    sd_library: &mut ReaderStore,
    index: u16,
    target: u32,
    book_id: u32,
    epub_scratch: &mut Option<&'static mut ReaderCacheScratch<'static>>,
    font_metrics: &mut crate::custom_font::MetricCache,
    background_build: &mut Option<BackgroundBuild>,
) -> bool {
    if sd_library.covers_global_page(index as usize, target) {
        return true;
    }
    let scratch = host.ensure_scratch(epub_scratch);
    let outcome = book_build::build_or_load_book_cache(
        card,
        sd_library,
        index as usize,
        0,
        target as usize,
        scratch,
        font_metrics,
    );
    apply_build_outcome(background_build, outcome, book_id);
    sd_library.covers_global_page(index as usize, target)
}

/// Put some page of this book under the reader, weakening the position until
/// one lands.
///
/// A saved place that cannot be restored is a worse position, not a different
/// book. The reader chose this one, so the ladder stays inside it: the page
/// the open was landing on, then the start of the book. `None` is a book that
/// would give up no page at all, the one thing that makes an open unreadable.
///
/// Answers with the page rather than acting on it, because the two callers do
/// different things with it and must not drift apart about how they got there:
/// an open resolves its transaction, a background slice tells the app.
#[expect(clippy::too_many_arguments)] // The open's whole world: the card, the store, the transaction, and the page it is trying to reach
fn settle_on_a_readable_page(
    card: &mut impl Card,
    host: &mut impl Host,
    sd_library: &mut ReaderStore,
    index: u16,
    landing: u32,
    book_id: u32,
    epub_scratch: &mut Option<&'static mut ReaderCacheScratch<'static>>,
    font_metrics: &mut crate::custom_font::MetricCache,
    background_build: &mut Option<BackgroundBuild>,
) -> Option<u32> {
    for page in [landing, 0] {
        if load_target_page(
            card,
            host,
            sd_library,
            index,
            page,
            book_id,
            epub_scratch,
            font_metrics,
            background_build,
        ) {
            if page != landing {
                slog!("restore: falling back to the start of the book");
            }
            return Some(page);
        }
    }
    slog!("restore: no page of this book would load");
    None
}

/// What a slice's look at a waiting place came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceOutcome {
    /// The reader belongs on this page now.
    Moved(u32),
    /// Nothing to tell the app: the place is still waiting, or it is gone, or
    /// the reader is already where it settled.
    Waiting,
    /// The book gave up no page at all. Not a position any more: the text the
    /// reader is looking at is gone with it.
    Unreadable,
}

/// A stored place the open could not turn into a page yet.
///
/// The page a place names lives in pagination that a progressive build has
/// yet to reach, and reaching it is the background walk's job, one sliced
/// step at a time. So the open finishes where it can and the place waits here
/// for the walk to come far enough, rather than a second pagination driver
/// running inside the open and holding the executor through a minute of
/// building.
pub struct PendingPlace {
    /// The book and page this place is holding the card's copy against.
    hold: app_core::storage_loop::PlaceHold,
    index: u16,
    /// What the row held when this was armed, checked again before the row is
    /// dereferenced. A rescan reorders rows, and `book_id` is the row number,
    /// so neither it nor the hold moves when another copy takes the index.
    /// Resolving then reads this anchor against that copy's pagination. Same
    /// fence `BookBuildResume::belongs_to` puts on a suspended walk, for the
    /// same reason.
    source_identity: (u32, u32),
    place: book_build::SavedPlace,
    /// How many times the card has refused a read of this place.
    refusals: u8,
    /// Set when the asking is over: the refusals ran out, or the restore
    /// settled the reader on a page nobody chose. The place stays here so a
    /// progress save still cannot write that page over the stored one, and
    /// the settle slices stop being scheduled for it.
    stopped: bool,
}

/// Whether a progress record may replace its book's stored place, ending the
/// hold when it may.
///
/// A restore still owed holds the reader on a page the open picked, so writing
/// that page's anchor over the stored one discards the position the restore
/// exists to reach. The reader turning a page supersedes the restore and is
/// worth storing.
///
/// Ends the hold here rather than in the settle slices, which a stopped hold
/// does not schedule, and before the write is attempted, since the reader
/// chose the page whether or not the card takes it.
pub fn place_may_be_replaced(
    pending_place: &mut Option<PendingPlace>,
    record: &AppStateRecord,
) -> bool {
    retire_superseded_place(pending_place, record);
    !matches!(
        pending_place
            .as_ref()
            .map(|waiting| waiting.hold.verdict(record.book_id, record.screen)),
        Some(app_core::storage_loop::HoldVerdict::Held)
    )
}

/// Drop a restore's claim once a record proves the reader has gone somewhere
/// they chose.
///
/// Called on arrival as well as at the write, because the two are different
/// moments: a record can be coalesced away or refused by the card, and the
/// reader moved either way.
fn retire_superseded_place(pending_place: &mut Option<PendingPlace>, record: &AppStateRecord) {
    let superseded = pending_place.as_ref().is_some_and(|waiting| {
        waiting.hold.verdict(record.book_id, record.screen)
            == app_core::storage_loop::HoldVerdict::Superseded
    });
    if superseded {
        *pending_place = None;
    }
}

/// How many refused reads a waiting place takes before it is let go.
///
/// A bound on a card that is not answering, not on how far a walk may go: the
/// place is durable on the card either way, so the cost of giving up is this
/// session's resume, and the next open asks again.
const PLACE_READ_REFUSALS: u8 = 8;

/// List the library root again after a scan, and tell the app.
///
/// A scan replaces the catalog, and the rows on screen came off the card it
/// was built from. Nobody pressed anything, so what goes out is unsolicited,
/// naming the catalog it was taken from. A move already in flight against
/// that same catalog keeps the screen, since it can still land; a move
/// against the catalog this scan replaced is overruled, because the reset has
/// already taken the storage task somewhere else and the move can only come
/// back refused.
///
/// A card that answered the scan and then would not answer for the rows is
/// reported as unreadable rather than as an empty library. The two look
/// identical in a row count and are not the same thing: one is a library to
/// add books to, the other is a library that could not be read, and the
/// screen says different things about them.
pub fn relist_library_folder(
    card: &mut impl Card,
    host: &mut impl Host,
    sd_library: &mut ReaderStore,
    portrait: bool,
) {
    let started = Instant::now();
    let listed = card
        .with_root(|root| reader_cache::browse::relist_root(sd_library, root, portrait))
        .ok()
        .flatten();
    // Part of what a boot costs, and the one listing nobody pressed for.
    bench_log!(
        "bench: folder_relist rows={} ok={} ms={} t_ms={}",
        listed.map_or(0, |listing| listing.count),
        listed.is_some(),
        started.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
    let browse_epoch = sd_library.browse_epoch();
    match listed {
        Some(listing) => host.send_required(&LibraryEvent::FolderListed {
            request_id: None,
            browse_epoch,
            depth: listing.depth,
            count: listing.count,
            books: listing.books,
            selection: listing.selection,
        }),
        None => {
            slog!("library: the card would not list the root after a scan");
            // The scan's own verdict stands for the catalog; this one is
            // about whether the library can be shown at all, and it cannot.
            sd_library.status = LibraryScanStatus::Error;
            host.send_required(&LibraryEvent::LibraryUnreadable { browse_epoch });
        }
    }
}

/// Relist the folder browsing is in, unasked, after a rescan shows it still
/// exists, with the cursor on the book at `locator` under `at`, the one the
/// reader picked there.
pub fn relist_library_folder_here(
    card: &mut impl Card,
    host: &mut impl Host,
    sd_library: &mut ReaderStore,
    portrait: bool,
    at: proto::library_path::BookRoot,
    locator: &proto::library_path::LibraryPath,
) {
    let listed = card
        .with_root(|root| {
            reader_cache::browse::relist_on_book(sd_library, root, portrait, at, locator)
        })
        .ok()
        .flatten();
    match listed {
        Some(listing) => host.send_required(&LibraryEvent::FolderListed {
            request_id: None,
            browse_epoch: sd_library.browse_epoch(),
            depth: listing.depth,
            count: listing.count,
            books: listing.books,
            selection: listing.selection,
        }),
        // The folder would not list, so fall back to the root, which reports
        // its own failure.
        None => relist_library_folder(card, host, sd_library, portrait),
    }
}

/// A book whose spine walk is still running after a progressive open published
/// it early.
///
/// This is not the build's state — that lives in the EPUB scratch, beside the
/// section records it describes, so the two cannot drift. This is only what the
/// loop needs: which book, and when the walk began, for the closing bench line.
#[derive(Clone, Copy)]
pub struct BackgroundBuild {
    book_id: u32,
    started: Instant,
    /// Consecutive steps that never began. Cleared by anything that proves the
    /// card is answering — a step that actually ran, or a foreground open that
    /// carried this walk through — so a hiccup does not go on slowing a build
    /// the card has already come back for.
    attempts: u8,
}

/// Carry the loop's background-build handle across one open or extend.
///
/// The distinction that matters is `Carried`: a page turn crossing a section
/// boundary arrives as an extend and is answered from the cache, which must
/// not be read as "the build ended". Only the reader-cache layer can tell the
/// difference — it knows whether the fast path answered — so this just follows
/// its verdict.
pub fn apply_build_outcome(
    background_build: &mut Option<BackgroundBuild>,
    outcome: book_build::BookBuildOutcome,
    book_id: u32,
) {
    match outcome {
        book_build::BookBuildOutcome::Settled => *background_build = None,
        book_build::BookBuildOutcome::Started => {
            *background_build = Some(BackgroundBuild {
                book_id,
                started: Instant::now(),
                attempts: 0,
            })
        }
        // The handle is normally already there, and what it needs is its retry
        // budget cleared: reaching here means a foreground open just took an SD
        // session, read this book's index and a section out of it, and came back
        // with the walk still valid. That is direct evidence the card is
        // answering again, so a walk sitting out a 30 s backoff should not wait
        // the rest of it — the reader can cross the frontier inside that window.
        //
        // Adopting a *missing* handle is the separate safety net, for anything
        // that drops it without ending the walk — a cache clear for another
        // book, say — so a still-valid build is picked back up rather than
        // stranded half-written. A handle naming another book is stale by the
        // same reasoning, since `Carried` proves the resume belongs to this one.
        book_build::BookBuildOutcome::Carried => match background_build {
            Some(pending) if pending.book_id == book_id => pending.attempts = 0,
            _ => {
                *background_build = Some(BackgroundBuild {
                    book_id,
                    started: Instant::now(),
                    attempts: 0,
                })
            }
        },
    }
}

/// Apply one admitted storage command, updating the store and reporting
/// outcomes through `host`. `portrait` selects the folder listing's layout.
/// A stale pick whose progress flush succeeds is announced and retained in
/// `pending_rescan` for the caller to finish after painting its progress plate.
///
/// Kept out of line so the task loop's poll frame stays small; the storage
/// arms below carry multi-KB scratch and run near the stack floor.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub fn handle_storage_command(
    command: StorageCommand,
    card: &mut impl Card,
    host: &mut impl Host,
    sd_library: &mut ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    epub_scratch: &mut Option<&'static mut ReaderCacheScratch<'static>>,
    sync_session: &mut SyncSession,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
    state_restored: &mut StateRestore,
    background_build: &mut Option<BackgroundBuild>,
    pending_place: &mut Option<PendingPlace>,
    catalog_refresh: &mut CatalogRefresh,
    portrait: bool,
    pending_rescan: &mut Option<PendingRescan>,
) {
    // The session decides what may run: progress writes stay alive during a
    // sync session (they are cheap and harmless); everything
    // that touches the EPUB scratch is gone until the session's reset.
    if !sync_session.admits(&command) {
        slog!("storage: refused during sync session");
        // A command the app is waiting on gets its refusal, not silence. The
        // reader can leave the Wireless screen while a join still holds the
        // loan, and an open or a Library move dropped here held the input
        // gate, or the Library rail, until the session's reset.
        match command {
            // Only the open still being waited on: a failed open rolls back
            // whichever open is current.
            StorageCommand::OpenBook {
                request_id,
                book_id,
                ..
            } if request_id == host.latest_reader_request_id() => {
                host.send_required(&LibraryEvent::BookOpenFailed { book_id });
            }
            StorageCommand::ChooseLibraryRow { request_id, .. }
            | StorageCommand::LeaveLibraryFolder { request_id, .. } => {
                host.send_required(&LibraryEvent::RowFailed { request_id });
            }
            StorageCommand::ClearBookCache { request_id, .. } => {
                host.send(&LibraryEvent::CacheCleared {
                    request_id,
                    ok: false,
                });
            }
            _ => {}
        }
        return;
    }
    match command {
        StorageCommand::LoanSyncMemory => {
            // The background handle is deliberately *not* dropped here. A loan
            // that gets refused below returns with the scratch — and the walk's
            // section records in it — completely intact, and dropping the only
            // thing that schedules that walk would strand it: the loop's branch
            // is gated on the handle, and a reader already at the frontier
            // cannot issue the extend that would re-adopt it. It is cleared
            // once the scratch is actually gone.
            //
            // The session only ends in a reset, so any coalesced position
            // must reach the card before the scratch is dismantled.
            if !flush_pending_progress(
                card,
                sd_library,
                pending_progress,
                last_progress_write,
                pending_place,
            ) {
                // The wifi task is blocked on this answer; a silent return
                // would strand it (and the Wireless screen) forever. Refuse
                // observably so it can report the failure and re-park.
                slog!("storage: sync loan refused; progress persistence failed");
                host.refuse_sync_loan();
                return;
            }
            host.ensure_scratch(epub_scratch);
            let Some(scratch) = epub_scratch.take() else {
                host.refuse_sync_loan();
                return;
            };
            // The scratch is out of the reader's hands now, taking the walk's
            // section records with it. Past this point the loan is granted and
            // the session ends in a reset, so there is nothing left to schedule.
            *background_build = None;
            sync_session.loan_granted();
            host.grant_sync_loan(card, scratch);
        }
        StorageCommand::LoadCatalogCache => {
            // Boot-time probe: name the saved network so the Wireless
            // screen can offer connect/forget honestly. The command runs
            // once per boot, before any session can start.
            if let Some(record) = book_build::load_wifi_credentials(card) {
                let ssid = app_core::WifiSsid {
                    bytes: record.ssid,
                    len: record.ssid_len,
                };
                slog!("wifi: saved network '{}'", ssid.as_str());
                host.network_saved(ssid);
            } else {
                slog!("wifi: no saved network");
            }
            book_build::load_custom_font_manifest(card, sd_library);
            host.send(&LibraryEvent::CustomFont {
                available: sd_library.custom_font_available(),
            });
            if crate::library_sd::load_catalog_cache(card, sd_library) {
                // Restored goes out first so the very next Home repaint
                // already shows the saved book; the Scanned default then
                // sees an SD book active and leaves it alone.
                restore_saved_state(card, host, sd_library, state_restored, false);
                let count = sd_library.catalog_count_u16();
                host.send(&LibraryEvent::Scanned {
                    count,
                    catalog_epoch: sd_library.catalog_epoch(),
                });
                relist_library_folder(card, host, sd_library, portrait);
            } else {
                host.requeue(StorageCommand::RefreshCatalog);
            }
        }
        StorageCommand::RefreshCatalog => {
            book_build::load_custom_font_manifest(card, sd_library);
            host.send(&LibraryEvent::CustomFont {
                available: sd_library.custom_font_available(),
            });
            if !scan_books_after_flush(
                card,
                sd_library,
                pending_progress,
                last_progress_write,
                pending_place,
                catalog_refresh,
            ) {
                catalog_refresh.owed = true;
                catalog_refresh.refusals = catalog_refresh.refusals.saturating_add(1);
                return;
            }
            // Past the write, this is an ordinary refresh whatever its scan
            // did, so the refused writes' backoff is spent.
            *catalog_refresh = CatalogRefresh::default();
            restore_saved_state(card, host, sd_library, state_restored, false);
            host.send(&LibraryEvent::Scanned {
                count: sd_library.catalog_count_u16(),
                catalog_epoch: sd_library.catalog_epoch(),
            });
            relist_library_folder(card, host, sd_library, portrait);
        }
        StorageCommand::OpenBook {
            request_id,
            book_id,
            index,
            ..
        }
        | StorageCommand::ExtendSection {
            request_id,
            book_id,
            index,
            ..
        } => {
            let storage_start = Instant::now();
            let latest_request_id = host.latest_reader_request_id();
            // The transaction's order lives in `OpenSequence` so a host test can
            // drive it against a card model that fails whichever write it likes;
            // this arm supplies a real card and reports back what it did.
            let Some(mut open) =
                OpenSequence::begin(&command, latest_request_id, sd_library.catalog_epoch())
            else {
                slog!(
                    "storage: stale open skipped request={} latest={} book_id={} index={}",
                    request_id,
                    latest_request_id,
                    book_id,
                    index
                );
                return;
            };
            // `Some(ram_hit)` once a section load was reached; a transaction the
            // close-out refused never gets that far and must not report an open
            // that did not happen.
            let mut section_loaded = None;
            // Set by a section load that left nothing readable, and read by
            // the announcement, which must then say so rather than report the
            // counts of an empty store.
            let mut landed_nothing = false;
            // Read at the saved-position step and spent at the section load,
            // which is where a place first has a pagination to resolve in.
            let mut opening_place: Option<book_build::SavedPlace> = None;
            loop {
                match open.next() {
                    OpenAction::CloseOutDeparting(previous) => {
                        let stored = close_out_departing_book(
                            card,
                            sd_library,
                            pending_progress,
                            last_progress_write,
                            pending_place,
                            previous,
                        );
                        if !stored {
                            slog!(
                                "storage: book open {:?} book_id={} departing={}",
                                book_open_outcome(false, false),
                                book_id,
                                previous.book_id,
                            );
                        }
                        open.departing_stored(stored);
                    }
                    OpenAction::Refuse { book_id } => {
                        // Nothing has been opened, so the reader is still whole
                        // on the book it was reading. Announcing the new one
                        // would strand that page: the app has already left the
                        // book that owns it and will never reissue it.
                        host.send_required(&LibraryEvent::BookOpenFailed { book_id });
                        open.refused();
                    }
                    OpenAction::StageBook {
                        index,
                        type_settings,
                        portrait,
                    } => {
                        // A place belongs to the open that read it, so an
                        // open ends whatever an earlier one left waiting. An
                        // extend is that same open asking for more of its
                        // book and inherits it instead: a restore settling on
                        // a page raises one, and ending the place here would
                        // have the restore cancel itself. An overtaken place
                        // is retired in `resolve_pending_place`, where the
                        // reader's own move can be told apart.
                        if !open.is_extend() {
                            *pending_place = None;
                        }
                        // Read this book's catalog record into the active-entry
                        // slot so the reader pipeline (load_position,
                        // build_or_load) resolves it from the card rather than
                        // the list window. A failure leaves the entry unset and
                        // the open falls through to the usual bad-index error.
                        crate::library_sd::load_active_entry(card, sd_library, index as usize);
                        // Adopt the command's type settings before the RAM fast
                        // path: a settings change drops the loaded page
                        // coverage, so the request falls through to the cache
                        // load/rebuild below.
                        sd_library.set_layout(type_settings, portrait);
                        open.staged();
                    }
                    OpenAction::LoadSavedPosition { index } => {
                        // The place is read here and resolved after the load:
                        // it names content, and which page that content falls
                        // on is decided by the pagination this open is about
                        // to build.
                        opening_place = book_build::load_place(card, sd_library, index as usize);
                        // If unreadable, keep the incoming position and retry
                        // resolution after loading the section.
                        open.saved_position(opening_place.and_then(|place| match place {
                            book_build::SavedPlace::Unreadable => None,
                            place => Some(place.provisional()),
                        }));
                        if open.resumed() {
                            slog!(
                                "storage: resume book {} at chapter {} screen {}",
                                book_id,
                                open.target_chapter(),
                                open.target_page()
                            );
                        }
                    }
                    OpenAction::LoadSection {
                        index,
                        chapter,
                        page,
                    } => {
                        // The requested page is usually inside the section
                        // window that is already loaded; answering from RAM
                        // keeps ordinary page turns free of card init, FAT, and
                        // cache-file traffic.
                        let ram_hit = sd_library.covers_global_page(index as usize, page as u32);
                        section_loaded = Some(ram_hit);
                        if ram_hit {
                            slog!(
                                "storage: open hit in RAM request={} book_id={} page={}",
                                request_id,
                                book_id,
                                page
                            );
                        } else {
                            slog!(
                                "storage: open command request={} book_id={} index={} chapter={} target={}",
                                request_id,
                                book_id,
                                index,
                                chapter,
                                page
                            );
                            sd_library.set_reader_status(BookLoadStatus::Loading);
                            let scratch = host.ensure_scratch(epub_scratch);
                            // The transaction around this call is untouched by
                            // a progressive publish: it moves positions, and
                            // this book's position is real whether or not its
                            // tail is indexed yet.
                            let outcome = book_build::build_or_load_book_cache(
                                card,
                                sd_library,
                                index as usize,
                                chapter,
                                page as usize,
                                scratch,
                                font_metrics,
                            );
                            apply_build_outcome(background_build, outcome, book_id);
                            // The store's own answer, not the build's: a build
                            // can report it did what it could and leave the
                            // page short of resident, and a failed one leaves
                            // the store cleared. The announcement reads a
                            // cleared store as a one-page book and clamps the
                            // reader into it, so weaken the position the way a
                            // place does. The build has had its turn at this
                            // page, so the ladder starts below it.
                            if !sd_library.covers_global_page(index as usize, page as u32) {
                                let fell_back = page != 0
                                    && load_target_page(
                                        card,
                                        host,
                                        sd_library,
                                        index,
                                        0,
                                        book_id,
                                        epub_scratch,
                                        font_metrics,
                                        background_build,
                                    );
                                if fell_back {
                                    slog!(
                                        "open: page {} would not load; falling back to the \
                                         start of the book",
                                        page
                                    );
                                    open.resolve_place(0);
                                } else {
                                    slog!("open: no page of this book would load");
                                    landed_nothing = true;
                                }
                            }
                        }
                        // The index now describes this book under this
                        // layout, which is the first moment a stored place
                        // can be turned into a page. Once, here: a place
                        // beyond the frontier needs the walk to come to it,
                        // and the walk runs in slices so page turns keep
                        // working while it does.
                        if let Some(place) = opening_place.take() {
                            let resolved =
                                book_build::resolve_place(card, sd_library, index as usize, place);
                            let landing = u32::from(open.target_page());
                            // Where the open actually leaves the reader. The
                            // ladder below can settle somewhere other than the
                            // landing, and the hold has to stand on the page
                            // they are on: one standing anywhere else reads as
                            // the reader having moved, which retires the place
                            // and frees the save it exists to hold back.
                            let mut settled = landing;
                            let landed_on = match resolved {
                                book_build::PlaceTarget::Page(target) => {
                                    let loaded = load_target_page(
                                        card,
                                        host,
                                        sd_library,
                                        index,
                                        target,
                                        book_id,
                                        epub_scratch,
                                        font_metrics,
                                        background_build,
                                    );
                                    if loaded {
                                        landed_nothing = false;
                                        section_loaded = Some(false);
                                        open.resolve_place(target);
                                        None
                                    } else {
                                        // The page is the reader's and its
                                        // text would not come off the card.
                                        // Telling the app they are there
                                        // would put them on a page nothing
                                        // has loaded, so the open lands where
                                        // it is and the place waits.
                                        //
                                        // The attempt cleared the store on
                                        // its way in, so the page this open
                                        // was landing on has to be fetched
                                        // back before the open announces it.
                                        slog!(
                                            "restore: the place resolved and its section \
                                             would not load"
                                        );
                                        section_loaded = Some(false);
                                        // A place that cannot be restored is a
                                        // worse position, not a different
                                        // book. The reader asked for this one,
                                        // so the position weakens and the book
                                        // stays.
                                        match settle_on_a_readable_page(
                                            card,
                                            host,
                                            sd_library,
                                            index,
                                            landing,
                                            book_id,
                                            epub_scratch,
                                            font_metrics,
                                            background_build,
                                        ) {
                                            Some(page) => {
                                                landed_nothing = false;
                                                settled = page;
                                                open.resolve_place(page);
                                            }
                                            None => landed_nothing = true,
                                        }
                                        Some(())
                                    }
                                }
                                // The pagination holding it does not exist
                                // yet, or the card refused to say. Either
                                // way the reader gets the book now and the
                                // place waits for a later slice.
                                book_build::PlaceTarget::Extend(_)
                                | book_build::PlaceTarget::Unavailable => Some(()),
                                book_build::PlaceTarget::Keep => None,
                            };
                            // An open that read nothing is over, and the app
                            // rolls back off this book. A place left waiting
                            // would resolve later and send a `Loaded` for a
                            // transaction that ended, which the open
                            // bookkeeping matches by book alone: it would
                            // answer whichever open of this book is current by
                            // then and take its rollback with it. The place is
                            // on the card, so the next open reads it again.
                            let landed_on = if landed_nothing { None } else { landed_on };
                            *pending_place = landed_on.map(|()| PendingPlace {
                                hold: app_core::storage_loop::PlaceHold::new(book_id, settled),
                                index,
                                source_identity: source_identity(sd_library, book_id),
                                place,
                                refusals: 0,
                                stopped: false,
                            });
                        }
                        if landed_nothing {
                            open.section_failed();
                        } else {
                            open.section_loaded();
                        }
                    }
                    OpenAction::StorePointer(state) => {
                        let record = record_for_persisted(sd_library, state);
                        let stored = book_build::store_global_state(card, record);
                        if stored {
                            *pending_progress = None;
                            *last_progress_write = Some(Instant::now());
                        } else {
                            // Left owed rather than retried here: the book is
                            // open and the reader is in it, so the only cost of
                            // waiting for the next flush is a reboot in that
                            // window landing back on the old book.
                            *pending_progress = Some(record);
                        }
                        let outcome = book_open_outcome(true, stored);
                        debug_assert!(outcome.book_changed());
                        slog!(
                            "storage: book open {:?} book_id={} page={}",
                            outcome,
                            record.book_id,
                            record.screen,
                        );
                        bench_log!(
                            "bench: store_global_state ok={} book_id={} page={} t_ms={}",
                            stored,
                            record.book_id,
                            record.screen,
                            Instant::now().as_millis(),
                        );
                        open.pointer_stored(stored);
                    }
                    OpenAction::Announce { book_id, position } => {
                        // The one thing only this task knows: whether the load
                        // put different text under the reader. `None` means no
                        // section load was reached at all (a refused
                        // transaction), which is not a RAM hit; only a
                        // confirmed one read nothing from the card.
                        //
                        // Sent unconditionally. The app clamps navigation
                        // against the counts in here and clears its open lock
                        // on the event itself, so withholding it is never a
                        // saved refresh — it decides the repaint for itself in
                        // `loaded_repaints`.
                        // Notify the app that the book could not be read,
                        // avoiding invalid page clamp and save operations.
                        if landed_nothing {
                            host.send_loaded(&LibraryEvent::BookOpenUnreadable { book_id });
                            open.announced();
                            continue;
                        }
                        let text_replaced = !matches!(section_loaded, Some(true));
                        host.send_loaded(&LibraryEvent::Loaded {
                            book_id,
                            pages: sd_library.advertised_page_count(),
                            chapters: sd_library.chapter_count_for_ui(),
                            current_chapter: sd_library.current_chapter(),
                            chapter_pages: reader_cache::store::chapter_pages_for_event(sd_library),
                            position,
                            text_replaced,
                        });
                        open.announced();
                    }
                    OpenAction::Done => break,
                }
            }
            if let Some(ram_hit) = section_loaded {
                if !ram_hit {
                    // Also the bench harness's legacy parse of a completed open.
                    slog!(
                        "storage: open complete status={:?} pages={} chapters={}",
                        sd_library.reader_status(),
                        sd_library.advertised_page_count(),
                        sd_library.chapter_count_for_ui()
                    );
                }
                bench_log!(
                    "bench: storage_open request={} book_id={} index={} ram_hit={} elapsed_ms={} status={:?} pages={} chapters={}",
                    request_id,
                    book_id,
                    index,
                    ram_hit,
                    storage_start.elapsed().as_millis(),
                    sd_library.reader_status(),
                    sd_library.advertised_page_count(),
                    sd_library.chapter_count_for_ui(),
                );
            }
        }
        StorageCommand::LoadChapters {
            request_id,
            book_id,
            index,
        } => {
            if request_id != host.latest_reader_request_id() {
                return;
            }
            crate::library_sd::load_active_entry(card, sd_library, index as usize);
            // The overview opens with the cursor on the current chapter, so
            // center the first TOC window there.
            let ok = book_build::load_chapters_into_store(
                card,
                sd_library,
                index as usize,
                sd_library.current_chapter() as usize,
            );
            slog!(
                "storage: chapters loaded book_id={} ok={} count={}",
                book_id,
                ok,
                sd_library.overview_chapter_count()
            );
            // Re-render the overview with the full list resident, syncing the
            // selection range to the full chapter count. The reader has not
            // moved, so the app's own page stands.
            host.send_loaded(&LibraryEvent::Loaded {
                book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                current_chapter: sd_library.current_chapter(),
                chapter_pages: reader_cache::store::chapter_pages_for_event(sd_library),
                position: None,
                // The full list replaced the reading section in the buffer,
                // and the overview is holding its frame for this event.
                text_replaced: true,
            });
        }
        StorageCommand::JumpChapter {
            request_id,
            book_id,
            index,
            chapter,
            type_settings,
            portrait,
        } => {
            if request_id != host.latest_reader_request_id() {
                return;
            }
            crate::library_sd::load_active_entry(card, sd_library, index as usize);
            sd_library.set_layout(type_settings, portrait);
            // The TOC is still in the buffer; resolve the chapter's start page
            // before loading the section overwrites it. Re-ensure the window
            // covers the selection in case it slid since the overview render.
            book_build::ensure_toc_window(
                card,
                sd_library,
                index as usize,
                chapter as usize,
                portrait,
            );
            let target_page = sd_library.overview_page_at(chapter as usize);
            let scratch = host.ensure_scratch(epub_scratch);
            let outcome = book_build::build_or_load_book_cache(
                card,
                sd_library,
                index as usize,
                chapter,
                target_page as usize,
                scratch,
                font_metrics,
            );
            apply_build_outcome(background_build, outcome, book_id);
            // A failed build leaves the store cleared, which announces as a
            // one-page book. Fall back as an open does: the first page, then
            // the book's error state.
            let mut landed = target_page as u32;
            if !sd_library.covers_global_page(index as usize, landed) {
                let fell_back = landed != 0
                    && load_target_page(
                        card,
                        host,
                        sd_library,
                        index,
                        0,
                        book_id,
                        epub_scratch,
                        font_metrics,
                        background_build,
                    );
                if !fell_back {
                    slog!("jump: no page of this book would load");
                    host.send_loaded(&LibraryEvent::BookOpenUnreadable { book_id });
                    return;
                }
                slog!(
                    "jump: page {} would not load; falling back to the start of the book",
                    landed
                );
                landed = 0;
            }
            // The page came from the on-disk TOC, not from the app, so it
            // rides with the load rather than following as a second event.
            host.send_loaded(&LibraryEvent::Loaded {
                book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                current_chapter: sd_library.current_chapter(),
                chapter_pages: reader_cache::store::chapter_pages_for_event(sd_library),
                position: Some(landed),
                // A jump lands the reader on another chapter's text.
                text_replaced: true,
            });
        }
        StorageCommand::ReceiveUpload => {
            // Handled in the task loop before dispatch; reaching here means
            // the loop refused it already.
        }
        StorageCommand::StoreWifiCredentials(credentials) => {
            let record = proto::nvm::WifiCredentialsRecord {
                ssid: credentials.ssid,
                ssid_len: credentials.ssid_len,
                password: credentials.password,
                password_len: credentials.password_len,
            };
            let written = book_build::store_wifi_credentials(card, record);
            // Reacquire the card and use the exact boot-time read path before
            // telling the portal it may show success. This proves the record
            // survived handle/volume closure, closing the race where the
            // portal's success page beat a write that never actually landed
            // and the session-ending reset lost the credentials.
            let confirmed = written
                && book_build::load_wifi_credentials(card).is_some_and(|stored| stored == record);
            slog!(
                "storage: wifi credentials written={} confirmed={}",
                written,
                confirmed
            );
            host.wifi_storage_result(confirmed);
        }
        StorageCommand::StoreWifiApHint { ssid, hint } => {
            let record = proto::nvm::WifiApHintRecord {
                ssid_hash: proto::nvm::WifiApHintRecord::hash_ssid(ssid.as_str().as_bytes()),
                bssid: hint.bssid,
                channel: hint.channel,
            };
            // No confirmation channel, unlike the credentials: nothing waits
            // on this and a lost hint costs one scan.
            let written = book_build::store_wifi_ap_hint(card, record);
            slog!(
                "storage: wifi ap hint written={} channel={}",
                written,
                hint.channel
            );
        }
        StorageCommand::ForgetWifiCredentials => {
            let forgotten = book_build::forget_wifi_credentials(card);
            slog!("storage: wifi credentials forgotten={}", forgotten);
        }
        StorageCommand::ClearBookCache {
            request_id,
            index,
            browse_epoch,
        } => {
            // Deleting a cache dir out from under a background build would
            // leave it writing an index for section files that no longer
            // exist. Both halves of the walk go, not just the handle: leaving
            // the resume in the scratch would let the next open of this row
            // report `Carried` and schedule steps over a cache that is gone.
            //
            // Ended unconditionally, even though the clear may name a different
            // book than the one building. A walk is only ever an optimisation —
            // the worst case is one redundant rebuild — and "the handle and the
            // resume die together" is an invariant worth more than that.
            *background_build = None;
            if let Some(scratch) = epub_scratch.as_mut() {
                book_build::clear_build_resume(scratch);
            }
            // The row was picked in a folder this task may since have left,
            // which would leave a different book sitting under it. Refuse
            // rather than guess: the user can pick again from the list they
            // can actually see.
            //
            // A Library row is a row of the folder listing, not a catalog
            // index, so it is resolved the way an open resolves one: by the
            // identity its locator, root, and size give, which is what the
            // catalog record was written under. Reading it as a catalog index
            // would clear whatever book happened to sit at that number.
            let ok = if browse_epoch == sd_library.browse_epoch() {
                match reader_cache::browse::row_book(sd_library, index) {
                    Some((at, locator, size)) => {
                        // By the place the row named, for the same reason the
                        // open does: the identity derived from it is 32 bits
                        // and can collide, and clearing nothing is the mild
                        // end of that.
                        match crate::library_sd::find_index_by_locator(
                            card,
                            at,
                            locator.as_str(),
                            size,
                        ) {
                            crate::library_sd::CatalogRow::Found(row) => {
                                book_build::clear_book_cache(card, sd_library, row)
                            }
                            // Clearing a cache is not worth a rebuild, and a
                            // catalog that would not answer is not worth
                            // anything. Both report nothing cleared.
                            crate::library_sd::CatalogRow::Rebuild
                            | crate::library_sd::CatalogRow::Unreadable => false,
                        }
                    }
                    // A folder, or a row the resident page does not cover.
                    None => false,
                }
            } else {
                slog!(
                    "storage: clear cache index={} stale browse epoch={} now={}",
                    index,
                    browse_epoch,
                    sd_library.browse_epoch()
                );
                false
            };
            slog!(
                "storage: clear cache request={} index={} ok={}",
                request_id,
                index,
                ok
            );
            host.send(&LibraryEvent::CacheCleared { request_id, ok });
        }
        StorageCommand::ChooseLibraryRow {
            request_id,
            index,
            browse_epoch,
        } => {
            // The row was picked in a folder this task may since have left: a
            // scan takes browsing back to the root, and the same row number
            // there names a different child of a different place. Refuse
            // rather than guess, and the reader picks again from the list
            // they can see.
            let choice = if browse_epoch == sd_library.browse_epoch() {
                crate::library_sd::choose_library_row(card, sd_library, index, portrait)
            } else {
                slog!(
                    "storage: choose row={} stale browse epoch={} now={}",
                    index,
                    browse_epoch,
                    sd_library.browse_epoch()
                );
                crate::library_sd::RowChoice::Failed
            };
            match choice {
                crate::library_sd::RowChoice::Entered(listing) => {
                    host.send_required(&LibraryEvent::FolderListed {
                        request_id: Some(request_id),
                        browse_epoch: sd_library.browse_epoch(),
                        depth: listing.depth,
                        count: listing.count,
                        books: listing.books,
                        selection: listing.selection,
                    });
                }
                crate::library_sd::RowChoice::Book(index) => {
                    // Answer and stop. The app owns opening: it commits the
                    // reader request id a later open is checked against, arms
                    // the gate that keeps input off the panel until the book
                    // lands, and keeps the rollback that puts the reader back
                    // if the command is refused. An open sent from here would
                    // have none of that, and would carry an id from the browse
                    // counter that the staleness check reads as old.
                    host.send_required(&LibraryEvent::RowIsBook {
                        request_id,
                        index,
                        catalog_epoch: sd_library.catalog_epoch(),
                    });
                }
                crate::library_sd::RowChoice::Failed => {
                    host.send_required(&LibraryEvent::RowFailed { request_id });
                }
                crate::library_sd::RowChoice::Stale { at, locator, size } => {
                    // A book the card holds and the catalog does not, because
                    // boot keeps a snapshot that still loads and a computer
                    // can add or move books while the device is off. Rebuild
                    // rather than tell a reader that a book they can see
                    // cannot be opened. Only a card edited since the last
                    // scan pays for this, once, which is what keeps every
                    // other boot on the warm snapshot.
                    //
                    // The scan drops the pages the coalesced position is
                    // anchored to, so that goes to the card first.
                    if !flush_pending_progress(
                        card,
                        sd_library,
                        pending_progress,
                        last_progress_write,
                        pending_place,
                    ) {
                        slog!("sd: the reading position would not save; not scanning over it");
                        // Nothing moved, and the page is still resident for
                        // the next pick to write.
                        host.send_required(&LibraryEvent::RowFailed { request_id });
                        return;
                    }
                    // Say so before the scan, then hand it back: the caller
                    // paints the note while it still has the bus.
                    host.send(&LibraryEvent::Rescanning { request_id });
                    *pending_rescan = Some(PendingRescan {
                        request_id,
                        at,
                        locator,
                        size,
                    });
                }
            }
        }
        StorageCommand::LeaveLibraryFolder {
            request_id,
            browse_epoch,
        } => {
            let listed = if browse_epoch == sd_library.browse_epoch() {
                crate::library_sd::leave_library_folder(card, sd_library, portrait)
            } else {
                slog!(
                    "storage: leave folder stale browse epoch={} now={}",
                    browse_epoch,
                    sd_library.browse_epoch()
                );
                None
            };
            match listed {
                Some(listing) => host.send_required(&LibraryEvent::FolderListed {
                    request_id: Some(request_id),
                    browse_epoch: sd_library.browse_epoch(),
                    depth: listing.depth,
                    count: listing.count,
                    books: listing.books,
                    selection: listing.selection,
                }),
                None => host.send_required(&LibraryEvent::RowFailed { request_id }),
            }
        }
        StorageCommand::StoreProgress(record) => {
            let record = record_for_persisted(sd_library, record);
            // Clear superseded hold immediately on navigation, before coalescing.
            retire_superseded_place(pending_place, &record);
            // Drop records with no source identity since they cannot be written.
            if app_core::ReaderSource::from_book_id(record.book_id).is_sd()
                && (record.source_hash, record.source_size) == (0, 0)
            {
                slog!(
                    "storage: dropping a progress record with no source identity book_id={}",
                    record.book_id
                );
                return;
            }
            // Coalesce same-context page turns; anything beyond the screen
            // number changing (book, chapter, orientation, policy) is rare
            // and worth landing immediately. A pending record for the same
            // book is superseded by the new one; only a different book's
            // pending position must be preserved first.
            let context_changed = pending_progress
                .map(|pending| {
                    AppStateRecord {
                        screen: record.screen,
                        ..pending
                    } != record
                })
                .unwrap_or(false);
            let due = last_progress_write
                .map(|written| written.elapsed().as_secs() >= PROGRESS_WRITE_MIN_SECS)
                .unwrap_or(true);
            if pending_progress
                .map(|pending| pending.book_id != record.book_id)
                .unwrap_or(false)
                && !flush_pending_progress(
                    card,
                    sd_library,
                    pending_progress,
                    last_progress_write,
                    pending_place,
                )
            {
                // The other book's position couldn't land; overwriting the
                // pending record now would silently discard it.
                slog!("storage: progress context switch deferred after write failure");
                return;
            }
            if context_changed || due {
                let progress_start = Instant::now();
                let stored = book_build::store_app_state(
                    card,
                    sd_library,
                    record,
                    place_may_be_replaced(pending_place, &record),
                );
                if stored {
                    *pending_progress = None;
                    *last_progress_write = Some(Instant::now());
                } else {
                    *pending_progress = Some(record);
                }
                bench_log!(
                    "bench: storage_progress action=write ok={} book_id={} page={} elapsed_ms={} t_ms={}",
                    stored,
                    record.book_id,
                    record.screen,
                    progress_start.elapsed().as_millis(),
                    Instant::now().as_millis(),
                );
            } else {
                *pending_progress = Some(record);
                bench_log!(
                    "bench: storage_progress action=coalesce book_id={} page={} t_ms={}",
                    record.book_id,
                    record.screen,
                    Instant::now().as_millis(),
                );
            }
        }
    }
}

/// Step one of a book-open transaction: get the departing book's page onto
/// the card, and clear anything the coalescer was still holding for it.
///
/// Returns whether the open may proceed. A refusal leaves the reader entirely
/// on the old book, with that book's position still owed and retried by the
/// next flush — there is no half-applied switch to reconcile later, which is
/// what lets the pending state stay a single latest-value slot.
pub fn close_out_departing_book(
    card: &mut impl Card,
    sd_library: &ReaderStore,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
    pending_place: &mut Option<PendingPlace>,
    previous: PersistedAppState,
) -> bool {
    // A coalesced record for another book still has to land: it names that
    // book in the global state file, and this transaction is about to point
    // that file somewhere else.
    if pending_progress.is_some_and(|pending| pending.book_id != previous.book_id)
        && !flush_pending_progress(
            card,
            sd_library,
            pending_progress,
            last_progress_write,
            pending_place,
        )
    {
        return false;
    }
    // A book the store holds nothing under was neither opened nor restored
    // this session, so its position is already on the card and the page in
    // hand is a default. Writing it would put that guess over the real one,
    // or under whatever book a rescan has since put at that row.
    if ReaderSource::from_book_id(previous.book_id).is_sd()
        && !sd_library.holds_book(previous.book_id)
    {
        slog!(
            "storage: nothing held for departing book_id={}; nothing to close out",
            previous.book_id
        );
        *pending_progress = None;
        return true;
    }
    let record = record_for_persisted(sd_library, previous);
    let start = Instant::now();
    // Preserve a held place; otherwise the departing position may replace it.
    let stored = book_build::store_book_position(
        card,
        sd_library,
        record,
        place_may_be_replaced(pending_place, &record),
    );
    bench_log!(
        "bench: store_book_position ok={} book_id={} page={} elapsed_ms={} t_ms={}",
        stored,
        record.book_id,
        record.screen,
        start.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
    if stored {
        // Whatever the coalescer held for this book is now on the card, and
        // the global half of it is about to be rewritten by step three.
        *pending_progress = None;
    } else {
        // Keep it owed so the next flush retries it; the reader is staying on
        // this book, so the record is still the right one to write.
        *pending_progress = Some(record);
    }
    stored
}

pub fn source_identity(library: &ReaderStore, book_id: u32) -> (u32, u32) {
    library.current_catalog_identity(book_id)
}

pub fn record_for_persisted(library: &ReaderStore, state: PersistedAppState) -> AppStateRecord {
    // The loaded book's own identity when the state is for it, not its row's:
    // the catalog may have been rebuilt under that row since it opened.
    let (source_hash, source_size) = library.persisted_identity(state.book_id);
    let chapter = if ReaderSource::from_book_id(state.book_id).is_sd()
        && library.loaded_index == ReaderStore::selected_book_index(state.book_id)
    {
        library.current_chapter()
    } else {
        state.chapter
    };
    AppStateRecord {
        book_id: state.book_id,
        chapter,
        screen: state.screen,
        shell_orientation: state.shell_orientation,
        reading_orientation: state.reading_orientation,
        refresh_policy: state.refresh_policy,
        font_size: state.font_size,
        line_spacing: state.line_spacing,
        font_weight: state.font_weight,
        font_family: state.font_family,
        front_buttons: state.front_buttons,
        source_hash,
        source_size,
        // Freshly derived from the active entry, so always the current
        // interpretation; the flag exists for records read back from the
        // card.
        legacy_source_identity: false,
    }
}

/// Where the reader is in the book at `index`.
///
/// The book's own position file is authoritative. It is written when that book
/// is left and read when it is opened, so it can only ever describe this book —
/// which is the whole point of keeping it: the global state record is a single
/// slot that names one book, and reading position out of it is what let a stale
/// record hand one book's page to another.
///
/// The record's own `chapter`/`screen` are a mirror, still written so MarigoldOS
/// (which reads position from the global file) keeps resuming from cards this
/// firmware wrote. They are consulted only when the per-book file is missing or
/// fails its checksum, and they are safe in that role because the identity that
/// selected this book came from the very same record.
pub fn book_position(
    card: &mut impl Card,
    library: &ReaderStore,
    index: u16,
    mirror: AppStateRecord,
) -> (u16, u32) {
    // The boot mirror only understands a page, so a place resolves to the
    // chapter it names and page zero inside it. The open that follows refines
    // it against the pagination it builds, the same way an ordinary open does.
    match book_build::load_place(card, library, usize::from(index)) {
        // If absent or unreadable, fall back to the mirror position.
        None | Some(book_build::SavedPlace::Unreadable) => {
            slog!(
                "restore: no per-book position for index={}; using the global mirror",
                index
            );
            (mirror.chapter, mirror.screen)
        }
        Some(place) => place.provisional(),
    }
}

/// Whether durable reader state has been handed to the app this boot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StateRestore {
    /// Not yet tried: no catalog with entries has landed.
    #[default]
    Pending,
    /// Handed over, or there was nothing to hand over.
    Done,
    /// Tried, and the catalog held no book under the saved identity, usually
    /// because the book moved on a computer. The settings were sent anyway.
    Missed,
}

/// One boot-time attempt to map durable reader state back onto the scanned
/// catalog by stable source identity (path hash + byte size) and hand the
/// saved position to the app as a `Restored` event. The volatile book id
/// stored in the record is never trusted directly.
///
/// `retry_missed` retries a lookup that found nothing, after a rescan. Only
/// a caller about to name the book to open may set it, because `Restored`
/// changes the current book.
pub fn restore_saved_state(
    card: &mut impl Card,
    host: &mut impl Host,
    library: &mut ReaderStore,
    state_restored: &mut StateRestore,
    retry_missed: bool,
) {
    let due = match *state_restored {
        StateRestore::Pending => true,
        StateRestore::Missed => retry_missed,
        StateRestore::Done => false,
    };
    if !due || library.catalog_is_empty() {
        return;
    }
    *state_restored = StateRestore::Done;
    let Some(record) = book_build::load_app_state(card) else {
        slog!("restore: no usable durable state");
        return;
    };
    let Some(index) = crate::library_sd::find_index_by_identity(
        card,
        record.source_hash,
        record.source_size,
        record.legacy_source_identity,
    ) else {
        slog!(
            "restore: no catalog match hash={:08x} size={}",
            record.source_hash,
            record.source_size
        );
        *state_restored = StateRestore::Missed;
        // Send the settings even without the book, or the next save writes
        // the defaults over them.
        host.send_required(&LibraryEvent::SettingsRestored {
            reading_orientation: record.reading_orientation,
            refresh_policy: record.refresh_policy,
            font_size: record.font_size,
            line_spacing: record.line_spacing,
            font_weight: record.font_weight,
            font_family: record.font_family,
            front_buttons: record.front_buttons,
        });
        return;
    };
    // Stage the restored book's catalog entry so the position, colophon, and
    // page-count reads below resolve it, and so the first Home paint names it
    // before any open.
    crate::library_sd::load_active_entry(card, library, usize::from(index));
    // The app is about to hold this book under `index` without opening it,
    // and staging the row under the Library cursor replaces the active entry.
    // Without this, the save as the reader leaves it has no identity to name.
    if !library.adopt_active_as_reading_book(usize::from(index)) {
        slog!(
            "restore: index={} not staged; its departure cannot be saved",
            index
        );
    }
    let (chapter, screen) = book_position(card, library, index, record);
    slog!(
        "restore: index={} chapter={} screen={}",
        index,
        chapter,
        screen
    );
    // Resolve the chapter title now so wake-to-Home (rendered before the book
    // is opened) names the chapter; without this the colophon shows a numeral
    // until the book is first opened this session.
    book_build::load_chapter_title(card, usize::from(index), chapter, library);
    // The book's total page count, so the Home progress bar has a denominator
    // on wake before the book is opened (read from the cache index header).
    let page_count = book_build::restore_book_page_count(card, usize::from(index), library);
    host.send_required(&LibraryEvent::Restored {
        book_id: ReaderSource::sd(index).book_id(),
        chapter,
        page: screen,
        page_count,
        reading_orientation: record.reading_orientation,
        refresh_policy: record.refresh_policy,
        font_size: record.font_size,
        line_spacing: record.line_spacing,
        font_weight: record.font_weight,
        font_family: record.font_family,
        front_buttons: record.front_buttons,
    });
}

/// Scan the card after writing any coalesced position, or not at all. The
/// scan borrows the text arena and drops the resident pages, after which the
/// reader's page has no anchor and its place record cannot be written. False
/// when the write was refused, which leaves it owed and the pages resident.
fn scan_books_after_flush(
    card: &mut impl Card,
    sd_library: &mut ReaderStore,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
    pending_place: &mut Option<PendingPlace>,
    catalog_refresh: &mut CatalogRefresh,
) -> bool {
    if !flush_pending_progress(
        card,
        sd_library,
        pending_progress,
        last_progress_write,
        pending_place,
    ) {
        slog!("sd: the reading position would not save; not scanning over it");
        return false;
    }
    // A scan that lands is the refresh any refused one was owed. One that
    // fails leaves it as it was: still owed after a pick's scan, and settled
    // by the RefreshCatalog arm after the refresh's own.
    if crate::library_sd::scan_books(card, sd_library) {
        *catalog_refresh = CatalogRefresh::default();
    }
    true
}

pub fn flush_pending_progress(
    card: &mut impl Card,
    sd_library: &ReaderStore,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
    pending_place: &mut Option<PendingPlace>,
) -> bool {
    if let Some(record) = *pending_progress {
        let start = Instant::now();
        let may_replace = place_may_be_replaced(pending_place, &record);
        let stored = book_build::store_app_state(card, sd_library, record, may_replace);
        if stored {
            *pending_progress = None;
            *last_progress_write = Some(Instant::now());
        }
        bench_log!(
            "bench: storage_progress action=flush ok={} book_id={} page={} elapsed_ms={} t_ms={}",
            stored,
            record.book_id,
            record.screen,
            start.elapsed().as_millis(),
            Instant::now().as_millis(),
        );
        stored
    } else {
        true
    }
}
