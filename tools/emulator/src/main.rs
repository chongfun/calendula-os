#[cfg(not(feature = "device-x3"))]
mod panel;
#[cfg(feature = "device-x3")]
#[path = "panel_uc8253.rs"]
mod panel;
mod panel_common;
mod render;
mod scenario;

use app_core::{
    touch::{GestureConfig, GestureEvent, RecognizerPhase, TouchPoint, TouchRecognizer},
    AppView, Button, InputEvent, LibraryEvent, ReaderSource, RefreshPlanner, StorageCommand,
};
use display::fb::FbFrame;
#[cfg(feature = "gui")]
use display::{HEIGHT, WIDTH};
#[cfg(feature = "gui")]
use eframe::egui;
use panel::PanelModel;
#[cfg(feature = "gui")]
use render::framebuffer_to_color_image;
use render::{write_png, write_presented_png};
use scenario::Scenario;
use std::env;
use std::path::{Path, PathBuf};
use ui::legacy_touch::{route_gesture, LegacyTouchContext, LegacyTouchOutcome};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse()?;
    if args.gui {
        run_gui(args)?;
    } else {
        run_headless(args)?;
    }
    Ok(())
}

struct Args {
    scenario: Option<PathBuf>,
    sd_root: Option<PathBuf>,
    dump: Option<PathBuf>,
    present_dump: Option<PathBuf>,
    check: Option<PathBuf>,
    gui: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut scenario = None;
        let mut sd_root = None;
        let mut dump = None;
        let mut present_dump = None;
        let mut check = None;
        let mut gui = false;
        let mut iter = env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--scenario" => {
                    scenario = Some(PathBuf::from(iter.next().ok_or("--scenario needs a path")?))
                }
                "--sd-root" => {
                    sd_root = Some(PathBuf::from(iter.next().ok_or("--sd-root needs a path")?))
                }
                "--dump" => dump = Some(PathBuf::from(iter.next().ok_or("--dump needs a path")?)),
                "--present-dump" => {
                    present_dump = Some(PathBuf::from(
                        iter.next().ok_or("--present-dump needs a path")?,
                    ))
                }
                "--check" => {
                    check = Some(PathBuf::from(iter.next().ok_or("--check needs a path")?))
                }
                "--gui" => gui = true,
                "--help" | "-h" => {
                    return Err(
                        "usage: x4-emulator [--gui] [--scenario PATH] [--sd-root PATH] [--dump out.png] [--present-dump out.png] [--check golden.png]"
                            .into(),
                    );
                }
                value => return Err(format!("unknown option: {value}")),
            }
        }
        Ok(Self {
            scenario,
            sd_root,
            dump,
            present_dump,
            check,
            gui,
        })
    }
}

fn run_headless(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let scenarios = scenario_paths(args.scenario.as_deref())?;
    if scenarios.is_empty() {
        let emu = Emulator::boot(args.sd_root);
        if let Some(path) = args.dump {
            write_png(&path, emu.framebuffer())?;
        }
        if let Some(path) = args.present_dump {
            write_presented_png(&path, emu.framebuffer())?;
        }
        if let Some(path) = args.check {
            compare_png(&path, emu.framebuffer())?;
        }
        return Ok(());
    }

    for path in scenarios {
        let mut emu = Emulator::boot(args.sd_root.clone());
        let scenario = Scenario::load(&path)?;
        scenario
            .run(&mut emu)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        scenario
            .assert(&emu)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        if let Some(dump) = &args.dump {
            let path = output_path(dump, &path)?;
            write_png(&path, emu.framebuffer())?;
            println!("dumped {}", path.display());
        }
        if let Some(dump) = &args.present_dump {
            let path = output_path(dump, &path)?;
            write_presented_png(&path, emu.framebuffer())?;
            println!("presented {}", path.display());
        }
        if let Some(check) = &args.check {
            let path = output_path(check, &path)?;
            compare_png(&path, emu.framebuffer())?;
            println!("matched {}", path.display());
        }
        println!("ok {}", path.display());
    }
    Ok(())
}

fn scenario_paths(path: Option<&Path>) -> Result<Vec<PathBuf>, String> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    if path.is_file() {
        return Ok(vec![path.to_owned()]);
    }
    if !path.is_dir() {
        return Err(format!("scenario path does not exist: {}", path.display()));
    }
    let mut paths = Vec::new();
    let entries = std::fs::read_dir(path).map_err(|err| format!("{}: {err}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn output_path(base: &Path, scenario: &Path) -> Result<PathBuf, String> {
    let suffix = if cfg!(feature = "device-x3") {
        "-x3"
    } else {
        ""
    };
    if base.extension().is_some_and(|ext| ext == "png") {
        return Ok(base.to_owned());
    }
    let name = scenario
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("scenario has no valid file stem: {}", scenario.display()))?;
    Ok(base.join(format!("{name}{suffix}.png")))
}

fn compare_png(path: &Path, fb: &display::fb::Framebuffer) -> Result<(), String> {
    // Compare decoded pixels, strictly: encoded PNG bytes would also hinge
    // on the `png` crate's encoder, so an encoder change in a dependency
    // bump would fail every golden without any pixel differing.
    let file = std::fs::File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .map_err(|err| format!("{}: {err}", path.display()))?;
    let mut expected = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut expected)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    expected.truncate(info.buffer_size());
    let matches = info.width as usize == display::WIDTH
        && info.height as usize == display::HEIGHT
        && info.color_type == png::ColorType::Grayscale
        && info.bit_depth == png::BitDepth::Eight
        && render::grayscale_pixels(fb) == expected;
    if matches {
        Ok(())
    } else {
        Err(format!("frame does not match {}", path.display()))
    }
}

fn run_gui(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(not(feature = "gui"))]
    {
        let _ = args;
        Err("--gui requires building with --features gui".into())
    }
    #[cfg(feature = "gui")]
    {
        let native_options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1100.0, 720.0])
                .with_min_inner_size([900.0, 560.0]),
            ..Default::default()
        };
        let sd_root = args.sd_root.clone();
        eframe::run_native(
            "Xteink X4 Emulator",
            native_options,
            Box::new(move |_cc| Ok(Box::new(EmulatorApp::new(sd_root)))),
        )?;
        Ok(())
    }
}

pub struct Emulator {
    state: app_core::ReaderState,
    ctx: app_core::ReducerContext,
    refresh_planner: RefreshPlanner,
    panel: PanelModel,
    fb: display::fb::Framebuffer,
    prev_fb: display::fb::Framebuffer,
    prev_prestaged: bool,
    sleeping: bool,
    _sd_root: Option<PathBuf>,
    /// The Library list the scenario is showing: the name of each row and
    /// whether it is a folder, books first the way the firmware lists them.
    library_entries: Vec<(String, bool)>,
    /// The folder being shown, empty at the library root.
    library_folder: String,
    /// The listing a folder was entered from, and the row it was entered on,
    /// so leaving replays the parent rather than describing it with the
    /// child's rows.
    library_parent: Vec<(u16, u16, u16)>,
    /// The counts (count, books) of the latest depth-0 listing, used as a
    /// fallback on wake if no parent history exists.
    library_root_counts: (u16, u16),
    last_storage: Option<StorageCommand>,
    /// Scenario-driven: leave picked per-book actions unsettled so `Busy`
    /// reaches a frame. Off for interactive use, where a clear that never
    /// answered would freeze the list for good.
    hold_storage: bool,
    sd_reader_status: EmulatedReaderStatus,
    touch: TouchRecognizer,
    frame_epoch: u32,
    fb_revision: u64,
    displayed_chapters: ui::render::DisplayedChapters,
    down_context: Option<LegacyTouchContext>,
    last_gesture: Option<GestureEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EmulatedReaderStatus {
    Empty,
    Loading,
    Ready,
}

impl Emulator {
    pub fn boot(sd_root: Option<PathBuf>) -> Self {
        let mut emu = Self {
            state: app_core::ReaderState::boot(),
            ctx: app_core::ReducerContext::new(1, 4),
            refresh_planner: RefreshPlanner::new(),
            panel: PanelModel::new(),
            fb: display::fb::Framebuffer::new(),
            prev_fb: display::fb::Framebuffer::new(),
            prev_prestaged: false,
            sleeping: false,
            _sd_root: sd_root,
            library_entries: Vec::new(),
            library_folder: String::new(),
            library_parent: Vec::new(),
            library_root_counts: (0, 0),
            last_storage: None,
            hold_storage: false,
            sd_reader_status: EmulatedReaderStatus::Empty,
            touch: TouchRecognizer::new(),
            frame_epoch: 0,
            fb_revision: 0,
            displayed_chapters: ui::render::DisplayedChapters::EMPTY,
            down_context: None,
            last_gesture: None,
        };
        emu.panel.init_sequence().expect("panel init");
        // A saved network, like the firmware's boot probe of WIFI.BIN:
        // scenarios must be able to walk the whole connect flow. The
        // no-network screen stays reachable through the forget flow (or
        // pinned by app-core unit tests).
        emu.state = emu
            .state
            .apply_sync_event(app_core::SyncEvent::NetworkSaved(
                app_core::WifiSsid::new("HOME-WIFI").unwrap(),
            ));
        emu.render(app_core::RenderKind::Boot);
        emu
    }

    pub fn input(&mut self, button: Button) {
        if self.touch.is_active() {
            if let Some(event) = self.touch.cancel() {
                self.last_gesture = Some(event);
            }
            self.down_context = None;
        }
        if button == Button::Power {
            if self.sleeping {
                self.sleeping = false;
                self.panel.init_sequence().expect("panel wake init");
                self.prev_prestaged = false;
                self.forget_what_a_boot_loses();
                self.render(app_core::RenderKind::Page);
            } else {
                self.sleeping = true;
                self.state.sync_status = self.state.wireless_entry_status();
                self.sleep_panel();
            }
            return;
        }
        if self.sleeping {
            return;
        }
        self.apply_input_event(InputEvent::button(button));
    }

    fn apply_input_event(&mut self, event: InputEvent) {
        let previous = self.state;
        self.state = self.state.apply_input(self.ctx, event);
        self.render(app_core::RenderKind::Page);
        if let Some(command) = storage_command_for_transition(previous, self.state) {
            if matches!(command, StorageCommand::OpenBook { .. }) {
                self.sd_reader_status = EmulatedReaderStatus::Loading;
                self.render(app_core::RenderKind::Page);
            }
            self.last_storage = Some(command);
        }
        if let Some(command) =
            app_core::library_action_command_for_transition(&previous, &self.state)
        {
            self.last_storage = Some(command);
            // The emulated card has no real cache; the clear settles
            // instantly and always succeeds — unless a scenario is holding
            // storage, which is the only way `Busy` survives long enough to
            // be drawn.
            if let StorageCommand::ClearBookCache { request_id, .. } = command {
                if !self.hold_storage {
                    self.library_event(LibraryEvent::CacheCleared {
                        request_id,
                        ok: true,
                    });
                }
            }
        }
        if let Some(command) =
            app_core::library_browse_command_for_transition(&previous, &self.state)
        {
            self.last_storage = Some(command);
            // The emulated card answers a move at once, the way it answers a
            // clear: a scenario holding storage is the only way the wait
            // survives long enough to be drawn.
            if !self.hold_storage {
                self.answer_library_move(command);
            }
        }
    }

    pub fn touch_config(&self) -> GestureConfig {
        self.touch.config()
    }

    pub fn set_touch_config(
        &mut self,
        config: GestureConfig,
    ) -> Result<(), app_core::touch::GestureConfigError> {
        self.touch.set_config(config)
    }

    pub fn touch_phase(&self) -> RecognizerPhase {
        self.touch.phase()
    }

    pub fn disarm_touch_hold(&mut self) {
        self.touch.disarm_hold();
    }

    pub fn last_gesture(&self) -> Option<GestureEvent> {
        self.last_gesture
    }

    pub fn fb_revision(&self) -> u64 {
        self.fb_revision
    }

    fn touch_open_pending(&self) -> bool {
        self.state.view == AppView::Reading
            && ReaderSource::from_book_id(self.state.book_id).is_sd()
            && self.sd_reader_status == EmulatedReaderStatus::Loading
    }

    pub fn touch_down(
        &mut self,
        x: u16,
        y: u16,
        now_ms: u32,
        target_bounds: Option<display::Rect>,
    ) -> Option<GestureEvent> {
        if self.sleeping || self.touch_open_pending() {
            return None;
        }
        let frame_width = self.fb.frame_width() as u16;
        let frame_height = self.fb.frame_height() as u16;
        if x >= frame_width || y >= frame_height {
            return None;
        }
        let ctx = LegacyTouchContext::from_request(
            self.state.render_request(app_core::RenderKind::Page),
            self.displayed_chapters,
        );
        let event = self.touch.down(
            TouchPoint::new(x, y),
            now_ms,
            self.frame_epoch,
            frame_width,
            target_bounds,
        );
        if self.touch.is_active() && self.down_context.is_none() {
            self.down_context = Some(ctx);
        }
        if let Some(ev) = event {
            let down_ctx = self.down_context.take();
            self.handle_gesture_event(ev, down_ctx);
        }
        event
    }

    pub fn touch_move(&mut self, x: u16, y: u16, now_ms: u32) -> Option<GestureEvent> {
        if self.sleeping {
            return None;
        }
        let pt = self.clamp_logical_point(x, y);
        let event = self.touch.move_to(pt, now_ms);
        if let Some(ev) = event {
            let down_ctx = if self.touch.is_active() {
                self.down_context
            } else {
                self.down_context.take()
            };
            self.handle_gesture_event(ev, down_ctx);
        }
        event
    }

    pub fn touch_up(&mut self, point: Option<(u16, u16)>, now_ms: u32) -> Option<GestureEvent> {
        let pt = point.map(|(x, y)| self.clamp_logical_point(x, y));
        let event = self.touch.up(pt, now_ms);
        let down_ctx = self.down_context.take();
        if let Some(ev) = event {
            self.handle_gesture_event(ev, down_ctx);
        }
        event
    }

    pub fn touch_cancel(&mut self) -> Option<GestureEvent> {
        let event = self.touch.cancel();
        let down_ctx = self.down_context.take();
        if let Some(ev) = event {
            self.handle_gesture_event(ev, down_ctx);
        }
        event
    }

    pub fn touch_error(&mut self) -> Option<GestureEvent> {
        let event = self.touch.on_error();
        let down_ctx = self.down_context.take();
        if let Some(ev) = event {
            self.handle_gesture_event(ev, down_ctx);
        }
        event
    }

    pub fn touch_tick(&mut self, now_ms: u32) -> Option<GestureEvent> {
        if self.sleeping {
            return None;
        }
        let event = self.touch.tick(now_ms);
        if let Some(ev) = event {
            let down_ctx = if self.touch.is_active() {
                self.down_context
            } else {
                self.down_context.take()
            };
            self.handle_gesture_event(ev, down_ctx);
        }
        event
    }

    fn clamp_logical_point(&self, x: u16, y: u16) -> TouchPoint {
        let max_x = self.fb.frame_width().saturating_sub(1) as u16;
        let max_y = self.fb.frame_height().saturating_sub(1) as u16;
        TouchPoint::new(x.min(max_x), y.min(max_y))
    }

    fn handle_gesture_event(&mut self, event: GestureEvent, down_ctx: Option<LegacyTouchContext>) {
        self.last_gesture = Some(event);
        if self.sleeping
            || self.touch_open_pending()
            || event.contact().frame_epoch != self.frame_epoch
        {
            return;
        }
        let Some(ctx) = down_ctx else {
            return;
        };
        let Some(outcome) = route_gesture(event, ctx) else {
            return;
        };
        match outcome {
            LegacyTouchOutcome::Button(button) => {
                if button == Button::Power {
                    self.input(button);
                } else {
                    self.apply_input_event(InputEvent::button(button));
                }
            }
            LegacyTouchOutcome::ListRow { row, toggle_expand } => {
                self.apply_input_event(InputEvent::ListRowTap {
                    view: ctx.view,
                    frame_epoch: self.frame_epoch,
                    row,
                    toggle_expand,
                    battery_mv: 4000,
                    battery_percent: 77,
                });
            }
        }
    }

    /// Stand in for the storage task's half of a move through the tree.
    ///
    /// Rows here are synthetic, so what a row *is* comes from the list the
    /// scenario built: rows below the book count are books, the rest folders.
    fn answer_library_move(&mut self, command: StorageCommand) {
        let depth = self.state.library_depth;
        match command {
            StorageCommand::ChooseLibraryRow {
                request_id, index, ..
            } => match self.library_entries.get(index as usize) {
                Some((_, true)) => {
                    // Remember the parent before its rows are replaced, so
                    // Back can put the reader back where they pressed.
                    let rows = self.library_entries.len().min(u16::MAX as usize) as u16;
                    let books = self
                        .library_entries
                        .iter()
                        .filter(|(_, is_folder)| !is_folder)
                        .count()
                        .min(u16::MAX as usize) as u16;
                    self.library_parent.push((rows, books, index));
                    self.library_event(LibraryEvent::FolderListed {
                        request_id: Some(request_id),
                        browse_epoch: self.state.library_browse_epoch,
                        depth: depth.saturating_add(1),
                        count: 4,
                        books: 3,
                        selection: 0,
                    })
                }
                // Storage answers the row and stops; the open is the app's,
                // and `library_event` dispatches it off the transition into
                // Reading, the same way a keypress into Reading does.
                Some((_, false)) => self.library_event(LibraryEvent::RowIsBook {
                    request_id,
                    index,
                    catalog_epoch: self.state.catalog_epoch,
                }),
                None => self.library_event(LibraryEvent::RowFailed { request_id }),
            },
            StorageCommand::LeaveLibraryFolder { request_id, .. } => {
                // The parent as it was, not the child described in its place:
                // the rows here belong to the folder being left.
                let (count, books, selection) = self.library_parent.pop().unwrap_or((0, 0, 0));
                self.library_event(LibraryEvent::FolderListed {
                    request_id: Some(request_id),
                    browse_epoch: self.state.library_browse_epoch,
                    depth: depth.saturating_sub(1),
                    count,
                    books,
                    selection,
                })
            }
            _ => {}
        }
    }

    /// Deep sleep does not return on the device: waking is a boot, which
    /// keeps only what was saved (the book, its place, the settings). A sheet
    /// that was up, the folder the Library was in and any Wi-Fi session are
    /// gone, and the boot's scan relists the root. Applied straight to the state, with no render
    /// of its own: the wake paints once, as the device's boot does.
    fn forget_what_a_boot_loses(&mut self) {
        self.state.view = app_core::AppView::Home;
        self.state.selection = 0;
        self.state.reading_sheet = false;
        self.state.library_menu = app_core::LibraryMenu::None;
        self.state.sync_status = self.state.wireless_entry_status();
        // A root listing needs no relist below, but an unanswered pick is
        // still volatile state: it cannot hold the Library after a boot.
        self.state.library_browse = app_core::LibraryBrowse::Idle;
        if self.state.library_depth == 0 {
            self.library_parent.clear();
            return;
        }
        let (count, books, _) = self.library_parent.first().copied().unwrap_or((
            self.library_root_counts.0,
            self.library_root_counts.1,
            0,
        ));
        self.library_parent.clear();
        self.state = self.state.apply_library_event(
            self.ctx,
            LibraryEvent::FolderListed {
                request_id: None,
                browse_epoch: self.state.library_browse_epoch.wrapping_add(1),
                depth: 0,
                count,
                books,
                selection: 0,
            },
        );
        self.relist_entries();
    }

    /// The emulated card's rows for the listing the reducer last took.
    fn relist_entries(&mut self) {
        let books = self.state.library_books;
        let folders = self.state.library_count.saturating_sub(books);
        self.library_entries.clear();
        self.library_entries
            .extend((0..books).map(|index| (format!("SD Book {}", index + 1), false)));
        self.library_entries
            .extend((0..folders).map(|index| (format!("Folder {}", index + 1), true)));
        self.library_folder = if self.state.library_depth > 0 {
            "Fiction".to_string()
        } else {
            String::new()
        };
    }

    pub fn library_event(&mut self, event: LibraryEvent) {
        if self.sleeping {
            return;
        }
        if let LibraryEvent::Scanned { count, .. } = event {
            // A scan puts the reader at the library root, with every
            // catalogued book as a row: the shape of a card with no folders.
            self.library_entries.clear();
            self.library_folder.clear();
            self.library_parent.clear();
            self.library_root_counts = (count, count);
            self.library_entries
                .extend((0..count).map(|index| (format!("SD Book {}", index + 1), false)));
        }
        let before = self.state;
        self.state = self.state.apply_library_event(self.ctx, event);
        // After the reducer, and only for the book it settled on. A load
        // answering a book the reader has already left would otherwise show
        // the emulated reader as ready for the one still loading.
        if let LibraryEvent::Loaded { book_id, .. } = event {
            if book_id == self.state.book_id {
                self.sd_reader_status = EmulatedReaderStatus::Ready;
            }
        }
        // Built from what the reducer took, not from what arrived. A listing
        // answering a press the reader has moved past is refused, and reading
        // the event directly would rewrite these rows anyway, leaving the
        // emulated card showing a folder the device would not be in.
        if matches!(event, LibraryEvent::FolderListed { .. }) && self.state != before {
            if self.state.library_depth == 0 {
                self.library_root_counts = (self.state.library_count, self.state.library_books);
            }
            self.relist_entries();
        }
        // A library event can move the reader onto a different book, which
        // owes an open exactly as a keypress into Reading does.
        if let Some(command) = storage_command_for_transition(before, self.state) {
            if matches!(command, StorageCommand::OpenBook { .. }) {
                self.sd_reader_status = EmulatedReaderStatus::Loading;
            }
            self.last_storage = Some(command);
        }
        self.render(app_core::RenderKind::Page);
        // A scan says how many books the catalog holds; what the Library
        // screen shows is a folder listing, which the storage task sends
        // straight after. Mirrored here so a scripted scan leaves the same
        // state a real one does.
        if let LibraryEvent::Scanned { count, .. } = event {
            self.library_event(LibraryEvent::FolderListed {
                request_id: None,
                // A scripted scan repositions to the root, retiring the rows
                // the reader was looking at, the way a real one does.
                browse_epoch: self.state.library_browse_epoch.wrapping_add(1),
                depth: 0,
                count,
                books: count,
                selection: self.state.selection,
            });
        }
    }

    /// A rescan's progress repaint, as the firmware's display task paints it
    /// mid-scan: only the note's line is redrawn over the frame on the glass,
    /// on a fast refresh, and only while that frame is the note.
    pub fn rescan_progress(&mut self, percent: u8) -> bool {
        if self.sleeping {
            return false;
        }
        let Some(request) = self.refresh_planner.rescan_progress_frame(percent) else {
            return false;
        };
        self.fb.copy_from(&self.prev_fb);
        ui::app_render::render_library_rescan_progress(&mut self.fb, request);
        let mode = self.refresh_planner.mode_for(request);
        let flushed = self
            .panel
            .flush(&self.fb, &self.prev_fb, mode, self.prev_prestaged)
            .expect("panel flush");
        // The X3 promotes Fast to FastClean when its charge pump is off.
        // Account for the waveform that ran, not the mode requested.
        self.refresh_planner
            .record_render(request, flushed.effective_mode);
        self.prev_fb.copy_from(&self.fb);
        self.fb_revision = self.fb_revision.wrapping_add(1);
        // A plate: held, and no software prestage follows it. On X3 all
        // completed refreshes synchronize old RAM; X4 remains conservative.
        self.panel.settle(flushed.settle_ms).expect("panel settle");
        self.prev_prestaged = flushed.old_plane_synced;
        true
    }

    /// The percentage the frame on the glass shows, if it is a rescan note.
    pub fn rescan_percent_shown(&self) -> Option<u8> {
        self.refresh_planner
            .last_request()
            .and_then(|request| request.footer_percent)
    }

    pub fn sync_event(&mut self, event: app_core::SyncEvent) {
        if self.sleeping {
            return;
        }
        self.state = self.state.apply_sync_event(event);
        self.render(app_core::RenderKind::Page);
    }

    /// The request id of the per-book action currently in flight, if any.
    pub fn outstanding_request(&self) -> Option<u32> {
        match self.state.library_menu {
            app_core::LibraryMenu::Busy { request_id, .. } => Some(request_id),
            _ => None,
        }
    }

    pub fn set_hold_storage(&mut self, hold: bool) {
        self.hold_storage = hold;
    }

    pub fn state(&self) -> app_core::ReaderState {
        self.state
    }

    pub fn panel(&self) -> &PanelModel {
        &self.panel
    }

    pub fn sleeping(&self) -> bool {
        self.sleeping
    }

    pub fn framebuffer(&self) -> &display::fb::Framebuffer {
        &self.fb
    }

    pub fn pending_storage_name(&self) -> Option<&'static str> {
        match self.last_storage {
            Some(StorageCommand::LoadCatalogCache) => Some("LoadCatalogCache"),
            Some(StorageCommand::RefreshCatalog) => Some("RefreshCatalog"),
            Some(StorageCommand::OpenBook { .. }) => Some("OpenBook"),
            Some(StorageCommand::ExtendSection { .. }) => Some("ExtendSection"),
            Some(StorageCommand::StoreProgress(_)) => Some("StoreProgress"),
            Some(StorageCommand::StoreFrontlight { .. }) => Some("StoreFrontlight"),
            Some(StorageCommand::LoanSyncMemory) => Some("LoanSyncMemory"),
            Some(StorageCommand::StoreWifiCredentials(_)) => Some("StoreWifiCredentials"),
            Some(StorageCommand::StoreWifiApHint { .. }) => Some("StoreWifiApHint"),
            Some(StorageCommand::ForgetWifiCredentials) => Some("ForgetWifiCredentials"),
            Some(StorageCommand::ReceiveUpload) => Some("ReceiveUpload"),
            Some(StorageCommand::LoadChapters { .. }) => Some("LoadChapters"),
            Some(StorageCommand::JumpChapter { .. }) => Some("JumpChapter"),
            Some(StorageCommand::ClearBookCache { .. }) => Some("ClearBookCache"),
            Some(StorageCommand::ChooseLibraryRow { .. }) => Some("ChooseLibraryRow"),
            Some(StorageCommand::LeaveLibraryFolder { .. }) => Some("LeaveLibraryFolder"),
            Some(StorageCommand::AdoptTitleIndex { .. }) => Some("AdoptTitleIndex"),
            None => None,
        }
    }

    pub fn reader_status_name(&self) -> &'static str {
        match self.sd_reader_status {
            EmulatedReaderStatus::Empty => "Empty",
            EmulatedReaderStatus::Loading => "Loading",
            EmulatedReaderStatus::Ready => "Ready",
        }
    }

    fn render(&mut self, kind: app_core::RenderKind) {
        if self.touch.is_active() {
            if let Some(event) = self.touch.cancel() {
                self.last_gesture = Some(event);
            }
            self.down_context = None;
        }
        self.frame_epoch = self.frame_epoch.wrapping_add(1);
        let request = self.state.render_request(kind);
        let loading_plate = request.view == AppView::Reading
            && ReaderSource::from_book_id(request.book_id).is_sd()
            && self.sd_reader_status != EmulatedReaderStatus::Ready;
        if loading_plate {
            // This stand-in plate has always been drawn in raw buffer
            // coordinates (the old pipeline never flipped this branch), and
            // the framebuffer now remembers the previous render's frame.
            self.fb.set_frame(display::fb::FbFrame::Native);
            self.fb.clear(true);
            display::render::draw_ascii(&mut self.fb, "OPENING EPUB", 20, 72, false);
            self.displayed_chapters = ui::render::DisplayedChapters::EMPTY;
        } else {
            self.displayed_chapters = crate::render::render_request(
                &mut self.fb,
                &self.state,
                request,
                &self.library_entries,
                &self.library_folder,
            );
        }
        let mode = self.refresh_planner.mode_for(request);
        let flushed = self
            .panel
            .flush(&self.fb, &self.prev_fb, mode, self.prev_prestaged)
            .expect("panel flush");
        // The X3 promotes Fast to FastClean when its charge pump is off.
        // Account for the waveform that ran, not the mode requested.
        if loading_plate {
            self.refresh_planner
                .record_loading_plate(request, flushed.effective_mode);
        } else {
            self.refresh_planner
                .record_render(request, flushed.effective_mode);
        }
        self.prev_fb.copy_from(&self.fb);
        self.fb_revision = self.fb_revision.wrapping_add(1);
        // The firmware's order (`fw::tasks::display`): report the frame, then
        // hold the interval, then prestage. The model rejects a plane write
        // taken inside it, so dropping this line fails the panel tests.
        self.panel.settle(flushed.settle_ms).expect("panel settle");
        // Only flushes that explicitly synchronized the old plane can
        // omit a subsequent prestage; others need the software write.
        self.prev_prestaged =
            flushed.old_plane_synced || self.panel.prestage_previous(&self.fb).is_ok();
    }

    fn sleep_panel(&mut self) {
        if self.touch.is_active() {
            if let Some(event) = self.touch.cancel() {
                self.last_gesture = Some(event);
            }
            self.down_context = None;
        }
        self.frame_epoch = self.frame_epoch.wrapping_add(1);
        self.displayed_chapters = ui::render::DisplayedChapters::EMPTY;
        crate::render::render_sleep(
            &mut self.fb,
            &self.state,
            self.state.render_request(app_core::RenderKind::Page),
            &self.library_entries,
            &self.library_folder,
        );
        #[cfg(feature = "device-x3")]
        let sleep_mode = display::epd::RefreshMode::PowerDown;
        #[cfg(not(feature = "device-x3"))]
        let sleep_mode = display::epd::RefreshMode::Full;
        let flushed = self
            .panel
            .flush(&self.fb, &self.prev_fb, sleep_mode, self.prev_prestaged)
            .expect("panel sleep flush");
        self.prev_fb.copy_from(&self.fb);
        self.fb_revision = self.fb_revision.wrapping_add(1);
        self.prev_prestaged = false;
        // Both Full and PowerDown settle between their two refreshes, not
        // after the flush. Still honor the result returned by the plan.
        self.panel
            .settle(flushed.settle_ms)
            .expect("panel sleep settle");
        self.panel.deep_sleep().expect("panel deep sleep");
        self.refresh_planner.record_sleep(true);
        // prev_fb holds the sleep screen on the panel, so the wake is a page turn.
        self.refresh_planner.hold_sleep_frame();
    }
}

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

/// Maps a pointer position inside the displayed native panel rectangle onto
/// logical drawing-frame coordinates by inverting `FbFrame::map_to_native`
/// against `framebuffer_to_color_image`'s raw native buffer.
#[allow(clippy::too_many_arguments)]
pub fn gui_pointer_to_logical(
    pos_x: f32,
    pos_y: f32,
    rect_min_x: f32,
    rect_min_y: f32,
    rect_w: f32,
    rect_h: f32,
    frame: FbFrame,
    clamp: bool,
) -> Option<TouchPoint> {
    if !pos_x.is_finite()
        || !pos_y.is_finite()
        || !rect_w.is_finite()
        || !rect_h.is_finite()
        || rect_w <= 0.0
        || rect_h <= 0.0
    {
        return None;
    }
    let raw_u = (pos_x - rect_min_x) / rect_w;
    let raw_v = (pos_y - rect_min_y) / rect_h;
    if !clamp && (!(0.0..1.0).contains(&raw_u) || !(0.0..1.0).contains(&raw_v)) {
        return None;
    }
    let u = raw_u.clamp(0.0, 0.999_999);
    let v = raw_v.clamp(0.0, 0.999_999);
    let nx = ((u * display::WIDTH as f32).floor() as usize).min(display::WIDTH - 1);
    let ny = ((v * display::HEIGHT as f32).floor() as usize).min(display::HEIGHT - 1);
    let (lx, ly) = frame.native_to_frame(nx, ny)?;
    Some(TouchPoint::new(lx as u16, ly as u16))
}

#[cfg(feature = "gui")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum CompanionSkip {
    None,
    Active,
    UnobservableRelease,
}

#[cfg(feature = "gui")]
#[derive(Default)]
struct GuiTouchBridge {
    pointer_down: bool,
    active_touch_id: Option<(egui::TouchDeviceId, egui::TouchId)>,
    last_gui_point: Option<TouchPoint>,
    last_rect: Option<egui::Rect>,
    companion_touch_id: Option<(egui::TouchDeviceId, egui::TouchId)>,
    // egui's aggregate primary_down can be cleared by a finger's release.
    // Once ambiguous, recover on a genuine release, loss, fresh press,
    // or the recognizer's stationary timeout.
    mouse_pointer_state_ambiguous: bool,
    // Independent of button state: a genuine mouse move restores the
    // backend's pointer position, but not its aggregate button state.
    mouse_position_ambiguous: bool,
    mouse_release_unobservable: bool,
    ambiguous_since_ms: Option<u32>,
    hold_test_enabled: bool,
    hold_test_recognized: bool,
}

#[cfg(feature = "gui")]
impl GuiTouchBridge {
    fn reset(&mut self) {
        self.pointer_down = false;
        self.active_touch_id = None;
        self.last_gui_point = None;
        self.mouse_pointer_state_ambiguous = false;
        self.mouse_position_ambiguous = false;
        self.mouse_release_unobservable = false;
        self.ambiguous_since_ms = None;
    }

    fn cancel_active(&mut self, emulator: &mut Emulator, now_ms: u32) {
        if self.pointer_down || emulator.touch_phase() != RecognizerPhase::Idle {
            emulator.touch_cancel();
            emulator.touch_up(None, now_ms);
            self.reset();
        }
    }

    fn note_ambiguous_mouse(&mut self, emulator: &mut Emulator, now_ms: u32) {
        self.mouse_pointer_state_ambiguous = true;
        self.mouse_position_ambiguous = true;
        if self.ambiguous_since_ms.is_none() {
            self.ambiguous_since_ms = Some(now_ms);
        }
        emulator.disarm_touch_hold();
    }

    fn expire_stale_or_ambiguous_mouse(&mut self, emulator: &mut Emulator, now_ms: u32) {
        if self.pointer_down
            && self.active_touch_id.is_none()
            && (emulator.touch_phase() == RecognizerPhase::Stale
                || self.ambiguous_since_ms.is_some_and(|since| {
                    app_core::touch::elapsed_ms(now_ms, since)
                        >= emulator.touch_config().max_stationary_hold_ms
                }))
        {
            let _ = emulator.touch_tick(now_ms);
            self.cancel_active(emulator, now_ms);
        }
    }

    fn record_hold_result(&mut self, event: Option<GestureEvent>) {
        if self.hold_test_enabled && matches!(event, Some(GestureEvent::LongPress { .. })) {
            self.hold_test_recognized = true;
        }
    }

    fn set_hold_test_enabled(&mut self, enabled: bool, emulator: &mut Emulator, now_ms: u32) {
        self.cancel_active(emulator, now_ms);
        self.hold_test_enabled = enabled;
        self.hold_test_recognized = false;
    }

    fn record_hold_if_unambiguous(&mut self, event: Option<GestureEvent>) {
        if !(self.active_touch_id.is_none() && self.mouse_pointer_state_ambiguous) {
            self.record_hold_result(event);
        }
    }

    fn handle_move(&mut self, emulator: &mut Emulator, pt: TouchPoint, now_ms: u32) {
        let event = if self.last_gui_point != Some(pt) {
            self.last_gui_point = Some(pt);
            emulator.touch_move(pt.x, pt.y, now_ms)
        } else {
            emulator.touch_tick(now_ms)
        };
        self.record_hold_if_unambiguous(event);
    }

    fn hold_target(&self, pt: TouchPoint) -> Option<display::Rect> {
        // Milestone 1 has no hold-capable legacy actions. Use the same explicit
        // test rectangle as the scripted hold scenario, only when opted in.
        let target = display::Rect::new(150, 150, 120, 120);
        (self.hold_test_enabled && target.contains(pt.x, pt.y)).then_some(target)
    }

    // egui-winit 0.31 emits Touch followed immediately by its companion
    // pointer events. Consume only that sequence, not other mouse events
    // in the batch. Track the backend's emulated pointer independently of
    // which contact owns the emulator gesture.
    fn skip_touch_companions(
        &mut self,
        events: &mut std::iter::Peekable<std::slice::Iter<'_, egui::Event>>,
        device_id: egui::TouchDeviceId,
        id: egui::TouchId,
        phase: egui::TouchPhase,
        pos: egui::Pos2,
    ) -> CompanionSkip {
        let key = (device_id, id);
        // The backend also emits move/end/cancel companions when no touch
        // owns its pointer (e.g. a second finger outlives the first).
        let may_emit = self.companion_touch_id.is_none() || self.companion_touch_id == Some(key);
        let mut skipped = false;
        match phase {
            egui::TouchPhase::Start => {
                let mut lookahead = events.clone();
                if may_emit
                    && matches!(lookahead.next(), Some(egui::Event::PointerMoved(p)) if *p == pos)
                    && matches!(
                        lookahead.next(),
                        Some(egui::Event::PointerButton {
                            pos: p,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            ..
                        }) if *p == pos
                    )
                {
                    events.next();
                    events.next();
                    self.companion_touch_id = Some(key);
                    skipped = true;
                }
            }
            egui::TouchPhase::Move if may_emit => {
                skipped = events
                    .next_if(|ev| matches!(ev, egui::Event::PointerMoved(p) if *p == pos))
                    .is_some();
            }
            egui::TouchPhase::End | egui::TouchPhase::Cancel if may_emit => {
                self.companion_touch_id = None;
                if phase == egui::TouchPhase::End {
                    // The backend uses its last pointer position for release,
                    // not necessarily the final native touch position.
                    let mut lookahead = events.clone();
                    if matches!(
                        lookahead.next(),
                        Some(egui::Event::PointerButton {
                            button: egui::PointerButton::Primary,
                            pressed: false,
                            ..
                        })
                    ) && matches!(lookahead.next(), Some(egui::Event::PointerGone))
                    {
                        events.next();
                        skipped = true;
                    }
                }
                skipped |= events
                    .next_if(|ev| matches!(ev, egui::Event::PointerGone))
                    .is_some();
                if skipped {
                    return CompanionSkip::UnobservableRelease;
                }
            }
            _ => {}
        }
        if skipped {
            CompanionSkip::Active
        } else {
            CompanionSkip::None
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn process_events(
        &mut self,
        emulator: &mut Emulator,
        events: &[egui::Event],
        focused: bool,
        primary_down: bool,
        rect: egui::Rect,
        admit_rect: egui::Rect,
        displayed_epoch: Option<u32>,
        now_ms: u32,
    ) {
        let rect_changed = self.last_rect.is_some_and(|prev| prev != rect);
        self.last_rect = Some(rect);
        if !focused || rect_changed {
            self.cancel_active(emulator, now_ms);
        }
        // Expire stale or timed-out ambiguous mouse ownership before processing
        // this batch so a native Touch::Start arriving right at the deadline is
        // admitted instead of rejected by stale mouse state.
        self.expire_stale_or_ambiguous_mouse(emulator, now_ms);
        let mut saw_activity = false;
        self.mouse_release_unobservable = false;
        let mut events = events.iter().peekable();
        while let Some(event) = events.next() {
            if let egui::Event::Touch {
                device_id,
                id,
                phase,
                pos,
                ..
            } = *event
            {
                match self.skip_touch_companions(&mut events, device_id, id, phase, pos) {
                    CompanionSkip::None => {}
                    CompanionSkip::Active => {
                        if self.pointer_down && self.active_touch_id.is_none() {
                            self.note_ambiguous_mouse(emulator, now_ms);
                        }
                    }
                    CompanionSkip::UnobservableRelease => {
                        if self.pointer_down && self.active_touch_id.is_none() {
                            self.note_ambiguous_mouse(emulator, now_ms);
                            self.mouse_release_unobservable = true;
                        }
                    }
                }
            }
            if !focused {
                continue;
            }
            let frame = emulator.framebuffer().frame();
            let frame_changed = displayed_epoch != Some(emulator.frame_epoch);
            match *event {
                egui::Event::Touch {
                    device_id,
                    id,
                    phase,
                    pos,
                    ..
                } => match phase {
                    egui::TouchPhase::Start => {
                        if !frame_changed
                            && !rect_changed
                            && !self.pointer_down
                            && self.active_touch_id.is_none()
                            && admit_rect.contains(pos)
                        {
                            if let Some(pt) = gui_pointer_to_logical(
                                pos.x,
                                pos.y,
                                rect.min.x,
                                rect.min.y,
                                rect.width(),
                                rect.height(),
                                frame,
                                false,
                            ) {
                                self.pointer_down = true;
                                self.active_touch_id = Some((device_id, id));
                                self.last_gui_point = Some(pt);
                                self.hold_test_recognized = false;
                                emulator.touch_down(pt.x, pt.y, now_ms, self.hold_target(pt));
                                saw_activity = true;
                            }
                        }
                    }
                    egui::TouchPhase::Move => {
                        if self.pointer_down && self.active_touch_id == Some((device_id, id)) {
                            if let Some(pt) = gui_pointer_to_logical(
                                pos.x,
                                pos.y,
                                rect.min.x,
                                rect.min.y,
                                rect.width(),
                                rect.height(),
                                frame,
                                true,
                            ) {
                                self.handle_move(emulator, pt, now_ms);
                                saw_activity = true;
                            }
                        }
                    }
                    egui::TouchPhase::End => {
                        if self.pointer_down && self.active_touch_id == Some((device_id, id)) {
                            let up_pt = gui_pointer_to_logical(
                                pos.x,
                                pos.y,
                                rect.min.x,
                                rect.min.y,
                                rect.width(),
                                rect.height(),
                                frame,
                                true,
                            );
                            let event = emulator.touch_up(up_pt.map(|p| (p.x, p.y)), now_ms);
                            self.record_hold_result(event);
                            self.reset();
                            saw_activity = true;
                        }
                    }
                    egui::TouchPhase::Cancel => {
                        if self.active_touch_id == Some((device_id, id)) {
                            self.cancel_active(emulator, now_ms);
                            saw_activity = true;
                        }
                    }
                },
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    ..
                } => {
                    if pressed {
                        // A new genuine mouse press proves the previous mouse
                        // contact ended, even if egui dropped its release.
                        if self.pointer_down
                            && self.active_touch_id.is_none()
                            && (self.mouse_pointer_state_ambiguous
                                || emulator.touch_phase() == RecognizerPhase::Stale)
                        {
                            self.cancel_active(emulator, now_ms);
                        }
                        if !frame_changed
                            && !rect_changed
                            && !self.pointer_down
                            && self.active_touch_id.is_none()
                            && admit_rect.contains(pos)
                        {
                            if let Some(pt) = gui_pointer_to_logical(
                                pos.x,
                                pos.y,
                                rect.min.x,
                                rect.min.y,
                                rect.width(),
                                rect.height(),
                                frame,
                                false,
                            ) {
                                self.pointer_down = true;
                                self.last_gui_point = Some(pt);
                                self.hold_test_recognized = false;
                                emulator.touch_down(pt.x, pt.y, now_ms, self.hold_target(pt));
                                saw_activity = true;
                            }
                        }
                    } else if self.pointer_down && self.active_touch_id.is_none() {
                        // A touch companion can replace egui's stored pointer
                        // position. None tells the recognizer to use its last
                        // genuine mouse sample rather than the finger's point.
                        let up_pt = if self.mouse_position_ambiguous {
                            None
                        } else {
                            gui_pointer_to_logical(
                                pos.x,
                                pos.y,
                                rect.min.x,
                                rect.min.y,
                                rect.width(),
                                rect.height(),
                                frame,
                                true,
                            )
                        };
                        let event = emulator.touch_up(up_pt.map(|p| (p.x, p.y)), now_ms);
                        self.record_hold_result(event);
                        self.reset();
                        saw_activity = true;
                    }
                }
                egui::Event::PointerMoved(pos) => {
                    if self.pointer_down && self.active_touch_id.is_none() {
                        self.mouse_position_ambiguous = false;
                        // A genuine move restores egui-winit's stored pointer
                        // position, making a subsequent release observable.
                        // Button ownership itself remains ambiguous.
                        self.mouse_release_unobservable = false;
                        if let Some(pt) = gui_pointer_to_logical(
                            pos.x,
                            pos.y,
                            rect.min.x,
                            rect.min.y,
                            rect.width(),
                            rect.height(),
                            frame,
                            true,
                        ) {
                            self.handle_move(emulator, pt, now_ms);
                            saw_activity = true;
                        }
                    }
                }
                egui::Event::PointerGone => {
                    if self.active_touch_id.is_none()
                        && (self.pointer_down || emulator.touch_phase() != RecognizerPhase::Idle)
                    {
                        self.cancel_active(emulator, now_ms);
                        saw_activity = true;
                    }
                }
                egui::Event::WindowFocused(false) => {
                    self.cancel_active(emulator, now_ms);
                    saw_activity = true;
                }
                _ => {}
            }
        }
        if self.mouse_release_unobservable && self.pointer_down && self.active_touch_id.is_none() {
            // A companion Touch::End/Cancel clears egui-winit's stored pointer
            // position, making any subsequent stationary mouse release
            // unobservable across frames. Cancel rather than stranding ownership.
            self.cancel_active(emulator, now_ms);
        } else if self.pointer_down
            && self.active_touch_id.is_none()
            && !primary_down
            && !self.mouse_pointer_state_ambiguous
        {
            self.cancel_active(emulator, now_ms);
        } else if self.pointer_down && !saw_activity {
            self.record_hold_if_unambiguous(emulator.touch_tick(now_ms));
        }
        self.mouse_release_unobservable = false;
        self.expire_stale_or_ambiguous_mouse(emulator, now_ms);
    }
}

#[cfg(feature = "gui")]
#[derive(Default)]
struct FrameUploadState {
    uploaded_revision: Option<u64>,
    uploaded_epoch: Option<u32>,
}

#[cfg(feature = "gui")]
impl FrameUploadState {
    /// Records a newly rendered framebuffer if needed and returns
    /// `(needs_upload, displayed_epoch)` where `displayed_epoch` is the epoch
    /// that was already shown before this update.
    fn begin_update(
        &mut self,
        has_texture: bool,
        fb_revision: u64,
        frame_epoch: u32,
    ) -> (bool, Option<u32>) {
        let displayed_epoch = self.uploaded_epoch;
        let needs_upload = self.uploaded_revision != Some(fb_revision) || !has_texture;
        if needs_upload {
            self.uploaded_revision = Some(fb_revision);
            self.uploaded_epoch = Some(frame_epoch);
        }
        (needs_upload, displayed_epoch)
    }

    fn is_stale(&self, fb_revision: u64) -> bool {
        self.uploaded_revision != Some(fb_revision)
    }
}

#[cfg(feature = "gui")]
struct EmulatorApp {
    emulator: Emulator,
    texture: Option<egui::TextureHandle>,
    frame_upload: FrameUploadState,
    started_at: std::time::Instant,
    touch_bridge: GuiTouchBridge,
}

#[cfg(feature = "gui")]
impl EmulatorApp {
    fn new(sd_root: Option<PathBuf>) -> Self {
        Self {
            emulator: Emulator::boot(sd_root),
            texture: None,
            frame_upload: FrameUploadState::default(),
            started_at: std::time::Instant::now(),
            touch_bridge: GuiTouchBridge::default(),
        }
    }

    fn now_ms(&self) -> u32 {
        (self.started_at.elapsed().as_millis() & 0xFFFF_FFFF) as u32
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let bindings = [
            (egui::Key::Q, Button::Power),
            (egui::Key::Escape, Button::Back),
            (egui::Key::Enter, Button::Confirm),
            (egui::Key::ArrowLeft, Button::PagePrevious),
            (egui::Key::ArrowRight, Button::PageNext),
        ];
        let now_ms = self.now_ms();
        for (key, button) in bindings {
            if ctx.input(|input| input.key_pressed(key)) {
                self.touch_bridge.cancel_active(&mut self.emulator, now_ms);
                self.emulator.input(button);
            }
        }
    }
}

#[cfg(feature = "gui")]
impl eframe::App for EmulatorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (needs_upload, displayed_epoch) = self.frame_upload.begin_update(
            self.texture.is_some(),
            self.emulator.fb_revision(),
            self.emulator.frame_epoch,
        );
        if needs_upload {
            let image = framebuffer_to_color_image(self.emulator.framebuffer());
            match &mut self.texture {
                Some(texture) => texture.set(image, egui::TextureOptions::NEAREST),
                None => {
                    self.texture = Some(ctx.load_texture(
                        "x4-framebuffer",
                        image,
                        egui::TextureOptions::NEAREST,
                    ));
                }
            }
        }
        self.handle_keys(ctx);

        let now_ms = self.now_ms();
        egui::SidePanel::right("state").show(ctx, |ui| {
            let state = self.emulator.state();
            ui.heading("X4 Emulator");
            ui.label(format!("View: {:?}", state.view));
            ui.label(format!("Book: {}", state.book_id));
            ui.label(format!("Chapter: {}", state.chapter));
            ui.label(format!("Page: {}", state.page));
            ui.label(format!("Selection: {}", state.selection));
            ui.label(format!("Battery: {}%", state.battery_percent));
            ui.label(format!(
                "Refresh: {:?}",
                self.emulator.panel().last_refresh()
            ));
            ui.separator();
            ui.label("Keys");
            ui.label("Q Power");
            ui.label("Esc Back");
            ui.label("Enter Confirm");
            ui.label("Left Page Previous");
            ui.label("Right Page Next");
            ui.horizontal(|ui| {
                if ui.button("Back").clicked() {
                    self.touch_bridge.cancel_active(&mut self.emulator, now_ms);
                    self.emulator.input(Button::Back);
                }
                if ui.button("OK").clicked() {
                    self.touch_bridge.cancel_active(&mut self.emulator, now_ms);
                    self.emulator.input(Button::Confirm);
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Prev").clicked() {
                    self.touch_bridge.cancel_active(&mut self.emulator, now_ms);
                    self.emulator.input(Button::Previous);
                }
                if ui.button("Next").clicked() {
                    self.touch_bridge.cancel_active(&mut self.emulator, now_ms);
                    self.emulator.input(Button::Next);
                }
            });
            if ui.button("Power").clicked() {
                self.touch_bridge.cancel_active(&mut self.emulator, now_ms);
                self.emulator.input(Button::Power);
            }
            ui.separator();
            let mut hold_test_enabled = self.touch_bridge.hold_test_enabled;
            if ui
                .checkbox(&mut hold_test_enabled, "Enable hold test")
                .changed()
            {
                self.touch_bridge.set_hold_test_enabled(
                    hold_test_enabled,
                    &mut self.emulator,
                    now_ms,
                );
            }
            if self.touch_bridge.hold_test_enabled {
                ui.label("Hold target: logical x/y 150..270");
                ui.label(if self.touch_bridge.hold_test_recognized {
                    "Hold recognized"
                } else {
                    "Hold not recognized"
                });
            }
            ui.separator();
            ui.label("Panel history");
            for entry in self.emulator.panel().history().iter().rev().take(12).rev() {
                ui.label(entry);
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(texture) = &self.texture {
                let available = ui.available_size();
                let scale = (available.x / WIDTH as f32)
                    .min(available.y / HEIGHT as f32)
                    .max(1.0);
                let size = egui::Vec2::new(WIDTH as f32 * scale, HEIGHT as f32 * scale);
                let response = ui.add(
                    egui::Image::new((texture.id(), size)).sense(egui::Sense::click_and_drag()),
                );
                let admit_rect = response.rect.intersect(ui.clip_rect());
                let (events, focused, primary_down) =
                    ctx.input(|i| (i.events.clone(), i.focused, i.pointer.primary_down()));
                self.touch_bridge.process_events(
                    &mut self.emulator,
                    &events,
                    focused,
                    primary_down,
                    response.rect,
                    admit_rect,
                    displayed_epoch,
                    now_ms,
                );
                if self.frame_upload.is_stale(self.emulator.fb_revision()) {
                    ctx.request_repaint();
                } else if self.touch_bridge.pointer_down
                    || self.emulator.touch_phase() != RecognizerPhase::Idle
                {
                    ctx.request_repaint_after(std::time::Duration::from_millis(16));
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "device-x3")]
    #[test]
    fn x3_wake_clean_does_not_count_toward_periodic_page_cleans() {
        use display::epd::RefreshMode;

        let mut emu = Emulator::boot(None);
        // FullOnWake is the default; explicitly exercise the fast-enabled
        // policy so this wake runs FastClean rather than deep Full.
        emu.state.refresh_policy = app_core::RefreshPolicy::FastOnly;
        emu.input(Button::Power);
        emu.input(Button::Power);
        assert_eq!(emu.panel().last_refresh(), Some(RefreshMode::FastClean));

        emu.state.refresh_policy = app_core::RefreshPolicy::FullEveryTen;
        for selection in 1..=app_core::DEFAULT_FULL_REFRESH_INTERVAL {
            emu.state.selection = u16::from(selection);
            emu.render(app_core::RenderKind::Page);
            assert_eq!(emu.panel().last_refresh(), Some(RefreshMode::Fast));
        }
        emu.state.selection += 1;
        assert_eq!(
            emu.refresh_planner
                .mode_for(emu.state.render_request(app_core::RenderKind::Page)),
            RefreshMode::FastClean
        );
    }

    #[test]
    fn touch_does_not_route_while_sd_open_is_pending() {
        let mut emu = Emulator::boot(None);
        emu.state.view = AppView::Reading;
        emu.state.book_id = ReaderSource::sd(0).book_id();
        emu.sd_reader_status = EmulatedReaderStatus::Loading;
        emu.render(app_core::RenderKind::Page);
        let before = emu.state;
        let storage = emu.last_storage;
        emu.touch_down(300, 200, 100, None);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);
        emu.touch_move(100, 200, 120);
        assert_eq!(emu.touch_up(None, 140), None);
        assert_eq!(emu.state, before);
        assert_eq!(emu.last_storage, storage);

        // A new contact after the open settles is recognized normally.
        emu.sd_reader_status = EmulatedReaderStatus::Ready;
        emu.render(app_core::RenderKind::Page);
        emu.touch_down(300, 200, 200, None);
        emu.touch_move(100, 200, 220);
        assert!(matches!(
            emu.touch_up(None, 240),
            Some(GestureEvent::Swipe { .. })
        ));
    }

    #[test]
    fn loading_plate_is_cleaned_before_the_first_page() {
        use display::epd::RefreshMode;

        let mut emu = Emulator::boot(None);
        emu.state.view = AppView::Reading;
        emu.state.book_id = ReaderSource::sd(0).book_id();
        emu.sd_reader_status = EmulatedReaderStatus::Loading;
        emu.render(app_core::RenderKind::Page);
        assert_eq!(emu.panel().last_refresh(), Some(RefreshMode::FastClean));

        emu.sd_reader_status = EmulatedReaderStatus::Ready;
        emu.render(app_core::RenderKind::Page);
        assert_eq!(emu.panel().last_refresh(), Some(RefreshMode::FastClean));
        emu.render(app_core::RenderKind::Page);
        assert_eq!(emu.panel().last_refresh(), Some(RefreshMode::Fast));
    }

    #[cfg(feature = "device-x3")]
    #[test]
    fn x3_sleep_scrubs_black_before_displaying_the_sleep_frame() {
        use display::epd::{RefreshMode, CDI_ABSOLUTE, CDI_FULL};

        let mut emu = Emulator::boot(None);
        emu.input(Button::Power);

        assert!(emu.sleeping());
        assert!(emu.panel().is_deep_sleep());
        assert_eq!(emu.panel().last_refresh(), Some(RefreshMode::PowerDown));
        let refreshes = emu.panel().refreshes();
        assert_eq!(refreshes.len(), 2, "sleep needs a scrub and a target pass");
        assert!(refreshes[0].displayed.iter().all(|&byte| byte == 0x00));
        assert_eq!(refreshes[0].cdi[0], CDI_ABSOLUTE);
        assert!(refreshes[0].is_half_lut());
        assert_eq!(refreshes[1].cdi[0], CDI_FULL);
        assert!(refreshes[1].is_full_lut());
        assert_eq!(
            refreshes[1].displayed.as_slice(),
            emu.framebuffer().bytes().as_slice()
        );
    }

    /// A load answers one book. One arriving late, for a book the reader
    /// has already left, would otherwise show the emulated reader as ready
    /// while the book it is actually on is still loading.
    #[test]
    fn a_late_load_for_another_book_leaves_the_reader_loading() {
        let mut emu = Emulator::boot(None);
        emu.state.book_id = app_core::ReaderSource::sd(1).book_id();
        emu.sd_reader_status = EmulatedReaderStatus::Loading;

        emu.library_event(LibraryEvent::Loaded {
            book_id: app_core::ReaderSource::sd(0).book_id(),
            pages: 10,
            complete: true,
            chapters: 1,
            current_chapter: 0,
            chapter_pages: [0; app_core::MAX_SD_CHAPTERS],
            chapter_subsections: [0; app_core::MAX_SD_CHAPTER_SUBSECTIONS / 8],
            position: None,
            text_replaced: false,
        });

        assert_eq!(
            emu.sd_reader_status,
            EmulatedReaderStatus::Loading,
            "the book being waited on is not the one that answered"
        );
    }

    #[test]
    fn waking_drops_a_held_pick_at_the_library_root() {
        let mut emu = Emulator::boot(None);
        emu.library_event(LibraryEvent::Scanned {
            count: 3,
            catalog_epoch: 1,
        });
        emu.set_hold_storage(true);
        emu.input(Button::Back);
        emu.input(Button::Confirm);
        assert!(!emu.state.library_browse.is_idle());
        assert_eq!(emu.state.library_depth, 0);

        emu.input(Button::Power);
        emu.input(Button::Power);
        assert!(
            emu.state.library_browse.is_idle(),
            "a boot forgets the pick"
        );
        emu.input(Button::Back);
        assert_eq!(emu.state.view, app_core::AppView::Library);
        emu.input(Button::Next);
        assert_eq!(emu.state.selection, 1, "the old pick cannot hold the list");
    }

    /// A Wi-Fi session does not survive the boot either: the status is the
    /// one a boot shows, the connect offer for the saved network.
    #[test]
    fn waking_drops_the_wireless_session() {
        let mut emu = Emulator::boot(None);
        emu.input(Button::Previous);
        assert_eq!(emu.state.view, app_core::AppView::Wireless);
        emu.input(Button::Confirm);
        emu.sync_event(app_core::SyncEvent::Serving([192, 168, 0, 233]));

        emu.input(Button::Power);
        assert_eq!(emu.state.sync_status, app_core::SyncStatus::Idle);
        // Late events while asleep are dropped without panic or mutating status.
        emu.sync_event(app_core::SyncEvent::Serving([192, 168, 0, 233]));
        assert_eq!(emu.state.sync_status, app_core::SyncStatus::Idle);

        emu.input(Button::Power);
        assert_eq!(emu.state.view, app_core::AppView::Home);
        assert_eq!(emu.state.sync_status, app_core::SyncStatus::Idle);
    }

    /// Waking is a boot on the device: a sheet left up and the folder the
    /// Library was in do not survive it, and the Library reopens at the root.
    #[test]
    fn waking_drops_a_sheet_and_the_folder_the_library_was_in() {
        let mut emu = Emulator::boot(None);
        emu.library_event(LibraryEvent::Scanned {
            count: 3,
            catalog_epoch: 1,
        });
        // A root of three books and one folder.
        emu.library_event(LibraryEvent::FolderListed {
            request_id: None,
            browse_epoch: emu.state.library_browse_epoch.wrapping_add(1),
            depth: 0,
            count: 4,
            books: 3,
            selection: 0,
        });
        emu.input(Button::Back);
        assert_eq!(emu.state.view, app_core::AppView::Library);
        emu.input(Button::Previous);
        emu.input(Button::Confirm);
        assert_eq!(emu.state.library_depth, 1, "into the folder");
        emu.input(Button::PagePrevious);
        assert!(matches!(
            emu.state.library_menu,
            app_core::LibraryMenu::Sheet { .. }
        ));

        emu.input(Button::Power);
        emu.input(Button::Power);
        assert_eq!(emu.state.view, app_core::AppView::Home);
        assert_eq!(emu.state.library_menu, app_core::LibraryMenu::None);
        assert_eq!(
            (emu.state.library_depth, emu.state.library_count),
            (0, 4),
            "back at the root"
        );
        assert!(emu.library_folder.is_empty());

        emu.input(Button::Back);
        assert_eq!(emu.state.view, app_core::AppView::Library);
        assert_eq!(emu.state.library_menu, app_core::LibraryMenu::None);
        assert_eq!(emu.state.selection, 0);
    }

    /// A rescan resets the simulated card to the root with the newly
    /// scanned books, so waking from sleep must keep those rows rather than
    /// restoring the parent listing of a folder entered before the scan.
    #[test]
    fn waking_after_rescan_in_folder_keeps_new_root_listing() {
        let mut emu = Emulator::boot(None);
        emu.library_event(LibraryEvent::Scanned {
            count: 3,
            catalog_epoch: 1,
        });
        // A root of three books and one folder.
        emu.library_event(LibraryEvent::FolderListed {
            request_id: None,
            browse_epoch: emu.state.library_browse_epoch.wrapping_add(1),
            depth: 0,
            count: 4,
            books: 3,
            selection: 0,
        });
        emu.input(Button::Back);
        assert_eq!(emu.state.view, app_core::AppView::Library);
        emu.input(Button::Previous);
        emu.input(Button::Confirm);
        assert_eq!(emu.state.library_depth, 1, "into the folder");

        // Rescan while inside the folder, discovering 6 books.
        emu.library_event(LibraryEvent::Scanned {
            count: 6,
            catalog_epoch: 2,
        });
        assert_eq!(
            (
                emu.state.library_depth,
                emu.state.library_count,
                emu.state.library_books
            ),
            (0, 6, 6),
            "rescan returns to root with new book count"
        );

        // Sleep and wake: must keep the rescanned 6 books, not restore the old 4 items.
        emu.input(Button::Power);
        emu.input(Button::Power);
        assert_eq!(emu.state.view, app_core::AppView::Home);
        assert_eq!(
            (
                emu.state.library_depth,
                emu.state.library_count,
                emu.state.library_books
            ),
            (0, 6, 6),
            "waking preserves the rescanned root counts"
        );
        assert_eq!(emu.library_entries.len(), 6);
        assert!(
            emu.library_parent.is_empty(),
            "rescan clears parent history"
        );

        emu.input(Button::Back);
        assert_eq!(emu.state.view, app_core::AppView::Library);
        assert_eq!(emu.state.library_count, 6);
        assert_eq!(emu.state.library_books, 6);
        assert_eq!(emu.library_entries.len(), 6);
        assert_eq!(emu.library_entries[0].0, "SD Book 1");
        assert_eq!(emu.library_entries[5].0, "SD Book 6");
    }

    /// A scenario can inject a FolderListed event at depth > 0 without
    /// navigating through a parent. Waking must restore the root counts saved
    /// from the latest depth-0 listing rather than adopting the child folder's
    /// counts.
    #[test]
    fn waking_after_unsolicited_folder_listing_restores_saved_root_counts() {
        let mut emu = Emulator::boot(None);
        emu.library_event(LibraryEvent::Scanned {
            count: 6,
            catalog_epoch: 1,
        });
        assert_eq!(emu.state.library_count, 6);
        assert_eq!(emu.state.library_depth, 0);

        // Inject an unsolicited depth-1 listing with 2 books (no parent navigation).
        emu.library_event(LibraryEvent::FolderListed {
            request_id: None,
            browse_epoch: emu.state.library_browse_epoch.wrapping_add(1),
            depth: 1,
            count: 2,
            books: 2,
            selection: 0,
        });
        assert_eq!(emu.state.library_depth, 1);
        assert_eq!(emu.state.library_count, 2);
        assert!(emu.library_parent.is_empty(), "no parent was recorded");

        // Sleep and wake: must return to depth 0 with the 6 root books, not 2 books.
        emu.input(Button::Power);
        emu.input(Button::Power);
        assert_eq!(emu.state.view, app_core::AppView::Home);
        assert_eq!(
            (
                emu.state.library_depth,
                emu.state.library_count,
                emu.state.library_books
            ),
            (0, 6, 6),
            "waking restores the saved depth-0 counts"
        );
        assert_eq!(emu.library_entries.len(), 6);
    }

    /// While sleeping, late library events are dropped and cannot mutate the
    /// state or attempt to render to the sleeping panel.
    #[test]
    fn late_library_event_while_sleeping_is_dropped() {
        let mut emu = Emulator::boot(None);
        emu.input(Button::Power);
        assert!(emu.sleeping);
        let before = emu.state;

        // Scanned would normally panic on panel.flush() if not guarded.
        emu.library_event(LibraryEvent::Scanned {
            count: 3,
            catalog_epoch: 1,
        });
        assert_eq!(
            emu.state, before,
            "late library event does not mutate state or panic"
        );

        // FolderListed while sleeping is also dropped.
        emu.library_event(LibraryEvent::FolderListed {
            request_id: None,
            browse_epoch: emu.state.library_browse_epoch.wrapping_add(1),
            depth: 0,
            count: 3,
            books: 3,
            selection: 0,
        });
        assert_eq!(emu.state, before);
    }

    #[test]
    fn explicit_png_output_path_is_preserved() {
        let base = Path::new("fixtures/golden/home-x3.png");
        let scenario = Path::new("fixtures/scenarios/home.toml");
        assert_eq!(output_path(base, scenario).unwrap(), base);
    }

    #[test]
    fn directory_output_path_uses_the_selected_panel_suffix() {
        let base = Path::new("fixtures/golden");
        let scenario = Path::new("fixtures/scenarios/home.toml");
        let expected = if cfg!(feature = "device-x3") {
            Path::new("fixtures/golden/home-x3.png")
        } else {
            Path::new("fixtures/golden/home.png")
        };
        assert_eq!(output_path(base, scenario).unwrap(), expected);
    }

    #[test]
    fn all_scenario_contracts_pass_for_selected_panel() {
        let scenarios = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/scenarios");
        for path in scenario_paths(Some(&scenarios)).expect("list scenarios") {
            let mut emulator = Emulator::boot(None);
            let scenario = Scenario::load(&path).expect("load scenario");
            scenario
                .run(&mut emulator)
                .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
            scenario
                .assert(&emulator)
                .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        }
    }

    #[test]
    fn all_touch_scenario_contracts_pass_for_selected_panel() {
        let scenarios =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/touch-scenarios");
        let paths = scenario_paths(Some(&scenarios)).expect("list touch scenarios");
        assert!(
            !paths.is_empty(),
            "expected at least one touch scenario fixture"
        );
        for path in paths {
            let mut emulator = Emulator::boot(None);
            let scenario = Scenario::load(&path).expect("load touch scenario");
            scenario
                .run(&mut emulator)
                .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
            scenario
                .assert(&emulator)
                .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        }
    }

    #[test]
    fn gui_pointer_to_logical_inverts_every_frame_orientation() {
        let rect_min_x = 120.0f32;
        let rect_min_y = 40.0f32;
        let scale = 1.5f32;
        let rect_w = display::WIDTH as f32 * scale;
        let rect_h = display::HEIGHT as f32 * scale;

        for frame in [
            FbFrame::Native,
            FbFrame::Landscape,
            FbFrame::LandscapeFlipped,
            FbFrame::Portrait,
        ] {
            let fw = frame.width();
            let fh = frame.height();
            for &(lx, ly) in &[
                (0usize, 0usize),
                (17, 29),
                (fw / 2, fh / 2),
                (fw - 1, fh - 1),
            ] {
                let mut fb = display::fb::Framebuffer::new();
                fb.set_frame(frame);
                fb.clear(true);
                fb.set_pixel(lx, ly, false);

                // Find the darkened native pixel the GUI displays in native row-major order.
                let mut found = None;
                for ny in 0..display::HEIGHT {
                    for nx in 0..display::WIDTH {
                        if !fb.native_pixel(nx, ny) {
                            found = Some((nx, ny));
                        }
                    }
                }
                let (nx, ny) = found.expect("set_pixel wrote one native pixel");
                let px = rect_min_x + (nx as f32 + 0.5) * scale;
                let py = rect_min_y + (ny as f32 + 0.5) * scale;
                assert_eq!(
                    gui_pointer_to_logical(
                        px, py, rect_min_x, rect_min_y, rect_w, rect_h, frame, false
                    ),
                    Some(TouchPoint::new(lx as u16, ly as u16)),
                    "frame {frame:?} ({lx}, {ly})"
                );
            }

            // Out-of-bounds rejected when clamp=false, clamped when clamp=true.
            assert_eq!(
                gui_pointer_to_logical(
                    rect_min_x - 1.0,
                    rect_min_y + 10.0,
                    rect_min_x,
                    rect_min_y,
                    rect_w,
                    rect_h,
                    frame,
                    false
                ),
                None
            );
            assert!(gui_pointer_to_logical(
                rect_min_x - 1.0,
                rect_min_y + 10.0,
                rect_min_x,
                rect_min_y,
                rect_w,
                rect_h,
                frame,
                true
            )
            .is_some());
        }
    }

    #[cfg(feature = "gui")]
    #[test]
    fn framebuffer_to_color_image_aligns_with_gui_pointer_to_logical() {
        let mut fb = display::fb::Framebuffer::new();
        fb.set_frame(FbFrame::Portrait);
        fb.clear(true);
        let lx = 123usize;
        let ly = 456usize;
        fb.set_pixel(lx, ly, false);

        let image = framebuffer_to_color_image(&fb);
        assert_eq!(image.size, [WIDTH, HEIGHT]);
        let (idx, _) = image
            .pixels
            .iter()
            .enumerate()
            .find(|(_, c)| **c == egui::Color32::from_rgb(22, 22, 20))
            .expect("one ink pixel in ColorImage");
        let nx = idx % WIDTH;
        let ny = idx / WIDTH;
        let mapped = gui_pointer_to_logical(
            nx as f32 + 0.5,
            ny as f32 + 0.5,
            0.0,
            0.0,
            WIDTH as f32,
            HEIGHT as f32,
            FbFrame::Portrait,
            false,
        );
        assert_eq!(mapped, Some(TouchPoint::new(lx as u16, ly as u16)));
    }

    #[cfg(feature = "gui")]
    #[test]
    fn gui_touch_bridge_handles_same_frame_swipe_and_touch_cancel() {
        let mut emu = Emulator::boot(None);
        let mut bridge = GuiTouchBridge::default();
        let rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH as f32, HEIGHT as f32),
        );
        let dev = egui::TouchDeviceId(1);
        let tid = egui::TouchId(42);

        // Press and release in one egui frame with distinct native positions
        // must preserve the start/end positions and emit a swipe rather than
        // collapsing to a tap at interact_pos().
        // In Portrait on X4/X3: native (400, 360) -> logical y ~400, native (400, 200) -> swipe left in portrait?
        // Actually native_y maps to logical x: dx >= 60 px in logical x requires |ny_end - ny_start| >= 60.
        let same_frame_swipe = [
            egui::Event::PointerButton {
                pos: egui::pos2(300.0, 350.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: egui::pos2(300.0, 150.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        let epoch = emu.frame_epoch;
        bridge.process_events(
            &mut emu,
            &same_frame_swipe,
            true,
            false,
            rect,
            rect,
            Some(epoch),
            100,
        );
        assert!(matches!(
            emu.last_gesture(),
            Some(GestureEvent::Swipe { .. })
        ));
        assert!(!bridge.pointer_down);

        // Native TouchPhase::Cancel while window remains focused must cancel the
        // active contact and allow the next TouchPhase::Start to begin cleanly.
        let touch_start = [egui::Event::Touch {
            device_id: dev,
            id: tid,
            phase: egui::TouchPhase::Start,
            pos: egui::pos2(100.0, 100.0),
            force: None,
        }];
        let epoch = emu.frame_epoch;
        bridge.process_events(
            &mut emu,
            &touch_start,
            true,
            true,
            rect,
            rect,
            Some(epoch),
            200,
        );
        assert!(bridge.pointer_down);
        assert_eq!(bridge.active_touch_id, Some((dev, tid)));

        let touch_cancel = [egui::Event::Touch {
            device_id: dev,
            id: tid,
            phase: egui::TouchPhase::Cancel,
            pos: egui::pos2(100.0, 100.0),
            force: None,
        }];
        let epoch = emu.frame_epoch;
        bridge.process_events(
            &mut emu,
            &touch_cancel,
            true,
            true,
            rect,
            rect,
            Some(epoch),
            220,
        );
        assert!(!bridge.pointer_down);
        assert_eq!(bridge.active_touch_id, None);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);

        // Fresh touch starts cleanly after cancel.
        let tid2 = egui::TouchId(43);
        let epoch = emu.frame_epoch;
        bridge.process_events(
            &mut emu,
            &[
                egui::Event::Touch {
                    device_id: dev,
                    id: tid2,
                    phase: egui::TouchPhase::Start,
                    pos: egui::pos2(200.0, 200.0),
                    force: None,
                },
                egui::Event::Touch {
                    device_id: dev,
                    id: tid2,
                    phase: egui::TouchPhase::End,
                    pos: egui::pos2(200.0, 200.0),
                    force: None,
                },
            ],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            260,
        );
        assert!(matches!(emu.last_gesture(), Some(GestureEvent::Tap { .. })));
        assert!(!bridge.pointer_down);

        // Unrelated PointerGone does not cancel an active native touch.
        let tid3 = egui::TouchId(44);
        let epoch = emu.frame_epoch;
        bridge.process_events(
            &mut emu,
            &[
                egui::Event::Touch {
                    device_id: dev,
                    id: tid3,
                    phase: egui::TouchPhase::Start,
                    pos: egui::pos2(200.0, 200.0),
                    force: None,
                },
                egui::Event::PointerGone,
            ],
            true,
            true,
            rect,
            rect,
            Some(epoch),
            300,
        );
        assert!(bridge.pointer_down);
        assert_eq!(bridge.active_touch_id, Some((dev, tid3)));
        assert_eq!(emu.touch_phase(), RecognizerPhase::Tracking);
        let epoch = emu.frame_epoch;
        bridge.process_events(
            &mut emu,
            &[egui::Event::Touch {
                device_id: dev,
                id: tid3,
                phase: egui::TouchPhase::End,
                pos: egui::pos2(200.0, 200.0),
                force: None,
            }],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            320,
        );
        assert!(!bridge.pointer_down);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);

        // Queuing two clicks in one batch must not route the second click
        // against an undisplayed frame after the first click changed frame_epoch,
        // and fb_revision advances so the GUI requests a repaint.
        let mut emu2 = Emulator::boot(None);
        let rev_before = emu2.fb_revision();
        let mut bridge2 = GuiTouchBridge::default();
        let (nx, ny) = FbFrame::Portrait.map_to_native(100, 760).unwrap();
        let p = egui::pos2(nx as f32 + 0.5, ny as f32 + 0.5);
        let epoch2 = emu2.frame_epoch;
        bridge2.process_events(
            &mut emu2,
            &[
                egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            true,
            false,
            rect,
            rect,
            Some(epoch2),
            400,
        );
        assert_eq!(emu2.state().view, AppView::Library);
        assert_ne!(emu2.fb_revision(), rev_before);

        // A contact clipped out of admit_rect or arriving after displayed_epoch
        // advanced must not start.
        let clipped_rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH as f32 - 50.0, HEIGHT as f32),
        );
        let clipped_pos = egui::pos2(WIDTH as f32 - 10.0, 100.0);
        let epoch2 = emu2.frame_epoch;
        bridge2.process_events(
            &mut emu2,
            &[egui::Event::PointerButton {
                pos: clipped_pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            true,
            true,
            rect,
            clipped_rect,
            Some(epoch2),
            450,
        );
        assert!(!bridge2.pointer_down);
        assert_eq!(emu2.touch_phase(), RecognizerPhase::Idle);

        // Stale displayed_epoch (e.g. sidebar changed frame before CentralPanel)
        // rejects new contacts.
        let stale_epoch = emu2.frame_epoch.wrapping_sub(1);
        bridge2.process_events(
            &mut emu2,
            &[egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            true,
            true,
            rect,
            rect,
            Some(stale_epoch),
            460,
        );
        assert!(!bridge2.pointer_down);

        // Hold + Escape (`cancel_active` + `input(Back)`) + subsequent release
        // returns to Idle cleanly without sticking in Stale.
        let epoch2 = emu2.frame_epoch;
        bridge2.process_events(
            &mut emu2,
            &[egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            true,
            true,
            rect,
            rect,
            Some(epoch2),
            500,
        );
        assert!(bridge2.pointer_down);
        bridge2.cancel_active(&mut emu2, 510);
        emu2.input(Button::Back);
        assert_eq!(emu2.state().view, AppView::Home);
        assert!(!bridge2.pointer_down);
        assert_eq!(emu2.touch_phase(), RecognizerPhase::Idle);
        let epoch2 = emu2.frame_epoch;
        bridge2.process_events(
            &mut emu2,
            &[egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            true,
            false,
            rect,
            rect,
            Some(epoch2),
            520,
        );
        assert_eq!(emu2.touch_phase(), RecognizerPhase::Idle);
        assert!(!bridge2.pointer_down);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn gui_two_taps_across_separate_updates_rejects_tap_queued_before_new_frame_is_shown() {
        let mut emu = Emulator::boot(None);
        let mut upload = FrameUploadState::default();
        let mut bridge = GuiTouchBridge::default();
        let rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH as f32, HEIGHT as f32),
        );
        let (nx, ny) = FbFrame::Portrait.map_to_native(100, 760).unwrap();
        let p = egui::pos2(nx as f32 + 0.5, ny as f32 + 0.5);
        let click = [
            egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ];

        // Update 1: initial texture upload; no prior frame was shown yet, so a
        // click already queued for that update is rejected.
        let (needs_upload, displayed_epoch) =
            upload.begin_update(false, emu.fb_revision(), emu.frame_epoch);
        assert!(needs_upload);
        assert_eq!(displayed_epoch, None);
        bridge.process_events(
            &mut emu,
            &click,
            true,
            false,
            rect,
            rect,
            displayed_epoch,
            50,
        );
        assert_eq!(emu.state().view, AppView::Home);

        // Update 2: Home frame is already displayed -> tap 1 opens Library.
        let (needs_upload, displayed_epoch) =
            upload.begin_update(true, emu.fb_revision(), emu.frame_epoch);
        assert!(!needs_upload);
        bridge.process_events(
            &mut emu,
            &click,
            true,
            false,
            rect,
            rect,
            displayed_epoch,
            100,
        );
        assert_eq!(emu.state().view, AppView::Library);
        assert!(upload.is_stale(emu.fb_revision()));

        // Update 3: uploads Library frame while `displayed_epoch` is still
        // Home's epoch. A second tap queued before Library was presented is
        // rejected instead of hitting Library's Back slot.
        let (needs_upload, displayed_epoch) =
            upload.begin_update(true, emu.fb_revision(), emu.frame_epoch);
        assert!(needs_upload);
        assert_ne!(displayed_epoch, Some(emu.frame_epoch));
        bridge.process_events(
            &mut emu,
            &click,
            true,
            false,
            rect,
            rect,
            displayed_epoch,
            140,
        );
        assert_eq!(emu.state().view, AppView::Library);

        // Update 4: Library frame is now displayed -> next tap hits Back -> Home.
        let (needs_upload, displayed_epoch) =
            upload.begin_update(true, emu.fb_revision(), emu.frame_epoch);
        assert!(!needs_upload);
        bridge.process_events(
            &mut emu,
            &click,
            true,
            false,
            rect,
            rect,
            displayed_epoch,
            200,
        );
        assert_eq!(emu.state().view, AppView::Home);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn gui_rejected_touch_companion_pointer_release_does_not_end_mouse_gesture() {
        let mut emu = Emulator::boot(None);
        let mut bridge = GuiTouchBridge::default();
        let rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH as f32, HEIGHT as f32),
        );
        let (nx, ny) = FbFrame::Portrait.map_to_native(100, 760).unwrap();
        let library_pos = egui::pos2(nx as f32 + 0.5, ny as f32 + 0.5);

        // Mouse starts owning the gesture.
        let epoch = emu.frame_epoch;
        bridge.process_events(
            &mut emu,
            &[egui::Event::PointerButton {
                pos: library_pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            true,
            true,
            rect,
            rect,
            Some(epoch),
            100,
        );
        assert!(bridge.pointer_down);
        assert_eq!(bridge.active_touch_id, None);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Tracking);

        // While a secondary finger starts and moves, its companion pointer
        // events do not end the active mouse gesture (even across an idle
        // frame where aggregate primary_down flickers false). Once the finger
        // lifts and companion PointerGone clears egui-winit's pointer position
        // without a same-batch mouse release, the bridge cancels the now
        // unobservable mouse contact instead of stranding ownership.
        let dev = egui::TouchDeviceId(1);
        let tid = egui::TouchId(9);
        bridge.process_events(
            &mut emu,
            &[
                egui::Event::Touch {
                    device_id: dev,
                    id: tid,
                    phase: egui::TouchPhase::Start,
                    pos: egui::pos2(200.0, 100.0),
                    force: None,
                },
                egui::Event::PointerMoved(egui::pos2(200.0, 100.0)),
                egui::Event::PointerButton {
                    pos: egui::pos2(200.0, 100.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::Touch {
                    device_id: dev,
                    id: tid,
                    phase: egui::TouchPhase::Move,
                    pos: egui::pos2(400.0, 100.0),
                    force: None,
                },
                egui::Event::PointerMoved(egui::pos2(400.0, 100.0)),
            ],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            120,
        );
        assert!(bridge.pointer_down);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Tracking);
        assert_eq!(emu.state().view, AppView::Home);

        bridge.process_events(&mut emu, &[], true, false, rect, rect, Some(epoch), 136);
        assert!(bridge.pointer_down);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Tracking);

        bridge.process_events(
            &mut emu,
            &[
                egui::Event::Touch {
                    device_id: dev,
                    id: tid,
                    phase: egui::TouchPhase::End,
                    pos: egui::pos2(400.0, 100.0),
                    force: None,
                },
                egui::Event::PointerButton {
                    pos: egui::pos2(400.0, 100.0),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerGone,
            ],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            150,
        );
        assert!(!bridge.pointer_down);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);
        assert_eq!(emu.state().view, AppView::Home);

        // Next genuine touch works immediately even if the backend dropped the
        // mouse release while pointer_pos_in_points was cleared.
        let tid2 = egui::TouchId(10);
        bridge.process_events(
            &mut emu,
            &[
                egui::Event::Touch {
                    device_id: dev,
                    id: tid2,
                    phase: egui::TouchPhase::Start,
                    pos: library_pos,
                    force: None,
                },
                egui::Event::Touch {
                    device_id: dev,
                    id: tid2,
                    phase: egui::TouchPhase::End,
                    pos: library_pos,
                    force: None,
                },
            ],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            170,
        );
        assert_eq!(emu.state().view, AppView::Library);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn gui_mouse_move_after_companion_end_preserves_drag_across_updates() {
        let mut emu = Emulator::boot(None);
        let mut bridge = GuiTouchBridge::default();
        let rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH as f32, HEIGHT as f32),
        );
        let start = egui::pos2(100.0, 200.0);
        let end = egui::pos2(200.0, 200.0);
        let finger = egui::pos2(400.0, 100.0);
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let touch = |phase| egui::Event::Touch {
            device_id: egui::TouchDeviceId(1),
            id: egui::TouchId(9),
            phase,
            pos: finger,
            force: None,
        };
        let epoch = emu.frame_epoch;
        bridge.process_events(
            &mut emu,
            &[button(start, true)],
            true,
            true,
            rect,
            rect,
            Some(epoch),
            100,
        );
        bridge.process_events(
            &mut emu,
            &[
                touch(egui::TouchPhase::Start),
                egui::Event::PointerMoved(finger),
                button(finger, true),
                touch(egui::TouchPhase::End),
                button(finger, false),
                egui::Event::PointerGone,
                egui::Event::PointerMoved(end),
            ],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            120,
        );
        assert!(bridge.pointer_down);
        assert!(bridge.mouse_pointer_state_ambiguous);
        assert!(!bridge.mouse_position_ambiguous);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Tracking);

        bridge.process_events(
            &mut emu,
            &[button(end, false)],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            140,
        );
        assert!(matches!(
            emu.last_gesture(),
            Some(GestureEvent::Swipe { .. })
        ));
        assert!(!bridge.pointer_down);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn gui_same_batch_new_mouse_press_after_resolved_companion_end_is_not_canceled() {
        let mut emu = Emulator::boot(None);
        emu.library_event(LibraryEvent::Scanned {
            count: 3,
            catalog_epoch: 1,
        });
        emu.input(Button::Back);
        assert_eq!(emu.state().view, AppView::Library);

        let mut bridge = GuiTouchBridge::default();
        let rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH as f32, HEIGHT as f32),
        );
        let (bx, by) = FbFrame::Portrait.map_to_native(220, 735).unwrap();
        let blank_pos = egui::pos2(bx as f32 + 0.5, by as f32 + 0.5);
        let (rx, ry) = FbFrame::Portrait.map_to_native(220, 166).unwrap();
        let row1_pos = egui::pos2(rx as f32 + 0.5, ry as f32 + 0.5);
        let finger = egui::pos2(400.0, 100.0);
        let epoch = emu.frame_epoch;

        // First mouse contact starts on blank Library space (releasing it does
        // not advance frame_epoch).
        bridge.process_events(
            &mut emu,
            &[egui::Event::PointerButton {
                pos: blank_pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            true,
            true,
            rect,
            rect,
            Some(epoch),
            100,
        );
        assert!(bridge.pointer_down);

        // Native Touch Start/End emits companion PointerGone (arming
        // mouse_release_unobservable), then genuine PointerMoved + release
        // resolves the first contact, and a fresh mouse press starts on row 1
        // in the same update.
        let dev = egui::TouchDeviceId(1);
        let tid = egui::TouchId(9);
        bridge.process_events(
            &mut emu,
            &[
                egui::Event::Touch {
                    device_id: dev,
                    id: tid,
                    phase: egui::TouchPhase::Start,
                    pos: finger,
                    force: None,
                },
                egui::Event::PointerMoved(finger),
                egui::Event::PointerButton {
                    pos: finger,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::Touch {
                    device_id: dev,
                    id: tid,
                    phase: egui::TouchPhase::End,
                    pos: finger,
                    force: None,
                },
                egui::Event::PointerButton {
                    pos: finger,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerGone,
                egui::Event::PointerMoved(blank_pos),
                egui::Event::PointerButton {
                    pos: blank_pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: row1_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            true,
            true,
            rect,
            rect,
            Some(epoch),
            120,
        );
        assert!(bridge.pointer_down);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Tracking);
        assert_eq!(emu.frame_epoch, epoch);

        bridge.process_events(
            &mut emu,
            &[egui::Event::PointerButton {
                pos: row1_pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            140,
        );
        assert!(!bridge.pointer_down);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);
        assert_eq!(emu.state().view, AppView::Reading);
        assert_eq!(emu.state().book_id, app_core::ReaderSource::sd(1).book_id());
    }

    #[cfg(feature = "gui")]
    #[test]
    fn gui_mixed_batch_preserves_real_mouse_release_and_loss() {
        let rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH as f32, HEIGHT as f32),
        );
        let (nx, ny) = FbFrame::Portrait.map_to_native(100, 760).unwrap();
        let mouse = egui::pos2(nx as f32 + 0.5, ny as f32 + 0.5);
        let finger = egui::pos2(200.0, 100.0);
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let touch = |phase| egui::Event::Touch {
            device_id: egui::TouchDeviceId(1),
            id: egui::TouchId(9),
            phase,
            pos: finger,
            force: None,
        };
        for mouse_started_earlier in [false, true] {
            let mut emu = Emulator::boot(None);
            let mut bridge = GuiTouchBridge::default();
            let epoch = emu.frame_epoch;
            let mut events = vec![button(mouse, true)];
            if mouse_started_earlier {
                bridge.process_events(&mut emu, &events, true, true, rect, rect, Some(epoch), 100);
                events.clear();
            }
            events.extend([
                touch(egui::TouchPhase::Start),
                egui::Event::PointerMoved(finger),
                button(finger, true),
                touch(egui::TouchPhase::End),
                button(finger, false),
                egui::Event::PointerGone,
                button(mouse, false),
            ]);
            bridge.process_events(&mut emu, &events, true, false, rect, rect, Some(epoch), 120);
            assert_eq!(emu.state().view, AppView::Library);
            assert!(!bridge.pointer_down);
            assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);
        }

        // egui-winit reports the finger's position on the real mouse
        // release unless a genuine mouse move restores the stored position.
        for mouse_moved in [false, true] {
            let mut emu = Emulator::boot(None);
            let mut bridge = GuiTouchBridge::default();
            let epoch = emu.frame_epoch;
            let mut events = vec![
                button(mouse, true),
                touch(egui::TouchPhase::Start),
                egui::Event::PointerMoved(finger),
                button(finger, true),
            ];
            let (nx, ny) = FbFrame::Portrait.map_to_native(104, 760).unwrap();
            let release = egui::pos2(nx as f32 + 0.5, ny as f32 + 0.5);
            if mouse_moved {
                events.push(egui::Event::PointerMoved(mouse));
                events.push(button(release, false));
            } else {
                events.push(button(finger, false));
            }
            bridge.process_events(&mut emu, &events, true, false, rect, rect, Some(epoch), 100);
            assert_eq!(emu.state().view, AppView::Library);
            assert!(!bridge.pointer_down);
            let expected = TouchPoint::new(if mouse_moved { 104 } else { 100 }, 760);
            assert!(matches!(
                emu.last_gesture(),
                Some(GestureEvent::Tap { end, .. }) if end == expected
            ));
        }

        // Real loss after a finger's companion loss still cancels the mouse.
        let mut emu = Emulator::boot(None);
        let mut bridge = GuiTouchBridge::default();
        let epoch = emu.frame_epoch;
        bridge.process_events(
            &mut emu,
            &[
                button(mouse, true),
                touch(egui::TouchPhase::Start),
                egui::Event::PointerMoved(finger),
                button(finger, true),
                touch(egui::TouchPhase::Cancel),
                egui::Event::PointerGone,
                egui::Event::PointerGone,
            ],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            100,
        );
        assert!(matches!(
            emu.last_gesture(),
            Some(GestureEvent::Canceled { .. })
        ));
        assert!(!bridge.pointer_down);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn gui_recovers_mouse_ownership_after_dropped_release() {
        let rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH as f32, HEIGHT as f32),
        );
        let (nx, ny) = FbFrame::Portrait.map_to_native(100, 760).unwrap();
        let library = egui::pos2(nx as f32 + 0.5, ny as f32 + 0.5);
        let old_mouse = egui::pos2(200.0, 100.0);
        let finger = egui::pos2(400.0, 100.0);
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let touch = |phase| egui::Event::Touch {
            device_id: egui::TouchDeviceId(1),
            id: egui::TouchId(9),
            phase,
            pos: finger,
            force: None,
        };
        for timeout in [false, true] {
            let mut emu = Emulator::boot(None);
            let mut bridge = GuiTouchBridge::default();
            let epoch = emu.frame_epoch;
            bridge.process_events(
                &mut emu,
                &[
                    button(old_mouse, true),
                    touch(egui::TouchPhase::Start),
                    egui::Event::PointerMoved(finger),
                    button(finger, true),
                ],
                true,
                false,
                rect,
                rect,
                Some(epoch),
                100,
            );
            assert!(bridge.pointer_down);
            assert!(bridge.mouse_pointer_state_ambiguous);
            let now = if timeout { 8_100 } else { 200 };
            bridge.process_events(&mut emu, &[], true, false, rect, rect, Some(epoch), now);
            assert_eq!(bridge.pointer_down, !timeout);
            if timeout {
                assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);
                assert!(matches!(
                    emu.last_gesture(),
                    Some(GestureEvent::Canceled {
                        reason: app_core::touch::CancelReason::StationaryTimeout,
                        ..
                    })
                ));
            }
            // No real release arrived. The next press must start from the
            // Library control, not finish the old gesture from another point.
            bridge.process_events(
                &mut emu,
                &[button(library, true), button(library, false)],
                true,
                false,
                rect,
                rect,
                Some(epoch),
                now + 20,
            );
            assert_eq!(emu.state().view, AppView::Library);
            assert!(matches!(emu.last_gesture(), Some(GestureEvent::Tap { .. })));
            assert!(!bridge.pointer_down);
            assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);
        }
    }

    #[cfg(feature = "gui")]
    #[test]
    fn gui_hold_test_target_arms_mouse_and_native_touch() {
        let rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH as f32, HEIGHT as f32),
        );
        for native_touch in [false, true] {
            for (logical_x, expect_hold) in [(200, true), (300, false)] {
                let mut emu = Emulator::boot(None);
                let mut bridge = GuiTouchBridge {
                    hold_test_enabled: true,
                    ..GuiTouchBridge::default()
                };
                let (nx, ny) = FbFrame::Portrait.map_to_native(logical_x, 200).unwrap();
                let pos = egui::pos2(nx as f32 + 0.5, ny as f32 + 0.5);
                let event = |pressed| {
                    if native_touch {
                        egui::Event::Touch {
                            device_id: egui::TouchDeviceId(1),
                            id: egui::TouchId(1),
                            phase: if pressed {
                                egui::TouchPhase::Start
                            } else {
                                egui::TouchPhase::End
                            },
                            pos,
                            force: None,
                        }
                    } else {
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        }
                    }
                };
                let epoch = emu.frame_epoch;
                bridge.process_events(
                    &mut emu,
                    &[event(true)],
                    true,
                    true,
                    rect,
                    rect,
                    Some(epoch),
                    100,
                );
                bridge.process_events(&mut emu, &[], true, true, rect, rect, Some(epoch), 550);
                assert_eq!(bridge.hold_test_recognized, expect_hold);
                bridge.process_events(
                    &mut emu,
                    &[event(false)],
                    true,
                    false,
                    rect,
                    rect,
                    Some(epoch),
                    600,
                );
                assert_eq!(bridge.hold_test_recognized, expect_hold);
                assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);

                // The result survives lift, but not a new attempt.
                bridge.process_events(
                    &mut emu,
                    &[event(true)],
                    true,
                    true,
                    rect,
                    rect,
                    Some(epoch),
                    700,
                );
                assert!(!bridge.hold_test_recognized);
                bridge.process_events(&mut emu, &[], true, true, rect, rect, Some(epoch), 1150);
                assert_eq!(bridge.hold_test_recognized, expect_hold);
                bridge.process_events(
                    &mut emu,
                    &[event(false)],
                    true,
                    false,
                    rect,
                    rect,
                    Some(epoch),
                    1200,
                );
                bridge.set_hold_test_enabled(false, &mut emu, 1250);
                bridge.set_hold_test_enabled(true, &mut emu, 1300);
                assert!(!bridge.hold_test_recognized);
                // A previous long press is still recorded by the emulator,
                // but must not be shown as the result of the new hold test.
                if expect_hold {
                    assert!(matches!(
                        emu.last_gesture(),
                        Some(GestureEvent::LongPress { .. })
                    ));
                }
            }
        }
    }

    #[cfg(feature = "gui")]
    #[test]
    fn gui_image_rect_resize_cancels_active_gesture() {
        let mut emu = Emulator::boot(None);
        emu.input(Button::Back);
        assert_eq!(emu.state().view, AppView::Library);
        let mut bridge = GuiTouchBridge::default();
        let rect1 = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH as f32, HEIGHT as f32),
        );
        let pos = egui::pos2(300.0, 200.0);
        let epoch = emu.frame_epoch;
        bridge.process_events(
            &mut emu,
            &[egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            true,
            true,
            rect1,
            rect1,
            Some(epoch),
            100,
        );
        assert!(bridge.pointer_down);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Tracking);

        // Window/image grows to 1.5x before release at the same screen coords:
        // the active contact cancels instead of turning into a bogus swipe.
        let rect2 = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH as f32 * 1.5, HEIGHT as f32 * 1.5),
        );
        bridge.process_events(
            &mut emu,
            &[egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            true,
            false,
            rect2,
            rect2,
            Some(epoch),
            140,
        );
        assert!(!bridge.pointer_down);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);
        assert_eq!(emu.state().view, AppView::Library);

        // A new press queued in the same batch as a layout change must also
        // be rejected, even though the framebuffer epoch has not changed.
        for native_touch in [false, true] {
            let start = if native_touch {
                egui::Event::Touch {
                    device_id: egui::TouchDeviceId(1),
                    id: egui::TouchId(1),
                    phase: egui::TouchPhase::Start,
                    pos,
                    force: None,
                }
            } else {
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                }
            };
            bridge.last_rect = Some(rect1);
            bridge.process_events(
                &mut emu,
                &[start],
                true,
                true,
                rect2,
                rect2,
                Some(epoch),
                160,
            );
            assert!(!bridge.pointer_down);
            assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);
        }
    }

    #[test]
    fn listing_replacement_cancels_active_library_row_tap() {
        let mut emu = Emulator::boot(None);
        emu.library_event(LibraryEvent::Scanned {
            count: 3,
            catalog_epoch: 1,
        });
        emu.input(Button::Back);
        assert_eq!(emu.state().view, AppView::Library);

        emu.touch_down(220, 118, 100, None);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Tracking);

        emu.library_event(LibraryEvent::FolderListed {
            request_id: None,
            browse_epoch: emu.state().library_browse_epoch.wrapping_add(1),
            depth: 0,
            count: 2,
            books: 2,
            selection: 0,
        });
        assert_eq!(emu.touch_phase(), RecognizerPhase::Stale);
        assert_eq!(emu.touch_up(Some((220, 118)), 140), None);
        assert_eq!(emu.state().view, AppView::Library);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);
    }

    #[test]
    fn stationary_hold_interrupted_by_listing_replacement_does_not_fire_on_replacement_row() {
        let mut emu = Emulator::boot(None);
        emu.library_event(LibraryEvent::Scanned {
            count: 3,
            catalog_epoch: 1,
        });
        emu.input(Button::Back);
        assert_eq!(emu.state().view, AppView::Library);

        let target = display::Rect::new(40, 80, 400, 56);
        emu.touch_down(220, 118, 100, Some(target));
        assert_eq!(emu.touch_phase(), RecognizerPhase::Tracking);

        // Storage repaint replaces rows before long_press_ms (450 ms) elapses.
        emu.library_event(LibraryEvent::FolderListed {
            request_id: None,
            browse_epoch: emu.state().library_browse_epoch.wrapping_add(1),
            depth: 0,
            count: 2,
            books: 2,
            selection: 0,
        });
        assert_eq!(emu.touch_phase(), RecognizerPhase::Stale);
        assert_eq!(emu.touch_tick(600), None);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Stale);
        assert!(!matches!(
            emu.last_gesture(),
            Some(GestureEvent::LongPress { .. })
        ));
    }

    #[test]
    fn frame_change_from_portrait_to_landscape_cancels_active_touch_until_clean_lift() {
        let mut emu = Emulator::boot(None);
        assert_eq!(
            emu.state().orientation,
            app_core::DisplayOrientation::PortraitButtonsLeft
        );
        emu.touch_down(100, 300, 100, None);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Tracking);

        // Changing frame/view via render cancels active touch into Stale.
        emu.state.orientation = app_core::DisplayOrientation::LandscapeButtonsBottom;
        emu.render(app_core::RenderKind::Page);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Stale);

        // Lifting at a completely different logical point cannot dispatch a swipe.
        assert_eq!(emu.touch_up(Some((500, 100)), 140), None);
        assert_eq!(emu.state().view, AppView::Home);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);

        // Next clean touch works in the new landscape frame.
        emu.touch_down(60, 120, 200, None);
        assert!(matches!(
            emu.touch_up(Some((60, 120)), 240),
            Some(GestureEvent::Tap { .. })
        ));
        assert_eq!(emu.state().view, AppView::Library);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn gui_ambiguous_mouse_pointer_state_does_not_fire_hold_on_tick() {
        let mut emu = Emulator::boot(None);
        let mut bridge = GuiTouchBridge::default();
        bridge.set_hold_test_enabled(true, &mut emu, 50);
        let rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH as f32, HEIGHT as f32),
        );
        let (nx, ny) = FbFrame::Portrait.map_to_native(210, 210).unwrap();
        let hold_pos = egui::pos2(nx as f32 + 0.5, ny as f32 + 0.5);
        let finger = egui::pos2(400.0, 100.0);
        let epoch = emu.frame_epoch;

        // Arm the mouse hold, then keep a secondary native touch active so its
        // companion pointer events leave mouse_pointer_state_ambiguous = true.
        bridge.process_events(
            &mut emu,
            &[
                egui::Event::PointerButton {
                    pos: hold_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::Touch {
                    device_id: egui::TouchDeviceId(1),
                    id: egui::TouchId(9),
                    phase: egui::TouchPhase::Start,
                    pos: finger,
                    force: None,
                },
                egui::Event::PointerMoved(finger),
                egui::Event::PointerButton {
                    pos: finger,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            100,
        );
        assert!(bridge.mouse_pointer_state_ambiguous);

        // Idle frame past long_press_ms (450 ms) must not report "Hold recognized"
        // or consume the contact, and still cleans up after timeout.
        bridge.process_events(&mut emu, &[], true, false, rect, rect, Some(epoch), 600);
        assert!(!bridge.hold_test_recognized);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Tracking);

        // Even while cursor keeps moving, ambiguous ownership expires at
        // max_stationary_hold_ms instead of renewing forever.
        for t in (1_500..8_000).step_by(1_000) {
            let (sx, sy) = FbFrame::Portrait
                .map_to_native(210 + ((t / 1_000) as usize % 3), 210)
                .unwrap();
            bridge.process_events(
                &mut emu,
                &[egui::Event::PointerMoved(egui::pos2(
                    sx as f32 + 0.5,
                    sy as f32 + 0.5,
                ))],
                true,
                false,
                rect,
                rect,
                Some(epoch),
                t,
            );
            assert!(bridge.pointer_down);
        }

        // First GUI update at the deadline containing a native Touch::Start
        // expires ambiguous mouse ownership before admitting the new contact.
        bridge.process_events(
            &mut emu,
            &[
                egui::Event::Touch {
                    device_id: egui::TouchDeviceId(1),
                    id: egui::TouchId(10),
                    phase: egui::TouchPhase::Start,
                    pos: finger,
                    force: None,
                },
                egui::Event::Touch {
                    device_id: egui::TouchDeviceId(1),
                    id: egui::TouchId(10),
                    phase: egui::TouchPhase::End,
                    pos: finger,
                    force: None,
                },
            ],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            8_100,
        );
        assert!(!bridge.hold_test_recognized);
        assert!(!bridge.pointer_down);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Idle);
        assert!(matches!(emu.last_gesture(), Some(GestureEvent::Tap { .. })));
    }

    #[cfg(feature = "gui")]
    #[test]
    fn gui_ambiguous_mouse_move_does_not_fire_hold() {
        let mut emu = Emulator::boot(None);
        let mut bridge = GuiTouchBridge::default();
        bridge.set_hold_test_enabled(true, &mut emu, 50);
        let rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(WIDTH as f32, HEIGHT as f32),
        );
        let (nx, ny) = FbFrame::Portrait.map_to_native(210, 210).unwrap();
        let hold_pos = egui::pos2(nx as f32 + 0.5, ny as f32 + 0.5);
        let (mx, my) = FbFrame::Portrait.map_to_native(211, 210).unwrap();
        let moved_pos = egui::pos2(mx as f32 + 0.5, my as f32 + 0.5);
        let (sx, sy) = FbFrame::Portrait.map_to_native(110, 210).unwrap();
        let swipe_end = egui::pos2(sx as f32 + 0.5, sy as f32 + 0.5);
        let finger = egui::pos2(400.0, 100.0);
        let epoch = emu.frame_epoch;

        bridge.process_events(
            &mut emu,
            &[
                egui::Event::PointerButton {
                    pos: hold_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::Touch {
                    device_id: egui::TouchDeviceId(1),
                    id: egui::TouchId(9),
                    phase: egui::TouchPhase::Start,
                    pos: finger,
                    force: None,
                },
                egui::Event::PointerMoved(finger),
                egui::Event::PointerButton {
                    pos: finger,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            100,
        );
        assert!(bridge.mouse_pointer_state_ambiguous);

        // Moving the mouse 1 logical px at t = 600 ms restores position, not
        // button state, and never consumes the contact as a hold so a later
        // genuine mouse drag + release completes a Swipe.
        bridge.process_events(
            &mut emu,
            &[egui::Event::PointerMoved(moved_pos)],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            600,
        );
        assert!(!bridge.hold_test_recognized);
        assert_eq!(emu.touch_phase(), RecognizerPhase::Tracking);

        bridge.process_events(
            &mut emu,
            &[
                egui::Event::PointerMoved(swipe_end),
                egui::Event::PointerButton {
                    pos: swipe_end,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            true,
            false,
            rect,
            rect,
            Some(epoch),
            650,
        );
        assert!(!bridge.hold_test_recognized);
        assert!(matches!(
            emu.last_gesture(),
            Some(GestureEvent::Swipe {
                direction: app_core::touch::SwipeDirection::Left,
                ..
            })
        ));
    }

    #[test]
    fn sd_book_contents_draw_count_governs_row_taps() {
        // 1-chapter SD book draws only row 0, so tapping row 2 (previously
        // filled by demo chapter "Power") is ignored cleanly.
        let mut one = Emulator::boot(None);
        one.library_event(LibraryEvent::Scanned {
            count: 3,
            catalog_epoch: 1,
        });
        one.library_event(LibraryEvent::Loaded {
            book_id: app_core::ReaderSource::sd(0).book_id(),
            pages: 10,
            complete: true,
            chapters: 1,
            current_chapter: 0,
            chapter_pages: [0; app_core::MAX_SD_CHAPTERS],
            chapter_subsections: [0; app_core::MAX_SD_CHAPTER_SUBSECTIONS / 8],
            position: Some(0),
            text_replaced: true,
        });
        one.input(Button::Confirm);
        one.input(Button::Confirm);
        one.input(Button::Confirm);
        assert_eq!(one.state().view, AppView::Chapters);
        assert_eq!(one.displayed_chapters.rows_drawn, 1);

        one.touch_down(220, 190, 100, None);
        one.touch_up(Some((220, 190)), 140);
        assert_eq!(one.state().view, AppView::Chapters);
        assert_eq!(one.state().chapter, 0);

        one.touch_down(220, 118, 200, None);
        one.touch_up(Some((220, 118)), 240);
        assert_eq!(one.state().view, AppView::Reading);
        assert_eq!(one.state().chapter, 0);

        // 10-chapter SD book draws all 10 chapters in portrait, so row 2 opens chapter 2
        // while undrawn row 10 below it is ignored.
        let mut ten = Emulator::boot(None);
        ten.library_event(LibraryEvent::Scanned {
            count: 3,
            catalog_epoch: 1,
        });
        ten.library_event(LibraryEvent::Loaded {
            book_id: app_core::ReaderSource::sd(0).book_id(),
            pages: 50,
            complete: true,
            chapters: 10,
            current_chapter: 0,
            chapter_pages: [0; app_core::MAX_SD_CHAPTERS],
            chapter_subsections: [0; app_core::MAX_SD_CHAPTER_SUBSECTIONS / 8],
            position: Some(0),
            text_replaced: true,
        });
        ten.input(Button::Confirm);
        ten.input(Button::Confirm);
        ten.input(Button::Confirm);
        assert_eq!(ten.state().view, AppView::Chapters);
        assert_eq!(ten.displayed_chapters.rows_drawn, 10);

        ten.touch_down(220, 478, 100, None);
        ten.touch_up(Some((220, 478)), 140);
        assert_eq!(ten.state().view, AppView::Chapters);

        ten.touch_down(220, 190, 200, None);
        ten.touch_up(Some((220, 190)), 240);
        assert_eq!(ten.state().view, AppView::Reading);
        assert_eq!(ten.state().chapter, 2);
    }
}
