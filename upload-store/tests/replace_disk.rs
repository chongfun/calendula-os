//! Managed replacement against a real FAT filesystem.
//!
//! The rules are unit-tested in `proto::identity`. What is checked here is
//! the protocol: that an upload landing over an adopted copy leaves the
//! copy's id in place with the new size and digest, at whatever spelling the
//! install used; that a power cut at every write from the intent's
//! publication through the ledger's rewrite and the intent's clear leaves a
//! card whose recovery, in the order the identity design fixes, ends with
//! the same id naming whichever bytes the destination holds, under whichever
//! spelling; that a predecessor replaced on a computer between transactions
//! is still recognised; and that a destination holding neither legal
//! landing is refused rather than guessed at.
//!
//! The card model is the one `ledger_disk.rs` uses: a RAM image whose writes
//! can be cut off from a chosen point onward, or torn inside one sector.

use std::cell::RefCell;
use std::rc::Rc;

use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Directory, Mode, TimeSource, Timestamp, VolumeIdx,
    VolumeManager,
};
use proto::cache::{source_hash_at, CACHE_ROOT_DIR, CATALOG_FILE};
use proto::catalog::{
    catalog_record_book_id, encode_catalog_header, encode_catalog_placeholder_header,
    encode_catalog_record, CATALOG_HEADER_BYTES, CATALOG_RECORD_BYTES,
};
use proto::identity::{BookId, Landing, LedgerRecord, Predecessor, REPLACE_JOURNAL_MAGIC};
use proto::library_path::{BookRoot, LibraryPath};
use proto::source::{digest_of, CachedSourceDigest, SourceDigest};
use upload_store::install::{self, InstallError, StagedUpload};
use upload_store::ledger::{self, Assignment, Carry, Kept, LedgerFault, LEDGER_MAX_RECORDS};
use upload_store::replace::{self, PredecessorSeen, Recovery};
use upload_store::{library, reclaim};

const BLOCK_BYTES: usize = 512;
/// 16 MiB, the size every other disk test uses.
const DISK_BLOCKS: u32 = 32 * 1024;
const PART_START_BLOCK: u32 = 64;

struct RamDisk {
    data: RefCell<Vec<u8>>,
    blocks: u32,
    fail_writes_from: RefCell<Option<u32>>,
    writes_seen: RefCell<u32>,
    tear_write_at: RefCell<Option<(u32, usize)>>,
    written_blocks: RefCell<Vec<u32>>,
}

#[derive(Debug)]
struct DiskError;

impl core::fmt::Display for DiskError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "injected disk error")
    }
}

impl std::error::Error for DiskError {}

#[derive(Clone)]
struct SharedDisk(Rc<RamDisk>);

impl SharedDisk {
    fn image(&self) -> Vec<u8> {
        self.0.data.borrow().clone()
    }

    fn restore(&self, image: &[u8]) {
        self.0.data.borrow_mut().copy_from_slice(image);
    }

    /// Cut the power before the `n`th write from now on.
    fn cut_writes_from(&self, n: Option<u32>) {
        *self.0.writes_seen.borrow_mut() = 0;
        *self.0.fail_writes_from.borrow_mut() = n;
        *self.0.tear_write_at.borrow_mut() = None;
        self.0.written_blocks.borrow_mut().clear();
    }

    /// Tear the `n`th write from now on after `k` bytes of its first block,
    /// and cut the power there.
    fn tear_write_at(&self, n: u32, k: usize) {
        self.cut_writes_from(None);
        *self.0.tear_write_at.borrow_mut() = Some((n, k));
    }

    fn writes_seen(&self) -> u32 {
        *self.0.writes_seen.borrow()
    }

    fn written_blocks(&self) -> Vec<u32> {
        self.0.written_blocks.borrow().clone()
    }
}

impl BlockDevice for SharedDisk {
    type Error = DiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), DiskError> {
        let data = self.0.data.borrow();
        for (i, block) in blocks.iter_mut().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            block.copy_from_slice(&data[at..at + BLOCK_BYTES]);
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), DiskError> {
        let seen = {
            let mut seen = self.0.writes_seen.borrow_mut();
            *seen += 1;
            *seen
        };
        self.0.written_blocks.borrow_mut().push(start.0);
        if let Some((n, k)) = *self.0.tear_write_at.borrow() {
            if seen == n {
                let mut data = self.0.data.borrow_mut();
                let at = start.0 as usize * BLOCK_BYTES;
                data[at..at + k].copy_from_slice(&blocks[0][..k]);
                *self.0.fail_writes_from.borrow_mut() = Some(seen);
                return Err(DiskError);
            }
        }
        if let Some(from) = *self.0.fail_writes_from.borrow() {
            if seen >= from {
                return Err(DiskError);
            }
        }
        let mut data = self.0.data.borrow_mut();
        for (i, block) in blocks.iter().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            data[at..at + BLOCK_BYTES].copy_from_slice(&block[..]);
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, DiskError> {
        Ok(BlockCount(self.0.blocks))
    }
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
type CardFile<'a> = embedded_sdmmc::File<'a, SharedDisk, StaticTime, 8, 8, 1>;

fn format_disk(blocks: u32) -> Vec<u8> {
    let mut disk = vec![0u8; blocks as usize * BLOCK_BYTES];
    let part_blocks = blocks - PART_START_BLOCK;
    let mut partition = vec![0u8; part_blocks as usize * BLOCK_BYTES];
    fatfs::format_volume(
        std::io::Cursor::new(partition.as_mut_slice()),
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat16),
    )
    .expect("format");
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

fn new_card() -> SharedDisk {
    new_card_of(DISK_BLOCKS)
}

fn new_card_of(blocks: u32) -> SharedDisk {
    SharedDisk(Rc::new(RamDisk {
        data: RefCell::new(format_disk(blocks)),
        blocks,
        fail_writes_from: RefCell::new(None),
        writes_seen: RefCell::new(0),
        tear_write_at: RefCell::new(None),
        written_blocks: RefCell::new(Vec::new()),
    }))
}

fn open_mgr(disk: &SharedDisk) -> Mgr {
    VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000)
}

/// The card root and the shelf, made if it is not there yet.
fn open_dirs(mgr: &Mgr) -> (Dir<'_>, Dir<'_>) {
    let volume = mgr.open_volume(VolumeIdx(0)).expect("open volume");
    let raw_root = mgr
        .open_root_dir(volume.to_raw_volume())
        .expect("open root");
    let root = Directory::new(raw_root, mgr);
    if root.open_dir("BOOKS").is_err() {
        root.make_dir_in_dir("BOOKS").expect("make BOOKS");
    }
    let books = root.open_dir("BOOKS").expect("open BOOKS");
    (root, books)
}

/// A deterministic word source for minting. The tests assert on which ids
/// survive, not on entropy.
fn words() -> impl FnMut() -> u32 {
    let mut state = 0x2545_F491u32;
    move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        state
    }
}

fn read_exact(file: &CardFile<'_>, mut out: &mut [u8]) -> bool {
    while !out.is_empty() {
        let Ok(read) = file.read(out) else {
            return false;
        };
        if read == 0 {
            return false;
        }
        let rest = out;
        out = &mut rest[read..];
    }
    true
}

const BOOK: &str = "Dune.epub";
/// The same name as the card matches it, spelled another way.
const BOOK_RESPELLED: &str = "dune.epub";

/// A body of `len` bytes, distinct per `seed`. Big enough to span clusters.
fn body(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect()
}

/// Put a book on the shelf the way a computer does: under its long name,
/// with no record anywhere.
fn sideload(books: &Dir<'_>, name: &str, bytes: &[u8]) {
    let file = books.create_file_in_dir_lfn(name).expect("create book");
    file.write(bytes).expect("write book");
    file.close().expect("close book");
}

/// What the shelf holds under `name`, spelled exactly so, or `None`.
fn shelf_bytes(root: &Dir<'_>, name: &str) -> Option<Vec<u8>> {
    let path = LibraryPath::parse(name).unwrap();
    library::with_book_at(root, BookRoot::Library, &path, |dir, alias| {
        let mut alias_text = heapless::String::<12>::new();
        use core::fmt::Write as _;
        write!(alias_text, "{}", alias).unwrap();
        let file = dir
            .open_file_in_dir(alias_text.as_str(), Mode::ReadOnly)
            .expect("open book");
        let mut out = vec![0u8; file.length() as usize];
        assert!(read_exact(&file, &mut out));
        out
    })
    .expect("shelf readable")
}

/// Overwrite what the shelf holds under `name`, as a computer would while
/// the card was in it.
fn overwrite_shelf(root: &Dir<'_>, name: &str, bytes: &[u8]) {
    let path = LibraryPath::parse(name).unwrap();
    library::with_book_at(root, BookRoot::Library, &path, |dir, alias| {
        let mut alias_text = heapless::String::<12>::new();
        use core::fmt::Write as _;
        write!(alias_text, "{}", alias).unwrap();
        let file = dir
            .open_file_in_dir(alias_text.as_str(), Mode::ReadWriteCreateOrTruncate)
            .expect("open book");
        file.write(bytes).expect("write book");
        file.close().expect("close book");
    })
    .expect("shelf readable")
    .expect("the book is there to overwrite");
}

/// One row of the catalog a scan would write.
type Row<'a> = (BookRoot, &'a str, u32);

/// The production scan, as far as the ledger is concerned: rows into an
/// uncommitted `CATALOG.BIN`, the ledger opened and joined, the header
/// committed.
fn scan(
    root: &Dir<'_>,
    rows: &[Row<'_>],
) -> Result<(Assignment, Vec<Option<BookId>>), LedgerFault> {
    if root.open_dir(CACHE_ROOT_DIR).is_err() {
        root.make_dir_in_dir(CACHE_ROOT_DIR).expect("mkdir READER");
    }
    let cache_root = root.open_dir(CACHE_ROOT_DIR).expect("open READER");
    let file = cache_root
        .open_file_in_dir(CATALOG_FILE, Mode::ReadWriteCreateOrTruncate)
        .expect("create catalog");
    let mut header = [0u8; CATALOG_HEADER_BYTES];
    encode_catalog_placeholder_header(&mut header);
    file.write(&header).expect("placeholder");
    let mut record = [0u8; CATALOG_RECORD_BYTES];
    for (at, locator, size) in rows {
        encode_catalog_record(
            &mut record,
            locator,
            *at,
            locator,
            "",
            "",
            *size,
            source_hash_at(*at, locator, *size),
        );
        file.write(&record).expect("row");
    }
    let live = ledger::open(root)?;
    let mut scratch = vec![0u8; 16 * 1024];
    let assigned = ledger::assign_book_ids(
        root,
        &file,
        rows.len() as u16,
        &mut scratch,
        &mut words(),
        live,
    )?;
    encode_catalog_header(rows.len() as u16, &mut header);
    file.seek_from_start(0).map_err(|_| LedgerFault::Device)?;
    file.write(&header).map_err(|_| LedgerFault::Device)?;
    file.flush().map_err(|_| LedgerFault::Device)?;
    let mut ids = Vec::with_capacity(rows.len());
    for index in 0..rows.len() {
        file.seek_from_start((CATALOG_HEADER_BYTES + index * CATALOG_RECORD_BYTES) as u32)
            .expect("seek row");
        assert!(read_exact(&file, &mut record));
        ids.push(catalog_record_book_id(&record));
    }
    Ok((assigned, ids))
}

/// The ledger record with `id`: its exact locator, size, and what it says
/// about the bytes.
fn record_by_id(root: &Dir<'_>, id: BookId) -> Option<(String, u32, Option<CachedSourceDigest>)> {
    let live = ledger::open(root).expect("open ledger")?;
    let mut found = None;
    ledger::for_each_record(root, &live, &mut |_, record: &LedgerRecord<'_>| {
        if record.id == id {
            found = Some((record.locator.to_owned(), record.byte_size, record.source));
        }
        Ok(())
    })
    .expect("read ledger");
    found
}

/// The ledger record naming `name` on the shelf, spelled exactly so.
fn record_for(root: &Dir<'_>, name: &str) -> Option<(BookId, u32, Option<CachedSourceDigest>)> {
    let live = ledger::open(root).expect("open ledger")?;
    let mut found = None;
    ledger::for_each_record(root, &live, &mut |_, record: &LedgerRecord<'_>| {
        if found.is_none() && record.root == BookRoot::Library && record.locator == name {
            found = Some((record.id, record.byte_size, record.source));
        }
        Ok(())
    })
    .expect("read ledger");
    found
}

fn record_count(root: &Dir<'_>) -> usize {
    ledger::open(root)
        .expect("open ledger")
        .map_or(0, |live| live.count as usize)
}

/// Stage and install `bytes` under `name`, the way the upload session does.
/// `arm` runs between staging and the install, which is where a test cuts
/// the power.
fn upload(
    root: &Dir<'_>,
    books: &Dir<'_>,
    name: &str,
    bytes: &[u8],
    arm: impl FnOnce(),
) -> Result<Option<install::Landed>, InstallError> {
    let mut staged = StagedUpload::begin(root, books, name, None)?;
    staged.write(bytes)?;
    arm();
    staged.install(root, books, &mut words())
}

/// Recovery in the order the identity design fixes: the reclaim journal,
/// the install journal, then the library intent. Hands back what the
/// library step found.
fn recover(root: &Dir<'_>, books: &Dir<'_>) -> Recovery {
    reclaim::recover(root, Some(books)).expect("reclaim settles");
    let outcome = install::recover_installs(root, books);
    assert!(outcome.complete, "the install journal settles: {outcome:?}");
    replace::recover(root).expect("the library intent resolves or refuses")
}

fn digest_agrees(recorded: Option<CachedSourceDigest>, bytes: &[u8]) -> bool {
    recorded.is_some_and(|recorded| recorded.agrees_with(&digest_of(bytes)))
}

/// An upload over an adopted copy is the same copy: its id stays, and its
/// record takes the size and digest of what landed. A second upload over it
/// does the same again.
#[test]
fn a_replacement_keeps_the_id_and_records_what_landed() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let old = body(1, 3_000);
    sideload(&books, BOOK, &old);
    let (assigned, ids) = scan(&root, &[(BookRoot::Library, BOOK, old.len() as u32)]).unwrap();
    assert_eq!(assigned.minted, 1);
    let id = ids[0].unwrap();
    assert_eq!(record_for(&root, BOOK), Some((id, old.len() as u32, None)));

    let new = body(2, 4_100);
    let landed = upload(&root, &books, BOOK, &new, || {})
        .unwrap()
        .expect("the book lands");
    assert_eq!(landed.source, digest_of(&new));
    assert_eq!(shelf_bytes(&root, BOOK).as_deref(), Some(&new[..]));
    let (found, size, source) = record_for(&root, BOOK).expect("still a record");
    assert_eq!(found, id, "the copy kept its id");
    assert_eq!(size, new.len() as u32);
    assert!(digest_agrees(source, &new), "and took the new digest");
    assert_eq!(record_count(&root), 1, "one copy, one record");
    assert_eq!(replace::read(&root).unwrap(), None, "the intent is cleared");
    assert_eq!(
        install::read_intent(&root).unwrap(),
        install::IntentState::Absent
    );

    // The next scan sees the new size at the old place and does not mint.
    let (assigned, ids) = scan(&root, &[(BookRoot::Library, BOOK, new.len() as u32)]).unwrap();
    assert_eq!(assigned.matched, 1);
    assert_eq!(assigned.minted, 0);
    assert_eq!(ids[0], Some(id));

    let newer = body(3, 2_500);
    upload(&root, &books, BOOK, &newer, || {})
        .unwrap()
        .expect("lands again");
    let (found, size, source) = record_for(&root, BOOK).unwrap();
    assert_eq!(found, id);
    assert_eq!(size, newer.len() as u32);
    assert!(digest_agrees(source, &newer));
    assert_eq!(record_count(&root), 1);
}

/// The card matches names by FAT's rules and the ledger exactly. An upload
/// spelled another way replaces the copy the installer found under the
/// name, and the copy's record moves to the new spelling with its id.
#[test]
fn a_replacement_spelled_another_way_keeps_the_id_and_respells_the_record() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let old = body(1, 3_000);
    sideload(&books, BOOK, &old);
    let (_, ids) = scan(&root, &[(BookRoot::Library, BOOK, old.len() as u32)]).unwrap();
    let id = ids[0].unwrap();

    let new = body(2, 4_100);
    upload(&root, &books, BOOK_RESPELLED, &new, || {})
        .unwrap()
        .expect("lands");
    assert_eq!(shelf_bytes(&root, BOOK), None, "the old spelling is gone");
    assert_eq!(
        shelf_bytes(&root, BOOK_RESPELLED).as_deref(),
        Some(&new[..]),
        "the shelf spells it as typed"
    );
    let (locator, size, source) = record_by_id(&root, id).expect("the id survives");
    assert_eq!(locator, BOOK_RESPELLED, "and its record spells it the same");
    assert_eq!(size, new.len() as u32);
    assert!(digest_agrees(source, &new));
    assert_eq!(
        record_count(&root),
        1,
        "one copy, one record, not a stale twin"
    );
    let (assigned, ids) = scan(
        &root,
        &[(BookRoot::Library, BOOK_RESPELLED, new.len() as u32)],
    )
    .unwrap();
    assert_eq!(assigned.matched, 1);
    assert_eq!(assigned.minted, 0);
    assert_eq!(ids[0], Some(id));
}

/// An upload to a name nothing holds adopts the copy with its digest, so
/// the next scan matches it and does not mint.
#[test]
fn a_fresh_upload_is_adopted_with_its_digest() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let new = body(5, 2_048);
    upload(&root, &books, BOOK, &new, || {})
        .unwrap()
        .expect("lands");
    let (id, size, source) = record_for(&root, BOOK).expect("adopted at install");
    assert_eq!(size, new.len() as u32);
    assert!(digest_agrees(source, &new));
    assert_eq!(replace::read(&root).unwrap(), None);

    let (assigned, ids) = scan(&root, &[(BookRoot::Library, BOOK, new.len() as u32)]).unwrap();
    assert_eq!(assigned.matched, 1);
    assert_eq!(assigned.minted, 0);
    assert_eq!(ids[0], Some(id));
}

/// What a cut replacement must look like once recovered: the copy's id
/// names whichever body the destination holds, under whichever spelling it
/// holds it, with a size that agrees and a digest that agrees or was left
/// as it was.
struct Expectation<'a> {
    id: BookId,
    old_name: &'a str,
    old: &'a [u8],
    /// What the ledger said of the old copy's bytes before the upload.
    old_source: Option<CachedSourceDigest>,
    new_name: &'a str,
    new: &'a [u8],
}

/// Check the card after a recovered cut, and say which body it held.
fn check_recovered(root: &Dir<'_>, expected: &Expectation<'_>, label: &str) -> bool {
    assert_eq!(
        replace::read(root).unwrap(),
        None,
        "{label}: the intent is cleared"
    );
    assert_eq!(
        install::read_intent(root).unwrap(),
        install::IntentState::Absent,
        "{label}"
    );
    let under_new = shelf_bytes(root, expected.new_name);
    let under_old = if expected.old_name == expected.new_name {
        None
    } else {
        shelf_bytes(root, expected.old_name)
    };
    let (spelled, held) = match (under_new, under_old) {
        (Some(held), None) => (expected.new_name, held),
        (None, Some(held)) => (expected.old_name, held),
        (new, old) => panic!("{label}: exactly one spelling holds a file: {new:?} {old:?}"),
    };
    let (locator, size, source) =
        record_by_id(root, expected.id).unwrap_or_else(|| panic!("{label}: the id survives"));
    assert_eq!(
        locator, spelled,
        "{label}: the record spells the place as the card does"
    );
    assert_eq!(
        size,
        held.len() as u32,
        "{label}: the record's size is the file's"
    );
    assert_eq!(record_count(root), 1, "{label}: one copy, one record");
    let landed_new = held == expected.new;
    if landed_new {
        assert!(
            digest_agrees(source, expected.new),
            "{label}: the new digest"
        );
    } else {
        assert_eq!(held, expected.old, "{label}: one of the two bodies");
        assert_eq!(
            source, expected.old_source,
            "{label}: the record's digest as it was"
        );
    }
    let (assigned, ids) = scan(root, &[(BookRoot::Library, spelled, held.len() as u32)]).unwrap();
    assert_eq!(assigned.matched, 1, "{label}");
    assert_eq!(assigned.minted, 0, "{label}");
    assert_eq!(ids[0], Some(expected.id), "{label}");
    landed_new
}

/// A power cut at every write from before the intent is published through
/// the install, the ledger's rewrite and the intent's clear, for a base
/// prepared by `prepare`, which returns the copy's id and what the ledger
/// says of its bytes. `old_name` is the spelling on the shelf; the upload is
/// spelled `new_name`. Both sides of the swap must be reached.
fn sweep_replacement(
    prepare: impl Fn(&Dir<'_>, &Dir<'_>) -> (BookId, Option<CachedSourceDigest>),
    old_name: &str,
    old: &[u8],
    new_name: &str,
    new: &[u8],
) {
    let disk = new_card();
    let (base, id, old_source) = {
        let mgr = open_mgr(&disk);
        let (root, books) = open_dirs(&mgr);
        let (id, old_source) = prepare(&root, &books);
        (disk.image(), id, old_source)
    };
    let expected = Expectation {
        id,
        old_name,
        old,
        old_source,
        new_name,
        new,
    };

    let uncut = {
        let mgr = open_mgr(&disk);
        let (root, books) = open_dirs(&mgr);
        upload(&root, &books, new_name, new, || disk.cut_writes_from(None))
            .unwrap()
            .expect("lands");
        disk.writes_seen()
    };
    assert!(
        uncut > 8,
        "a replacement that takes {uncut} writes proves little"
    );

    let mut saw = [false; 2];
    for cut in 1..=uncut {
        disk.restore(&base);
        {
            let mgr = open_mgr(&disk);
            let (root, books) = open_dirs(&mgr);
            let _ = upload(&root, &books, new_name, new, || {
                disk.cut_writes_from(Some(cut))
            });
            disk.cut_writes_from(None);
        }
        // Power is back.
        let mgr = open_mgr(&disk);
        let (root, books) = open_dirs(&mgr);
        let label = format!("cut at write {cut}");
        let recovered = recover(&root, &books);
        assert_ne!(recovered, Recovery::Refused, "{label}");
        let landed_new = check_recovered(&root, &expected, &label);
        saw[usize::from(landed_new)] = true;
    }
    assert_eq!(saw, [true; 2], "cuts landed on both sides of the swap");
}

#[test]
fn a_power_cut_anywhere_in_a_replacement_keeps_the_id_and_a_consistent_record() {
    let old = body(1, 3_000);
    let new = body(2, 4_100);
    sweep_replacement(
        |root, books| {
            sideload(books, BOOK, &old);
            let (_, ids) = scan(root, &[(BookRoot::Library, BOOK, old.len() as u32)]).unwrap();
            (ids[0].unwrap(), None)
        },
        BOOK,
        &old,
        BOOK,
        &new,
    );
}

/// A rollback puts the predecessor back under the spelling that was typed,
/// so a cut replacement spelled another way can leave the old bytes under
/// the new name. The record follows the file either way.
#[test]
fn a_power_cut_in_a_replacement_spelled_another_way_keeps_the_id_under_either_spelling() {
    let old = body(1, 3_000);
    let new = body(2, 4_100);
    sweep_replacement(
        |root, books| {
            sideload(books, BOOK, &old);
            let (_, ids) = scan(root, &[(BookRoot::Library, BOOK, old.len() as u32)]).unwrap();
            (ids[0].unwrap(), None)
        },
        BOOK,
        &old,
        BOOK_RESPELLED,
        &new,
    );
}

/// Between transactions a computer may replace a book with another of the
/// same size, which the ledger cannot see. The next upload over it must
/// still recover from any cut: the ledger's digest is evidence about bytes
/// that are gone, and the intent does not carry it as the predecessor's.
#[test]
fn a_predecessor_replaced_on_a_computer_is_still_recognised_after_a_cut() {
    let uploaded = body(1, 3_000);
    let swapped_in = body(7, 3_000);
    let new = body(2, 4_100);
    assert_eq!(
        uploaded.len(),
        swapped_in.len(),
        "same size, the case the ledger cannot see"
    );
    sweep_replacement(
        |root, books| {
            upload(root, books, BOOK, &uploaded, || {})
                .unwrap()
                .expect("lands");
            let (id, _, source) = record_for(root, BOOK).unwrap();
            assert!(
                digest_agrees(source, &uploaded),
                "the ledger recorded the upload"
            );
            overwrite_shelf(root, BOOK, &swapped_in);
            // The record stands, describing bytes that are no longer there.
            (id, source)
        },
        BOOK,
        &swapped_in,
        BOOK,
        &new,
    );
}

/// The intent's own writes, torn inside the sector, at every length that
/// leaves a partial entry. Two slots keep the entry before, and recovery
/// ends the same way.
#[test]
fn a_torn_intent_write_still_resolves() {
    let disk = new_card();
    let old = body(1, 3_000);
    let new = body(2, 4_100);
    let (base, id) = {
        let mgr = open_mgr(&disk);
        let (root, books) = open_dirs(&mgr);
        sideload(&books, BOOK, &old);
        let (_, ids) = scan(&root, &[(BookRoot::Library, BOOK, old.len() as u32)]).unwrap();
        (disk.image(), ids[0].unwrap())
    };
    let expected = Expectation {
        id,
        old_name: BOOK,
        old: &old,
        old_source: None,
        new_name: BOOK,
        new: &new,
    };

    // Which writes of an uncut replacement land on the intent's blocks: the
    // journal's creation, the publication and the clear. An entry spans two
    // sectors, so only the first of each slot carries the magic.
    let intent_writes: Vec<u32> = {
        let mgr = open_mgr(&disk);
        let (root, books) = open_dirs(&mgr);
        upload(&root, &books, BOOK, &new, || disk.cut_writes_from(None))
            .unwrap()
            .expect("lands");
        let image = disk.image();
        let blocks: Vec<u32> = (0..DISK_BLOCKS)
            .filter(|block| {
                let at = *block as usize * BLOCK_BYTES;
                image[at..at + 4] == REPLACE_JOURNAL_MAGIC
            })
            .collect();
        assert_eq!(blocks.len(), 2, "both slots hold an entry by now");
        disk.written_blocks()
            .iter()
            .enumerate()
            .filter(|(_, block)| blocks.contains(block))
            .map(|(index, _)| index as u32 + 1)
            .collect()
    };
    assert!(
        intent_writes.len() >= 2,
        "a publication and a clear at least: {intent_writes:?}"
    );

    for (transition, write) in intent_writes.iter().enumerate() {
        for landed in [1usize, 4, 5, 8, 24, 100, 300, 511] {
            disk.restore(&base);
            {
                let mgr = open_mgr(&disk);
                let (root, books) = open_dirs(&mgr);
                let _ = upload(&root, &books, BOOK, &new, || {
                    disk.tear_write_at(*write, landed)
                });
                disk.cut_writes_from(None);
            }
            let mgr = open_mgr(&disk);
            let (root, books) = open_dirs(&mgr);
            let label = format!("transition {transition}, {landed} bytes landed");
            assert_ne!(recover(&root, &books), Recovery::Refused, "{label}");
            check_recovered(&root, &expected, &label);
        }
    }
}

/// A destination holding neither the bytes that were meant to land nor a
/// predecessor whose digest was read in this session is nothing this can
/// resolve: the intent stands, the ledger is left alone, and no other change
/// to the shelf begins until the card is looked at. Putting the predecessor
/// back is looking at it.
#[test]
fn a_destination_holding_neither_landing_is_refused_until_looked_at() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let old = body(1, 3_000);
    upload(&root, &books, BOOK, &old, || {})
        .unwrap()
        .expect("lands");
    let (id, _, source) = record_for(&root, BOOK).unwrap();
    assert!(digest_agrees(source, &old));

    // An intent whose caller read the predecessor, published and never acted
    // on. Then a stranger arrives at the place, which the sole-writer
    // contract forbids and which this therefore does not read as either.
    let new = body(2, 4_100);
    let standing = replace::begin(
        &root,
        BookRoot::Library,
        BOOK,
        Some(PredecessorSeen {
            locator: BOOK,
            byte_size: old.len() as u32,
            digest: Some(digest_of(&old)),
        }),
        digest_of(&new),
        &mut words(),
    )
    .unwrap();
    assert_eq!(standing.id, id);
    assert!(matches!(standing.predecessor, Predecessor::Known(_)));
    let stranger = body(9, 3_000);
    overwrite_shelf(&root, BOOK, &stranger);

    assert_eq!(recover(&root, &books), Recovery::Refused);
    assert!(replace::read(&root).unwrap().is_some(), "the intent stands");
    let (found, size, source) = record_for(&root, BOOK).unwrap();
    assert_eq!((found, size), (id, old.len() as u32));
    assert!(digest_agrees(source, &old), "the ledger is untouched");
    assert!(
        matches!(
            StagedUpload::begin(&root, &books, "Other.epub", None).err(),
            Some(InstallError::Busy)
        ),
        "no other change to the shelf begins beside it"
    );
    assert_eq!(
        replace::begin(
            &root,
            BookRoot::Library,
            BOOK,
            None,
            digest_of(&new),
            &mut words()
        )
        .err(),
        Some(LedgerFault::Busy)
    );

    // The predecessor put back is the old landing.
    overwrite_shelf(&root, BOOK, &old);
    assert_eq!(recover(&root, &books), Recovery::Settled(Landing::Old));
    assert_eq!(replace::read(&root).unwrap(), None);
    upload(&root, &books, "Other.epub", &body(4, 1_000), || {})
        .unwrap()
        .expect("the shelf takes changes again");
}

/// The installer does not read the predecessor, so its intent says
/// "unknown" whatever the ledger recorded of it, and a stranger at the place
/// under a standing intent is read as the predecessor: the sole-writer
/// contract is what makes that sound, and the ledger's digest is left as
/// the evidence it is.
#[test]
fn the_installer_does_not_promote_the_ledgers_digest_to_the_predecessors() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let old = body(1, 3_000);
    upload(&root, &books, BOOK, &old, || {})
        .unwrap()
        .expect("lands");
    let (id, _, source) = record_for(&root, BOOK).unwrap();
    assert!(digest_agrees(source, &old));

    let new = body(2, 4_100);
    let standing = replace::begin(
        &root,
        BookRoot::Library,
        BOOK,
        Some(PredecessorSeen {
            locator: BOOK,
            byte_size: old.len() as u32,
            digest: None,
        }),
        digest_of(&new),
        &mut words(),
    )
    .unwrap();
    assert_eq!(standing.id, id, "the id is the record's");
    assert_eq!(
        standing.predecessor,
        Predecessor::Unknown,
        "but the digest is not the record's"
    );
    overwrite_shelf(&root, BOOK, &body(9, 3_000));
    assert_eq!(recover(&root, &books), Recovery::Settled(Landing::Old));
    let (found, size, source) = record_for(&root, BOOK).unwrap();
    assert_eq!((found, size), (id, old.len() as u32));
    assert!(
        digest_agrees(source, &old),
        "the ledger's evidence is left as it was"
    );
}

/// An intent for a place nothing held, whose install rolled back or was cut
/// before it began, resolves to the old landing on an empty destination and
/// leaves no record behind.
#[test]
fn where_nothing_stood_nothing_standing_is_the_old_landing() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let new = body(2, 4_100);
    replace::begin(
        &root,
        BookRoot::Library,
        BOOK,
        None,
        digest_of(&new),
        &mut words(),
    )
    .unwrap();
    assert_eq!(recover(&root, &books), Recovery::Settled(Landing::Old));
    assert_eq!(record_for(&root, BOOK), None);
    assert_eq!(record_count(&root), 0);

    // But a stranger where nothing stood is not a landing.
    replace::begin(
        &root,
        BookRoot::Library,
        BOOK,
        None,
        digest_of(&new),
        &mut words(),
    )
    .unwrap();
    sideload(&books, BOOK, &body(9, 500));
    assert_eq!(recover(&root, &books), Recovery::Refused);
}

/// The new digest is decisive whatever was recorded about the predecessor.
/// Recovery that finds the new bytes on the shelf publishes the record even
/// when the install journal is long gone.
#[test]
fn the_new_bytes_at_the_destination_are_published_under_the_old_id() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let old = body(1, 3_000);
    sideload(&books, BOOK, &old);
    let (_, ids) = scan(&root, &[(BookRoot::Library, BOOK, old.len() as u32)]).unwrap();
    let id = ids[0].unwrap();

    let new = body(2, 4_100);
    replace::begin(
        &root,
        BookRoot::Library,
        BOOK,
        Some(PredecessorSeen {
            locator: BOOK,
            byte_size: old.len() as u32,
            digest: None,
        }),
        digest_of(&new),
        &mut words(),
    )
    .unwrap();
    overwrite_shelf(&root, BOOK, &new);
    assert_eq!(recover(&root, &books), Recovery::Settled(Landing::New));
    let (found, size, source) = record_for(&root, BOOK).unwrap();
    assert_eq!(found, id);
    assert_eq!(size, new.len() as u32);
    assert!(digest_agrees(source, &new));
    assert_eq!(replace::read(&root).unwrap(), None);
    assert_eq!(recover(&root, &books), Recovery::Nothing);
}

/// A ledger with no room for one more record lets a missing copy go to make
/// it, and refuses cleanly before anything is journalled when there is none
/// to let go of. A 64 MiB card, since the records alone are 21 MiB.
#[test]
fn a_full_ledger_yields_a_missing_copy_or_refuses_before_the_install_begins() {
    let disk = new_card_of(128 * 1024);
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    // Every record a live copy but one, which the last scan found missing.
    let ids: Vec<BookId> = (0..LEDGER_MAX_RECORDS)
        .map(|index| {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&(index as u64 + 1).to_le_bytes());
            BookId::from_bytes(bytes).unwrap()
        })
        .collect();
    let missing_index = 777usize;
    ledger::write_generation(
        &root,
        None,
        &mut |_, record| Carry::Keep(Kept::of(record)),
        |writer| {
            for (index, id) in ids.iter().enumerate() {
                let mut locator = heapless::String::<32>::new();
                use core::fmt::Write as _;
                write!(locator, "B{index}.epub").unwrap();
                writer.append(&LedgerRecord {
                    id: *id,
                    root: BookRoot::Library,
                    locator: locator.as_str(),
                    byte_size: 1_000,
                    misses: u8::from(index == missing_index),
                    source: None,
                })?;
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(record_count(&root), LEDGER_MAX_RECORDS);

    // A fresh upload lands and is adopted: the missing copy made room.
    let new = body(2, 2_048);
    upload(&root, &books, BOOK, &new, || {})
        .unwrap()
        .expect("lands");
    let (fresh, size, source) = record_for(&root, BOOK).expect("adopted");
    assert_eq!(size, new.len() as u32);
    assert!(digest_agrees(source, &new));
    assert!(!ids.contains(&fresh));
    assert_eq!(record_count(&root), LEDGER_MAX_RECORDS);
    assert_eq!(
        record_by_id(&root, ids[missing_index]),
        None,
        "the missing copy's record made the room"
    );
    assert!(replace::read(&root).unwrap().is_none());

    // With every record a live copy, a second fresh upload is refused before
    // any journal is written, and the shelf and the ledger are left as they
    // were.
    let refused = upload(&root, &books, "Other.epub", &body(3, 1_500), || {});
    assert!(
        matches!(refused, Err(InstallError::Ledger)),
        "refused by the ledger: {refused:?}"
    );
    assert_eq!(
        replace::read(&root).unwrap(),
        None,
        "no intent was published"
    );
    assert_eq!(
        install::read_intent(&root).unwrap(),
        install::IntentState::Absent,
        "and no install journal"
    );
    assert_eq!(shelf_bytes(&root, "Other.epub"), None);
    assert_eq!(record_count(&root), LEDGER_MAX_RECORDS);
    // A replacement of an adopted copy still goes through: no record is
    // added.
    upload(&root, &books, BOOK, &body(4, 3_000), || {})
        .unwrap()
        .expect("a replacement needs no room");
    assert_eq!(record_count(&root), LEDGER_MAX_RECORDS);
    assert_eq!(
        record_by_id(&root, fresh).map(|(_, size, _)| size),
        Some(3_000)
    );
}

/// A digest the install verified travels into the record; a stored digest
/// that disagrees with the file is what `SourceDigest` versus its cached form
/// exists to keep apart, and the record's type says which it holds.
#[test]
fn the_recorded_digest_is_evidence_that_agrees_with_the_bytes() {
    let bytes = body(7, 999);
    let fresh: SourceDigest = digest_of(&bytes);
    let cached = CachedSourceDigest::new(fresh);
    assert!(cached.agrees_with(&fresh));
    assert!(!cached.agrees_with(&digest_of(&body(8, 999))));
}
