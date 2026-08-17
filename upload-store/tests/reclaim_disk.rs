//! Reading the reclaim journal off a real FAT filesystem.
//!
//! The record format and slot arithmetic are tested in the module itself.
//! What is checked here is the part that only shows up against a card: that
//! finding out whether a reclaim is outstanding does not change the card, and
//! that the three answers it can give — nothing here, something here I cannot
//! read, and a record — stay distinct.

use std::cell::RefCell;
use std::rc::Rc;

use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Directory, Mode, TimeSource, Timestamp, VolumeIdx,
    VolumeManager,
};
use upload_store::reclaim::{
    self, Batch, Journal, Place, ReclaimError, Slot, JOURNAL_BYTES, SLOT_BYTES,
};

const BLOCK_BYTES: usize = 512;
const DISK_BLOCKS: u32 = 32 * 1024;
const PART_START_BLOCK: u32 = 64;

struct RamDisk {
    data: RefCell<Vec<u8>>,
    /// Reads of exactly this block fail, standing in for a card that has
    /// stopped answering for one sector. Exact rather than a range so a test
    /// can take out one slot of the journal and leave everything it took to
    /// get there -- the directory, the other slot -- still readable.
    fail_block: RefCell<Option<u32>>,
    /// Writes from this number onward do nothing and report failure.
    ///
    /// This is the power cut. Everything before the cut reached the card and
    /// everything after it did not, which is the one thing a reset
    /// guarantees and the only thing these tests may assume.
    fail_writes_from: RefCell<Option<u32>>,
    writes_seen: RefCell<u32>,
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

    fn fail_block(&self, block: Option<u32>) {
        *self.0.fail_block.borrow_mut() = block;
    }

    /// Cut the power before the `n`th write from now on.
    fn cut_writes_from(&self, n: Option<u32>) {
        *self.0.writes_seen.borrow_mut() = 0;
        *self.0.fail_writes_from.borrow_mut() = n;
    }

    fn writes_seen(&self) -> u32 {
        *self.0.writes_seen.borrow()
    }

    /// The block holding `needle`, for aiming a fault at one sector.
    fn block_containing(&self, needle: &[u8]) -> u32 {
        let data = self.0.data.borrow();
        let at = data
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("needle is on the card");
        (at / BLOCK_BYTES) as u32
    }
}

impl BlockDevice for SharedDisk {
    type Error = DiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), DiskError> {
        if let Some(bad) = *self.0.fail_block.borrow() {
            if (start.0..start.0 + blocks.len() as u32).contains(&bad) {
                return Err(DiskError);
            }
        }
        let data = self.0.data.borrow();
        for (index, block) in blocks.iter_mut().enumerate() {
            let at = (start.0 as usize + index) * BLOCK_BYTES;
            block.contents.copy_from_slice(&data[at..at + BLOCK_BYTES]);
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), DiskError> {
        {
            let mut seen = self.0.writes_seen.borrow_mut();
            *seen += 1;
            if let Some(from) = *self.0.fail_writes_from.borrow() {
                if *seen >= from {
                    return Err(DiskError);
                }
            }
        }
        let mut data = self.0.data.borrow_mut();
        for (index, block) in blocks.iter().enumerate() {
            let at = (start.0 as usize + index) * BLOCK_BYTES;
            data[at..at + BLOCK_BYTES].copy_from_slice(&block.contents);
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

fn format_disk() -> Vec<u8> {
    let mut disk = vec![0u8; DISK_BLOCKS as usize * BLOCK_BYTES];
    let part_blocks = DISK_BLOCKS - PART_START_BLOCK;
    let mut partition = vec![0u8; part_blocks as usize * BLOCK_BYTES];
    fatfs::format_volume(
        std::io::Cursor::new(partition.as_mut_slice()),
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat16),
    )
    .expect("format");
    disk[PART_START_BLOCK as usize * BLOCK_BYTES..].copy_from_slice(&partition);
    let entry = 446;
    disk[entry + 4] = 0x06;
    disk[entry + 8..entry + 12].copy_from_slice(&PART_START_BLOCK.to_le_bytes());
    disk[entry + 12..entry + 16].copy_from_slice(&part_blocks.to_le_bytes());
    disk[510] = 0x55;
    disk[511] = 0xAA;
    disk
}

fn new_card() -> SharedDisk {
    SharedDisk(Rc::new(RamDisk {
        data: RefCell::new(format_disk()),
        fail_block: RefCell::new(None),
        fail_writes_from: RefCell::new(None),
        writes_seen: RefCell::new(0),
    }))
}

/// A card whose clusters are a single 512-byte sector.
///
/// The journal is two sectors, so growing it from one slot to two must
/// allocate a second cluster here. On a card with larger clusters both slots
/// can land inside the first one and the allocation never happens, which
/// would let a first-use ordering bug pass unnoticed.
fn new_card_with_tiny_clusters() -> SharedDisk {
    let mut disk = vec![0u8; DISK_BLOCKS as usize * BLOCK_BYTES];
    let part_blocks = DISK_BLOCKS - PART_START_BLOCK;
    let mut partition = vec![0u8; part_blocks as usize * BLOCK_BYTES];
    fatfs::format_volume(
        std::io::Cursor::new(partition.as_mut_slice()),
        fatfs::FormatVolumeOptions::new()
            .fat_type(fatfs::FatType::Fat16)
            .bytes_per_cluster(512),
    )
    .expect("format");
    disk[PART_START_BLOCK as usize * BLOCK_BYTES..].copy_from_slice(&partition);
    let entry = 446;
    disk[entry + 4] = 0x06;
    disk[entry + 8..entry + 12].copy_from_slice(&PART_START_BLOCK.to_le_bytes());
    disk[entry + 12..entry + 16].copy_from_slice(&part_blocks.to_le_bytes());
    disk[510] = 0x55;
    disk[511] = 0xAA;
    SharedDisk(Rc::new(RamDisk {
        data: RefCell::new(disk),
        fail_block: RefCell::new(None),
        fail_writes_from: RefCell::new(None),
        writes_seen: RefCell::new(0),
    }))
}

fn root_of(mgr: &Mgr) -> Dir<'_> {
    let volume = mgr.open_volume(VolumeIdx(0)).expect("volume");
    let raw_root = mgr.open_root_dir(volume.to_raw_volume()).expect("root");
    Directory::new(raw_root, mgr)
}

/// Put a journal on the card with these bytes.
fn write_journal(root: &Dir<'_>, bytes: &[u8]) {
    if root.open_dir(proto::cache::CACHE_ROOT_DIR).is_err() {
        root.make_dir_in_dir(proto::cache::CACHE_ROOT_DIR)
            .expect("make cache root");
    }
    let cache_root = root.open_dir(proto::cache::CACHE_ROOT_DIR).expect("open");
    let file = cache_root
        .open_file_in_dir(reclaim::JOURNAL_FILE, Mode::ReadWriteCreateOrTruncate)
        .expect("create journal");
    file.write(bytes).expect("write");
    file.close().expect("close");
}

fn a_batch(seq: u32) -> Batch {
    let mut name = heapless::String::new();
    name.push_str("BOOK~1.EPU").expect("fits");
    Batch {
        seq,
        place: Place::Books,
        name,
        entry_cluster: 40,
        clusters: heapless::Vec::from_slice(&[40, 41, 42]).expect("fits"),
        continuation: 43,
    }
}

#[test]
fn a_card_with_no_journal_reports_absent() {
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    assert_eq!(reclaim::read_journal(&root), Ok(Journal::Absent));
}

#[test]
fn finding_out_whether_a_reclaim_is_outstanding_does_not_change_the_card() {
    // Every mount asks this question. Answering it by creating directories
    // would mean a card that is only ever read from still accumulates our
    // furniture -- and a card that merely failed a read would be answered
    // with a write.
    let disk = new_card();
    let before = {
        let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
        let root = root_of(&mgr);
        // Touch the volume the way a mount would, then snapshot.
        let _ = root.open_dir("BOOKS");
        disk.image()
    };
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    assert_eq!(reclaim::read_journal(&root), Ok(Journal::Absent));
    drop(root);
    drop(mgr);
    assert!(
        disk.image() == before,
        "reading the journal wrote to the card"
    );
}

#[test]
fn a_journal_this_build_cannot_read_is_not_an_idle_one() {
    // The upgrade case: an older build unlinked a book and got part way
    // through freeing its chain. Its record is the only thing that can find
    // the rest. Reading that as "nothing in flight" would abandon the
    // reclaim and leak the chain for the life of the card.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    write_journal(&root, &[0xA5; JOURNAL_BYTES]);
    assert_eq!(reclaim::read_journal(&root), Ok(Journal::Unrecognized));
}

#[test]
fn a_journal_without_a_whole_first_record_is_a_bootstrap_to_retry() {
    // The failure this replaces. The first record is durable before anything
    // is unlinked or freed, so a journal that never got one describes a
    // transaction that never began -- and refusing it, once Unrecognized
    // makes the storage owner refuse mutations, would strand the card on the
    // one transaction that provably did nothing.
    for length in [1usize, 100, SLOT_BYTES - 1] {
        let disk = new_card();
        let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
        let root = root_of(&mgr);
        write_journal(&root, &vec![0u8; length]);
        assert_eq!(
            reclaim::read_journal(&root),
            Ok(Journal::Absent),
            "a {length}-byte journal is an interrupted bootstrap",
        );
    }
}

#[test]
fn one_slot_of_nothing_is_also_a_bootstrap_to_retry() {
    // The first record torn part way through: a whole slot's worth of file,
    // no record in it, and no second slot yet.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    write_journal(&root, &[0u8; SLOT_BYTES]);
    assert_eq!(reclaim::read_journal(&root), Ok(Journal::Absent));
}

#[test]
fn a_first_record_alone_is_acted_on_and_the_second_slot_comes_later() {
    // The journal reaches its full size in two steps, and that is what makes
    // the case above distinguishable. A first record living in a one-slot
    // file is perfectly usable, and the next record goes to the slot that
    // does not exist yet.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    write_journal(&root, &a_batch(1).encode());
    match reclaim::read_journal(&root) {
        Ok(Journal::Found(live)) => {
            assert_eq!(live.index, Some(0));
            assert_eq!(live.slot, Slot::Work(a_batch(1)));
            assert_eq!(live.next_index(), 1);
            assert_eq!(live.next_seq(), 2);
        }
        other => panic!("expected the first record, got {other:?}"),
    }
}

#[test]
fn a_cut_while_growing_the_second_slot_leaves_the_first_record_standing() {
    // The state a reset between batches leaves: the journal is past one slot
    // and short of two, with whatever reached the card sitting in the tail.
    //
    // Only slot 0 is read there, which is what makes it safe. The second
    // record never became whole, so by the ordering its batch was never
    // freed, and the first record still describes work that is safe to redo.
    for tail in [1usize, 200, SLOT_BYTES - 1] {
        let disk = new_card();
        let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
        let root = root_of(&mgr);
        let mut bytes = a_batch(1).encode().to_vec();
        // Part of a second record, and deliberately not zeroes: a torn write
        // leaves whatever the card managed to take.
        bytes.extend(a_batch(2).encode().iter().take(tail));
        write_journal(&root, &bytes);
        match reclaim::read_journal(&root) {
            Ok(Journal::Found(live)) => {
                assert_eq!(live.index, Some(0), "with a {tail}-byte tail");
                assert_eq!(live.slot, Slot::Work(a_batch(1)));
                // And the next attempt writes the second slot again.
                assert_eq!(live.next_index(), 1);
                assert_eq!(live.next_seq(), 2);
            }
            other => panic!("expected the first record with a {tail}-byte tail, got {other:?}"),
        }
    }
}

#[test]
fn an_unsupported_first_record_still_fails_stop() {
    // Bootstrap leniency does not extend to a whole record this build cannot
    // read. That one may describe work another build has already done.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    write_journal(&root, &from_a_later_build(1));
    assert_eq!(reclaim::read_journal(&root), Ok(Journal::Unrecognized));
}

#[test]
fn two_slots_of_nothing_are_refused_rather_than_restarted() {
    // Once the journal has reached two slots it is never shortened, so a
    // full-length file with nothing readable in it is not a bootstrap: it
    // held records, and they are unreadable now. That may be a reclaim part
    // way through, so it is refused.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    write_journal(&root, &[0u8; JOURNAL_BYTES]);
    assert_eq!(reclaim::read_journal(&root), Ok(Journal::Unrecognized));
}

#[test]
fn a_written_record_reads_back() {
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let mut bytes = [0u8; JOURNAL_BYTES];
    bytes[..SLOT_BYTES].copy_from_slice(&a_batch(1).encode());
    write_journal(&root, &bytes);
    match reclaim::read_journal(&root) {
        Ok(Journal::Found(live)) => {
            assert_eq!(live.index, Some(0));
            assert_eq!(live.slot, Slot::Work(a_batch(1)));
            assert_eq!(
                live.next_index(),
                1,
                "the next record goes in the other slot"
            );
            assert_eq!(live.next_seq(), 2);
        }
        other => panic!("expected a record, got {other:?}"),
    }
}

#[test]
fn the_later_record_wins_across_the_two_slots() {
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let mut bytes = [0u8; JOURNAL_BYTES];
    bytes[..SLOT_BYTES].copy_from_slice(&a_batch(4).encode());
    bytes[SLOT_BYTES..].copy_from_slice(&a_batch(5).encode());
    write_journal(&root, &bytes);
    match reclaim::read_journal(&root) {
        Ok(Journal::Found(live)) => assert_eq!(live.index, Some(1)),
        other => panic!("expected a record, got {other:?}"),
    }
}

#[test]
fn a_card_that_will_not_answer_is_not_a_card_with_an_older_record() {
    // The finding this test exists for.
    //
    // Falling back to the older slot is only safe when the newer one was
    // *torn* -- by contract its batch was never freed, because a batch is
    // freed only once the slot describing it is durable. A newer slot that
    // simply would not read is a different thing: it may well have been
    // durable, and its clusters may already be part freed. Replaying the
    // older record would then walk a continuation whose chain has been taken
    // apart, which is the lost-topology problem the journal exists to
    // prevent. So the pass stops and is retried at the next mount.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let mut bytes = [0u8; JOURNAL_BYTES];
    bytes[..SLOT_BYTES].copy_from_slice(&a_batch(4).encode());
    bytes[SLOT_BYTES..].copy_from_slice(&a_batch(5).encode());
    write_journal(&root, &bytes);

    // Both records are readable to start with, and the newer one wins.
    match reclaim::read_journal(&root) {
        Ok(Journal::Found(live)) => assert_eq!(live.index, Some(1)),
        other => panic!("expected a record, got {other:?}"),
    }
    drop(root);
    drop(mgr);

    // Take out exactly the sector holding the newer record, leaving the
    // directory and the older record perfectly readable. Anything coarser
    // would fail the directory lookup instead and prove nothing.
    let newer = a_batch(5).encode();
    let newer_block = disk.block_containing(&newer[..32]);
    let older_block = disk.block_containing(&a_batch(4).encode()[..32]);
    assert_ne!(
        newer_block, older_block,
        "the slots must be separate sectors"
    );
    disk.fail_block(Some(newer_block));

    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    assert_eq!(
        reclaim::read_journal(&root),
        Err(reclaim::ReclaimError::Card),
        "a card that would not answer must not be reported as an older record",
    );

    // And with the card answering again, the newer record is back.
    drop(root);
    drop(mgr);
    disk.fail_block(None);
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    match reclaim::read_journal(&root) {
        Ok(Journal::Found(live)) => assert_eq!(live.index, Some(1)),
        other => panic!("expected the newer record back, got {other:?}"),
    }
}

/// A whole record from a build this one does not know: the checksum is
/// recomputed over the bumped version, so these bytes are exactly what that
/// build meant to write.
fn from_a_later_build(seq: u32) -> [u8; SLOT_BYTES] {
    let mut bytes = a_batch(seq).encode();
    // Version lives at offset 4, checksum in the last four bytes -- the
    // envelope the format promises to keep stable across versions.
    bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
    let mut hash: u32 = 0x811c_9dc5;
    for byte in &bytes[..SLOT_BYTES - 4] {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    bytes[SLOT_BYTES - 4..].copy_from_slice(&hash.to_le_bytes());
    bytes
}

#[test]
fn a_later_builds_record_beside_an_older_readable_one_refuses_the_journal() {
    // The downgrade case, end to end.
    //
    // A later build wrote slot B durably and began freeing the batch it
    // describes. The card then boots this build, which cannot read B. If it
    // fell back to A it would act on a record whose continuation may point
    // into a chain already taken apart -- and if A says Clear, it would call
    // the journal idle and let unrelated work proceed over an unfinished
    // reclaim.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let mut bytes = [0u8; JOURNAL_BYTES];
    bytes[..SLOT_BYTES].copy_from_slice(&a_batch(10).encode());
    bytes[SLOT_BYTES..].copy_from_slice(&from_a_later_build(11));
    write_journal(&root, &bytes);
    assert_eq!(
        reclaim::read_journal(&root),
        Ok(Journal::Unrecognized),
        "a record this build cannot read must stop it, not be stepped over",
    );
}

#[test]
fn a_later_builds_record_beside_a_clear_one_still_refuses() {
    // The more dangerous shape of the same thing: the readable slot is the
    // one that says there is nothing to do.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let mut bytes = [0u8; JOURNAL_BYTES];
    bytes[..SLOT_BYTES].copy_from_slice(&Batch::clear(10));
    bytes[SLOT_BYTES..].copy_from_slice(&from_a_later_build(11));
    write_journal(&root, &bytes);
    assert_eq!(reclaim::read_journal(&root), Ok(Journal::Unrecognized));
}

#[test]
fn a_torn_slot_beside_a_readable_one_is_still_acted_on() {
    // The distinction the previous two tests rest on: torn is what the
    // second slot exists for, and is still safe to fall back over.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let mut bytes = [0u8; JOURNAL_BYTES];
    bytes[..SLOT_BYTES].copy_from_slice(&a_batch(10).encode());
    let mut torn = a_batch(11).encode();
    torn[200] ^= 0xFF;
    bytes[SLOT_BYTES..].copy_from_slice(&torn);
    write_journal(&root, &bytes);
    match reclaim::read_journal(&root) {
        Ok(Journal::Found(live)) => assert_eq!(live.index, Some(0)),
        other => panic!("expected the surviving record, got {other:?}"),
    }
}

/// Open (creating if needed) the shelf directory.
fn shelf<'a>(root: &Dir<'a>) -> Dir<'a> {
    if root.open_dir("BOOKS").is_err() {
        root.make_dir_in_dir("BOOKS").expect("make BOOKS");
    }
    root.open_dir("BOOKS").expect("open BOOKS")
}

/// Put a book on the shelf with `bytes` of body, and report its first
/// cluster.
fn shelve(books: &Dir<'_>, name: &str, bytes: usize) -> u32 {
    let file = books
        .open_file_in_dir(name, Mode::ReadWriteCreateOrTruncate)
        .expect("create");
    file.write(&vec![0xC3; bytes]).expect("write");
    file.close().expect("close");
    books
        .find_directory_entry(name)
        .expect("entry")
        .cluster
        .value()
}

/// The clusters among `wanted` whose FAT16 entry is not free.
///
/// Per cluster rather than by counting free space: the journal allocates
/// while the reclaim runs, so an aggregate can stay level while a batch
/// leaks. One image read for the whole list -- the image is megabytes, and
/// copying it per cluster turned a one-second sweep into a ninety-second
/// one.
fn still_allocated(disk: &SharedDisk, wanted: &[u32]) -> Vec<u32> {
    let image = disk.image();
    let boot = PART_START_BLOCK as usize * BLOCK_BYTES;
    let reserved = u16::from_le_bytes([image[boot + 14], image[boot + 15]]) as usize;
    let fat_start = (PART_START_BLOCK as usize + reserved) * BLOCK_BYTES;
    wanted
        .iter()
        .copied()
        .filter(|cluster| {
            let at = fat_start + *cluster as usize * 2;
            u16::from_le_bytes([image[at], image[at + 1]]) != 0
        })
        .collect()
}

fn free_clusters(disk: &SharedDisk) -> u32 {
    // Count of zero FAT16 entries, as a proxy for space the volume believes
    // it has. Compared before and after rather than absolutely.
    let image = disk.image();
    // Read the FAT's position out of the BPB rather than guessing it.
    let boot = PART_START_BLOCK as usize * BLOCK_BYTES;
    let reserved = u16::from_le_bytes([image[boot + 14], image[boot + 15]]) as usize;
    let fat_start = (PART_START_BLOCK as usize + reserved) * BLOCK_BYTES;
    let mut free = 0;
    for pair in image[fat_start..fat_start + 8192].chunks(2) {
        if pair == [0, 0] {
            free += 1;
        }
    }
    free
}

#[test]
fn a_reclaim_takes_the_name_then_the_space() {
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let books = shelf(&root);
    shelve(&books, "BOOK.EPU", 40_000);
    let before = free_clusters(&disk);

    reclaim::reclaim_entry(&root, Some(&books), Place::Books, "BOOK.EPU").expect("reclaim");

    assert!(
        books.find_directory_entry("BOOK.EPU").is_err(),
        "the name should be gone"
    );
    assert!(
        free_clusters(&disk) > before,
        "the space should have come back"
    );
    // And the journal says so rather than being deleted.
    match reclaim::read_journal(&root) {
        Ok(Journal::Found(live)) => assert!(matches!(live.slot, Slot::Clear { .. })),
        other => panic!("expected a clear record, got {other:?}"),
    }
}

#[test]
fn a_reclaim_of_a_multi_batch_chain_finishes_all_of_it() {
    // Big enough to need more than one batch, so the continuation path runs.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let books = shelf(&root);
    shelve(&books, "BIG.EPU", 400_000);
    let before = free_clusters(&disk);
    reclaim::reclaim_entry(&root, Some(&books), Place::Books, "BIG.EPU").expect("reclaim");
    assert!(books.find_directory_entry("BIG.EPU").is_err());
    assert!(free_clusters(&disk) > before);
}

#[test]
fn a_zero_length_file_is_reclaimed_like_any_other() {
    // Worth pinning because the obvious assumption is wrong: a file written
    // with no bytes still has a cluster allocated here, so it goes through
    // the journal like anything else. The driver's no-cluster shortcut is
    // for an entry that genuinely points nowhere, which this is not.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let books = shelf(&root);
    let first = shelve(&books, "EMPTY.EPU", 0);
    assert_ne!(first, 0, "a zero-length file still holds a cluster");
    reclaim::reclaim_entry(&root, Some(&books), Place::Books, "EMPTY.EPU").expect("reclaim");
    assert!(books.find_directory_entry("EMPTY.EPU").is_err());
    match reclaim::read_journal(&root) {
        Ok(Journal::Found(live)) => assert!(matches!(live.slot, Slot::Clear { .. })),
        other => panic!("expected a clear record, got {other:?}"),
    }
}

#[test]
fn reclaiming_something_already_gone_is_not_an_error() {
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let books = shelf(&root);
    shelve(&books, "BOOK.EPU", 1000);
    books.delete_entry_in_dir("BOOK.EPU").expect("unlink");
    reclaim::reclaim_entry(&root, Some(&books), Place::Books, "BOOK.EPU").expect("no-op");
}

#[test]
fn recovery_over_nothing_is_a_no_op() {
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let books = shelf(&root);
    shelve(&books, "KEEP.EPU", 1000);
    reclaim::recover(&root, Some(&books)).expect("recover");
    assert!(books.find_directory_entry("KEEP.EPU").is_ok(), "left alone");
}

#[test]
fn a_journal_this_build_cannot_read_stops_a_new_reclaim() {
    // The refusal that protects an outstanding transaction from being
    // overwritten by a new one.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let books = shelf(&root);
    shelve(&books, "BOOK.EPU", 1000);
    write_journal(&root, &from_a_later_build(1));
    assert_eq!(
        reclaim::reclaim_entry(&root, Some(&books), Place::Books, "BOOK.EPU"),
        Err(ReclaimError::Unrecognized),
    );
    assert!(
        books.find_directory_entry("BOOK.EPU").is_ok(),
        "the book must still be there",
    );
}

/// Every cluster of a file, read out through the journal's own walker.
fn chain_of(dir: &Dir<'_>, name: &str) -> Vec<u32> {
    let first = dir.find_directory_entry(name).expect("entry").cluster;
    let mut chain = vec![first.value()];
    let mut at = first;
    while let Some(next) = dir.next_cluster_in_chain(at).expect("walk") {
        chain.push(next.value());
        at = next;
    }
    chain
}

#[test]
fn the_journal_reaches_its_full_size_before_anything_is_freed() {
    // The first-use hazard, on a card where it is reachable.
    //
    // The journal grows from one slot to two exactly once. If that happens
    // after a batch has been freed, the allocator picks from the clusters
    // this transaction just released -- its free-cluster hint points
    // straight at them -- and the journal ends up living in a cluster that
    // slot 0 still lists. A cut before the second slot became whole would
    // then have recovery re-free that batch, out of the journal itself.
    let disk = new_card_with_tiny_clusters();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let books = shelf(&root);
    shelve(&books, "BOOK.EPU", 40_000);
    let target = chain_of(&books, "BOOK.EPU");
    assert!(target.len() > 1, "the book needs several clusters");

    reclaim::reclaim_entry(&root, Some(&books), Place::Books, "BOOK.EPU").expect("reclaim");

    // The journal is at its full two slots...
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    let journal = cache_root
        .open_file_in_dir(reclaim::JOURNAL_FILE, Mode::ReadOnly)
        .expect("journal");
    assert_eq!(journal.length(), JOURNAL_BYTES as u32);
    journal.close().expect("close");

    // ...and none of it is living in a cluster the reclaim freed.
    let journal_chain = chain_of(&cache_root, reclaim::JOURNAL_FILE);
    for cluster in &journal_chain {
        assert!(
            !target.contains(cluster),
            "the journal took cluster {cluster} from the chain it was reclaiming: \
             journal {journal_chain:?}, target {target:?}",
        );
    }
}

/// An entry whose first cluster really is zero: created and never written.
/// Ordinary filesystem-made files do not have this shape -- even a
/// zero-length write allocates -- so it has to be made deliberately.
fn shelve_nothing(books: &Dir<'_>, name: &str) -> u32 {
    let file = books
        .open_file_in_dir(name, Mode::ReadWriteCreate)
        .expect("create");
    file.close().expect("close");
    let first = books
        .find_directory_entry(name)
        .expect("entry")
        .cluster
        .value();
    assert_eq!(
        first, 0,
        "this fixture exists to produce a cluster-zero entry"
    );
    first
}

#[test]
fn a_zero_cluster_entry_waits_its_turn_like_any_other() {
    // The regression: such an entry used to be unlinked before the journal
    // was read at all, so neither an unreadable journal nor an outstanding
    // reclaim stopped it -- a hole in the rule those states exist to
    // enforce, and one the storage owner's mutation freeze would inherit.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let books = shelf(&root);
    shelve_nothing(&books, "NOWT.EPU");
    write_journal(&root, &from_a_later_build(1));
    assert_eq!(
        reclaim::reclaim_entry(&root, Some(&books), Place::Books, "NOWT.EPU"),
        Err(ReclaimError::Unrecognized),
    );
    assert!(
        books.find_directory_entry("NOWT.EPU").is_ok(),
        "an entry pointing nowhere is still not this build's to remove",
    );
}

#[test]
fn a_zero_cluster_entry_is_reclaimed_through_the_journal() {
    // And with the journal clear, it goes through the same transaction as
    // anything else: a record with an empty batch, an unlink, then Clear.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let books = shelf(&root);
    shelve_nothing(&books, "NOWT.EPU");
    reclaim::reclaim_entry(&root, Some(&books), Place::Books, "NOWT.EPU").expect("reclaim");
    assert!(books.find_directory_entry("NOWT.EPU").is_err());
    match reclaim::read_journal(&root) {
        Ok(Journal::Found(live)) => assert!(matches!(live.slot, Slot::Clear { .. })),
        other => panic!("expected a clear record, got {other:?}"),
    }
}

#[test]
fn an_unreadable_journal_stops_a_reclaim_of_an_ordinary_book_too() {
    // What the mis-built test above was actually proving. Kept, because the
    // general rule is worth its own coverage.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let books = shelf(&root);
    shelve(&books, "BOOK.EPU", 1000);
    write_journal(&root, &from_a_later_build(1));
    assert_eq!(
        reclaim::reclaim_entry(&root, Some(&books), Place::Books, "BOOK.EPU"),
        Err(ReclaimError::Unrecognized),
    );
    assert!(books.find_directory_entry("BOOK.EPU").is_ok());
}

/// What a card shows after a cut and a replay.
#[derive(Debug, PartialEq, Eq)]
enum Landed {
    /// The book is still there, and reads back as the book it was.
    Whole,
    /// The book is gone.
    Gone,
}

/// Cut the power before the `cut`th write of a reclaim, reboot, replay to
/// completion, and report which of the two legal states the card is in.
///
/// Panics with the detail if it is in neither -- in particular if the entry
/// survives pointing at a chain that no longer reads, which is the defect
/// this whole transaction exists to prevent.
fn reclaim_cut_at(disk: &SharedDisk, cut: u32, body: usize) -> Landed {
    let original = {
        let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
        let root = root_of(&mgr);
        let books = shelf(&root);
        shelve(&books, "BOOK.EPU", body);
        chain_of(&books, "BOOK.EPU")
    };

    // The cut.
    {
        let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
        let root = root_of(&mgr);
        let books = shelf(&root);
        disk.cut_writes_from(Some(cut));
        let _ = reclaim::reclaim_entry(&root, Some(&books), Place::Books, "BOOK.EPU");
    }
    disk.cut_writes_from(None);

    // What the card looks like *before* anything replays, which is the state
    // a reader would see if the device were examined now -- the card pulled
    // and put in a computer, or a mount whose recovery could not run.
    //
    // This is where the ordering earns its keep, and it is the check that
    // fails if freeing is ever allowed to precede the unlink: a book still
    // listed must still be readable. Convergence after a replay is a weaker
    // promise and would not notice.
    {
        let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
        let root = root_of(&mgr);
        let books = shelf(&root);
        let listed = match books.find_directory_entry("BOOK.EPU") {
            Ok(entry) => Some(entry),
            Err(embedded_sdmmc::Error::NotFound) => None,
            // Anything else is the shelf itself being unreadable, which is
            // not "the book is gone" and must not be counted as it.
            Err(error) => {
                panic!("cut at write {cut}: the shelf would not answer before replay: {error:?}",)
            }
        };
        if let Some(entry) = listed {
            assert_eq!(
                entry.cluster.value(),
                original[0],
                "cut at write {cut}: before any replay, the listed book points elsewhere",
            );
            let mut at = entry.cluster;
            let mut walked = vec![at.value()];
            loop {
                match books.next_cluster_in_chain(at) {
                    Ok(Some(next)) => {
                        walked.push(next.value());
                        at = next;
                    }
                    Ok(None) => break,
                    Err(error) => panic!(
                        "cut at write {cut}: the book is still listed but its chain no \
                         longer reads ({error:?}) -- listed and unreadable is the state \
                         this transaction exists to make unreachable",
                    ),
                }
            }
            assert_eq!(
                walked, original,
                "cut at write {cut}: the listed book's chain changed under it",
            );
        }
    }

    // The reboot, and the replay a mount would perform.
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let books = shelf(&root);
    reclaim::recover(&root, Some(&books)).unwrap_or_else(|error| {
        panic!("replay after a cut at write {cut} could not finish: {error:?}")
    });

    match books.find_directory_entry("BOOK.EPU") {
        Err(embedded_sdmmc::Error::NotFound) => {
            // Gone is only half the promise. Every cluster the book held
            // must have gone back to the volume, or a replay that skipped a
            // batch and published Clear would pass this sweep while leaking
            // the chain.
            drop(books);
            drop(root);
            drop(mgr);
            let leaked = still_allocated(disk, &original);
            assert!(
                leaked.is_empty(),
                "cut at write {cut}: the book is gone but {} of its clusters were never \
                 reclaimed: {leaked:?} of {original:?}",
                leaked.len(),
            );
            Landed::Gone
        }
        Err(error) => {
            panic!("cut at write {cut}: the shelf would not answer after replay: {error:?}",)
        }
        Ok(entry) => {
            // Listed. Then it must still be the book it was: same chain,
            // still walkable. A name over a chain that has been taken apart
            // is the failure this transaction was built to remove.
            assert_eq!(
                entry.cluster.value(),
                original[0],
                "cut at write {cut}: the entry points somewhere else now",
            );
            let now = chain_of(&books, "BOOK.EPU");
            assert_eq!(
                now, original,
                "cut at write {cut}: the book is listed but its chain has changed",
            );
            Landed::Whole
        }
    }
}

#[test]
fn a_cut_at_any_write_leaves_the_book_whole_or_gone() {
    // The matrix. Rather than pick the boundaries by hand -- establish,
    // unlink, free, advance, clear -- this cuts before every write the
    // transaction performs, which is every boundary there is including the
    // ones nobody thought to name.
    //
    // The card is examined twice, and the first look is the load-bearing
    // one: before anything replays, a listed book must still read. That is
    // the promise -- not that the card converges, which it would under the
    // wrong ordering too, but that it is never externally in the state the
    // old delete left behind. The second look holds the replay to finishing
    // the job, name and clusters both.
    let probe = new_card();
    let writes = {
        let mgr: Mgr = VolumeManager::new_with_limits(probe.clone(), StaticTime, 5000);
        let root = root_of(&mgr);
        let books = shelf(&root);
        shelve(&books, "BOOK.EPU", 40_000);
        probe.cut_writes_from(None);
        let before = probe.writes_seen();
        reclaim::reclaim_entry(&root, Some(&books), Place::Books, "BOOK.EPU").expect("uncut");
        probe.writes_seen() - before
    };
    assert!(
        writes > 4,
        "a reclaim should take several writes, took {writes}"
    );

    let mut gone = 0;
    let mut whole = 0;
    for cut in 1..=writes + 1 {
        match reclaim_cut_at(&new_card(), cut, 40_000) {
            Landed::Gone => gone += 1,
            Landed::Whole => whole += 1,
        }
    }
    // Both outcomes must actually occur, or the sweep is only testing one
    // half of the promise.
    assert!(whole > 0, "no cut landed before the transaction committed");
    assert!(gone > 0, "no cut landed after it committed");
    assert_eq!(gone + whole, writes + 1);
}

#[test]
fn a_cut_at_any_write_of_a_multi_batch_reclaim_still_converges() {
    // The same sweep over a chain long enough to need continuations, so the
    // advance boundary -- walk, write the alternate slot, free -- is inside
    // the range being cut.
    let probe = new_card();
    let writes = {
        let mgr: Mgr = VolumeManager::new_with_limits(probe.clone(), StaticTime, 5000);
        let root = root_of(&mgr);
        let books = shelf(&root);
        shelve(&books, "BOOK.EPU", 400_000);
        probe.cut_writes_from(None);
        let before = probe.writes_seen();
        reclaim::reclaim_entry(&root, Some(&books), Place::Books, "BOOK.EPU").expect("uncut");
        probe.writes_seen() - before
    };
    let mut gone = 0;
    let mut whole = 0;
    for cut in 1..=writes + 1 {
        match reclaim_cut_at(&new_card(), cut, 400_000) {
            Landed::Gone => gone += 1,
            Landed::Whole => whole += 1,
        }
    }
    assert!(whole > 0 && gone > 0, "{whole} whole, {gone} gone");
}

#[test]
fn a_cut_at_any_write_on_first_use_with_tiny_clusters_converges() {
    // First use, on the card where journal growth has to allocate. This is
    // the configuration the self-reallocation bug lived in, so it is the one
    // worth sweeping rather than assuming.
    let probe = new_card_with_tiny_clusters();
    let writes = {
        let mgr: Mgr = VolumeManager::new_with_limits(probe.clone(), StaticTime, 5000);
        let root = root_of(&mgr);
        let books = shelf(&root);
        shelve(&books, "BOOK.EPU", 20_000);
        probe.cut_writes_from(None);
        let before = probe.writes_seen();
        reclaim::reclaim_entry(&root, Some(&books), Place::Books, "BOOK.EPU").expect("uncut");
        probe.writes_seen() - before
    };
    let mut gone = 0;
    let mut whole = 0;
    for cut in 1..=writes + 1 {
        match reclaim_cut_at(&new_card_with_tiny_clusters(), cut, 20_000) {
            Landed::Gone => gone += 1,
            Landed::Whole => whole += 1,
        }
    }
    assert!(whole > 0 && gone > 0, "{whole} whole, {gone} gone");
}

#[test]
fn a_root_reclaim_needs_no_shelf() {
    // The OTA trigger lives at the card root, and can be there on a card
    // that has never held a book. Recovery reaches it without a shelf.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let file = root
        .open_file_in_dir("TRIGGER.BIN", Mode::ReadWriteCreateOrTruncate)
        .expect("create");
    file.write(&[0x5A; 20_000]).expect("write");
    file.close().expect("close");
    let chain = chain_of(&root, "TRIGGER.BIN");

    reclaim::reclaim_entry(&root, None, Place::Root, "TRIGGER.BIN").expect("reclaim");

    assert!(root.find_directory_entry("TRIGGER.BIN").is_err());
    assert!(
        still_allocated(&disk, &chain).is_empty(),
        "the trigger's clusters should have come back",
    );
}

#[test]
fn a_shelf_reclaim_without_a_shelf_is_refused_not_assumed_done() {
    // The dangerous shortcut: a record naming /BOOKS, replayed on a mount
    // that has no /BOOKS. Its clusters are detached and its numbers carry no
    // ownership, so calling it finished would leave them free for the next
    // allocation while the record still says to free them.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let mut bytes = [0u8; JOURNAL_BYTES];
    bytes[..SLOT_BYTES].copy_from_slice(&a_batch(1).encode());
    write_journal(&root, &bytes);
    assert_eq!(
        reclaim::recover(&root, None),
        Err(ReclaimError::ShelfMissing),
    );
}

#[test]
fn a_cleared_journal_needs_no_shelf_either() {
    // The common case on a card with no books: nothing outstanding, so the
    // absence of a shelf never comes up.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    write_journal(&root, &Batch::clear(1));
    reclaim::recover(&root, None).expect("nothing to do");
}

#[test]
fn a_rollback_reclaim_needs_no_shelf_and_finishes_on_replay() {
    // The install handoff, from the reclaim side.
    //
    // A parked predecessor lives under the cache root, so its reclaim
    // resolves from `root` alone -- which matters because install recovery
    // runs on paths that may not have opened the shelf. And a cut inside it
    // leaves the reclaim journal describing the rest, so the next mount
    // finishes the parked copy off before the install planner looks again
    // and sees it gone.
    let disk = new_card();
    let mgr: Mgr = VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000);
    let root = root_of(&mgr);
    let cache_root = {
        root.make_dir_in_dir(proto::cache::CACHE_ROOT_DIR).ok();
        let cache_root = root
            .open_dir(proto::cache::CACHE_ROOT_DIR)
            .expect("cache root");
        cache_root.make_dir_in_dir("ROLLBACK").ok();
        cache_root
    };
    let rollback = cache_root.open_dir("ROLLBACK").expect("rollback");
    let file = rollback
        .open_file_in_dir("OLD.OLD", Mode::ReadWriteCreateOrTruncate)
        .expect("create");
    file.write(&[0x77; 30_000]).expect("write");
    file.close().expect("close");
    let chain = chain_of(&rollback, "OLD.OLD");
    drop(rollback);
    drop(cache_root);

    reclaim::reclaim_entry(&root, None, Place::Rollback, "OLD.OLD").expect("reclaim");

    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    let rollback = cache_root.open_dir("ROLLBACK").expect("rollback");
    assert!(
        rollback.find_directory_entry("OLD.OLD").is_err(),
        "the parked copy should be gone",
    );
    drop(rollback);
    drop(cache_root);
    assert!(
        still_allocated(&disk, &chain).is_empty(),
        "and its chain returned to the volume",
    );
}
