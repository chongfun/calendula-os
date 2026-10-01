//! A device on the host: the storage task over a FAT image in RAM, and the
//! app's reducer over the events it sends, wired the way `fw/src/tasks/app.rs`
//! and `fw/src/tasks/display.rs` wire them.
//!
//! The card is the only thing that survives a power cycle, as on the device:
//! [`Device::sleep`] flushes what the firmware flushes before deep sleep and
//! [`Device::wake`] starts the store, the storage task and the app afresh over
//! the same card. Between the two, [`Card::rename`] stands in for a computer
//! moving a book while the device is off.

#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use app_core::{
    library_action_command_for_transition, library_browse_command_for_transition,
    storage_command_for_transition, AppView, Button, InputEvent, LibraryEvent, ReaderState,
    ReducerContext, RenderKind, RenderRequest, StorageCommand, SyncSession,
};
use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Directory, VolumeIdx, VolumeManager,
};
use reader_cache::store::ReaderStore;
use storage::book_build::ReaderCacheScratch;
use storage::card::{Root, SessionError, StaticTime};
use storage::custom_font::MetricCache;
use storage::task::{Host, StorageTask};

// ---------------------------------------------------------------------------
// The card
// ---------------------------------------------------------------------------

const BLOCK_BYTES: usize = 512;
const DISK_BLOCKS: u32 = 64 * 1024;
const PART_START_BLOCK: u32 = 64;

#[derive(Debug)]
pub struct DiskError;

impl core::fmt::Display for DiskError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "disk error")
    }
}

impl std::error::Error for DiskError {}

/// An SD card's blocks in RAM, shared between the device and the computer
/// that edits it while the device is off.
#[derive(Clone)]
pub struct Disk {
    bytes: Rc<RefCell<Vec<u8>>>,
    writes: Rc<Cell<u64>>,
}

impl Disk {
    /// Blocks written since the card was made, a witness for "nothing was
    /// rebuilt" that does not depend on log lines.
    pub fn writes(&self) -> u64 {
        self.writes.get()
    }
}

impl BlockDevice for Disk {
    type Error = DiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), DiskError> {
        let bytes = self.bytes.borrow();
        for (i, block) in blocks.iter_mut().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            block.copy_from_slice(&bytes[at..at + BLOCK_BYTES]);
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), DiskError> {
        let mut bytes = self.bytes.borrow_mut();
        for (i, block) in blocks.iter().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            bytes[at..at + BLOCK_BYTES].copy_from_slice(&block[..]);
        }
        self.writes.set(self.writes.get() + blocks.len() as u64);
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, DiskError> {
        Ok(BlockCount(DISK_BLOCKS))
    }
}

/// The card, as both the storage task and a computer reach it.
pub struct Card {
    pub disk: Disk,
}

impl Card {
    /// A freshly formatted card: MBR, one FAT16 partition, nothing on it.
    pub fn blank() -> Card {
        let mut image = vec![0u8; DISK_BLOCKS as usize * BLOCK_BYTES];
        let part_blocks = DISK_BLOCKS - PART_START_BLOCK;
        let mut partition = vec![0u8; part_blocks as usize * BLOCK_BYTES];
        fatfs::format_volume(
            std::io::Cursor::new(partition.as_mut_slice()),
            fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat16),
        )
        .expect("format the partition");
        image[PART_START_BLOCK as usize * BLOCK_BYTES..].copy_from_slice(&partition);
        let entry = 446;
        image[entry + 4] = 0x06;
        image[entry + 8..entry + 12].copy_from_slice(&PART_START_BLOCK.to_le_bytes());
        image[entry + 12..entry + 16].copy_from_slice(&part_blocks.to_le_bytes());
        image[510] = 0x55;
        image[511] = 0xAA;
        Card {
            disk: Disk {
                bytes: Rc::new(RefCell::new(image)),
                writes: Rc::new(Cell::new(0)),
            },
        }
    }

    /// Run `f` against the card's root, as one session.
    pub fn session<R>(
        &self,
        f: impl for<'a> FnOnce(&Directory<'a, Disk, StaticTime, 8, 8, 1>) -> R,
    ) -> R {
        let mgr: VolumeManager<Disk, StaticTime, 8, 8, 1> =
            VolumeManager::new_with_limits(self.disk.clone(), StaticTime, 5000);
        let volume = mgr.open_volume(VolumeIdx(0)).expect("open the volume");
        let root = volume.open_root_dir().expect("open the root");
        f(&root)
    }

    /// Put a file at `path` (components separated by `/`, long names allowed),
    /// making its folders, the way a computer copies a book on.
    pub fn put(&self, path: &str, bytes: &[u8]) {
        self.session(|root| {
            let parts: Vec<&str> = path.split('/').collect();
            let (name, folders) = parts.split_last().expect("a path");
            put_under(root, folders, name, bytes);
        });
    }

    /// Move the file at `from` to `to`, the way a computer moves a book:
    /// one rename on the card, folders made as needed, the bytes untouched.
    pub fn rename(&self, from: &str, to: &str) {
        let body = self.read(from).expect("the book to move is on the card");
        self.session(|root| {
            let parts: Vec<&str> = to.split('/').collect();
            let (name, folders) = parts.split_last().expect("a path");
            let from_parts: Vec<&str> = from.split('/').collect();
            let (from_name, from_folders) = from_parts.split_last().expect("a path");
            let source = walk(root, from_folders);
            let dest = walk_making(root, folders);
            source
                .move_file_in_dir_lfn(*from_name, &dest, name)
                .or_else(|_| {
                    // A name the 8.3 lookup cannot reach: find its alias.
                    let alias = alias_of(&source, from_name);
                    source.move_file_in_dir_lfn(alias.as_str(), &dest, name)
                })
                .expect("the move lands");
        });
        assert_eq!(
            self.read(to).as_deref(),
            Some(&body[..]),
            "the bytes moved intact"
        );
    }

    /// Remove the file at `path`, chain and all.
    pub fn delete(&self, path: &str) {
        self.session(|root| {
            let parts: Vec<&str> = path.split('/').collect();
            let (name, folders) = parts.split_last().expect("a path");
            let dir = walk(root, folders);
            let alias = alias_of(&dir, name);
            assert_eq!(
                upload_store::remove_file_reclaiming_clusters(&dir, alias.as_str()),
                upload_store::RemoveStatus::Removed,
                "{path} is gone"
            );
        });
    }

    /// The bytes of the file at `path`, if there is one.
    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        self.session(|root| {
            let parts: Vec<&str> = path.split('/').collect();
            let (name, folders) = parts.split_last()?;
            let dir = try_walk(root, folders)?;
            let alias = alias_of(&dir, name);
            let file = dir
                .open_file_in_dir(alias.as_str(), embedded_sdmmc::Mode::ReadOnly)
                .ok()?;
            let mut body = vec![0u8; file.length() as usize];
            let mut at = 0;
            while at < body.len() {
                let read = file.read(&mut body[at..]).ok()?;
                if read == 0 {
                    break;
                }
                at += read;
            }
            Some(body)
        })
    }
}

type Dir<'a> = Directory<'a, Disk, StaticTime, 8, 8, 1>;

fn alias_of(dir: &Dir<'_>, long: &str) -> String {
    let mut found = None;
    let mut buffer = [0u8; 256];
    let mut lfn = embedded_sdmmc::LfnBuffer::new(&mut buffer);
    dir.iterate_dir_lfn(&mut lfn, |entry, name| {
        if name.is_some_and(|name| name == long) || entry.name.to_string() == long {
            found = Some(entry.name.to_string());
            core::ops::ControlFlow::Break(())
        } else {
            core::ops::ControlFlow::Continue(())
        }
    })
    .expect("list the folder");
    found.unwrap_or_else(|| long.to_owned())
}

fn try_walk<'a>(root: &Dir<'a>, folders: &[&str]) -> Option<Dir<'a>> {
    let mut current: Option<Dir<'a>> = None;
    for folder in folders {
        let parent = current.as_ref().unwrap_or(root);
        let alias = alias_of(parent, folder);
        current = Some(parent.open_dir(alias.as_str()).ok()?);
    }
    match current {
        Some(dir) => Some(dir),
        None => root.open_dir(".").ok(),
    }
}

fn walk<'a>(root: &Dir<'a>, folders: &[&str]) -> Dir<'a> {
    try_walk(root, folders).expect("the folder is on the card")
}

fn walk_making<'a>(root: &Dir<'a>, folders: &[&str]) -> Dir<'a> {
    let mut current: Option<Dir<'a>> = None;
    for folder in folders {
        let parent = current.as_ref().unwrap_or(root);
        let alias = alias_of(parent, folder);
        let next = match parent.open_dir(alias.as_str()) {
            Ok(dir) => dir,
            Err(_) => {
                parent.make_dir_in_dir_lfn(folder).expect("make the folder");
                let alias = alias_of(parent, folder);
                parent
                    .open_dir(alias.as_str())
                    .expect("open the new folder")
            }
        };
        current = Some(next);
    }
    match current {
        Some(dir) => dir,
        None => root.open_dir(".").expect("reopen the root"),
    }
}

fn put_under(root: &Dir<'_>, folders: &[&str], name: &str, bytes: &[u8]) {
    let dir = walk_making(root, folders);
    let file = dir.create_file_in_dir_lfn(name).expect("create the file");
    file.write(bytes).expect("write the file");
    file.close().expect("close the file");
}

/// The storage code's view of the card: one volume manager per session, as
/// the firmware opens one per SPI session.
pub struct SessionCard {
    pub disk: Disk,
}

impl storage::card::Card for SessionCard {
    type Device<'a>
        = Disk
    where
        Self: 'a;

    fn with_root<R>(
        &mut self,
        f: impl for<'a> FnOnce(&Root<'a, Disk>) -> R,
    ) -> Result<R, SessionError> {
        let mgr: VolumeManager<Disk, StaticTime, 8, 8, 1> =
            VolumeManager::new_with_limits(self.disk.clone(), StaticTime, 5000);
        let volume = mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| SessionError::Volume)?;
        let root = volume.open_root_dir().map_err(|_| SessionError::Root)?;
        Ok(f(&root))
    }
}

// ---------------------------------------------------------------------------
// A book
// ---------------------------------------------------------------------------

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A zip of stored entries, which is all an EPUB needs to be.
fn zip(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, body) in entries {
        let offset = out.len() as u32;
        let crc = crc32(body);
        let size = body.len() as u32;
        let mut local = Vec::new();
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&(name.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&local);
        out.extend_from_slice(body);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// An EPUB of `chapters` chapters, each long enough to fill several pages.
/// `seed` makes two books' bytes differ.
pub fn epub(title: &str, chapters: usize, seed: u32) -> Vec<u8> {
    let mut entries: Vec<(&str, Vec<u8>)> = Vec::new();
    entries.push(("mimetype", b"application/epub+zip".to_vec()));
    entries.push((
        "META-INF/container.xml",
        br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#
            .to_vec(),
    ));
    let names: Vec<String> = (1..=chapters)
        .map(|n| format!("OEBPS/ch{n}.xhtml"))
        .collect();
    let mut manifest = String::from(
        r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>"#,
    );
    let mut spine = String::new();
    let mut nav = String::new();
    for n in 1..=chapters {
        manifest.push_str(&format!(
            r#"<item id="ch{n}" href="ch{n}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="ch{n}"/>"#));
        nav.push_str(&format!(
            r#"<li><a href="ch{n}.xhtml">Chapter {n}</a></li>"#
        ));
    }
    let opf = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:test:{seed}</dc:identifier><dc:title>{title}</dc:title><dc:language>en</dc:language>
  </metadata>
  <manifest>{manifest}</manifest>
  <spine>{spine}</spine>
</package>"#
    );
    let nav_doc = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>{title}</title></head>
<body><nav epub:type="toc"><ol>{nav}</ol></nav></body></html>"#
    );
    entries.push(("OEBPS/content.opf", opf.into_bytes()));
    entries.push(("OEBPS/nav.xhtml", nav_doc.into_bytes()));
    let mut bodies = Vec::new();
    for n in 1..=chapters {
        let mut body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Chapter {n}</title></head><body><h1>Chapter {n}</h1>"#
        );
        for p in 0..40 {
            body.push_str(&format!(
                "<p>Paragraph {p} of chapter {n} in book {seed}, with enough ordinary words \
                 to wrap across several lines of the page and fill it the way a novel does, \
                 so that each chapter runs to a handful of pages at any reading size.</p>"
            ));
        }
        body.push_str("</body></html>");
        bodies.push(body.into_bytes());
    }
    for (name, body) in names.iter().zip(bodies) {
        entries.push((name.as_str(), body));
    }
    zip(&entries)
}

// ---------------------------------------------------------------------------
// The firmware's side of the storage task
// ---------------------------------------------------------------------------

/// What the display task's channels would carry, kept for the test to read.
#[derive(Default)]
pub struct TestHost {
    /// Every event the storage task sent, in order, since the last take.
    pub events: Vec<LibraryEvent>,
    pub latest_request: u32,
    pub requeued: VecDeque<StorageCommand>,
}

impl Host for TestHost {
    fn send(&mut self, event: &LibraryEvent) {
        self.events.push(*event);
    }

    fn send_required(&mut self, event: &LibraryEvent) {
        self.events.push(*event);
    }

    fn send_loaded(&mut self, event: &LibraryEvent) {
        self.events.push(*event);
    }

    fn latest_reader_request_id(&self) -> u32 {
        self.latest_request
    }

    fn requeue(&mut self, command: StorageCommand) {
        self.requeued.push_back(command);
    }

    fn ensure_scratch<'s>(
        &mut self,
        slot: &'s mut Option<&'static mut ReaderCacheScratch<'static>>,
    ) -> &'s mut ReaderCacheScratch<'static> {
        if slot.is_none() {
            let zip = Box::leak(Box::new(proto::epub::ZipInflateScratch::new()));
            zip.prepare(Box::leak(Box::new(proto::epub::DecompressorOxide::new())));
            *slot = Some(Box::leak(Box::new(ReaderCacheScratch::new(
                Box::leak(Box::new([0; reader_cache::READER_TAIL_SCRATCH])),
                Box::leak(Box::new([0; reader_cache::READER_HEADER_SCRATCH])),
                Box::leak(Box::new([0; proto::epub::MAX_ENTRY_NAME_BYTES])),
                Box::leak(Box::new([0; reader_cache::READER_COMPRESSED_SCRATCH])),
                Box::leak(Box::new([0; reader_cache::READER_CONTAINER_SCRATCH])),
                Box::leak(Box::new([0; reader_cache::READER_OPF_SCRATCH])),
                Box::leak(Box::new([0; reader_cache::READER_XHTML_SCRATCH])),
                Box::leak(Box::new(
                    [reader_cache::store::EMPTY_BOOK_SECTION_RECORD;
                        reader_cache::store::MAX_BOOK_SECTIONS],
                )),
                zip,
            ))));
        }
        slot.as_deref_mut().expect("just built")
    }

    fn network_saved(&mut self, _ssid: app_core::WifiSsid) {}

    fn wifi_storage_result(&mut self, _confirmed: bool) {}

    fn grant_sync_loan(
        &mut self,
        _card: &mut impl storage::card::Card,
        _scratch: &'static mut ReaderCacheScratch<'static>,
    ) {
        panic!("no test loans the scratch to a radio");
    }

    fn refuse_sync_loan(&mut self) {}
}

// ---------------------------------------------------------------------------
// The device
// ---------------------------------------------------------------------------

const CTX: ReducerContext = ReducerContext::new(1, 4);

/// One power-on of the device: everything in RAM, over a card that outlives it.
pub struct Device {
    pub card: SessionCard,
    pub store: Box<ReaderStore>,
    pub metrics: Box<MetricCache>,
    pub task: StorageTask,
    pub sync: SyncSession,
    pub app: ReaderState,
    pub host: TestHost,
    /// Every event this power-on, for assertions.
    pub log: Vec<LibraryEvent>,
    queue: VecDeque<StorageCommand>,
    next_request_id: u32,
    last_render: Option<RenderRequest>,
}

impl Device {
    /// Power on over `card`: the boot the firmware runs, through to the
    /// catalog loaded and the library listed.
    pub fn wake(card: &Card) -> Device {
        let mut device = Device {
            card: SessionCard {
                disk: card.disk.clone(),
            },
            store: Box::new(ReaderStore::new()),
            metrics: Box::new(MetricCache::new()),
            task: StorageTask::default(),
            sync: SyncSession::default(),
            app: ReaderState::boot(),
            host: TestHost::default(),
            log: Vec::new(),
            queue: VecDeque::new(),
            next_request_id: 1,
            last_render: None,
        };
        device.render();
        // The app asks for the catalog once the first frame settles.
        device.queue.push_back(StorageCommand::LoadCatalogCache);
        device.settle();
        device
    }

    /// What the firmware does before deep sleep: put a coalesced position on
    /// the card. Everything else is RAM, and goes.
    pub fn sleep(mut self) {
        self.settle();
        assert!(
            self.task
                .flush_pending_progress(&mut self.card, &mut self.store),
            "the position reached the card before sleep"
        );
    }

    /// Press a button, and let both tasks run until neither owes anything.
    pub fn press(&mut self, button: Button) {
        let previous = self.app;
        self.app = self.app.apply_input(CTX, InputEvent::button(button));
        let request_id = self.next_request_id;
        let command = storage_command_for_transition(&previous, &self.app, request_id);
        self.dispatch_transition(command, request_id, None);
        if app_core::progress_owed(&previous, &self.app, command.as_ref()) {
            self.queue
                .push_back(StorageCommand::StoreProgress(self.app.persisted()));
        }
        if let Some(command) = library_browse_command_for_transition(&previous, &self.app) {
            self.queue.push_back(command);
        }
        if let Some(command) = library_action_command_for_transition(&previous, &self.app) {
            self.queue.push_back(command);
        }
        self.render();
        self.settle();
    }

    /// The fold an open owes, stamped with the catalog a `RowIsBook` was
    /// resolved in, as `dispatch_transition_storage` stamps it.
    fn dispatch_transition(
        &mut self,
        command: Option<StorageCommand>,
        request_id: u32,
        catalog_fence: Option<u32>,
    ) {
        let Some(command) = command else {
            return;
        };
        let command = match command {
            StorageCommand::OpenBook {
                request_id,
                book_id,
                index,
                chapter,
                target_pages,
                type_settings,
                portrait,
                previous,
                catalog_epoch,
                resolve_place,
            } => StorageCommand::OpenBook {
                request_id,
                book_id,
                index,
                chapter,
                target_pages,
                type_settings,
                portrait,
                previous,
                catalog_epoch: catalog_fence.or(catalog_epoch),
                resolve_place,
            },
            other => other,
        };
        // The id is committed only when a reader command goes out.
        if command_carries_request(&command) {
            self.host.latest_request = request_id;
            self.next_request_id = request_id + 1;
        }
        self.queue.push_back(command);
    }

    fn render(&mut self) {
        self.last_render = Some(self.app.render_request(RenderKind::Page));
    }

    /// Run the storage task until it owes nothing: the queued commands, then
    /// background slices, each event folded into the app as it arrives.
    pub fn settle(&mut self) {
        for _ in 0..10_000 {
            if let Some(command) = self.queue.pop_front() {
                let portrait = app_core::is_portrait(self.app.orientation);
                self.task.handle(
                    command,
                    &mut self.card,
                    &mut self.host,
                    &mut self.store,
                    &mut self.metrics,
                    &mut self.sync,
                    portrait,
                );
                self.deliver();
                continue;
            }
            if let Some(command) = self.host.requeued.pop_front() {
                self.queue.push_back(command);
                continue;
            }
            if self.task.background_owed(&self.store) && !self.store.text_holds_toc() {
                self.task.background_step(
                    &mut self.card,
                    &mut self.host,
                    &mut self.store,
                    &mut self.metrics,
                    self.last_render,
                );
                self.deliver();
                continue;
            }
            return;
        }
        panic!("the storage task never went quiet");
    }

    /// Fold the storage task's events into the app, as the app task does,
    /// dispatching whatever a fold owes.
    fn deliver(&mut self) {
        let events: Vec<LibraryEvent> = self.host.events.drain(..).collect();
        for event in events {
            self.log.push(event);
            let before = self.app;
            self.app = self.app.apply_library_event(CTX, event);
            let request_id = self.next_request_id;
            let rolled_back = matches!(event, LibraryEvent::BookOpenFailed { .. });
            let command = if rolled_back {
                None
            } else {
                storage_command_for_transition(&before, &self.app, request_id)
            };
            let fence = match event {
                LibraryEvent::RowIsBook { catalog_epoch, .. } => Some(catalog_epoch),
                _ => None,
            };
            self.dispatch_transition(command, request_id, fence);
            if app_core::progress_owed(&before, &self.app, command.as_ref()) {
                self.queue
                    .push_back(StorageCommand::StoreProgress(self.app.persisted()));
            }
            self.render();
        }
    }

    // -- Navigation, by what is on the screen ------------------------------

    /// From Home, the library.
    pub fn open_library(&mut self) {
        if self.app.view != AppView::Library {
            self.press(Button::Back);
        }
        assert_eq!(self.app.view, AppView::Library, "the library is open");
    }

    /// Names of the rows the Library shows, folders and books alike.
    pub fn rows(&self) -> Vec<String> {
        self.store
            .folder_rows()
            .iter()
            .map(|row| row.name.as_str().to_owned())
            .collect()
    }

    /// Move the cursor to the row named `name` and press Confirm.
    pub fn choose(&mut self, name: &str) {
        let rows = self.rows();
        let index = rows
            .iter()
            .position(|row| row == name)
            .unwrap_or_else(|| panic!("no row {name:?} in {rows:?}"));
        let start = self.store.folder_start();
        while usize::from(self.app.selection) < start + index {
            self.press(Button::Next);
        }
        while usize::from(self.app.selection) > start + index {
            self.press(Button::Previous);
        }
        self.press(Button::Confirm);
    }

    /// Turn `pages` pages forward.
    pub fn turn(&mut self, pages: u32) {
        for _ in 0..pages {
            self.press(Button::PageNext);
        }
    }

    /// Events of a kind this power-on, for assertions.
    pub fn saw(&self, matches: impl Fn(&LibraryEvent) -> bool) -> bool {
        self.log.iter().any(matches)
    }
}

fn command_carries_request(command: &StorageCommand) -> bool {
    matches!(
        command,
        StorageCommand::OpenBook { .. }
            | StorageCommand::ExtendSection { .. }
            | StorageCommand::LoadChapters { .. }
            | StorageCommand::JumpChapter { .. }
    )
}
