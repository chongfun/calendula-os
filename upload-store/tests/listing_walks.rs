//! One resolution serves a whole listing, however many pages it reads.
//!
//! Its own test binary on purpose. `walk_probe`'s counters are
//! process-global, so a count read here would otherwise carry whatever
//! another file's walk had added to it.
//!
//! Cargo runs one file's tests across threads too, and that is the same
//! hazard one level down: a test reading the counters sees the walks of
//! whichever test is running beside it. So every test here takes
//! [`one_at_a_time`] first, including the ones that read no counter, since
//! their walks are what the others would miscount.
#![cfg(feature = "bench-selftest")]

use std::cell::RefCell;
use std::rc::Rc;

use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx, TimeSource, Timestamp};
use embedded_sdmmc::{Directory, VolumeIdx, VolumeManager};
use proto::library_path::BookRoot;
use proto::library_path::LibraryPath;
use upload_store::library::{entry_in, open_listing, walk_probe, LibraryRow};

const BLOCK_BYTES: usize = 512;
// Large enough that fatfs picks FAT16, which the driver supports.
const DISK_BLOCKS: u32 = 32 * 1024;
const PART_START_BLOCK: u32 = 64;

struct RamDisk {
    data: RefCell<Vec<u8>>,
    /// Reads fail from this one onward, so a directory walk can fail the way
    /// a card does rather than the way a missing name does.
    fail_reads_from: RefCell<Option<u32>>,
    reads_seen: RefCell<u32>,
}

#[derive(Debug)]
struct DiskError;

impl core::fmt::Display for DiskError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "disk error")
    }
}

impl std::error::Error for DiskError {}

#[derive(Clone)]
struct SharedDisk(Rc<RamDisk>);

impl BlockDevice for SharedDisk {
    type Error = DiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), DiskError> {
        {
            let mut seen = self.0.reads_seen.borrow_mut();
            *seen += 1;
            if self
                .0
                .fail_reads_from
                .borrow()
                .is_some_and(|at| *seen >= at)
            {
                return Err(DiskError);
            }
        }
        let data = self.0.data.borrow();
        for (i, block) in blocks.iter_mut().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            block.copy_from_slice(&data[at..at + BLOCK_BYTES]);
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), DiskError> {
        let mut data = self.0.data.borrow_mut();
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

struct StaticTime;

impl TimeSource for StaticTime {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 55,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

type Mgr = VolumeManager<SharedDisk, StaticTime, 8, 8, 1>;
type Dir<'a> = Directory<'a, SharedDisk, StaticTime, 8, 8, 1>;

fn format_disk() -> Vec<u8> {
    let mut data = vec![0u8; DISK_BLOCKS as usize * BLOCK_BYTES];
    data[446 + 4] = 0x06;
    data[446 + 8..446 + 12].copy_from_slice(&PART_START_BLOCK.to_le_bytes());
    let sectors = DISK_BLOCKS - PART_START_BLOCK;
    data[446 + 12..446 + 16].copy_from_slice(&sectors.to_le_bytes());
    data[510] = 0x55;
    data[511] = 0xAA;
    let part_start = PART_START_BLOCK as usize * BLOCK_BYTES;
    let part_len = sectors as usize * BLOCK_BYTES;
    let cursor = std::io::Cursor::new(&mut data[part_start..part_start + part_len]);
    fatfs::format_volume(cursor, fatfs::FormatVolumeOptions::new()).expect("format");
    data
}

fn new_card() -> SharedDisk {
    SharedDisk(Rc::new(RamDisk {
        data: RefCell::new(format_disk()),
        fail_reads_from: RefCell::new(None),
        reads_seen: RefCell::new(0),
    }))
}

fn open_mgr(disk: SharedDisk) -> Mgr {
    VolumeManager::new_with_limits(disk, StaticTime, 7000)
}

fn open_root(mgr: &Mgr) -> Dir<'_> {
    let volume = mgr.open_volume(VolumeIdx(0)).expect("volume");
    let raw = volume.to_raw_volume();
    let raw_root = mgr.open_root_dir(raw).expect("root");
    Directory::new(raw_root, mgr)
}

/// Descend by long name, since making a directory hands back nothing to
/// descend through.
fn child<'a>(dir: &Dir<'a>, name: &str) -> Dir<'a> {
    let entry = entry_in(dir, name).expect("read").expect("present");
    dir.open_dir(entry.alias).expect("open")
}

/// Serializes this file's tests. Held for the whole of each one.
///
/// A poisoned lock is passed through rather than unwrapped: one test that
/// panicked has failed already, and taking the rest down with it hides
/// whatever else was wrong.
fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn path(text: &str) -> LibraryPath {
    LibraryPath::parse(text).expect("parse")
}

/// The guard is entry counts rather than time. Resolving a path scans every
/// entry of each directory on the way, so a second resolution shows up as
/// resolve entries climbing with each page read. Before this, entering a
/// folder resolved it for the count and again for the page, and walking a
/// folder a window at a time resolved it once per window.
///
/// The rows each page returns are asserted too, so the test stands on its
/// own: a listing that resolved once and paged wrongly would otherwise read
/// as a pass here and fail only in the wrapper tests next door.
#[test]
fn a_listing_resolves_its_path_once_however_many_pages_it_reads() {
    let _serial = one_at_a_time();
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    root.make_dir_in_dir_lfn("BOOKS").expect("mkdir");
    let books = child(&root, "BOOKS");
    books.make_dir_in_dir_lfn("Fiction").expect("mkdir");
    let fiction = child(&books, "Fiction");
    for n in 0..6 {
        let file = fiction
            .create_file_in_dir_lfn(&format!("Book {n}.epub"))
            .expect("create");
        file.write(b"x").expect("write");
        file.close().expect("close");
    }

    let _ = walk_probe::take();
    let listing = open_listing(&root, &path("Fiction"))
        .expect("read")
        .expect("a directory");
    let (_, resolved_opening, _) = walk_probe::take();
    assert!(
        resolved_opening > 0,
        "opening the listing walks the card to the folder",
    );

    let counts = listing.counts(&root).expect("count");
    assert_eq!(counts.total(), 6, "the fixture is what the walk reads");
    let mut window: [LibraryRow; 2] = Default::default();
    let mut seen = Vec::new();
    let mut cursor = None;
    for skip in [0usize, 2, 4] {
        let filled = listing
            .page(&root, counts, skip, &mut window, &mut cursor)
            .expect("page")
            .expect("rows");
        assert_eq!(filled, 2, "each page of two is full");
        seen.extend(
            window
                .iter()
                .take(filled)
                .map(|row| row.child.name.as_str().to_string()),
        );
    }
    let (_, resolved_after, iterated_after) = walk_probe::take();

    assert_eq!(
        seen,
        vec![
            "Book 0.epub",
            "Book 1.epub",
            "Book 2.epub",
            "Book 3.epub",
            "Book 4.epub",
            "Book 5.epub",
        ],
        "three pages off one listing walk the folder once through, in order",
    );
    assert_eq!(
        resolved_after, 0,
        "a count and three pages off an open listing resolve nothing",
    );
    assert!(
        iterated_after > 0,
        "they do read the folder, so the counter is live",
    );
}

/// The same, two components down, where a resolution costs two directory
/// scans rather than one and a per-page resolution would cost the most.
#[test]
fn a_listing_two_folders_down_resolves_once_as_well() {
    let _serial = one_at_a_time();
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    root.make_dir_in_dir_lfn("BOOKS").expect("mkdir");
    let books = child(&root, "BOOKS");
    books.make_dir_in_dir_lfn("Fiction").expect("mkdir");
    let fiction = child(&books, "Fiction");
    fiction.make_dir_in_dir_lfn("Space Opera").expect("mkdir");
    let nested = child(&fiction, "Space Opera");
    for name in ["Dune.epub", "Foundation.epub", "Hyperion.epub"] {
        let file = nested.create_file_in_dir_lfn(name).expect("create");
        file.write(b"x").expect("write");
        file.close().expect("close");
    }

    let _ = walk_probe::take();
    let listing = open_listing(&root, &path("Fiction/Space Opera"))
        .expect("read")
        .expect("a directory");
    let (_, resolved_opening, _) = walk_probe::take();
    assert!(
        resolved_opening > 0,
        "two components is two directory scans, and both happen here",
    );

    let counts = listing.counts(&root).expect("count");
    assert_eq!(counts.total(), 3);
    let mut window: [LibraryRow; 2] = Default::default();
    let mut cursor = None;
    let first = listing
        .page(&root, counts, 0, &mut window, &mut cursor)
        .expect("page")
        .expect("rows");
    assert_eq!(first, 2);
    let names: Vec<&str> = window
        .iter()
        .take(first)
        .map(|row| row.child.name.as_str())
        .collect();
    assert_eq!(names, vec!["Dune.epub", "Foundation.epub"]);
    let second = listing
        .page(&root, counts, 2, &mut window, &mut cursor)
        .expect("page")
        .expect("rows");
    assert_eq!(second, 1);
    assert_eq!(window[0].child.name.as_str(), "Hyperion.epub");

    let (_, resolved_after, _) = walk_probe::take();
    assert_eq!(
        resolved_after, 0,
        "depth does not make a page resolve again",
    );
}

/// The library root's own listing, which is the branch that reads two
/// directories: the shelf for its books and folders, and the card root for
/// the loose EPUBs copied on before the shelf existed.
///
/// Held open, the shelf is the folder being listed, so nothing is descended
/// into and the card root stays the handle the caller passed in.
#[test]
fn the_library_root_lists_both_its_roots_from_one_open_listing() {
    let _serial = one_at_a_time();
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    let loose = root.create_file_in_dir_lfn("Loose.epub").expect("create");
    loose.write(b"x").expect("write");
    loose.close().expect("close");
    root.make_dir_in_dir_lfn("BOOKS").expect("mkdir");
    let books = child(&root, "BOOKS");
    let shelved = books
        .create_file_in_dir_lfn("Shelved.epub")
        .expect("create");
    shelved.write(b"x").expect("write");
    shelved.close().expect("close");
    books.make_dir_in_dir_lfn("Fiction").expect("mkdir");

    let listing = open_listing(&root, &path("/"))
        .expect("read")
        .expect("the library root");
    let counts = listing.counts(&root).expect("count");
    assert_eq!(
        (counts.shelf_books, counts.root_books, counts.shelf_folders),
        (1, 1, 1),
        "one shelved book, one loose at the card root, one folder",
    );

    let _ = walk_probe::take();
    let mut window: [LibraryRow; 3] = Default::default();
    let filled = listing
        .page(&root, counts, 0, &mut window, &mut None)
        .expect("page")
        .expect("rows");
    let (_, resolved, _) = walk_probe::take();
    assert_eq!(resolved, 0, "the root listing resolves nothing to page");
    assert_eq!(filled, 3);
    let rows: Vec<(&str, bool, BookRoot)> = window
        .iter()
        .map(|row| (row.child.name.as_str(), row.child.is_dir, row.at))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("Shelved.epub", false, BookRoot::Library),
            ("Loose.epub", false, BookRoot::CardRoot),
            ("Fiction", true, BookRoot::Library),
        ],
        "the shelf's books, then the card root's, then the shelf's folders",
    );
}

/// A card with no shelf still has a library, made of whatever sits loose at
/// its root. The listing holds no shelf handle at all, so both halves have
/// to answer from the card root alone.
#[test]
fn a_card_with_no_shelf_still_lists_its_loose_books() {
    let _serial = one_at_a_time();
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    let loose = root.create_file_in_dir_lfn("Loose.epub").expect("create");
    loose.write(b"x").expect("write");
    loose.close().expect("close");

    let listing = open_listing(&root, &path("/"))
        .expect("read")
        .expect("the library root");
    let counts = listing.counts(&root).expect("count");
    assert_eq!(
        (counts.shelf_books, counts.root_books, counts.shelf_folders),
        (0, 1, 0),
        "nothing shelved, one loose book",
    );

    let mut window: [LibraryRow; 2] = Default::default();
    let filled = listing
        .page(&root, counts, 0, &mut window, &mut None)
        .expect("page")
        .expect("rows");
    assert_eq!(filled, 1);
    assert_eq!(window[0].child.name.as_str(), "Loose.epub");
    assert_eq!(window[0].at, BookRoot::CardRoot);
}

/// Below the shelf there is nothing to list on a card that has none, and
/// saying so is not the same as saying the card would not answer.
#[test]
fn a_card_with_no_shelf_has_no_folder_below_it() {
    let _serial = one_at_a_time();
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    assert!(
        open_listing(&root, &path("Fiction"))
            .expect("read")
            .is_none(),
        "a path under a shelf that is not there is an absence, not a fault",
    );
}
