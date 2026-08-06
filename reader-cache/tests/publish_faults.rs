//! Fault-injection tests for the publish tail, against a real embedded-sdmmc
//! FAT16 filesystem on an in-memory card that can fail the Nth read or write.
//!
//! These exist because of what B4 cost. Six review rounds, six defects, every
//! one of them in the code below, and every one the same shape: a write fails
//! *after* the store has already been updated, and the cleanup gets it wrong.
//! Deleting the cache the reader is reading out of. Returning before putting
//! their page back in the shared text arena. Reporting a walk finished when its
//! index never landed. None of it was subtle, and none of it was caught,
//! because the code lived in a `#![no_main]` firmware binary that no test could
//! reach.
//!
//! So each test below arms a fault at a specific write or read and asserts what
//! must survive it. The card model — `FaultyDisk`, `FaultPlan`, the MBR + FAT16
//! image — is copied from `upload-store/tests/transaction.rs` rather than shared:
//! integration tests cannot import each other, and exporting a harness from
//! `upload-store`'s public API to avoid ~120 duplicated lines would put test
//! scaffolding in a shipped crate. If a third crate ever needs it, that is the
//! point to extract it into a dev-only crate of its own.
//!
//! The last section covers the per-config cache layout (B7) rather than a
//! fault: which files a layout config owns, that a second config does not
//! overwrite the first, and which one eviction takes. It lives here for the
//! card harness above — a second integration test would have to copy all of
//! it, which is the cost this file's header already argues against.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use display::font::{FontSize, FontStyle, TypeSettings};
use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Directory, TimeSource, Timestamp, VolumeIdx,
    VolumeManager,
};
use proto::cache::{
    book_index_file_name, layout_cache_key, section_file_name, BookV2SectionRecord,
    CoverCacheHeader, BOOK_INDEX_FILE_BYTES, CACHE_BOOK_FILE, CACHE_CONFIG_FILE, CACHE_COVER_FILE,
    CACHE_SECTIONS_DIR, CACHE_SECTION_FILE_BYTES, COVER_BYTES, COVER_HEIGHT, COVER_STRIDE,
    COVER_WIDTH,
};
use proto::text::{TextAlign, TextRole};
use reader_cache::files::{self, CacheLoadResult};
use reader_cache::layout;
use reader_cache::publish::{self, BookPublishOutcome, PublishError};
use reader_cache::store::{BookLoadStatus, ReaderStore};

const BLOCK_BYTES: usize = 512;
/// 16 MiB card: big enough that fatfs picks FAT16 and small enough to stay fast.
const DISK_BLOCKS: u32 = 32 * 1024;
const PART_START_BLOCK: u32 = 64;

/// The book under test. The key is what names its cache directory; the identity
/// pair is the (source_hash, byte_size) every cache file is stamped with, and
/// the thing a mismatched file is rejected on.
const KEY: &str = "TESTBOOK";
const IDENTITY: (u32, u32) = (0xABCD_1234, 4096);

// ---------------------------------------------------------------------------
// Fault-injecting in-memory block device
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskError;

/// Arms exactly-once faults: `fail_write_in = Some(n)` fails the (n+1)th
/// subsequent write, then disarms. Exactly-once is the point — the cleanup
/// paths under test issue their *own* I/O after the fault, and a sticky fault
/// would conflate "this write failed" with "the card is gone", which is a
/// different scenario with different correct behaviour.
#[derive(Default)]
struct FaultPlan {
    fail_write_in: Cell<Option<u32>>,
    fail_read_in: Cell<Option<u32>>,
}

impl FaultPlan {
    fn take_fault(counter: &Cell<Option<u32>>) -> bool {
        match counter.get() {
            Some(0) => {
                counter.set(None);
                true
            }
            Some(n) => {
                counter.set(Some(n - 1));
                false
            }
            None => false,
        }
    }
}

struct FaultyDisk {
    data: RefCell<Vec<u8>>,
    fault: FaultPlan,
    writes: Cell<u32>,
    reads: Cell<u32>,
}

/// The test holds one `Rc` handle for arming faults while the `VolumeManager`
/// owns another. The newtype exists because the orphan rule forbids
/// implementing the foreign `BlockDevice` for `Rc<_>`.
#[derive(Clone)]
struct SharedDisk(Rc<FaultyDisk>);

impl std::ops::Deref for SharedDisk {
    type Target = FaultyDisk;
    fn deref(&self) -> &FaultyDisk {
        &self.0
    }
}

impl BlockDevice for SharedDisk {
    type Error = DiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), DiskError> {
        self.reads.set(self.reads.get() + 1);
        if FaultPlan::take_fault(&self.fault.fail_read_in) {
            return Err(DiskError);
        }
        let data = self.data.borrow();
        for (i, block) in blocks.iter_mut().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            block.copy_from_slice(&data[at..at + BLOCK_BYTES]);
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), DiskError> {
        self.writes.set(self.writes.get() + 1);
        if FaultPlan::take_fault(&self.fault.fail_write_in) {
            return Err(DiskError);
        }
        let mut data = self.data.borrow_mut();
        for (i, block) in blocks.iter().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            data[at..at + BLOCK_BYTES].copy_from_slice(&block[..]);
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, DiskError> {
        Ok(BlockCount(DISK_BLOCKS))
    }
}

// ---------------------------------------------------------------------------
// FAT16 image: MBR partition table + fatfs-formatted partition
// ---------------------------------------------------------------------------

fn format_disk() -> Vec<u8> {
    let mut disk = vec![0u8; DISK_BLOCKS as usize * BLOCK_BYTES];
    let part_blocks = DISK_BLOCKS - PART_START_BLOCK;

    let mut partition = vec![0u8; part_blocks as usize * BLOCK_BYTES];
    fatfs::format_volume(
        std::io::Cursor::new(partition.as_mut_slice()),
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat16),
    )
    .expect("format FAT16 partition");
    disk[PART_START_BLOCK as usize * BLOCK_BYTES..].copy_from_slice(&partition);

    let entry = 446;
    disk[entry] = 0x00;
    disk[entry + 4] = 0x06;
    disk[entry + 8..entry + 12].copy_from_slice(&PART_START_BLOCK.to_le_bytes());
    disk[entry + 12..entry + 16].copy_from_slice(&part_blocks.to_le_bytes());
    disk[510] = 0x55;
    disk[511] = 0xAA;
    disk
}

struct StaticTime;

impl TimeSource for StaticTime {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 56,
            zero_indexed_month: 4,
            zero_indexed_day: 19,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

type Mgr = VolumeManager<SharedDisk, StaticTime, 8, 8, 1>;
type Dir<'a> = Directory<'a, SharedDisk, StaticTime, 8, 8, 1>;

fn new_card() -> SharedDisk {
    SharedDisk(Rc::new(FaultyDisk {
        data: RefCell::new(format_disk()),
        fault: FaultPlan::default(),
        writes: Cell::new(0),
        reads: Cell::new(0),
    }))
}

fn open_mgr(disk: &SharedDisk) -> Mgr {
    VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000)
}

fn open_root(mgr: &Mgr) -> Dir<'_> {
    let volume = mgr.open_volume(VolumeIdx(0)).expect("open volume");
    let raw_root = mgr
        .open_root_dir(volume.to_raw_volume())
        .expect("open root");
    Directory::new(raw_root, mgr)
}

// ---------------------------------------------------------------------------
// A book on the card, built the way the firmware builds one
// ---------------------------------------------------------------------------

/// `ReaderStore` is ~47 KB, so it is boxed here for the same reason the
/// firmware keeps it in a static: nobody should be moving it through a stack
/// frame. (See the crate root on why it has no `Default`.)
fn new_store() -> Box<ReaderStore> {
    let mut store = Box::new(ReaderStore::new());
    store.set_cache_key(KEY);
    // A one-book catalog. `set_current_index` silently declines an index past
    // the catalog total, and `selected_cover` compares against `current_index`,
    // so without this the cover assertions below would pass or fail for reasons
    // that have nothing to do with the cover.
    store.set_catalog_total(1);
    store
}

/// Fill the store's line buffer with one section's worth of body text and
/// paginate it, leaving the store exactly as a finished spine item leaves it.
fn fill_section(store: &mut ReaderStore, spine: u16, lines: usize) {
    store.clear_lines();
    for n in 0..lines {
        let line = format!("section {spine} line {n} with enough words to occupy a row");
        assert!(
            store.push_line_block(
                &line,
                FontStyle::Regular,
                TextRole::Body,
                TextAlign::Left,
                true,
                spine,
            ),
            "line buffer should hold the fixture text"
        );
    }
    layout::rebuild_page_index(store);
    assert!(
        store.page_count() > 0,
        "fixture must paginate to real pages"
    );
}

/// Write one section file, returning the index record describing it.
fn write_section(
    root: &Dir<'_>,
    store: &mut ReaderStore,
    section: u16,
    start_page: u32,
) -> BookV2SectionRecord {
    fill_section(store, section, 6);
    // The section file records `cached_spine`, and the load rejects a file whose
    // spine disagrees with the index record pointing at it. Without this every
    // section is written as spine 0, which happens to match for section 0 and
    // silently fails for every other — a fixture flaw a mutation check caught
    // only once a test reached past the first section.
    store.set_cached_spine(section);
    let page_count = store.page_count().min(u16::MAX as usize) as u16;
    let wrote = files::with_v2_sections_dir(root, KEY, |sections| {
        let sections = sections.expect("sections dir should exist");
        files::write_v2_section_cache_in(sections, IDENTITY, section, store)
    });
    assert!(wrote, "section {section} should write");
    BookV2SectionRecord {
        section,
        spine: section,
        start_page,
        page_count,
        partial: false,
    }
}

/// A book whose cache directory and section files exist on the card, with the
/// store holding the last section written. Returns the section records.
fn build_book(
    root: &Dir<'_>,
    store: &mut ReaderStore,
    sections: usize,
) -> Vec<BookV2SectionRecord> {
    files::ensure_v2_cache_dirs(root, KEY).expect("cache dirs");
    let mut records = Vec::new();
    let mut start_page = 0;
    for n in 0..sections {
        let record = write_section(root, store, n as u16, start_page);
        start_page += u32::from(record.page_count);
        records.push(record);
    }
    records
}

fn total_pages(records: &[BookV2SectionRecord]) -> u32 {
    records.iter().map(|r| u32::from(r.page_count)).sum::<u32>()
}

/// Whether the book's cache directory still holds its section files. This is
/// the question the round-2 finding turned on: a failing publish must not take
/// the files out from under a reader who is reading them.
fn sections_still_on_card(root: &Dir<'_>, store: &ReaderStore, count: usize) -> bool {
    // Named for the store's layout config, the way the writer names them: a
    // section file belongs to one paginated copy of the book, so a helper
    // spelling the name itself would pass while the config key drifted.
    let layout_key = layout_cache_key_for(store);
    (0..count).all(|n| section_file_present(root, layout_key, n as u16))
}

/// The per-config cache key for a store's current type settings and page box.
fn layout_cache_key_for(store: &ReaderStore) -> u8 {
    layout_cache_key(layout::reader_layout_config(
        store.type_settings(),
        store.portrait(),
    ))
}

/// Whether one named file exists in the book's `SECTIONS/` directory. Takes a
/// raw name, for the cases that are deliberately not this config's: a stray
/// file, or another config's section.
fn file_in_sections_dir(root: &Dir<'_>, name: &str) -> bool {
    files::with_v2_sections_dir(root, KEY, |sections| {
        let Some(sections) = sections else {
            return false;
        };
        sections
            .open_file_in_dir(name, embedded_sdmmc::Mode::ReadOnly)
            .is_ok()
    })
}

/// Put a file that is not one of ours into `SECTIONS/`, to prove the prune
/// deletes by parsed ordinal rather than by "everything that is here".
fn write_stray_file(root: &Dir<'_>, name: &str) {
    files::with_v2_sections_dir(root, KEY, |sections| {
        let sections = sections.expect("sections dir should exist");
        let file = sections
            .open_file_in_dir(name, embedded_sdmmc::Mode::ReadWriteCreateOrTruncate)
            .expect("stray file should create");
        file.write(b"not a section").expect("stray write");
    });
}

/// Lay down a valid COVER.BIN for the book, the way a completed build does.
/// There is no public writer for it, so this encodes the header and body
/// directly — which also means the test is pinning the format the loader reads.
fn write_cover(root: &Dir<'_>) {
    let dir = files::open_v2_book_dir(root, KEY).expect("book cache dir");
    let file = dir
        .open_file_in_dir(
            CACHE_COVER_FILE,
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("create COVER.BIN");
    let mut header = [0u8; proto::cache::COVER_HEADER_BYTES];
    proto::cache::encode_cover_header(
        CoverCacheHeader {
            width: COVER_WIDTH as u16,
            height: COVER_HEIGHT as u16,
            stride: COVER_STRIDE as u16,
        },
        &mut header,
    )
    .expect("encode cover header");
    file.write(&header).expect("write cover header");
    file.write(&[0xA5u8; COVER_BYTES])
        .expect("write cover bits");
    file.flush().expect("flush cover");
}

// ---------------------------------------------------------------------------
// The regressions
// ---------------------------------------------------------------------------

/// Invariant: a background step whose final index write fails must not delete
/// the cache, and must put the reader's page back in the shared text arena.
///
/// This is review round 2, plus the promise `finish_background_walk` documents:
/// the builder drives the sink through the same arena the reading view renders
/// from, so between its last write and this call the arena holds the *builder's*
/// section, not the reader's page. Every exit has to restore it.
///
/// The fixture therefore models a reader who is already open on a page in one
/// section while the builder has left a later one resident — without that, the
/// restore is unguarded, and deleting the `restore_reader_page` call passes.
#[test]
fn a_failed_final_index_write_restores_the_reader_and_keeps_their_sections() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let records = build_book(&root, &mut store, 3);
    let pages = total_pages(&records);
    // A page in the middle section, so "restored" cannot be confused with
    // "never moved" or "always page zero".
    let reader_page = records[1].start_page;
    let reader_section_start = records[1].start_page;
    let builder_page = records[2].start_page;
    assert_ne!(reader_page, 0, "the reader must not sit on page zero");
    assert_ne!(reader_page, builder_page);

    // An open reader on their page.
    store.begin_book_load();
    store.set_book_index(pages, false, &records);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);
    assert!(matches!(
        files::load_v2_section_by_global_page(&root, KEY, IDENTITY, reader_page, &mut store),
        CacheLoadResult::Hit { .. }
    ));
    assert!(store.covers_global_page(0, reader_page));
    assert_eq!(store.current_section_start_page, reader_section_start);

    // The builder borrows the arena for a later section, which is the state a
    // step is in when its final publish runs.
    assert!(matches!(
        files::load_v2_section_by_global_page(&root, KEY, IDENTITY, builder_page, &mut store),
        CacheLoadResult::Hit { .. }
    ));
    assert_eq!(
        store.current_section_start_page, builder_page,
        "the arena should now hold the builder's section, not the reader's"
    );

    // Fail the very next write, which is BOOK.BIN's.
    disk.fault.fail_write_in.set(Some(0));
    let outcome = publish::publish_book_cache(
        &root,
        KEY,
        IDENTITY,
        reader_page,
        &mut store,
        &records,
        pages,
        false,
        0,
    );
    assert_eq!(outcome.outcome, BookPublishOutcome::IndexWriteFailed);
    assert_eq!(outcome.cover, None, "a failed publish reaches no cover");

    let tail = publish::finish_background_walk(
        &root,
        KEY,
        IDENTITY,
        reader_page,
        outcome.outcome,
        &mut store,
    );
    assert_eq!(
        tail,
        Err(PublishError::IndexWrite),
        "the walk must report the failure rather than claim it finished"
    );
    assert!(
        sections_still_on_card(&root, &store, 3),
        "the reader's section files must survive a failed index write"
    );
    assert_eq!(
        store.current_section_start_page, reader_section_start,
        "the reader's page must be back in the arena, not the builder's section"
    );
    assert!(
        store.covers_global_page(0, reader_page),
        "the restored window must cover the page the reader is on"
    );
}

/// Invariant: the same failure on the *open* path does clear the cache. Pinning
/// both halves together is the point — the asymmetry is deliberate, and a future
/// refactor that unifies the two tails would silently break one of them.
#[test]
fn a_failed_first_open_index_write_clears_the_cache_nobody_is_holding() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let records = build_book(&root, &mut store, 3);
    let pages = total_pages(&records);

    disk.fault.fail_write_in.set(Some(0));
    let result = publish::publish_first_open(
        &root, KEY, IDENTITY, 0, &mut store, &records, pages,
        // A provisional publish carries a non-zero cursor.
        1,
    );
    assert_eq!(result, Err(PublishError::IndexWrite));
    assert!(
        !sections_still_on_card(&root, &store, 3),
        "an open that never became readable should leave no debris"
    );
}

/// Invariant: a step that cannot put the reader's page back must not report
/// success, and must leave the window unusable rather than half-loaded.
///
/// This is review round 1's third finding and round 4's subject: the difference
/// between a walk that *ended* and one that *broke*, which the caller decides
/// entirely on whether the store came through coherent.
///
/// Proves both directions in one test on purpose. The first half establishes
/// that this fixture really can produce a covered window — without it the
/// negative assertion below passes for the wrong reason, which is exactly what
/// a mutation check caught here on the first attempt.
#[test]
fn a_section_that_will_not_read_back_marks_the_window_unusable() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let records = build_book(&root, &mut store, 2);
    let pages = total_pages(&records);
    assert!(files::write_v2_book_index(
        &root, KEY, IDENTITY, pages, &records, &store, false, 0,
    ));

    // Put the store in the state a completed open leaves it. The order is the
    // firmware's and it matters: `begin_book_load` clears the resident index, so
    // adopting it has to happen after the open begins and before it finishes --
    // which is exactly where `publish_book_cache` does it.
    store.begin_book_load();
    store.set_book_index(pages, false, &records);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    // Baseline: a clean load covers the page.
    assert!(
        matches!(
            files::load_v2_section_by_global_page(&root, KEY, IDENTITY, 0, &mut store),
            CacheLoadResult::Hit { .. }
        ),
        "fixture must be able to load a section, or the negative case below is vacuous"
    );
    assert!(
        store.covers_global_page(0, 0),
        "a clean load must leave the page renderable from RAM"
    );

    // Now drive the publish tail itself with the read armed to fail, so the
    // orchestration is what gets pinned rather than the loader beneath it.
    // `extend_background_index` adopts the grown index, then restores the
    // reader's page; that restore is what must fail and be reported.
    disk.fault.fail_read_in.set(Some(0));
    let extended = publish::extend_background_index(
        &root, KEY, IDENTITY, 1, 2, 0, &mut store, &records, pages,
    );
    assert_eq!(
        extended,
        Err(PublishError::SectionRead),
        "a step that cannot put the reader's page back must report it, not claim success"
    );
    assert!(
        !store.covers_global_page(0, 1),
        "a window that could not be loaded must not be advertised as covering the page"
    );
}

/// Invariant: an index a build walked away from is refused when no walk is
/// live, and accepted while one is.
///
/// The trap this closes: reading a partial index with nobody building caps the
/// book at whatever that build reached, and the reducer clamps the reader to the
/// advertised count, so no input can provoke the rebuild that would fix it. The
/// policy is `app_core::storage_loop::partial_index_is_usable`; what this test
/// pins is that `resume_spine` survives a write/read round trip, because the
/// policy is worthless if the flag does not.
#[test]
fn an_index_a_build_abandoned_is_recognisable_as_one() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let records = build_book(&root, &mut store, 2);
    let pages = total_pages(&records);

    // A provisional publish stamps the cursor it will resume from.
    assert!(files::write_v2_book_index(
        &root, KEY, IDENTITY, pages, &records, &store, true, 7,
    ));
    let loaded = files::load_v2_book_index(&root, KEY, IDENTITY, &mut store);
    assert!(
        matches!(loaded, files::BookIndexLoadResult::Hit { unfinished: true }),
        "an index stamped with a resume cursor must read back as unfinished, got {loaded:?}"
    );
    assert!(
        !app_core::storage_loop::partial_index_is_usable(true, false),
        "unfinished with no walk live must be refused"
    );
    assert!(
        app_core::storage_loop::partial_index_is_usable(true, true),
        "unfinished with its own walk live must be accepted"
    );

    // A completed publish stamps zero, which is what says the missing pages --
    // if any -- are missing for good.
    assert!(files::write_v2_book_index(
        &root, KEY, IDENTITY, pages, &records, &store, false, 0,
    ));
    assert!(
        matches!(
            files::load_v2_book_index(&root, KEY, IDENTITY, &mut store),
            files::BookIndexLoadResult::Hit { unfinished: false }
        ),
        "a finished index must not read back as unfinished"
    );
}

/// Invariant: a clean publish leaves the reader on the page it was asked for.
/// The baseline the fault cases are measured against — if this fails, the
/// assertions above are proving nothing.
///
/// Requests a non-zero page in a later section on purpose, so it catches both
/// "nothing was loaded" and "page zero is always loaded" regressions.
#[test]
fn a_clean_publish_leaves_the_requested_page_resident() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let records = build_book(&root, &mut store, 3);
    let pages = total_pages(&records);
    let requested = records[2].start_page;
    assert_ne!(requested, 0, "the fixture must exercise a non-zero page");

    store.begin_book_load();
    let outcome = publish::publish_book_cache(
        &root, KEY, IDENTITY, requested, &mut store, &records, pages, false, 0,
    );
    assert_eq!(outcome.outcome, BookPublishOutcome::Ready);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    assert_eq!(store.advertised_page_count(), pages);
    assert_eq!(
        store.current_section_start_page, requested,
        "the publish must leave the requested page's section resident"
    );
    assert!(
        store.covers_global_page(0, requested),
        "the requested page must be renderable from RAM after a clean publish"
    );
    assert!(
        sections_still_on_card(&root, &store, 3),
        "a clean publish must not delete anything"
    );
}

/// Invariant: a rebuild that produces fewer sections than the one before it
/// takes the stranded tail with it.
///
/// Section files are keyed by a dense section ordinal, so a rebuild producing
/// fewer of them rewrites `S000..S(new-1)` and leaves `S(new)..S(old-1)` on the
/// card referenced by nothing — `load_v2_section_by_global_page` indexes off
/// BOOK.BIN. Type-size and orientation changes do not cause this (sections are
/// content-bounded; see `prune_orphan_sections_in`), so the shrink is
/// constructed here rather than driven through a setting.
#[test]
fn a_rebuild_with_fewer_sections_prunes_the_stranded_tail() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let wide = build_book(&root, &mut store, 5);
    store.begin_book_load();
    let outcome = publish::publish_book_cache(
        &root,
        KEY,
        IDENTITY,
        0,
        &mut store,
        &wide,
        total_pages(&wide),
        false,
        0,
    );
    assert_eq!(outcome.outcome, BookPublishOutcome::Ready);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);
    assert!(sections_still_on_card(&root, &store, 5));

    // A completed rebuild over the same content with a smaller final section
    // set, as a change to the capacity constants would produce: it rewrites
    // S<cfg>000..S<cfg>002 and leaves S<cfg>003 and S<cfg>004 behind.
    let narrow = &wide[..3];
    store.begin_book_load();
    let outcome = publish::publish_book_cache(
        &root,
        KEY,
        IDENTITY,
        0,
        &mut store,
        narrow,
        total_pages(narrow),
        false,
        0,
    );
    assert_eq!(outcome.outcome, BookPublishOutcome::Ready);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    assert!(
        sections_still_on_card(&root, &store, 3),
        "the sections the new index names must survive"
    );
    let layout_key = layout_cache_key_for(&store);
    assert!(
        !section_file_present(&root, layout_key, 3) && !section_file_present(&root, layout_key, 4),
        "the sections the new index no longer names must be gone"
    );
}

/// Invariant: a suspended walk is never pruned against.
///
/// This is the one that makes the prune safe to have at all. A progressive
/// first open publishes a provisional index spanning only the sections written
/// so far, then keeps walking. Pruning against that frontier would delete the
/// sections the walk is about to need — turning a cheap continuation into a
/// full rebuild, and doing it to a reader who is mid-book. `resume_spine` is
/// the gate: non-zero means someone is coming back.
#[test]
fn a_suspended_walk_keeps_the_sections_past_its_frontier() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let all = build_book(&root, &mut store, 5);
    let frontier = &all[..2];

    store.begin_book_load();
    let outcome = publish::publish_book_cache(
        &root,
        KEY,
        IDENTITY,
        0,
        &mut store,
        frontier,
        total_pages(frontier),
        true,
        // Non-zero: this walk is suspended at spine 2 and will resume.
        2,
    );
    assert_eq!(outcome.outcome, BookPublishOutcome::Ready);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    assert!(
        sections_still_on_card(&root, &store, 5),
        "a provisional publish must leave every section the walk will resume into"
    );
}

/// Invariant: the prune deletes by parsed section ordinal, not by "whatever is
/// in the directory". `SECTIONS/` lives on removable media, so anything whose
/// name is not one of ours is somebody else's problem and must be left alone.
#[test]
fn the_prune_leaves_names_it_does_not_recognise() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let wide = build_book(&root, &mut store, 4);
    write_stray_file(&root, "NOTES.TXT");
    // Two digits, so not a name this code ever writes.
    write_stray_file(&root, "S12.BIN");
    // A name the *previous* scheme wrote: eight characters, no config key. Its
    // ordinal is in the range being pruned, so a prune reading ordinals without
    // the key would take it. Retiring these is the legacy purge's job, which is
    // gated on BOOK.BIN and knows to look; the prune must leave them.
    write_stray_file(&root, "S003.BIN");

    let narrow = &wide[..1];
    store.begin_book_load();
    let outcome = publish::publish_book_cache(
        &root,
        KEY,
        IDENTITY,
        0,
        &mut store,
        narrow,
        total_pages(narrow),
        false,
        0,
    );
    assert_eq!(outcome.outcome, BookPublishOutcome::Ready);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    assert!(
        !section_file_present(&root, layout_cache_key_for(&store), 1),
        "the orphaned section should still go"
    );
    assert!(
        file_in_sections_dir(&root, "NOTES.TXT") && file_in_sections_dir(&root, "S12.BIN"),
        "names this code does not write must be left alone"
    );
    assert!(
        file_in_sections_dir(&root, "S003.BIN"),
        "a legacy unkeyed name is not this prune's to take"
    );
}

/// Invariant: one refused delete does not strand the orphans behind it.
///
/// `remove_file_reclaiming_clusters` opens, truncates, closes and deletes, so a
/// fault in any of those fails one particular file. An earlier version read
/// that as "the card is refusing deletes" and returned, which left every later
/// orphan on the card — and because the prune only runs after a *completed*
/// rebuild, the next attempt needs another full rebuild, cache clear, or the
/// book leaving the card. For the capacity-constant change this feature exists
/// for, that may never come.
///
/// Exactly-once is the card model on purpose (see `FaultPlan`): a sticky fault
/// is a different scenario. This arms one refused write, which lands on the
/// first orphan's truncate, and asserts the rest still go.
#[test]
fn a_refused_delete_does_not_strand_the_orphans_behind_it() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    // Five sections, of which S002..S004 are about to become orphans. All
    // three land in one listing batch (SECTION_SWEEP_BATCH is 16), so this
    // exercises "kept going after the failure", not "picked it up next pass".
    build_book(&root, &mut store, 5);
    assert!(sections_still_on_card(&root, &store, 5));
    let layout_key = layout_cache_key_for(&store);

    disk.fault.fail_write_in.set(Some(0));
    let removed = files::prune_orphan_sections(&root, KEY, &store, 2);

    assert!(
        section_file_present(&root, layout_key, 0) && section_file_present(&root, layout_key, 1),
        "the sections the index still names must survive"
    );
    assert_eq!(
        removed, 3,
        "every orphan must come off the card, including the one behind the refused write"
    );
    assert!(
        !section_file_present(&root, layout_key, 2)
            && !section_file_present(&root, layout_key, 3)
            && !section_file_present(&root, layout_key, 4),
        "a refused delete must not strand the orphans after it"
    );
}

/// Invariant: the prune stays inside the config that was just published.
///
/// This is the invariant the per-config layout creates and the prune could
/// silently break. Two resident configs both number their sections from zero, so
/// their ordinal ranges overlap completely: a prune that went by ordinal alone
/// would delete the other config's tail past this one's count — a working
/// paginated copy of the book, and the whole reason for keeping two.
#[test]
fn the_prune_leaves_the_other_config_alone() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    // The config that keeps five sections, and stays untouched throughout.
    let keeper_key = build_under_current_config(&root, &mut store, 5);

    // A second config publishes a shorter book, which prunes its own tail.
    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let shrinking_key = layout_cache_key_for(&store);
    assert_ne!(
        shrinking_key, keeper_key,
        "the flip must land on a different config for this to test anything"
    );
    files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    let wide = build_book(&root, &mut store, 5);
    let narrow = &wide[..2];
    store.begin_book_load();
    let outcome = publish::publish_book_cache(
        &root,
        KEY,
        IDENTITY,
        0,
        &mut store,
        narrow,
        total_pages(narrow),
        false,
        0,
    );
    assert_eq!(outcome.outcome, BookPublishOutcome::Ready);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    for section in 2..5 {
        assert!(
            !section_file_present(&root, shrinking_key, section),
            "section {section} of the shrinking config is stranded and must go"
        );
        assert!(
            section_file_present(&root, keeper_key, section),
            "section {section} belongs to the other config and must survive"
        );
    }
    for section in 0..2 {
        assert!(
            section_file_present(&root, shrinking_key, section)
                && section_file_present(&root, keeper_key, section),
            "section {section} is named by both configs' indexes"
        );
    }
}

/// Invariant: a progressive first open adopts the cover, like every other
/// successful publish.
///
/// The regression this pins: the crate extraction moved the `COVER.BIN` load out
/// of `publish_book_cache` and into the firmware's reporting helper. That helper
/// only runs on the *final* publish, so a progressively opened book had no cover
/// until its background walk finished — and `selected_cover` gates the Library
/// and sleep renders on it, so backing out of the book or sleeping mid-build
/// rendered without a cover that was already sitting on the card. With the
/// indefinite step retry, a long card outage could hold that state for minutes.
///
/// Adopting the cover is publication, not telemetry, so it belongs on the path
/// every publish takes.
#[test]
fn a_progressive_first_open_adopts_the_cover() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let records = build_book(&root, &mut store, 3);
    write_cover(&root);

    // Only the first section is published, as a suspended walk would leave it.
    let first = &records[..1];
    let pages = total_pages(first);

    store.begin_book_load();
    let result = publish::publish_first_open(
        &root, KEY, IDENTITY, 0, &mut store, first, pages,
        // Non-zero: the walk is coming back for the rest.
        1,
    );
    assert_eq!(result, Ok(()), "the provisional publish should succeed");
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    assert!(
        store.covers_global_page(0, 0),
        "the requested page must be resident after a provisional publish"
    );
    assert!(
        store
            .selected_cover(app_core::ReaderSource::sd(0).book_id())
            .is_some(),
        "the cover must be available before the background walk finishes"
    );
}

/// Invariant: once a background walk has grown past the batching threshold, the
/// step that rewrites BOOK.BIN reports the new published frontier — and if that
/// write fails, it still must not delete the cache the reader is reading from.
///
/// The other `extend_background_index` test stays under
/// `INDEX_PUBLISH_SECTIONS`, so it only ever exercises the restore read and the
/// index write never happens. This one crosses the threshold, which is the only
/// way to reach the `published_now` advance and the refused-write branch beside
/// it.
#[test]
fn a_step_past_the_batching_threshold_publishes_and_survives_a_refused_write() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    // Enough sections that `grown` clears INDEX_PUBLISH_SECTIONS in one step.
    let sections = 17;
    let records = build_book(&root, &mut store, sections);
    let pages = total_pages(&records);
    let reader_page = records[1].start_page;

    store.begin_book_load();
    store.set_book_index(pages, false, &records);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    // A clean step: the index lands, so the frontier advances to every section
    // the walk has built.
    let published = publish::extend_background_index(
        &root,
        KEY,
        IDENTITY,
        reader_page,
        // The spine cursor the resume would carry.
        sections as u16,
        0,
        &mut store,
        &records,
        pages,
    );
    assert_eq!(
        published,
        Ok(sections as u16),
        "crossing the threshold must advance the published frontier to every built section"
    );

    // The same step with the index write refused. The reader is inside these
    // files, so a truncated BOOK.BIN is the correct cost -- never a deleted
    // cache.
    disk.fault.fail_write_in.set(Some(0));
    let refused = publish::extend_background_index(
        &root,
        KEY,
        IDENTITY,
        reader_page,
        sections as u16,
        0,
        &mut store,
        &records,
        pages,
    );
    assert_eq!(
        refused,
        Err(PublishError::IndexWrite),
        "a refused index write must be reported, not swallowed"
    );
    assert!(
        sections_still_on_card(&root, &store, sections),
        "a refused index write must not take the reader's sections with it"
    );
    assert!(
        store.covers_global_page(0, reader_page),
        "the reader's page must still be resident after the refused write"
    );
}

// ---------------------------------------------------------------------------
// Per-config caches (B7)
//
// A wrap-relevant settings change — type size, weight, family, or the
// portrait/landscape page box — used to overwrite the book's one cached
// pagination, so flipping back re-paid the whole rebuild (24-27 s on the
// measured 11.7 MB book). These pin that each config owns its own files, that
// the registry orders them by use, and that eviction takes the least recently
// used one and nothing else.
// ---------------------------------------------------------------------------

/// The store's layout config, and a second one that differs only in the page
/// box — the orientation flip, which is the flow that made B7 worth doing.
fn landscape_of(store: &ReaderStore) -> (TypeSettings, bool) {
    (store.type_settings(), !store.portrait())
}

/// A third config: a type-size change, the other flow that re-paid a rebuild.
fn larger_of(store: &ReaderStore) -> (TypeSettings, bool) {
    let mut settings = store.type_settings();
    settings.size = match settings.size {
        FontSize::Large => FontSize::Small,
        _ => FontSize::Large,
    };
    (settings, store.portrait())
}

fn section_file_present(root: &Dir<'_>, layout_key: u8, section: u16) -> bool {
    files::with_v2_sections_dir(root, KEY, |sections| {
        let Some(sections) = sections else {
            return false;
        };
        let mut name = heapless::String::<CACHE_SECTION_FILE_BYTES>::new();
        section_file_name(layout_key, section, &mut name);
        sections
            .open_file_in_dir(name.as_str(), embedded_sdmmc::Mode::ReadOnly)
            .is_ok()
    })
}

fn book_index_present(root: &Dir<'_>, layout_key: u8) -> bool {
    let dir = files::open_v2_book_dir(root, KEY).expect("book cache dir");
    let mut name = heapless::String::<BOOK_INDEX_FILE_BYTES>::new();
    book_index_file_name(layout_key, &mut name);
    // Bound rather than returned directly: the `Result` holds a `File` borrowing
    // `dir`, and as a tail expression that temporary would outlive `dir`.
    let present = dir
        .open_file_in_dir(name.as_str(), embedded_sdmmc::Mode::ReadOnly)
        .is_ok();
    present
}

/// Build and publish the whole book under the store's current layout config,
/// the way an open that misses does: adopt the config, write its sections,
/// write its index. Returns the config's cache key.
fn build_under_current_config(root: &Dir<'_>, store: &mut ReaderStore, sections: usize) -> u8 {
    let layout_key = layout_cache_key_for(store);
    files::adopt_layout_config(root, KEY, IDENTITY, store);
    let records = build_book(root, store, sections);
    let pages = total_pages(&records);
    assert!(
        files::write_v2_book_index(root, KEY, IDENTITY, pages, &records, store, false, 0),
        "the index for config {layout_key:#04x} should write"
    );
    store.begin_book_load();
    store.set_book_index(pages, false, &records);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);
    layout_key
}

/// Invariant: two layout configs keep separate caches, so returning to one
/// already built is a hit rather than a rebuild. This is B7's whole point.
#[test]
fn a_second_layout_config_does_not_overwrite_the_first() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let portrait_key = build_under_current_config(&root, &mut store, 3);

    // Flip the page box, the way an orientation toggle does, and build again.
    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let landscape_key = build_under_current_config(&root, &mut store, 3);
    assert_ne!(
        portrait_key, landscape_key,
        "an orientation flip must land on a different config key, or the caches still collide"
    );

    // Both configs' files are on the card at once -- the state the
    // single-file scheme could never hold.
    for key in [portrait_key, landscape_key] {
        assert!(
            book_index_present(&root, key),
            "index {key:#04x} must survive"
        );
        for section in 0..3 {
            assert!(
                section_file_present(&root, key, section),
                "section {section} of config {key:#04x} must survive"
            );
        }
    }

    // Flip back: the config is resident, and its index and section load
    // without any rebuild.
    let (settings, portrait) = (store.type_settings(), !store.portrait());
    store.set_layout(settings, portrait);
    let adoption = files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    assert!(
        adoption.resident,
        "the config just flipped away from must read as already built"
    );
    assert_eq!(adoption.evicted, None, "two configs fit; nothing should go");
    assert!(
        matches!(
            files::load_v2_book_index(&root, KEY, IDENTITY, &mut store),
            files::BookIndexLoadResult::Hit { unfinished: false }
        ),
        "the flipped-back config's index must load, not miss"
    );
    assert!(
        matches!(
            files::load_v2_section_by_global_page(&root, KEY, IDENTITY, 0, &mut store),
            CacheLoadResult::Hit { .. }
        ),
        "the flipped-back config's section must load from its own file"
    );
}

/// Invariant: a third config evicts the least recently used one, and takes
/// only its files -- not the surviving config's, and not the
/// settings-independent ones.
#[test]
fn a_third_layout_config_evicts_the_least_recently_used_one() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let first_key = build_under_current_config(&root, &mut store, 2);
    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let second_key = build_under_current_config(&root, &mut store, 2);
    // COVER.BIN is settings-independent; eviction must not reach it.
    write_cover(&root);

    let (settings, portrait) = larger_of(&store);
    store.set_layout(settings, portrait);
    let third_key = layout_cache_key_for(&store);
    let adoption = files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    assert_eq!(
        adoption.evicted,
        Some(first_key),
        "the third config must evict the least recently used one"
    );
    assert!(
        !adoption.resident,
        "a config never built cannot be resident"
    );
    assert_eq!(
        adoption.eviction_failed, None,
        "an eviction that took every file has nothing to report as left behind"
    );

    assert!(
        !book_index_present(&root, first_key),
        "the evicted config's index must be gone"
    );
    for section in 0..2 {
        assert!(
            !section_file_present(&root, first_key, section),
            "the evicted config's section {section} must be gone"
        );
        assert!(
            section_file_present(&root, second_key, section),
            "the surviving config's section {section} must not be collateral"
        );
    }
    assert!(
        book_index_present(&root, second_key),
        "the surviving config's index must not be collateral"
    );
    assert_ne!(third_key, first_key);
    assert_eq!(
        files::load_v2_cover_cache(&root, KEY, &mut store),
        files::CoverLoadResult::Hit,
        "COVER.BIN does not depend on the layout config and must survive eviction"
    );
}

/// Invariant: an eviction that cannot delete the config's index leaves that
/// config registered, and the next open retries it.
///
/// The registry is the only thing that counts a paginated copy of the book
/// toward the two-config bound. Promoting over a config whose files are still
/// there would unregister a full section set: no later eviction would look at
/// it again, and on a card refusing deletes for want of space the build this
/// open is about to run would be asked for a third set on top of it.
///
/// The delete is blocked here by holding the file open, which this
/// embedded-sdmmc rev refuses to reopen — the same `RemoveStatus::Failed` an
/// I/O fault produces, but aimed at one named file instead of the Nth write.
#[test]
fn an_eviction_that_cannot_delete_keeps_the_evicted_config_registered() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let first_key = build_under_current_config(&root, &mut store, 2);
    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let second_key = build_under_current_config(&root, &mut store, 2);

    // A third config arrives with the least recently used one's section file
    // undeletable.
    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    let sections = book.open_dir(CACHE_SECTIONS_DIR).expect("sections dir");
    let mut sname = heapless::String::<CACHE_SECTION_FILE_BYTES>::new();
    section_file_name(first_key, 0, &mut sname);
    let held = sections
        .open_file_in_dir(sname.as_str(), embedded_sdmmc::Mode::ReadOnly)
        .expect("hold section file open");

    let (settings, portrait) = larger_of(&store);
    store.set_layout(settings, portrait);
    let blocked = files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    assert_eq!(
        blocked.evicted, None,
        "nothing was deleted, so nothing may be reported evicted"
    );
    assert_eq!(
        blocked.eviction_failed,
        Some(first_key),
        "the config that would not go must be named, not silently dropped"
    );

    drop(held);
    drop(sections);
    drop(book);
    assert!(
        book_index_present(&root, first_key),
        "the refused delete leaves the index on the card"
    );
    for section in 0..2 {
        assert!(
            section_file_present(&root, second_key, section),
            "the surviving config is untouched either way"
        );
    }

    // The whole point of leaving the registry alone: the config is still
    // named, so the next open under the third config evicts it rather than
    // walking past files nothing counts.
    let retry = files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    assert_eq!(
        retry.evicted,
        Some(first_key),
        "the registry must still have named the config it could not delete"
    );
    assert_eq!(retry.eviction_failed, None);
    assert!(
        !book_index_present(&root, first_key),
        "the retry takes the index the first attempt could not"
    );
    for section in 0..2 {
        assert!(
            !section_file_present(&root, first_key, section),
            "the retry takes section {section} as well"
        );
    }
}

/// Invariant: the same holds when it is a *section* file that will not go,
/// after the index has already been deleted.
///
/// This is the half-deleted case: the config's index is gone, so nothing can
/// read it, but its section files still occupy the card. Reporting that as an
/// eviction would leave those files uncounted forever; keeping the config
/// registered is what sends the next open back for them.
#[test]
fn an_eviction_that_cannot_delete_a_section_file_reports_no_eviction() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let first_key = build_under_current_config(&root, &mut store, 2);
    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    build_under_current_config(&root, &mut store, 2);

    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    let sections = book.open_dir(CACHE_SECTIONS_DIR).expect("sections dir");
    let mut section_name = heapless::String::<CACHE_SECTION_FILE_BYTES>::new();
    section_file_name(first_key, 1, &mut section_name);
    let held = sections
        .open_file_in_dir(section_name.as_str(), embedded_sdmmc::Mode::ReadOnly)
        .expect("hold one of the evicted config's sections open");

    let (settings, portrait) = larger_of(&store);
    store.set_layout(settings, portrait);
    let blocked = files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    assert_eq!(
        blocked.evicted, None,
        "a sweep that left a file behind is not an eviction"
    );
    assert_eq!(blocked.eviction_failed, Some(first_key));

    drop(held);
    drop(sections);
    drop(book);
    assert!(
        book_index_present(&root, first_key),
        "sections go first, so the index remains when a section delete fails"
    );
    assert!(
        section_file_present(&root, first_key, 1),
        "the section that refused to go is still on the card"
    );

    let retry = files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    assert_eq!(
        retry.evicted,
        Some(first_key),
        "a config whose sections failed to delete is still the one owed a sweep"
    );
    assert!(
        !section_file_present(&root, first_key, 1),
        "the retry finishes the sweep the refusal interrupted"
    );
}

/// Invariant: re-reading a resident config moves it off the eviction block.
/// Without this the registry would be insertion-ordered, and the config the
/// reader had just come back to would be the next one thrown away.
#[test]
fn re_reading_a_config_keeps_it_off_the_eviction_block() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let first_key = build_under_current_config(&root, &mut store, 1);
    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let second_key = build_under_current_config(&root, &mut store, 1);

    // Go back to the first config without building: it becomes most recent.
    let (settings, portrait) = (store.type_settings(), !store.portrait());
    store.set_layout(settings, portrait);
    assert!(files::adopt_layout_config(&root, KEY, IDENTITY, &store).resident);

    // A third config now takes the *other* one's slot.
    let (settings, portrait) = larger_of(&store);
    store.set_layout(settings, portrait);
    assert_eq!(
        files::adopt_layout_config(&root, KEY, IDENTITY, &store).evicted,
        Some(second_key),
        "eviction must take the config that has not been read since, not the one just re-read"
    );
    assert!(
        book_index_present(&root, first_key),
        "the re-read config must still be on the card"
    );
}

/// Invariant: the first open of a cache written by the single-config firmware
/// deletes its files, and leaves the settings-independent ones alone.
///
/// Without this the old `BOOK.BIN` and unkeyed `S<spine>.BIN` files would sit
/// on the card forever: nothing reads them under the new names, and the
/// registry that drives eviction never learns they exist.
#[test]
fn the_first_open_after_the_single_config_scheme_purges_its_files() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    // A cache exactly as the previous firmware left it: an unkeyed index, an
    // unkeyed section file, and a cover.
    files::ensure_v2_cache_dirs(&root, KEY).expect("cache dirs");
    write_legacy_book_index(&root, IDENTITY);
    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    let sections = book.open_dir(CACHE_SECTIONS_DIR).expect("sections dir");
    let file = sections
        .open_file_in_dir("S000.BIN", embedded_sdmmc::Mode::ReadWriteCreateOrTruncate)
        .expect("legacy section file");
    file.write(&[0u8; 64]).expect("legacy file body");
    drop(file);
    drop(sections);
    write_cover(&root);
    drop(book);

    let adoption = files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    assert!(
        adoption.purged_legacy,
        "an unkeyed BOOK.BIN must be recognized as the previous scheme's"
    );
    assert!(
        !adoption.resident,
        "the purged cache cannot count as this config's"
    );

    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    assert!(
        book.open_file_in_dir(CACHE_BOOK_FILE, embedded_sdmmc::Mode::ReadOnly)
            .is_err(),
        "the unkeyed index must be gone"
    );
    let sections = book.open_dir(CACHE_SECTIONS_DIR).expect("sections dir");
    assert!(
        sections
            .open_file_in_dir("S000.BIN", embedded_sdmmc::Mode::ReadOnly)
            .is_err(),
        "the unkeyed section file must be gone"
    );
    drop(sections);
    drop(book);
    assert_eq!(
        files::load_v2_cover_cache(&root, KEY, &mut store),
        files::CoverLoadResult::Hit,
        "the purge must not reach the settings-independent files"
    );

    // A second open has nothing left to find, so the pass costs one failed
    // open rather than a sections listing.
    assert!(!files::adopt_layout_config(&root, KEY, IDENTITY, &store).purged_legacy);
}

/// Invariant: a cache whose registry was lost still identifies its book.
///
/// `read_cache_header`'s `Absent` is what licenses the orphan sweep to delete
/// a cache directory. The source identity now lives inside a per-config index
/// the registry names, so a lost registry must not be able to make a cache
/// that is plainly there read as absent.
#[test]
fn a_cache_whose_registry_is_lost_still_names_its_book() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    build_under_current_config(&root, &mut store, 2);
    match files::read_cache_header(&root, KEY) {
        files::CacheHeader::Present(header) => {
            assert_eq!((header.source_hash, header.source_size), IDENTITY)
        }
        other => panic!("a published cache must identify its book, got {other:?}"),
    }

    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    book.delete_file_in_dir(CACHE_CONFIG_FILE)
        .expect("registry deletes");
    drop(book);

    match files::read_cache_header(&root, KEY) {
        files::CacheHeader::Present(header) => {
            assert_eq!(
                (header.source_hash, header.source_size),
                IDENTITY,
                "the index is still on the card and still says whose it is"
            )
        }
        other => panic!("a lost registry must not make a live cache deletable, got {other:?}"),
    }
}

/// Invariant: a corrupt registry reads as `Unreadable`, never `Absent`.
///
/// `Absent` is what licenses the orphan sweep to delete a cache directory, so
/// a registry that is plainly there but says nothing usable must fail closed:
/// the clear path refuses to delete against a key it cannot prove, and the
/// sweep's own comment turns on the same distinction.
#[test]
fn a_registry_that_says_nothing_usable_fails_closed() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    build_under_current_config(&root, &mut store, 1);

    // Overwrite the registry with bytes that decode to nothing, and take the
    // index with it so no config can answer for the book.
    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    let mut name = heapless::String::<BOOK_INDEX_FILE_BYTES>::new();
    book_index_file_name(layout_cache_key_for(&store), &mut name);
    book.delete_file_in_dir(name.as_str())
        .expect("index deletes");
    let file = book
        .open_file_in_dir(
            CACHE_CONFIG_FILE,
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("registry opens");
    file.write(b"garbage!!").expect("registry body");
    drop(file);
    drop(book);

    assert_eq!(
        files::read_cache_header(&root, KEY),
        files::CacheHeader::Unreadable,
        "a registry that is there and unusable must not read as no cache at all"
    );
}

/// Invariant: a `CFG.BIN` write that will not land is reported, not swallowed.
///
/// The registry is opened create-or-truncate, so a refused write does not leave
/// the previous one behind -- it leaves nothing decodable. An open that shrugged
/// that off would go on to build under a config no registry names.
#[test]
fn a_registry_write_that_is_refused_is_reported() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    build_under_current_config(&root, &mut store, 1);

    // Block the registry the same way the eviction tests block a delete: hold
    // it open, which this embedded-sdmmc rev will not reopen for writing.
    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    let held = book
        .open_file_in_dir(CACHE_CONFIG_FILE, embedded_sdmmc::Mode::ReadOnly)
        .expect("hold the registry open");

    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let blocked = files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    assert!(
        blocked.registry_write_failed,
        "a registry that would not take the write must say so"
    );
    assert_eq!(
        blocked.eviction_failed, None,
        "nothing needed evicting -- this is the write, not the delete"
    );
    drop(held);
    drop(book);
}

/// Invariant: a registry left unusable by a refused write does not cost the
/// two-config bound. This is the accumulation the write failure above would
/// otherwise start.
///
/// A `CFG.BIN` that is there and will not decode used to read as an empty
/// registry, which unregisters every config on the card at once: eviction stops
/// counting them, so the next open adds a third full section set, and the one
/// after that a fourth. Rebuilding the registry from the index files that are
/// really there is what keeps the count honest.
#[test]
fn a_registry_left_unusable_is_rebuilt_from_the_index_files_on_the_card() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let first_key = build_under_current_config(&root, &mut store, 2);
    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let second_key = build_under_current_config(&root, &mut store, 2);

    // Exactly what a refused write leaves: the file there, its contents gone.
    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    let file = book
        .open_file_in_dir(
            CACHE_CONFIG_FILE,
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("registry opens");
    drop(file);
    drop(book);

    // A third config arrives. Both existing configs are still on the card, so
    // the bound says one of them goes -- which an empty registry could not know.
    let (settings, portrait) = larger_of(&store);
    store.set_layout(settings, portrait);
    let adoption = files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    let evicted = adoption
        .evicted
        .expect("a truncated registry must not hide the configs the card still holds");
    assert!(
        evicted == first_key || evicted == second_key,
        "eviction must take one of the two configs that were really there, got {evicted:#04x}"
    );
    assert!(
        !book_index_present(&root, evicted),
        "the evicted config's index must actually be gone"
    );
    for section in 0..2 {
        assert!(
            !section_file_present(&root, evicted, section),
            "section {section} of the evicted config must go with its index"
        );
    }
    let survivor = if evicted == first_key {
        second_key
    } else {
        first_key
    };
    assert!(
        book_index_present(&root, survivor),
        "eviction must take one config, not both"
    );
}

/// Invariant: a legacy purge that cannot finish leaves the marker that brings
/// the next open back for it.
///
/// `BOOK.BIN` is the only thing that makes a later open pay for a sections
/// listing, so deleting it before the sweep finished would strand the unkeyed
/// section files for good: nothing reads those names and no registry counts
/// them, so no later pass would ever look again.
#[test]
fn a_legacy_sweep_that_is_blocked_keeps_its_retry_marker() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let store = new_store();

    files::ensure_v2_cache_dirs(&root, KEY).expect("cache dirs");
    write_legacy_book_index(&root, IDENTITY);
    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    let sections = book.open_dir(CACHE_SECTIONS_DIR).expect("sections dir");
    let file = sections
        .open_file_in_dir("S000.BIN", embedded_sdmmc::Mode::ReadWriteCreateOrTruncate)
        .expect("legacy section file");
    file.write(&[0u8; 64]).expect("legacy file body");
    drop(file);

    // The legacy section will not delete while it is held open.
    let held = sections
        .open_file_in_dir("S000.BIN", embedded_sdmmc::Mode::ReadOnly)
        .expect("hold the legacy section open");

    assert!(
        files::adopt_layout_config(&root, KEY, IDENTITY, &store).purged_legacy,
        "the purge marker must survive a single-config sweep"
    );
    assert!(
        book.open_file_in_dir(CACHE_BOOK_FILE, embedded_sdmmc::Mode::ReadOnly)
            .is_ok(),
        "the sweep did not finish, so the marker must survive to bring the next open back"
    );

    drop(held);
    drop(sections);
    drop(book);

    // The block is gone; the retry the marker bought finishes the job.
    assert!(
        files::adopt_layout_config(&root, KEY, IDENTITY, &store).purged_legacy,
        "the surviving marker must send the next open back through the purge"
    );
    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    let sections = book.open_dir(CACHE_SECTIONS_DIR).expect("sections dir");
    assert!(
        sections
            .open_file_in_dir("S000.BIN", embedded_sdmmc::Mode::ReadOnly)
            .is_err(),
        "the retry takes the legacy section the first pass could not"
    );
    drop(sections);
    assert!(
        book.open_file_in_dir(CACHE_BOOK_FILE, embedded_sdmmc::Mode::ReadOnly)
            .is_err(),
        "and the marker goes once there is nothing left to come back for"
    );
}

/// Invariant: no single read fault can make a cache that is plainly on the card
/// read as `Absent`.
///
/// `Absent` is the one answer that licenses the clear path to delete a
/// directory, against a 28-bit key whose collisions the format admits. The
/// registry is deleted here so the fallback directory listing is the only route
/// to the index -- and a listing that fails must not be indistinguishable from
/// one that completed and found nothing.
#[test]
fn no_read_fault_can_make_a_cache_that_is_there_read_as_absent() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    build_under_current_config(&root, &mut store, 1);
    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    book.delete_file_in_dir(CACHE_CONFIG_FILE)
        .expect("registry deletes");
    drop(book);

    // Baseline: with no fault armed, the listing finds the index.
    let before = disk.reads.get();
    assert!(
        matches!(
            files::read_cache_header(&root, KEY),
            files::CacheHeader::Present(_)
        ),
        "the unlisted-index scan must find a cache whose registry is gone"
    );
    let reads = disk.reads.get() - before;
    assert!(
        reads > 0,
        "the read counter must be moving for this to mean anything"
    );

    // Every read that pass makes, failed one at a time. `Unreadable` is fine --
    // it fails closed. `Absent` is the one answer that would license a delete.
    let mut unreadable = 0;
    for nth in 0..reads {
        disk.fault.fail_read_in.set(Some(nth));
        let header = files::read_cache_header(&root, KEY);
        disk.fault.fail_read_in.set(None);
        assert_ne!(
            header,
            files::CacheHeader::Absent,
            "read fault {nth} made a cache that is on the card read as deletable"
        );
        if header == files::CacheHeader::Unreadable {
            unreadable += 1;
        }
    }
    assert!(
        unreadable > 0,
        "no fault position actually disturbed the read, so this proved nothing"
    );
}

/// Invariant: clearing a book with two configs resident leaves nothing behind
/// -- and says so. The clear reports success on what the directory actually
/// holds, so a per-config file it did not learn to delete would show up as a
/// failed clear rather than as silent leftovers.
#[test]
fn clearing_a_book_with_two_configs_resident_leaves_nothing_behind() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    build_under_current_config(&root, &mut store, 2);
    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    build_under_current_config(&root, &mut store, 2);
    write_cover(&root);

    assert!(
        files::empty_cache_dir(&root, KEY),
        "a clear must reclaim every config's files, not report failure on the ones it missed"
    );
    assert_eq!(
        files::read_cache_header(&root, KEY),
        files::CacheHeader::Absent,
        "nothing identifying the book may survive the clear"
    );
}

/// Invariant: CFG.BIN is preserved if a full-cache clear fails partway through,
/// ensuring surviving layout configurations remain tracked for LRU eviction.
#[test]
fn cfg_bin_is_preserved_if_cache_clear_fails() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    build_under_current_config(&root, &mut store, 2);

    // Block a section file from being deleted during clear.
    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    let sections = book.open_dir("SECTIONS").expect("sections dir");
    let section_file = sections
        .open_file_in_dir("S00000.BIN", embedded_sdmmc::Mode::ReadOnly)
        .expect("hold section file open");

    assert!(
        !files::empty_cache_dir(&root, KEY),
        "a clear with a blocked section file must return false"
    );

    // CFG.BIN must still be present in the book directory.
    assert!(
        book.open_file_in_dir(
            proto::cache::CACHE_CONFIG_FILE,
            embedded_sdmmc::Mode::ReadOnly
        )
        .is_ok(),
        "CFG.BIN must survive a failed clear so LRU tracking is not lost"
    );

    drop(section_file);
    drop(sections);
    drop(book);
}

/// Helper to build an index and section files without adopting into the registry.
fn build_raw_config_index(root: &Dir<'_>, store: &mut ReaderStore, sections: usize) -> u8 {
    let layout_key = layout_cache_key_for(store);
    files::ensure_v2_cache_dirs(root, KEY).expect("dirs");
    let records = build_book(root, store, sections);
    let pages = total_pages(&records);
    assert!(
        files::write_v2_book_index(root, KEY, IDENTITY, pages, &records, store, false, 0),
        "the index for config {layout_key:#04x} should write"
    );
    layout_key
}

/// Invariant: reconstructing a registry from more than two indexes explicitly
/// deletes excess non-surviving configurations from the card before committing.
#[test]
fn reconstruct_registry_removes_excess_configs_and_succeeds() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let _k1 = build_raw_config_index(&root, &mut store, 1);

    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let _k2 = build_raw_config_index(&root, &mut store, 1);

    let (settings, portrait) = larger_of(&store);
    store.set_layout(settings, portrait);
    let _k3 = build_raw_config_index(&root, &mut store, 1);

    // Now corrupt CFG.BIN so adoption must rebuild the registry from index files.
    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    let cfg_file = book
        .open_file_in_dir(
            proto::cache::CACHE_CONFIG_FILE,
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("open CFG.BIN");
    drop(cfg_file);
    drop(book);

    // Rebuilding will find 3 index files on disk (_k1, _k2, _k3). It must evict & delete the 3rd one found.
    let adoption = files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    assert!(
        adoption.succeeded(),
        "reconstruction from index files with excess configs must succeed"
    );
}

/// Invariant: registry reconstruction handles an overflowing inventory containing
/// five keyed indexes across multiple listing passes, deleting all excess configs
/// and committing a 2-slot registry.
#[test]
fn reconstruct_registry_handles_overflowing_inventory_with_five_configs() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let k1 = build_raw_config_index(&root, &mut store, 1);

    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let k2 = build_raw_config_index(&root, &mut store, 1);

    let (settings, portrait) = larger_of(&store);
    store.set_layout(settings, portrait);
    let k3 = build_raw_config_index(&root, &mut store, 1);

    let mut settings = store.type_settings();
    settings.size = display::font::FontSize::Small;
    store.set_layout(settings, store.portrait());
    let k4 = build_raw_config_index(&root, &mut store, 1);

    let mut settings = store.type_settings();
    settings.weight = display::font::FontWeight::Heavy;
    store.set_layout(settings, store.portrait());
    let k5 = build_raw_config_index(&root, &mut store, 1);

    // Corrupt CFG.BIN so adoption must reconstruct from 5 index files on disk.
    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    let cfg_file = book
        .open_file_in_dir(
            proto::cache::CACHE_CONFIG_FILE,
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("open CFG.BIN");
    drop(cfg_file);
    drop(book);

    let adoption = files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    assert!(
        adoption.succeeded(),
        "reconstructing from 5 index files must succeed by looping passes"
    );

    // Verify exactly 2 of the 5 configs remain present on disk.
    let present_count = [k1, k2, k3, k4, k5]
        .iter()
        .filter(|&&k| book_index_present(&root, k))
        .count();
    assert_eq!(
        present_count, 2,
        "reconstruction must leave exactly two config indexes on disk"
    );
}

/// Invariant: section files of an excess config are deleted before its index.
/// An interrupted section deletion leaves the index intact as a durable marker
/// so the next open retries the cleanup before committing CFG.BIN.
#[test]
fn reconstruct_retries_cleanup_when_excess_section_delete_fails() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let k1 = build_raw_config_index(&root, &mut store, 1);

    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let _k2 = build_raw_config_index(&root, &mut store, 1);

    let (settings, portrait) = larger_of(&store);
    store.set_layout(settings, portrait);
    let _k3 = build_raw_config_index(&root, &mut store, 1);

    // Corrupt CFG.BIN so adoption must reconstruct.
    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    let cfg_file = book
        .open_file_in_dir(
            proto::cache::CACHE_CONFIG_FILE,
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("open CFG.BIN");
    drop(cfg_file);

    // Hold a section file of k1 open so its section deletion fails.
    let sections = book.open_dir("SECTIONS").expect("sections dir");
    let mut sname = heapless::String::<CACHE_SECTION_FILE_BYTES>::new();
    section_file_name(k1, 0, &mut sname);
    let held_section = sections
        .open_file_in_dir(sname.as_str(), embedded_sdmmc::Mode::ReadOnly)
        .expect("hold k1 section file open");

    let blocked = files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    assert!(
        !blocked.succeeded(),
        "adoption must fail when excess section deletion fails"
    );
    assert!(
        book_index_present(&root, k1),
        "BK<k1>.BIN must survive when section deletion fails so it can be retried"
    );

    drop(held_section);
    drop(sections);
    drop(book);

    let retry = files::adopt_layout_config(&root, KEY, IDENTITY, &store);
    assert!(
        retry.succeeded(),
        "adoption must succeed on retry once section block is cleared"
    );
    assert!(
        !book_index_present(&root, k1),
        "BK<k1>.BIN must be removed after successful section deletion"
    );
}

/// Invariant: adopting a cache key occupied by a different source identity
/// (a 28-bit hash collision) clears the old owner's cache files before adopting
/// the new book under its own layout configuration.
#[test]
fn adopting_colliding_key_clears_old_owner_cache_and_adopts_new_owner() {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);
    const IDENTITY_B: (u32, u32) = (0x3333_4444, 2000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    // Book A builds under Layout 1 (default store layout)
    files::ensure_v2_cache_dirs(&root, KEY).expect("dirs");
    files::adopt_layout_config(&root, KEY, IDENTITY_A, &store);
    let records_a = build_book(&root, &mut store, 1);
    let pages_a = total_pages(&records_a);
    let k1 = layout_cache_key_for(&store);
    assert!(files::write_v2_book_index(
        &root, KEY, IDENTITY_A, pages_a, &records_a, &store, false, 0
    ));

    assert!(book_index_present(&root, k1));
    let files::CacheHeader::Present(header_a) = files::read_cache_header(&root, KEY) else {
        panic!("expected header A");
    };
    assert_eq!((header_a.source_hash, header_a.source_size), IDENTITY_A);

    // Book B opens under a colliding key with Layout 2 (landscape)
    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let k2 = layout_cache_key_for(&store);
    assert_ne!(k1, k2, "test requires two different layout keys");

    let adoption_b = files::adopt_layout_config(&root, KEY, IDENTITY_B, &store);
    assert!(
        adoption_b.purged_legacy,
        "adopting a colliding key must report legacy/colliding purge"
    );

    let records_b = build_book(&root, &mut store, 1);
    let pages_b = total_pages(&records_b);
    assert!(files::write_v2_book_index(
        &root, KEY, IDENTITY_B, pages_b, &records_b, &store, false, 0
    ));

    // Book A's index is gone; Book B's index is present.
    assert!(
        !book_index_present(&root, k1),
        "Book A's index must be purged when Book B adopts the colliding key"
    );
    assert!(
        book_index_present(&root, k2),
        "Book B's index must be present after build"
    );

    // read_cache_header now reports Book B's identity.
    let files::CacheHeader::Present(header_b) = files::read_cache_header(&root, KEY) else {
        panic!("expected header B");
    };
    assert_eq!((header_b.source_hash, header_b.source_size), IDENTITY_B);
}

/// Invariant: adoption fails closed (`dirs_failed = true`) when an existing index
/// file is corrupt or unreadable, preventing a colliding book from publishing
/// over an unproven directory.
#[test]
fn adopt_fails_closed_when_registered_index_is_unreadable() {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);
    const IDENTITY_B: (u32, u32) = (0x3333_4444, 2000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    files::ensure_v2_cache_dirs(&root, KEY).expect("dirs");
    files::adopt_layout_config(&root, KEY, IDENTITY_A, &store);
    let records = build_book(&root, &mut store, 1);
    let pages = total_pages(&records);
    let k1 = layout_cache_key_for(&store);
    assert!(files::write_v2_book_index(
        &root, KEY, IDENTITY_A, pages, &records, &store, false, 0
    ));

    // Corrupt BK<k1>.BIN to 0 bytes so read_book_index_header returns Some(None).
    let book = files::open_v2_book_dir(&root, KEY).expect("book dir");
    let mut iname = heapless::String::<BOOK_INDEX_FILE_BYTES>::new();
    book_index_file_name(k1, &mut iname);
    let ifile = book
        .open_file_in_dir(
            iname.as_str(),
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("open index file");
    drop(ifile);
    drop(book);

    let adoption = files::adopt_layout_config(&root, KEY, IDENTITY_B, &store);
    assert!(
        adoption.dirs_failed,
        "adoption must fail closed when an index file is unreadable"
    );
}

/// Invariant: adoption fails closed (`dirs_failed = true`) when TOC.BIN is
/// unreadable or corrupt, preventing publication over unverified directory state.
#[test]
fn adopt_fails_closed_when_toc_is_unreadable() {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);
    const IDENTITY_B: (u32, u32) = (0x3333_4444, 2000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    files::ensure_v2_cache_dirs(&root, KEY).expect("dirs");
    files::adopt_layout_config(&root, KEY, IDENTITY_A, &store);
    let records = build_book(&root, &mut store, 1);
    let _pages = total_pages(&records);
    assert!(files::write_v2_toc_file(
        &root, KEY, IDENTITY_A, 1, &[0u8; 32]
    ));

    // Delete CFG.BIN and BK*.BIN so TOC.BIN is the remaining identity source,
    // then truncate TOC.BIN to 2 bytes so decode_toc_file_header fails.
    let book = files::open_v2_book_dir(&root, KEY).expect("book dir");
    let _ = book.delete_file_in_dir(proto::cache::CACHE_CONFIG_FILE);
    let mut iname = heapless::String::<BOOK_INDEX_FILE_BYTES>::new();
    book_index_file_name(layout_cache_key_for(&store), &mut iname);
    let _ = book.delete_file_in_dir(iname.as_str());

    let toc_file = book
        .open_file_in_dir(
            proto::cache::CACHE_TOC_FILE,
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("truncate TOC.BIN");
    let _ = toc_file.write(&[0x01, 0x02]);
    drop(toc_file);
    drop(book);

    let adoption = files::adopt_layout_config(&root, KEY, IDENTITY_B, &store);
    assert!(
        adoption.dirs_failed,
        "adoption must fail closed when TOC.BIN is unreadable"
    );
}

/// Invariant: an interrupted collision section deletion preserves the old owner's
/// index/TOC markers on disk so subsequent opens continue to see a Mismatch and retry
/// the collision purge before adopting the new owner.
#[test]
fn collision_purge_retries_cleanup_when_old_owner_section_delete_fails() {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);
    const IDENTITY_B: (u32, u32) = (0x3333_4444, 2000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    files::ensure_v2_cache_dirs(&root, KEY).expect("dirs");
    files::adopt_layout_config(&root, KEY, IDENTITY_A, &store);
    let records_a = build_book(&root, &mut store, 1);
    let pages_a = total_pages(&records_a);
    let k1 = layout_cache_key_for(&store);
    assert!(files::write_v2_book_index(
        &root, KEY, IDENTITY_A, pages_a, &records_a, &store, false, 0
    ));

    // Hold Book A's section file open so section deletion during collision purge fails.
    let book = files::open_v2_book_dir(&root, KEY).expect("book dir");
    let sections = book.open_dir(CACHE_SECTIONS_DIR).expect("sections dir");
    let mut sname = heapless::String::<CACHE_SECTION_FILE_BYTES>::new();
    section_file_name(k1, 0, &mut sname);
    let held_section = sections
        .open_file_in_dir(sname.as_str(), embedded_sdmmc::Mode::ReadOnly)
        .expect("hold section file open");

    // Book B attempts to adopt the colliding key.
    let blocked = files::adopt_layout_config(&root, KEY, IDENTITY_B, &store);
    assert!(
        !blocked.succeeded(),
        "adoption must fail when old-owner section deletion fails"
    );
    assert!(
        book_index_present(&root, k1),
        "Book A's index marker must be preserved when section deletion fails"
    );

    drop(held_section);
    drop(sections);
    drop(book);

    // Second attempt retries collision purge and succeeds.
    let retry = files::adopt_layout_config(&root, KEY, IDENTITY_B, &store);
    assert!(
        retry.succeeded(),
        "adoption must succeed on retry once section block is cleared"
    );
    assert!(
        !book_index_present(&root, k1),
        "Book A's index marker must be purged after retry completes"
    );
}

/// Helper to build an index file under a specific source identity without registry adoption.
fn build_raw_config_index_for_identity(
    root: &Dir<'_>,
    store: &mut ReaderStore,
    identity: (u32, u32),
    sections: usize,
) -> u8 {
    let layout_key = layout_cache_key_for(store);
    files::ensure_v2_cache_dirs(root, KEY).expect("dirs");
    let records = build_book(root, store, sections);
    let pages = total_pages(&records);
    assert!(
        files::write_v2_book_index(root, KEY, identity, pages, &records, store, false, 0),
        "the index for config {layout_key:#04x} should write"
    );
    layout_key
}

/// Invariant: collision cleanup drains all old-owner index files across multiple
/// listing passes when more than four index files are present on disk.
#[test]
fn adopting_colliding_key_purges_all_five_old_owner_indexes() {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);
    const IDENTITY_B: (u32, u32) = (0x3333_4444, 2000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    // Create 5 raw config indexes for Book A
    let k1 = build_raw_config_index_for_identity(&root, &mut store, IDENTITY_A, 1);

    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let k2 = build_raw_config_index_for_identity(&root, &mut store, IDENTITY_A, 1);

    let (settings, portrait) = larger_of(&store);
    store.set_layout(settings, portrait);
    let k3 = build_raw_config_index_for_identity(&root, &mut store, IDENTITY_A, 1);

    let mut settings = store.type_settings();
    settings.size = display::font::FontSize::Medium;
    store.set_layout(settings, true);
    let k4 = build_raw_config_index_for_identity(&root, &mut store, IDENTITY_A, 1);

    let mut settings = store.type_settings();
    settings.weight = display::font::FontWeight::Heavy;
    store.set_layout(settings, store.portrait());
    let k5 = build_raw_config_index_for_identity(&root, &mut store, IDENTITY_A, 1);

    let keys = [k1, k2, k3, k4, k5];
    for i in 0..keys.len() {
        for j in (i + 1)..keys.len() {
            assert_ne!(
                keys[i], keys[j],
                "all five layout keys must be distinct to test five index files: keys={keys:?}"
            );
        }
    }

    // Book B adopts colliding key
    let adoption_b = files::adopt_layout_config(&root, KEY, IDENTITY_B, &store);
    assert!(
        adoption_b.succeeded(),
        "collision purge of 5 old-owner indexes must succeed"
    );

    // All 5 old-owner index files must be gone.
    for k in [k1, k2, k3, k4, k5] {
        assert!(
            !book_index_present(&root, k),
            "old-owner index {k:#04x} must be purged"
        );
    }
}

/// Invariant: a readable matching index does NOT override an unreadable second index.
/// Any unreadable artifact makes ownership unproven (Unreadable) and fails closed.
#[test]
fn adopt_fails_closed_when_matching_index_coexists_with_unreadable_index() {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    files::ensure_v2_cache_dirs(&root, KEY).expect("dirs");
    files::adopt_layout_config(&root, KEY, IDENTITY_A, &store);
    let records = build_book(&root, &mut store, 1);
    let pages = total_pages(&records);
    let k1 = layout_cache_key_for(&store);
    assert!(files::write_v2_book_index(
        &root, KEY, IDENTITY_A, pages, &records, &store, false, 0
    ));

    // Create a second unreadable index file (0 bytes).
    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let k2 = layout_cache_key_for(&store);
    assert_ne!(k1, k2);

    let book = files::open_v2_book_dir(&root, KEY).expect("book dir");
    let mut iname = heapless::String::<BOOK_INDEX_FILE_BYTES>::new();
    book_index_file_name(k2, &mut iname);
    let ifile = book
        .open_file_in_dir(
            iname.as_str(),
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("open corrupt index file");
    drop(ifile);
    drop(book);

    let adoption = files::adopt_layout_config(&root, KEY, IDENTITY_A, &store);
    assert!(
        adoption.dirs_failed,
        "adoption must fail closed when an unreadable index coexists with a matching index"
    );
}

/// Invariant: ownership checking inspects EVERY unlisted index file, detecting a
/// mismatching index even when four matching unlisted indexes precede it.
#[test]
fn adopting_colliding_key_inspects_all_unlisted_indexes_and_purges() {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);
    const IDENTITY_B: (u32, u32) = (0x3333_4444, 2000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    // Create 4 matching unlisted indexes for Book A
    let k1 = build_raw_config_index_for_identity(&root, &mut store, IDENTITY_A, 1);

    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let k2 = build_raw_config_index_for_identity(&root, &mut store, IDENTITY_A, 1);

    let (settings, portrait) = larger_of(&store);
    store.set_layout(settings, portrait);
    let k3 = build_raw_config_index_for_identity(&root, &mut store, IDENTITY_A, 1);

    let mut settings = store.type_settings();
    settings.size = display::font::FontSize::Medium;
    store.set_layout(settings, true);
    let k4 = build_raw_config_index_for_identity(&root, &mut store, IDENTITY_A, 1);

    // Create 1 mismatching unlisted index for Book B
    let mut settings = store.type_settings();
    settings.weight = display::font::FontWeight::Heavy;
    store.set_layout(settings, store.portrait());
    let k5 = build_raw_config_index_for_identity(&root, &mut store, IDENTITY_B, 1);

    let keys = [k1, k2, k3, k4, k5];
    for i in 0..keys.len() {
        for j in (i + 1)..keys.len() {
            assert_ne!(
                keys[i], keys[j],
                "all five layout keys must be distinct to test five index files: keys={keys:?}"
            );
        }
    }

    // Book A adopts KEY. It must detect Book B's mismatching 5th index and purge all 5 indexes!
    let adoption_a = files::adopt_layout_config(&root, KEY, IDENTITY_A, &store);
    assert!(
        adoption_a.purged_legacy,
        "adopting when a 5th mismatching unlisted index is present must run collision purge"
    );

    for k in [k1, k2, k3, k4, k5] {
        assert!(
            !book_index_present(&root, k),
            "index {k:#04x} must be purged during collision purge"
        );
    }
}

/// Invariant: when non-marker artifact (e.g. COVER.BIN) deletion fails during collision
/// purge, identity markers (BK*.BIN, TOC.BIN) and CFG.BIN are preserved so retry succeeds on next open.
#[test]
fn collision_purge_retries_cleanup_when_old_owner_cover_delete_fails() {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);
    const IDENTITY_B: (u32, u32) = (0x3333_4444, 2000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    files::ensure_v2_cache_dirs(&root, KEY).expect("dirs");
    files::adopt_layout_config(&root, KEY, IDENTITY_A, &store);
    let records_a = build_book(&root, &mut store, 1);
    let pages_a = total_pages(&records_a);
    let k1 = layout_cache_key_for(&store);
    assert!(files::write_v2_book_index(
        &root, KEY, IDENTITY_A, pages_a, &records_a, &store, false, 0
    ));

    // Write COVER.BIN for Book A
    let book = files::open_v2_book_dir(&root, KEY).expect("book dir");
    let cfile = book
        .open_file_in_dir(
            proto::cache::CACHE_COVER_FILE,
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("create COVER.BIN");
    let _ = cfile.write(&[0xAA; 16]);
    drop(cfile);

    // Hold COVER.BIN open so its deletion fails during collision purge
    let held_cover = book
        .open_file_in_dir(
            proto::cache::CACHE_COVER_FILE,
            embedded_sdmmc::Mode::ReadOnly,
        )
        .expect("hold COVER.BIN open");

    // Book B attempts to adopt colliding key
    let blocked = files::adopt_layout_config(&root, KEY, IDENTITY_B, &store);
    assert!(
        !blocked.succeeded(),
        "adoption must fail when COVER.BIN deletion fails during collision purge"
    );
    assert!(
        book_index_present(&root, k1),
        "Book A's index marker must be preserved when COVER.BIN deletion fails"
    );

    drop(held_cover);
    drop(book);

    // Second attempt retries collision purge and succeeds, removing COVER.BIN and identity markers
    let retry = files::adopt_layout_config(&root, KEY, IDENTITY_B, &store);
    assert!(
        retry.succeeded(),
        "adoption must succeed on retry once COVER.BIN is unblocked"
    );
    assert!(
        !book_index_present(&root, k1),
        "Book A's index marker must be purged after retry"
    );
    let book = files::open_v2_book_dir(&root, KEY).expect("book dir");
    assert!(
        book.open_file_in_dir(
            proto::cache::CACHE_COVER_FILE,
            embedded_sdmmc::Mode::ReadOnly
        )
        .is_err(),
        "COVER.BIN must be gone after successful retry"
    );
}

fn write_legacy_book_index(root: &Dir<'_>, identity: (u32, u32)) {
    let book = files::open_v2_book_dir(root, KEY).expect("book cache dir");
    let file = book
        .open_file_in_dir(
            CACHE_BOOK_FILE,
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("legacy BOOK.BIN");
    let mut header_bytes = [0u8; proto::cache::BOOK_V2_HEADER_BYTES];
    proto::cache::encode_book_v2_header(
        proto::cache::BookV2Header {
            partial: false,
            source_hash: identity.0,
            source_size: identity.1,
            total_pages: 10,
            section_count: 1,
            spine_count: 1,
            toc_count: 0,
            toc_text_bytes: 0,
            title_text_bytes: 0,
            author_text_bytes: 0,
            viewport_width: 800,
            viewport_height: 480,
            font_config: 0,
            custom_font_identity: 0,
            resume_spine: 0,
        },
        &mut header_bytes,
    )
    .expect("encode header");
    file.write(&header_bytes).expect("write header");
}

/// Invariant: adopting a layout config against a legacy-only cache belonging to a
/// colliding source purges the colliding BOOK.BIN, legacy sections, AND COVER.BIN.
#[test]
fn adopting_colliding_key_purges_legacy_owner_cache_and_cover() {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);
    const IDENTITY_B: (u32, u32) = (0x3333_4444, 2000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let store = new_store();

    files::ensure_v2_cache_dirs(&root, KEY).expect("cache dirs");
    write_legacy_book_index(&root, IDENTITY_B);

    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    let sections = book.open_dir(CACHE_SECTIONS_DIR).expect("sections dir");
    let sfile = sections
        .open_file_in_dir("S000.BIN", embedded_sdmmc::Mode::ReadWriteCreateOrTruncate)
        .expect("legacy section");
    sfile.write(&[0u8; 64]).expect("section body");
    drop(sfile);
    drop(sections);
    drop(book);

    write_cover(&root);

    // Book A adopts KEY. It must detect Book B's legacy BOOK.BIN identity mismatch,
    // run collision purge, and remove BOOK.BIN, S000.BIN, and COVER.BIN!
    let adoption = files::adopt_layout_config(&root, KEY, IDENTITY_A, &store);
    assert!(
        adoption.purged_legacy,
        "adopting colliding key against legacy cache must run collision purge"
    );

    let book = files::open_v2_book_dir(&root, KEY).expect("book cache dir");
    assert!(
        book.open_file_in_dir(CACHE_BOOK_FILE, embedded_sdmmc::Mode::ReadOnly)
            .is_err(),
        "legacy BOOK.BIN of colliding owner must be purged"
    );
    assert!(
        book.open_file_in_dir(CACHE_COVER_FILE, embedded_sdmmc::Mode::ReadOnly)
            .is_err(),
        "colliding owner's COVER.BIN must be purged so incoming book does not reuse it"
    );
}

/// Invariant: read_cache_header discovers legacy BOOK.BIN identity, so attempts to
/// clear a colliding book's legacy cache are refused.
#[test]
fn clearing_colliding_key_against_legacy_cache_refuses_deletion() {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);
    const IDENTITY_B: (u32, u32) = (0x3333_4444, 2000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    files::ensure_v2_cache_dirs(&root, KEY).expect("cache dirs");
    write_legacy_book_index(&root, IDENTITY_B);
    write_cover(&root);

    // read_cache_header must report Book B's identity from legacy BOOK.BIN
    let header = files::read_cache_header(&root, KEY);
    let files::CacheHeader::Present(h) = header else {
        panic!("read_cache_header must report Present for legacy BOOK.BIN, got {header:?}");
    };
    assert_eq!(h.source_hash, IDENTITY_B.0);
    assert_eq!(h.source_size, IDENTITY_B.1);

    // A clear operation for Book A must see the identity mismatch and refuse deletion!
    assert!(
        h.source_hash != IDENTITY_A.0 || h.source_size != IDENTITY_A.1,
        "identity must mismatch for Book A"
    );
}

/// Invariant: collision purge preserves legacy BOOK.BIN identity marker when cover deletion fails,
/// ensuring subsequent adoption attempts continue to see Mismatch and retry.
#[test]
fn collision_purge_retries_cleanup_when_legacy_old_owner_cover_delete_fails() {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);
    const IDENTITY_B: (u32, u32) = (0x3333_4444, 2000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let store = new_store();

    files::ensure_v2_cache_dirs(&root, KEY).expect("cache dirs");
    write_legacy_book_index(&root, IDENTITY_A);
    write_cover(&root);

    // Hold COVER.BIN open read-only so deletion during collision purge fails
    let book = files::open_v2_book_dir(&root, KEY).expect("book dir");
    let held_cover = book
        .open_file_in_dir(CACHE_COVER_FILE, embedded_sdmmc::Mode::ReadOnly)
        .expect("hold cover open");

    let blocked = files::adopt_layout_config(&root, KEY, IDENTITY_B, &store);
    assert!(
        !blocked.succeeded(),
        "adoption must fail when legacy cover deletion fails"
    );
    assert!(
        book.open_file_in_dir(CACHE_BOOK_FILE, embedded_sdmmc::Mode::ReadOnly)
            .is_ok(),
        "legacy BOOK.BIN identity marker must survive when cover deletion fails"
    );

    drop(held_cover);
    drop(book);

    let retry = files::adopt_layout_config(&root, KEY, IDENTITY_B, &store);
    assert!(
        retry.succeeded(),
        "adoption must succeed on retry once cover block is cleared"
    );
    let book = files::open_v2_book_dir(&root, KEY).expect("book dir");
    assert!(
        book.open_file_in_dir(CACHE_BOOK_FILE, embedded_sdmmc::Mode::ReadOnly)
            .is_err(),
        "legacy BOOK.BIN must be purged after retry completes"
    );
    assert!(
        book.open_file_in_dir(CACHE_COVER_FILE, embedded_sdmmc::Mode::ReadOnly)
            .is_err(),
        "COVER.BIN must be purged after retry completes"
    );
}

/// Invariant: read_cache_header checks the complete inventory and returns Unreadable if a matching
/// index coexists with a mismatching legacy index.
#[test]
fn read_cache_header_returns_unreadable_when_matching_index_coexists_with_mismatching_legacy_index()
{
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);
    const IDENTITY_B: (u32, u32) = (0x3333_4444, 2000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    files::ensure_v2_cache_dirs(&root, KEY).expect("dirs");
    files::adopt_layout_config(&root, KEY, IDENTITY_A, &store);
    let records = build_book(&root, &mut store, 1);
    let pages = total_pages(&records);
    assert!(files::write_v2_book_index(
        &root, KEY, IDENTITY_A, pages, &records, &store, false, 0
    ));

    // Write a legacy BOOK.BIN belonging to mismatching IDENTITY_B
    write_legacy_book_index(&root, IDENTITY_B);

    assert_eq!(
        files::read_cache_header(&root, KEY),
        files::CacheHeader::Unreadable,
        "read_cache_header must return Unreadable when matching index coexists with mismatching legacy index"
    );
}

/// Invariant: read_cache_header checks the complete inventory and returns Unreadable if a matching
/// registered index coexists with a mismatching unlisted index.
#[test]
fn read_cache_header_returns_unreadable_when_matching_index_coexists_with_mismatching_unlisted_index(
) {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);
    const IDENTITY_B: (u32, u32) = (0x3333_4444, 2000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    files::ensure_v2_cache_dirs(&root, KEY).expect("dirs");
    files::adopt_layout_config(&root, KEY, IDENTITY_A, &store);
    let records = build_book(&root, &mut store, 1);
    let pages = total_pages(&records);
    assert!(files::write_v2_book_index(
        &root, KEY, IDENTITY_A, pages, &records, &store, false, 0
    ));

    // Create an unlisted index for IDENTITY_B
    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    build_raw_config_index_for_identity(&root, &mut store, IDENTITY_B, 1);

    assert_eq!(
        files::read_cache_header(&root, KEY),
        files::CacheHeader::Unreadable,
        "read_cache_header must return Unreadable when matching index coexists with mismatching unlisted index"
    );
}

/// Invariant: read_cache_header checks the complete inventory and returns Unreadable if a matching
/// index coexists with an unreadable second index.
#[test]
fn read_cache_header_returns_unreadable_when_matching_index_coexists_with_unreadable_index() {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    files::ensure_v2_cache_dirs(&root, KEY).expect("dirs");
    files::adopt_layout_config(&root, KEY, IDENTITY_A, &store);
    let records = build_book(&root, &mut store, 1);
    let pages = total_pages(&records);
    let k1 = layout_cache_key_for(&store);
    assert!(files::write_v2_book_index(
        &root, KEY, IDENTITY_A, pages, &records, &store, false, 0
    ));

    let (settings, portrait) = landscape_of(&store);
    store.set_layout(settings, portrait);
    let k2 = layout_cache_key_for(&store);
    assert_ne!(k1, k2);

    let book = files::open_v2_book_dir(&root, KEY).expect("book dir");
    let mut iname = heapless::String::<BOOK_INDEX_FILE_BYTES>::new();
    book_index_file_name(k2, &mut iname);
    let ifile = book
        .open_file_in_dir(
            iname.as_str(),
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("create unreadable index");
    drop(ifile);
    drop(book);

    assert_eq!(
        files::read_cache_header(&root, KEY),
        files::CacheHeader::Unreadable,
        "read_cache_header must return Unreadable when matching index coexists with unreadable index"
    );
}

/// Invariant: adopting a colliding key purges the displaced owner's saved position files (POS.BIN, POSA.BIN, POSB.BIN).
#[test]
fn adopting_colliding_key_purges_displaced_owner_position_files() {
    const IDENTITY_A: (u32, u32) = (0x1111_2222, 1000);
    const IDENTITY_B: (u32, u32) = (0x3333_4444, 2000);

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    files::ensure_v2_cache_dirs(&root, KEY).expect("dirs");
    files::adopt_layout_config(&root, KEY, IDENTITY_A, &store);
    let records = build_book(&root, &mut store, 1);
    let pages = total_pages(&records);
    assert!(files::write_v2_book_index(
        &root, KEY, IDENTITY_A, pages, &records, &store, false, 0
    ));
    assert!(
        files::write_position_file(&root, KEY, 5, 42).is_ok(),
        "write position for Book A"
    );
    assert_eq!(files::read_position_file(&root, KEY), Some((5, 42)));

    // Book B adopts colliding KEY.
    let adoption = files::adopt_layout_config(&root, KEY, IDENTITY_B, &store);
    assert!(adoption.succeeded(), "Book B adoption must succeed");

    assert_eq!(
        files::read_position_file(&root, KEY),
        None,
        "Book A's saved position must be purged and not inherited by Book B"
    );
    let book = files::open_v2_book_dir(&root, KEY).expect("book dir");
    assert!(
        book.open_file_in_dir("POS.BIN", embedded_sdmmc::Mode::ReadOnly)
            .is_err(),
        "POS.BIN must be gone"
    );
}
