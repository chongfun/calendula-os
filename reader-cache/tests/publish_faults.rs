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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use display::font::FontStyle;
use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Directory, TimeSource, Timestamp, VolumeIdx,
    VolumeManager,
};
use proto::cache::{
    BookV2SectionRecord, CoverCacheHeader, CACHE_COVER_FILE, COVER_BYTES, COVER_HEIGHT,
    COVER_STRIDE, COVER_WIDTH,
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
fn sections_still_on_card(root: &Dir<'_>, count: usize) -> bool {
    (0..count).all(|n| {
        files::with_v2_sections_dir(root, KEY, |sections| {
            let Some(sections) = sections else {
                return false;
            };
            let mut name = heapless::String::<8>::new();
            use core::fmt::Write;
            let _ = write!(&mut name, "S{n:03}.BIN");
            sections
                .open_file_in_dir(name.as_str(), embedded_sdmmc::Mode::ReadOnly)
                .is_ok()
        })
    })
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
        sections_still_on_card(&root, 3),
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
        !sections_still_on_card(&root, 3),
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
        sections_still_on_card(&root, 3),
        "a clean publish must not delete anything"
    );
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
        sections_still_on_card(&root, sections),
        "a refused index write must not take the reader's sections with it"
    );
    assert!(
        store.covers_global_page(0, reader_page),
        "the reader's page must still be resident after the refused write"
    );
}
