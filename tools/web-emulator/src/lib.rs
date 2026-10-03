//! Browser build of the X4 emulator: the firmware's reducer, renderer, and
//! reading surface compiled to wasm32-unknown-unknown behind a small raw
//! C ABI. The page's JS feeds key presses, a monotonic clock, and fetched
//! book bodies in; it reads frames back as RGBA straight out of wasm memory.
//! No wasm-bindgen — the surface is a handful of scalars and buffer pointers.

mod books;
mod library;

use app_core::{
    AppView, Button, DisplayOrientation, InputEvent, LibraryEvent, LibraryMenu, ReaderSource,
    RefreshPlanner, RenderKind, StorageCommand, SyncEvent, SyncStatus, WifiSsid, MAX_SD_CHAPTERS,
};
use books::{BookStore, SHELF};
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use display::font::{draw_text, literata, measure_text, FontStyle, TypeSettings};
use display::{HEIGHT, WIDTH};
use library::Row;
use ui::app_render::{
    render_request as render_shared, render_sleep as render_shared_sleep, UiRenderModel,
};
use ui::reading::ReadingBlocks;
use ui::{UiBook, UiLibraryStatus, UiTocItem};

const PAPER: [u8; 3] = [238, 236, 226];
const INK: [u8; 3] = [22, 22, 20];

/// Simulated card latencies, in ms of the caller's clock.
const OPEN_BOOK_MS: f64 = 650.0;
const REOPEN_BOOK_MS: f64 = 180.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadStatus {
    Empty,
    Loading,
    Ready,
}

enum Op {
    FinishOpen { book_index: u16 },
    Sync(SyncEvent),
}

struct WebEmulator {
    state: app_core::ReaderState,
    ctx: app_core::ReducerContext,
    planner: RefreshPlanner,
    fb: Framebuffer,
    rgba: Vec<u8>,
    sleeping: bool,
    store: Option<BookStore>,
    store_book: Option<u16>,
    load_status: LoadStatus,
    /// Shelf index whose body the page should fetch (`x4_book_wanted`);
    /// cleared when `x4_book_ready` delivers it.
    wanted_book: Option<u16>,
    /// The folder the Library is listing, `/`-separated from the root; the
    /// storage task's browse position.
    library_path: String,
    ops: Vec<(f64, Op)>,
    frame_seq: u32,
    last_refresh: u32,
    snapshot: [u32; 10],
}

impl WebEmulator {
    fn boot() -> Self {
        let mut emu = Self {
            state: app_core::ReaderState::boot(),
            ctx: app_core::ReducerContext::new(1, 4),
            planner: RefreshPlanner::new(),
            fb: Framebuffer::new(),
            rgba: vec![0; WIDTH * HEIGHT * 4],
            sleeping: false,
            store: None,
            store_book: None,
            load_status: LoadStatus::Empty,
            wanted_book: None,
            library_path: String::new(),
            ops: Vec::new(),
            frame_seq: 0,
            last_refresh: 0,
            snapshot: [0; 10],
        };
        emu.state = emu.state.apply_library_event(
            emu.ctx,
            LibraryEvent::Scanned {
                count: SHELF.len() as u16,
                // The fake shelf is fixed for the life of the page, so it
                // never leaves its first catalog epoch.
                catalog_epoch: 1,
            },
        );
        // The storage task follows every scan with the listing the Library
        // draws from; the reducer takes the book/folder split from it, and
        // without that split the actions sheet treats no row as a book.
        emu.state = emu.state.apply_library_event(
            emu.ctx,
            folder_listed(
                None,
                emu.state.library_browse_epoch.wrapping_add(1),
                &emu.library_path,
                0,
            ),
        );
        emu.restore_active_book(ReaderSource::sd(0).book_id(), 0, 0);
        // The firmware's boot probe of /READER/WIFI.BIN, pretended: a saved
        // network so the Wireless screen opens on the connect/forget offer.
        // Forgetting it exposes the portal flow, which "saves" it back.
        emu.state = emu
            .state
            .apply_sync_event(SyncEvent::NetworkSaved(home_network()));
        emu.open_or_await_book(0);
        emu.render(RenderKind::Boot);
        emu
    }

    /// Hydrate `book_index` now if its body has been delivered; otherwise
    /// ask the page to fetch it and leave a poll op behind so the metadata
    /// (and any loading plate) resolves when `x4_book_ready` lands.
    fn open_or_await_book(&mut self, book_index: u16) {
        if self.hydrate_book_metadata(book_index) {
            self.apply_loaded_metadata(book_index);
            return;
        }
        self.load_status = LoadStatus::Loading;
        self.wanted_book = Some(book_index);
        self.ops
            .retain(|(_, op)| !matches!(op, Op::FinishOpen { .. }));
        self.ops.push((0.0, Op::FinishOpen { book_index }));
    }

    fn input(&mut self, button: Button, now: f64) {
        if button == Button::Power {
            if self.sleeping {
                self.wake();
            } else {
                self.sleeping = true;
                self.render_sleep();
            }
            return;
        }
        if self.sleeping {
            return;
        }
        let previous = self.state;
        self.state = self.state.apply_input(
            self.ctx,
            InputEvent::Sample {
                button: Some(button),
                aux_raw: 2000,
                nav_raw: 0,
                page_raw: 0,
                battery_mv: 4012,
                battery_percent: 87,
            },
        );
        // A Library pick waits for storage to say what the row is. The shelf
        // answers at once, before the open below is read off the transition:
        // a row that is a book moves the reader into Reading, and that move
        // owes the open exactly as a keypress into Reading does.
        if let Some(command) =
            app_core::library_browse_command_for_transition(&previous, &self.state)
        {
            self.answer_library_move(command);
        }

        if let Some(command) = storage_command_for_transition(previous, self.state) {
            match command {
                StorageCommand::OpenBook { book_id, .. } => {
                    let book_index = ReaderSource::from_book_id(book_id).sd_index().unwrap_or(0);
                    let warm = self.store_ready_for(book_index);
                    if !warm {
                        self.load_status = LoadStatus::Loading;
                    }
                    // Start the page's fetch now, in parallel with the
                    // simulated card latency — or, when the body is already
                    // cached, drop any want left over from a superseded open
                    // so the page stops retrying a book nobody is waiting on.
                    self.wanted_book = book_text(book_index).is_none().then_some(book_index);
                    let delay = if warm { REOPEN_BOOK_MS } else { OPEN_BOOK_MS };
                    // A new open supersedes any earlier one still waiting
                    // on its body.
                    self.ops
                        .retain(|(_, op)| !matches!(op, Op::FinishOpen { .. }));
                    self.ops.push((now + delay, Op::FinishOpen { book_index }));
                }
                StorageCommand::ExtendSection { .. } | StorageCommand::StoreProgress(_) => {
                    // The whole fake book is resident; progress persistence
                    // is the page's job via the snapshot buffer.
                }
                _ => {}
            }
        }
        if let Some(StorageCommand::ClearBookCache { request_id, .. }) =
            app_core::library_action_command_for_transition(&previous, &self.state)
        {
            // The fake shelf has no on-card cache; the clear settles
            // instantly and always succeeds.
            self.state = self.state.apply_library_event(
                self.ctx,
                LibraryEvent::CacheCleared {
                    request_id,
                    ok: true,
                },
            );
        }

        if self.state.sync_status == SyncStatus::Starting
            && previous.sync_status != SyncStatus::Starting
        {
            if self.state.wifi_network_saved() {
                // The real session joins the network and then serves the book
                // upload page until the user finishes; there is no separate
                // progress-exchange step to pretend at.
                self.ops
                    .push((now + 500.0, Op::Sync(SyncEvent::Connecting)));
                self.ops.push((
                    now + 1600.0,
                    Op::Sync(SyncEvent::Connected([192, 168, 1, 27])),
                ));
                self.ops.push((
                    now + 2600.0,
                    Op::Sync(SyncEvent::Serving([192, 168, 1, 27])),
                ));
            } else {
                // No saved network: the onboarding hotspot comes up (with
                // the fixed demo PSK in place of the per-session one the
                // firmware mints) and a pretend phone submits credentials
                // a few seconds later.
                self.ops.push((
                    now + 900.0,
                    Op::Sync(SyncEvent::PortalUp(
                        app_core::PortalPsk::EMULATOR_DEMO,
                        app_core::PortalSsid::EMULATOR_DEMO,
                    )),
                ));
                self.ops.push((
                    now + 7000.0,
                    Op::Sync(SyncEvent::CredentialsSaved(home_network())),
                ));
            }
        }
        if self.state.view != AppView::Wireless {
            self.ops.retain(|(_, op)| !matches!(op, Op::Sync(_)));
        }

        self.render(RenderKind::Page);
    }

    /// Deep sleep does not return on the device: waking is a boot. It keeps
    /// only what was saved (the book, its place, the settings); a sheet that
    /// was up and the folder the Library was in are gone, and the boot's scan
    /// relists the root. Without this the page woke with the reading
    /// key sheet or the actions sheet still up, acting on the next press.
    fn wake(&mut self) {
        self.sleeping = false;
        self.state.view = AppView::Home;
        self.state.selection = 0;
        self.state.reading_sheet = false;
        self.state.library_menu = LibraryMenu::None;
        self.library_path.clear();
        self.state = self.state.apply_library_event(
            self.ctx,
            folder_listed(
                None,
                self.state.library_browse_epoch.wrapping_add(1),
                &self.library_path,
                0,
            ),
        );
        self.render(RenderKind::Page);
    }

    fn tick(&mut self, now: f64) {
        let mut due: Vec<Op> = Vec::new();
        self.ops.retain_mut(|(deadline, op)| {
            if *deadline <= now {
                due.push(match op {
                    Op::FinishOpen { book_index } => Op::FinishOpen {
                        book_index: *book_index,
                    },
                    Op::Sync(event) => Op::Sync(*event),
                });
                false
            } else {
                true
            }
        });
        for op in due {
            match op {
                Op::FinishOpen { book_index } => {
                    if book_text(book_index).is_none() {
                        // Body still in flight from the page; keep the
                        // loading plate up and poll again shortly.
                        self.ops.push((now + 50.0, Op::FinishOpen { book_index }));
                    } else {
                        self.finish_open(book_index)
                    }
                }
                Op::Sync(event) => {
                    if self.state.view == AppView::Wireless {
                        self.state = self.state.apply_sync_event(event);
                        self.render(RenderKind::Page);
                    }
                }
            }
        }
    }

    /// Stand in for the storage task's half of a move through the Library.
    /// Answered within the press that asked, so the reducer always takes the
    /// answer and the browse position can move with it. Moves keep the
    /// browse epoch; only a scan moves it.
    fn answer_library_move(&mut self, command: StorageCommand) {
        let epoch = self.state.library_browse_epoch;
        let event = match command {
            StorageCommand::ChooseLibraryRow {
                request_id, index, ..
            } => match library::listing(&self.library_path).get(usize::from(index)) {
                Some(&Row::Book(book)) => LibraryEvent::RowIsBook {
                    request_id,
                    index: book,
                    catalog_epoch: self.state.catalog_epoch,
                },
                Some(&Row::Folder(name)) => {
                    self.library_path = library::child_path(&self.library_path, name);
                    folder_listed(Some(request_id), epoch, &self.library_path, 0)
                }
                None => LibraryEvent::RowFailed { request_id },
            },
            StorageCommand::LeaveLibraryFolder { request_id, .. } => {
                // Back on the row the folder was entered from, found by name.
                let (parent, name) = library::split_last(&self.library_path);
                let selection = library::listing(parent)
                    .iter()
                    .position(|&row| matches!(row, Row::Folder(folder) if folder == name))
                    .unwrap_or(0);
                self.library_path = parent.to_owned();
                folder_listed(
                    Some(request_id),
                    epoch,
                    &self.library_path,
                    selection as u16,
                )
            }
            _ => return,
        };
        self.state = self.state.apply_library_event(self.ctx, event);
    }

    fn store_ready_for(&self, book_index: u16) -> bool {
        let layout_current = self.store.as_ref().is_some_and(|store| {
            store.type_settings() == self.state.type_settings()
                && ReadingBlocks::page_box(store)
                    == ui::reading::PageBox::for_portrait(app_core::is_portrait(
                        self.state.orientation,
                    ))
        });
        self.store_book == Some(book_index)
            && layout_current
            && self.load_status == LoadStatus::Ready
    }

    fn finish_open(&mut self, book_index: u16) {
        if !self.hydrate_book_metadata(book_index) {
            return;
        }
        self.apply_loaded_metadata(book_index);
        self.render(RenderKind::Page);
    }

    /// Build the paginated store for `book_index` if its body has arrived
    /// from the page; false while the fetch is still in flight.
    fn hydrate_book_metadata(&mut self, book_index: u16) -> bool {
        if self.store_ready_for(book_index) {
            return true;
        }
        let Some(text) = book_text(book_index) else {
            return false;
        };
        self.store = Some(BookStore::build(
            text,
            self.state.type_settings(),
            app_core::is_portrait(self.state.orientation),
        ));
        self.store_book = Some(book_index);
        self.load_status = LoadStatus::Ready;
        true
    }

    fn apply_loaded_metadata(&mut self, book_index: u16) {
        let store = self.store.as_ref().unwrap();
        let mut chapter_pages = [0u16; MAX_SD_CHAPTERS];
        for (slot, chapter) in chapter_pages.iter_mut().zip(store.chapters.iter()) {
            *slot = chapter.start_page;
        }
        let event = LibraryEvent::Loaded {
            book_id: ReaderSource::sd(book_index).book_id(),
            pages: store.page_count(),
            chapters: store.chapters.len().max(1).min(u16::MAX as usize) as u16,
            current_chapter: store.chapter_for_page(self.state.page),
            chapter_pages,
            // The browser build restores position from localStorage through
            // its own path, so a load never relocates the reader.
            position: None,
            // Only called where the browser build has just made a book's text
            // resident; it folds the event directly and always redraws.
            text_replaced: true,
        };
        self.state = self.state.apply_library_event(self.ctx, event);
    }

    fn restore_active_book(&mut self, book_id: u32, chapter: u32, page: u32) {
        self.state = self.state.apply_library_event(
            self.ctx,
            LibraryEvent::Restored {
                book_id,
                chapter: chapter.min(u16::MAX as u32) as u16,
                page,
                page_count: 0,
                reading_orientation: self.state.orientation as u8,
                refresh_policy: self.state.refresh_policy as u8,
                font_size: self.state.font_size as u8,
                line_spacing: self.state.line_spacing as u8,
                font_weight: self.state.font_weight as u8,
                font_family: self.state.font_family as u8,
                front_buttons: self.state.front_buttons as u8,
            },
        );
    }

    fn restore(&mut self, snapshot: [u32; 10]) {
        self.state = self.state.apply_library_event(
            self.ctx,
            LibraryEvent::Restored {
                book_id: snapshot[0],
                chapter: snapshot[1].min(u16::MAX as u32) as u16,
                page: snapshot[2],
                page_count: 0,
                reading_orientation: snapshot[3] as u8,
                refresh_policy: snapshot[4] as u8,
                font_size: snapshot[5] as u8,
                line_spacing: snapshot[6] as u8,
                font_weight: snapshot[7] as u8,
                font_family: snapshot[8] as u8,
                front_buttons: snapshot[9] as u8,
            },
        );
        let book_index = ReaderSource::from_book_id(snapshot[0])
            .sd_index()
            .unwrap_or(0);
        self.open_or_await_book(book_index);
        self.render(RenderKind::Page);
    }

    fn refresh_snapshot(&mut self) {
        let persisted = self.state.persisted();
        self.snapshot = [
            persisted.book_id,
            u32::from(persisted.chapter),
            persisted.screen,
            u32::from(persisted.reading_orientation),
            u32::from(persisted.refresh_policy),
            u32::from(persisted.font_size),
            u32::from(persisted.line_spacing),
            u32::from(persisted.font_weight),
            u32::from(persisted.font_family),
            u32::from(persisted.front_buttons),
        ];
    }

    fn render(&mut self, kind: RenderKind) {
        // Async work keeps completing while asleep (a book body arriving for
        // a pending FinishOpen, sync ops); state may hydrate, but nothing
        // may paint over the sleep screen. Waking re-renders explicitly.
        if self.sleeping {
            return;
        }
        let request = self.state.render_request(kind);
        let sd_reading =
            request.view == AppView::Reading && ReaderSource::from_book_id(request.book_id).is_sd();
        if sd_reading {
            self.fb
                .set_frame(ui::app_render::fb_frame(request.orientation));
            self.fb.clear(true);
            self.draw_reader_page(request);
            ui::app_render::render_reading_sheet_overlay(&mut self.fb, request);
        } else {
            self.draw_shell(request, false);
        }
        self.finish_frame(request);
    }

    fn render_sleep(&mut self) {
        let request = self.state.render_request(RenderKind::Page);
        self.draw_shell(request, true);
        self.blit();
        self.planner.record_sleep(true);
        self.last_refresh = RefreshMode::Full as u32 + 1;
        self.frame_seq = self.frame_seq.wrapping_add(1);
    }

    fn finish_frame(&mut self, request: app_core::RenderRequest) {
        let mode = self.planner.mode_for(request);
        self.planner.record_render(request, mode);
        self.last_refresh = mode as u32 + 1;
        self.blit();
        self.frame_seq = self.frame_seq.wrapping_add(1);
    }

    /// Everything except the SD reading page goes through the shared shell
    /// renderer, exactly as the firmware's `views::render` does.
    fn draw_shell(&mut self, request: app_core::RenderRequest, sleep: bool) {
        let book_index = ReaderSource::from_book_id(request.book_id)
            .sd_index()
            .unwrap_or(0) as usize
            % SHELF.len();
        let source = &SHELF[book_index];
        // The whole folder is resident, so the window is every row.
        let titles: Vec<ui::UiLibraryRow<'_>> = library::listing(&self.library_path)
            .into_iter()
            .map(|row| ui::UiLibraryRow {
                name: row.name(),
                is_folder: matches!(row, Row::Folder(_)),
            })
            .collect();

        let mut toc: Vec<UiTocItem<'_>> = Vec::new();
        let mut chapter_title = "";
        if self.store_book == Some(book_index as u16) && self.load_status == LoadStatus::Ready {
            if let Some(store) = self.store.as_ref() {
                for chapter in &store.chapters {
                    toc.push(UiTocItem {
                        title: chapter.title.as_str(),
                        level: 1,
                        page: u32::from(chapter.start_page) + 1,
                    });
                }
                if let Some(chapter) = store.chapters.get(request.chapter as usize) {
                    chapter_title = chapter.title.as_str();
                }
            }
        }

        let progress = if request.page_count > 1 {
            (((request.page + 1).min(request.page_count) as u64 * 1000) / request.page_count as u64)
                as u16
        } else {
            0
        };

        let model = UiRenderModel {
            active_book: UiBook {
                title: source.title,
                author: source.author,
                progress_permille: progress,
                cover: None,
            },
            library_status: UiLibraryStatus::Ready,
            library_entries: &titles,
            library_folder: library::split_last(&self.library_path).1,
            library_window_start: 0,
            chapters: &toc,
            chapters_window_start: 0,
            chapters_total: toc.len() as u16,
            chapter_title,
            custom_font_name: "",
        };
        if sleep {
            render_shared_sleep(&mut self.fb, request, &model);
        } else {
            render_shared(&mut self.fb, request, &model);
        }
    }

    /// The firmware's SD reading page: body blocks through `ui::reading`,
    /// the page-in-chapter counter in the footer, or the loading book plate
    /// while the pretend card is busy.
    fn draw_reader_page(&mut self, request: app_core::RenderRequest) {
        let book_index = ReaderSource::from_book_id(request.book_id)
            .sd_index()
            .unwrap_or(0);
        let ready = self.store_ready_for(book_index)
            || (self.store_book == Some(book_index) && self.load_status == LoadStatus::Ready);
        if !ready {
            let source = &SHELF[book_index as usize % SHELF.len()];
            // Straddle the panel's vertical center (X4: 232/268) so the plate
            // stays centered on the taller X3 instead of riding high.
            let mid = HEIGHT as i16 / 2;
            draw_centered(
                &mut self.fb,
                literata(FontStyle::Bold),
                source.title,
                mid - 8,
            );
            draw_centered(
                &mut self.fb,
                literata(FontStyle::Italic),
                source.author,
                mid + 28,
            );
            return;
        }
        let store = self.store.as_ref().unwrap();
        let page = store.page(request.page);
        ui::reading::draw_reading_page_body(&mut self.fb, store, page);

        let (current, total) = store.chapter_page_position(request.page);
        let label = format!("{}/{}", current, total);
        ui::reading::draw_reading_page_counter_aligned(
            &mut self.fb,
            &label,
            request.orientation == DisplayOrientation::LandscapeButtonsTop,
        );
    }

    /// Convert the panel-mounted framebuffer (rows mirrored for the X4's
    /// upside-down panel) to viewer-oriented RGBA.
    fn blit(&mut self) {
        let flip_y = !display::DEVICE_IS_X3;
        for y in 0..HEIGHT {
            let src_y = if flip_y { HEIGHT - 1 - y } else { y };
            for x in 0..WIDTH {
                let color = if self.fb.native_pixel(x, src_y) {
                    PAPER
                } else {
                    INK
                };
                let offset = (y * WIDTH + x) * 4;
                self.rgba[offset] = color[0];
                self.rgba[offset + 1] = color[1];
                self.rgba[offset + 2] = color[2];
                self.rgba[offset + 3] = 255;
            }
        }
    }
}

fn home_network() -> WifiSsid {
    WifiSsid::new("HOME-WIFI").unwrap()
}

/// The listing of the folder at `path`, as the storage task reports one.
fn folder_listed(
    request_id: Option<u32>,
    browse_epoch: u32,
    path: &str,
    selection: u16,
) -> LibraryEvent {
    let rows = library::listing(path);
    let books = rows
        .iter()
        .filter(|row| matches!(row, Row::Book(_)))
        .count();
    LibraryEvent::FolderListed {
        request_id,
        browse_epoch,
        depth: library::depth(path),
        count: rows.len() as u16,
        books: books as u16,
        selection,
    }
}

// ---------------------------------------------------------------------------
// Runtime book delivery. Only the shelf metadata is compiled in; the page
// fetches each body (`_site/books/*.txt`) on demand and hands it over through
// `x4_book_alloc` + `x4_book_ready`. Same single-threaded static-cell rules
// as EMULATOR below.

const BOOK_NONE: Option<String> = None;
static mut BOOK_TEXTS: [Option<String>; SHELF.len()] = [BOOK_NONE; SHELF.len()];
static mut INCOMING_BOOK: Vec<u8> = Vec::new();

/// Delivered body of a shelf entry. The reference is only sound until the
/// page redelivers that index; callers use it transiently to build a store.
fn book_text(book_index: u16) -> Option<&'static str> {
    unsafe {
        #[allow(static_mut_refs)]
        BOOK_TEXTS[book_index as usize % SHELF.len()].as_deref()
    }
}

fn draw_centered(
    fb: &mut Framebuffer,
    font: &'static display::font::BitmapFont,
    text: &str,
    y: i16,
) {
    let width = measure_text(font, text) as i16;
    draw_text(fb, font, text, (WIDTH as i16 - width) / 2, y, false);
}

/// The desktop emulator's transition-to-storage-command mapping, verbatim.
fn storage_command_for_transition(
    previous: app_core::ReaderState,
    next: app_core::ReaderState,
) -> Option<StorageCommand> {
    let index = ReaderSource::from_book_id(next.book_id).sd_index()?;
    if next.view != AppView::Reading {
        return None;
    }

    if previous.book_id != next.book_id
        || previous.chapter != next.chapter
        || previous.view != AppView::Reading
    {
        return Some(StorageCommand::OpenBook {
            // The emulated shelf is one catalog that stands still, so no open
            // here is fenced to a generation it could fall out of.
            catalog_epoch: None,
            request_id: 0,
            book_id: next.book_id,
            index,
            chapter: next.chapter,
            target_pages: 5,
            type_settings: next.type_settings(),
            portrait: app_core::is_portrait(next.orientation),
            previous: (previous.book_id != next.book_id).then(|| previous.persisted()),
            // The emulator has no card, so no copy has a stored place to
            // resolve; its opens land on the page they name.
            resolve_place: false,
        });
    }

    if next.page.saturating_add(2) >= next.sd_page_count {
        return Some(StorageCommand::ExtendSection {
            request_id: 0,
            book_id: next.book_id,
            index,
            chapter: next.chapter,
            target_pages: next.page.saturating_add(5).min(u16::MAX as u32) as u16,
            type_settings: next.type_settings(),
            portrait: app_core::is_portrait(next.orientation),
        });
    }

    if previous.page != next.page {
        return Some(StorageCommand::StoreProgress(next.persisted()));
    }

    None
}

// ---------------------------------------------------------------------------
// Raw wasm ABI. Single-threaded by construction; the browser calls in on one
// thread and every export goes through the same static cell.

static mut EMULATOR: Option<WebEmulator> = None;

fn emulator() -> &'static mut WebEmulator {
    unsafe {
        #[allow(static_mut_refs)]
        EMULATOR.get_or_insert_with(WebEmulator::boot)
    }
}

#[no_mangle]
pub extern "C" fn x4_boot() {
    emulator();
}

#[no_mangle]
pub extern "C" fn x4_key(button: u32, now_ms: f64) {
    let button = match button {
        0 => Button::Power,
        1 => Button::Back,
        2 => Button::Confirm,
        3 => Button::Previous,
        4 => Button::Next,
        5 => Button::PagePrevious,
        _ => Button::PageNext,
    };
    emulator().input(button, now_ms);
}

#[no_mangle]
pub extern "C" fn x4_tick(now_ms: f64) {
    emulator().tick(now_ms);
}

#[no_mangle]
pub extern "C" fn x4_frame_ptr() -> *const u8 {
    emulator().rgba.as_ptr()
}

#[no_mangle]
pub extern "C" fn x4_frame_seq() -> u32 {
    emulator().frame_seq
}

/// Refresh mode of the most recent panel write: 0 none, 1 full, 2 fast,
/// 3 fast-clean (RefreshMode discriminant + 1).
#[no_mangle]
pub extern "C" fn x4_last_refresh() -> u32 {
    emulator().last_refresh
}

#[no_mangle]
pub extern "C" fn x4_sleeping() -> u32 {
    emulator().sleeping as u32
}

#[no_mangle]
pub extern "C" fn x4_snapshot_ptr() -> *const u32 {
    let emu = emulator();
    emu.refresh_snapshot();
    emu.snapshot.as_ptr()
}

#[no_mangle]
pub extern "C" fn x4_restore(
    book_id: u32,
    chapter: u32,
    page: u32,
    orientation: u32,
    policy: u32,
    size: u32,
    spacing: u32,
    weight: u32,
    family: u32,
    front_buttons: u32,
) {
    emulator().restore([
        book_id,
        chapter,
        page,
        orientation,
        policy,
        size,
        spacing,
        weight,
        family,
        front_buttons,
    ]);
}

/// Shelf index of the book body the emulator is waiting on, or -1. The page
/// polls this each frame and answers with `x4_book_alloc` + `x4_book_ready`.
#[no_mangle]
pub extern "C" fn x4_book_wanted() -> i32 {
    match emulator().wanted_book {
        Some(index) => i32::from(index),
        None => -1,
    }
}

/// Stage an incoming book body: returns a `len`-byte buffer for the page to
/// copy the fetched text into. Growing the buffer may move wasm memory, so
/// the page must take its view of memory after this call.
#[no_mangle]
pub extern "C" fn x4_book_alloc(len: u32) -> *mut u8 {
    unsafe {
        #[allow(static_mut_refs)]
        {
            INCOMING_BOOK.clear();
            INCOMING_BOOK.resize(len as usize, 0);
            INCOMING_BOOK.as_mut_ptr()
        }
    }
}

/// Commit the staged buffer as shelf entry `index`'s body. Any open waiting
/// on it (the loading plate, or boot's Continue metadata) resolves on the
/// next `x4_tick` via the poll op the open left behind.
#[no_mangle]
pub extern "C" fn x4_book_ready(index: u32) {
    let index = index as usize % SHELF.len();
    unsafe {
        #[allow(static_mut_refs)]
        {
            let bytes = core::mem::take(&mut INCOMING_BOOK);
            BOOK_TEXTS[index] = Some(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    let emu = emulator();
    if emu.wanted_book == Some(index as u16) {
        emu.wanted_book = None;
    }
}

// TypeSettings is compared in store_ready_for; keep the import used even in
// builds where inference would otherwise drop it.
#[allow(dead_code)]
fn _settings_witness(settings: TypeSettings) -> TypeSettings {
    settings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, PoisonError};

    /// The book bodies and the page's delivery buffer are process-wide
    /// statics, so tests take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    const BODIES: [&str; SHELF.len()] = [
        include_str!("../books/alice.txt"),
        include_str!("../books/carol.txt"),
        include_str!("../books/aesop.txt"),
        include_str!("../books/pegana.txt"),
        include_str!("../books/timemachine.txt"),
        include_str!("../books/warworlds.txt"),
        include_str!("../books/mars.txt"),
        include_str!("../books/lastmen.txt"),
    ];

    /// One frame of the page's loop: deliver the body the emulator asked
    /// for, the way `index.html`'s `deliverBook` does, then tick.
    fn pump(emu: &mut WebEmulator, now: f64) {
        if let Some(index) = emu.wanted_book.take() {
            // SAFETY: the caller holds SERIAL, so no other test is reading or
            // writing the book bodies.
            unsafe {
                #[allow(static_mut_refs)]
                {
                    BOOK_TEXTS[usize::from(index)] = Some(BODIES[usize::from(index)].to_string());
                }
            }
        }
        emu.tick(now);
    }

    fn settle(emu: &mut WebEmulator, from: f64) -> f64 {
        let mut now = from;
        for _ in 0..40 {
            now += 50.0;
            pump(emu, now);
        }
        now
    }

    fn booted() -> WebEmulator {
        let mut emu = WebEmulator::boot();
        settle(&mut emu, 0.0);
        emu
    }

    #[test]
    fn library_pick_opens_the_chosen_book() {
        let _turn = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let mut emu = booted();
        assert_eq!(emu.state.view, AppView::Home);
        emu.input(Button::Back, 3000.0);
        assert_eq!(emu.state.view, AppView::Library);
        emu.input(Button::Next, 3100.0);
        emu.input(Button::Confirm, 3200.0);
        assert_eq!(emu.state.view, AppView::Reading, "the pick must not hang");
        let picked = reading_shelf_index(&emu);
        assert_eq!(SHELF[usize::from(picked)].title, "Aesop's Fables");
        settle(&mut emu, 3200.0);
        assert!(
            emu.store_ready_for(picked),
            "the chosen book's text is laid out"
        );
        assert!(emu.state.sd_page_count > 1);
    }

    #[test]
    fn library_walks_into_folders_and_back_out() {
        let _turn = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let mut emu = booted();
        emu.input(Button::Back, 3000.0);
        // The root's last row is its one folder, below the four books.
        emu.input(Button::Previous, 3100.0);
        assert_eq!(emu.state.selection, 4);
        emu.input(Button::Confirm, 3200.0);
        assert_eq!(emu.library_path, "Science Fiction");
        assert_eq!((emu.state.library_depth, emu.state.library_count), (1, 3));
        assert_eq!(emu.state.library_books, 2);
        // Its folder sits below its two books.
        emu.input(Button::Previous, 3300.0);
        emu.input(Button::Confirm, 3400.0);
        assert_eq!(emu.library_path, "Science Fiction/H. G. Wells");
        assert_eq!(emu.state.library_depth, 2);
        // Back leaves one level, onto the folder that was entered.
        emu.input(Button::Back, 3500.0);
        assert_eq!(emu.state.view, AppView::Library);
        assert_eq!(emu.library_path, "Science Fiction");
        assert_eq!(emu.state.selection, 2);
        emu.input(Button::Confirm, 3600.0);
        emu.input(Button::Next, 3700.0);
        emu.input(Button::Confirm, 3800.0);
        assert_eq!(emu.state.view, AppView::Reading);
        let picked = reading_shelf_index(&emu);
        assert_eq!(SHELF[usize::from(picked)].title, "The War of the Worlds");
        settle(&mut emu, 3800.0);
        assert!(emu.store_ready_for(picked));
    }

    fn reading_shelf_index(emu: &WebEmulator) -> u16 {
        ReaderSource::from_book_id(emu.state.book_id)
            .sd_index()
            .expect("reading a shelf book")
    }

    /// Waking is a boot on the device, so a sheet left up and the folder the
    /// Library was in do not survive it.
    #[test]
    fn waking_drops_a_sheet_and_the_folder_the_library_was_in() {
        let _turn = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let mut emu = booted();
        emu.input(Button::Back, 3000.0);
        emu.input(Button::Previous, 3100.0);
        emu.input(Button::Confirm, 3200.0);
        assert_eq!(emu.library_path, "Science Fiction");
        emu.input(Button::PagePrevious, 3300.0);
        assert!(matches!(emu.state.library_menu, LibraryMenu::Sheet { .. }));

        emu.input(Button::Power, 3400.0);
        emu.input(Button::Power, 3500.0);
        assert_eq!(emu.state.view, AppView::Home);
        assert_eq!(emu.state.library_menu, LibraryMenu::None);
        assert_eq!(emu.library_path, "");
        assert_eq!((emu.state.library_depth, emu.state.library_count), (0, 5));

        // Into the Library: the root, no sheet, and OK picks a row rather
        // than confirming a cache clear.
        emu.input(Button::Back, 3600.0);
        assert_eq!(emu.state.view, AppView::Library);
        assert_eq!(emu.state.library_menu, LibraryMenu::None);
        assert_eq!(emu.state.selection, 0);
        emu.input(Button::Confirm, 3700.0);
        assert_eq!(emu.state.view, AppView::Reading);
    }

    /// The portrait reading key sheet, likewise: Reading reopens on the page,
    /// not on a sheet whose labels the next press would act on.
    #[test]
    fn waking_drops_the_reading_key_sheet() {
        let _turn = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let mut emu = booted();
        emu.input(Button::Confirm, 3000.0);
        assert_eq!(emu.state.view, AppView::Reading);
        emu.input(Button::Confirm, 3100.0);
        assert!(
            emu.state.reading_sheet,
            "portrait Confirm raises the key sheet"
        );

        emu.input(Button::Power, 3200.0);
        emu.input(Button::Power, 3300.0);
        emu.input(Button::Confirm, 3400.0);
        assert_eq!(emu.state.view, AppView::Reading);
        assert!(!emu.state.reading_sheet);
    }

    #[test]
    fn library_actions_sheet_settles_a_cache_clear() {
        let _turn = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let mut emu = booted();
        emu.input(Button::Back, 3000.0);
        emu.input(Button::PagePrevious, 3100.0);
        assert!(
            matches!(emu.state.library_menu, app_core::LibraryMenu::Sheet { .. }),
            "every shelf row is a book, so each has the actions sheet"
        );
        emu.input(Button::Confirm, 3200.0);
        assert!(matches!(
            emu.state.library_menu,
            app_core::LibraryMenu::Done { ok: true, .. }
        ));
    }
}
