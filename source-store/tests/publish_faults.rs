//! Fault-injection and simulated power-cut tests for the durable publication
//! protocol, against a real embedded-sdmmc FAT16 filesystem on an in-memory
//! card.
//!
//! Two failure models, deliberately distinct:
//!
//! - **Injected write/read failures**: the card refuses the Nth I/O and the
//!   code under test keeps running. Exercises the error paths and the
//!   invariant that a failed publication never disturbs committed authority.
//! - **Power-cut replay**: every block write during a publication is logged;
//!   the test then reconstructs the card as it stood after *each prefix* of
//!   those writes — plus torn variants of the next write — and runs startup
//!   selection on the reconstruction. This is the software half of the PRD's
//!   power-cut gate: for every cut point, selection must yield the previous
//!   committed state or the new committed state, and nothing else. The
//!   hardware half (real cards, real power removal) cannot be simulated and
//!   is tracked as a separate M0S acceptance item.
//!
//! The card model (`FaultyDisk`, MBR + FAT16 image) follows
//! `reader-cache/tests/publish_faults.rs`, which itself copied
//! `upload-store/tests/transaction.rs`: integration tests cannot import each
//! other, and a shared harness would put test scaffolding in a shipped
//! crate's public API. Third copy now — if a fourth appears, extract a
//! dev-only harness crate.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Directory, Mode, TimeSource, Timestamp, VolumeIdx,
    VolumeManager,
};
use source_store::bodies::{
    DisplayLabel, OperationKind, SourceMetadata, SourceOrigin, BOOK_TOKEN_BYTES,
    LOGICAL_BOOK_ID_BYTES, REQUEST_ID_BYTES, SHA256_BYTES, SOURCE_METADATA_LOGICAL_BYTES,
    SOURCE_METADATA_MAGIC, SOURCE_METADATA_SCHEMA,
};
use source_store::publish::{
    publish_record, select_authority, summarize_slot, PublishError, SlotPair, SlotSummary,
};
use source_store::record::{
    classify_record, encode_commit_sector, record_file_len, seal_body, RecordState, SealedBody,
};

const BLOCK_BYTES: usize = 512;
/// 16 MiB card: big enough that fatfs picks FAT16 and small enough to stay fast.
const DISK_BLOCKS: u32 = 32 * 1024;
const PART_START_BLOCK: u32 = 64;

const PAIR: SlotPair<'_> = SlotPair {
    names: ["SRCMETA.A", "SRCMETA.B"],
    magic: SOURCE_METADATA_MAGIC,
};
const SCRATCH_BYTES: usize = 4096;

// ---------------------------------------------------------------------------
// Fault-injecting, write-logging in-memory block device
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskError;

/// Arms exactly-once faults: `fail_write_in = Some(n)` fails the (n+1)th
/// subsequent write call, then disarms. Exactly-once because the paths under
/// test issue their own I/O after the fault; a sticky fault would conflate
/// "this write failed" with "the card is gone".
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

/// One logged block write: where, and the 512 bytes that landed.
#[derive(Clone)]
struct LoggedWrite {
    block: u32,
    bytes: [u8; BLOCK_BYTES],
}

struct FaultyDisk {
    data: RefCell<Vec<u8>>,
    fault: FaultPlan,
    writes: Cell<u32>,
    /// When present, every block write is appended here (after the fault
    /// check, so only writes that actually landed are replayable).
    log: RefCell<Option<Vec<LoggedWrite>>>,
}

#[derive(Clone)]
struct SharedDisk(Rc<FaultyDisk>);

impl std::ops::Deref for SharedDisk {
    type Target = FaultyDisk;
    fn deref(&self) -> &FaultyDisk {
        &self.0
    }
}

impl SharedDisk {
    fn snapshot(&self) -> Vec<u8> {
        self.data.borrow().clone()
    }

    fn start_logging(&self) {
        *self.log.borrow_mut() = Some(Vec::new());
    }

    fn take_log(&self) -> Vec<LoggedWrite> {
        self.log.borrow_mut().take().expect("logging was started")
    }

    /// Rebuild the card as it stood after `prefix` writes of `log` landed on
    /// `base`, then optionally merge the first `torn_bytes` of the next
    /// write into its old block — a torn sector.
    fn restore_cut(&self, base: &[u8], log: &[LoggedWrite], prefix: usize, torn_bytes: usize) {
        let mut data = base.to_vec();
        for write in &log[..prefix] {
            let at = write.block as usize * BLOCK_BYTES;
            data[at..at + BLOCK_BYTES].copy_from_slice(&write.bytes);
        }
        if torn_bytes > 0 {
            if let Some(next) = log.get(prefix) {
                let at = next.block as usize * BLOCK_BYTES;
                data[at..at + torn_bytes].copy_from_slice(&next.bytes[..torn_bytes]);
            }
        }
        *self.data.borrow_mut() = data;
    }
}

impl BlockDevice for SharedDisk {
    type Error = DiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), DiskError> {
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
        let mut log = self.log.borrow_mut();
        for (i, block) in blocks.iter().enumerate() {
            let block_idx = start.0 + i as u32;
            let at = block_idx as usize * BLOCK_BYTES;
            data[at..at + BLOCK_BYTES].copy_from_slice(&block[..]);
            if let Some(log) = log.as_mut() {
                let mut bytes = [0u8; BLOCK_BYTES];
                bytes.copy_from_slice(&block[..]);
                log.push(LoggedWrite {
                    block: block_idx,
                    bytes,
                });
            }
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
            zero_indexed_month: 6,
            zero_indexed_day: 3,
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
        log: RefCell::new(None),
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
// The record under test
// ---------------------------------------------------------------------------

/// A metadata body whose source generation tracks the record generation, so
/// a selected record proves which publication it came from by content, not
/// just by framing generation.
fn sample_metadata(source_generation: u64) -> SourceMetadata {
    SourceMetadata {
        logical_book_id: [1; LOGICAL_BOOK_ID_BYTES],
        source_generation,
        source_origin: SourceOrigin::ManagedUpload,
        operation_kind: OperationKind::ManagedUploadRequest,
        operation_request_id: [2; REQUEST_ID_BYTES],
        externally_recovered: false,
        physical_slot: 0,
        source_length: 1000 + source_generation,
        source_sha256: [source_generation as u8; SHA256_BYTES],
        quick_fingerprint_policy_version: 1,
        quick_fingerprint_sha256: [7; SHA256_BYTES],
        book_token: [source_generation as u8 + 1; BOOK_TOKEN_BYTES],
        display_label: DisplayLabel::new(b"Fixture Book").unwrap(),
    }
}

/// Seal `sample_metadata(generation)` into a publishable buffer; the record
/// generation and source generation are kept equal for legibility.
fn sealed_metadata(generation: u64) -> (Vec<u8>, SealedBody) {
    let mut buf = vec![0u8; record_file_len(SOURCE_METADATA_LOGICAL_BYTES).unwrap()];
    let logical = sample_metadata(generation)
        .encode_into(&mut buf)
        .expect("encode fixture metadata");
    let sealed = seal_body(
        SOURCE_METADATA_MAGIC,
        SOURCE_METADATA_SCHEMA,
        generation,
        logical,
        &mut buf,
    )
    .expect("seal fixture metadata");
    (buf, sealed)
}

fn publish_generation(root: &Dir<'_>, generation: u64) -> Result<usize, PublishError> {
    let (buf, sealed) = sealed_metadata(generation);
    let mut scratch = [0u8; SCRATCH_BYTES];
    publish_record(root, PAIR, &buf, &sealed, 0, &mut scratch, || true)
}

/// Mount the card fresh — the reboot view — and run startup selection,
/// decoding the selected record's body. Panics on I/O; returns `None` when
/// no committed authority exists.
fn reboot_authority(disk: &SharedDisk) -> Option<(u64, SourceMetadata)> {
    let mgr = open_mgr(disk);
    let root = open_root(&mgr);
    let mut scratch = [0u8; SCRATCH_BYTES];
    let selected = select_authority(&root, PAIR, &mut scratch).expect("selection must not error");
    let (slot, generation) = selected?;
    // Reread the selected slot and decode: authority is the *content*, and a
    // selected record that does not decode is a protocol failure.
    let file = root
        .open_file_in_dir(PAIR.names[slot], Mode::ReadOnly)
        .expect("selected slot must open");
    let len = file.length() as usize;
    assert!(len <= SCRATCH_BYTES, "selected record impossibly large");
    let mut at = 0usize;
    while at < len {
        let n = file.read(&mut scratch[at..len]).expect("read selected");
        assert!(n > 0, "short read of selected record");
        at += n;
    }
    file.close().expect("close selected");
    let RecordState::Committed(view) = classify_record(&scratch[..len], PAIR.magic) else {
        panic!("selected slot must classify committed");
    };
    assert_eq!(view.generation, generation);
    let decoded = SourceMetadata::decode(&view).expect("selected record must decode");
    Some((generation, decoded))
}

/// The publication invariant every cut point must satisfy: authority is
/// `old` or `new`, intact either way.
fn assert_old_or_new(disk: &SharedDisk, old: Option<u64>, new: u64, context: &str) {
    match reboot_authority(disk) {
        None => assert_eq!(old, None, "{context}: authority vanished"),
        Some((generation, decoded)) => {
            assert!(
                Some(generation) == old || generation == new,
                "{context}: selected generation {generation}, expected {old:?} or {new}"
            );
            // Content must match the generation it claims: a mixed record
            // (old body, new generation or vice versa) can never appear.
            assert_eq!(
                decoded,
                sample_metadata(generation),
                "{context}: selected record content does not match its generation"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[test]
fn publish_then_replace_alternates_slots() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let first = publish_generation(&root, 1).expect("first publication");
    let second = publish_generation(&root, 2).expect("second publication");
    let third = publish_generation(&root, 3).expect("third publication");
    assert_ne!(first, second, "A/B must alternate");
    assert_eq!(first, third, "third generation overwrites the oldest slot");

    drop(root);
    drop(mgr);
    let (generation, decoded) = reboot_authority(&disk).expect("authority exists");
    assert_eq!(generation, 3);
    assert_eq!(decoded.source_generation, 3);
}

#[test]
fn stale_or_equal_generation_is_refused() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    publish_generation(&root, 2).expect("baseline");
    assert_eq!(publish_generation(&root, 2), Err(PublishError::BadInput));
    assert_eq!(publish_generation(&root, 1), Err(PublishError::BadInput));
}

#[test]
fn revalidation_refusal_leaves_prepared_not_committed() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    publish_generation(&root, 1).expect("baseline");

    let (buf, sealed) = sealed_metadata(2);
    let mut scratch = [0u8; SCRATCH_BYTES];
    let refused = publish_record(&root, PAIR, &buf, &sealed, 0, &mut scratch, || false);
    assert_eq!(refused, Err(PublishError::RevalidationRefused));

    // The candidate slot holds a prepared generation 2; authority is still 1.
    drop(root);
    drop(mgr);
    let (generation, _) = reboot_authority(&disk).expect("authority survives refusal");
    assert_eq!(generation, 1);

    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut scratch = [0u8; SCRATCH_BYTES];
    let summaries = [
        summarize_slot(&root, PAIR.names[0], PAIR.magic, &mut scratch).unwrap(),
        summarize_slot(&root, PAIR.names[1], PAIR.magic, &mut scratch).unwrap(),
    ];
    assert!(
        summaries.contains(&SlotSummary::Prepared { generation: 2 }),
        "refused candidate stays prepared for cleanup, got {summaries:?}"
    );
}

// ---------------------------------------------------------------------------
// Injected I/O failures
// ---------------------------------------------------------------------------

#[test]
fn every_write_failure_preserves_authority() {
    // Arm an exactly-once write failure at every write index a publication
    // performs, one run per index; the publication must fail cleanly and a
    // reboot must still select the old generation.
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, 1).expect("baseline");
    }
    let base = disk.snapshot();

    let mut fault_index = 0u32;
    loop {
        disk.restore_cut(&base, &[], 0, 0);
        disk.fault.fail_write_in.set(Some(fault_index));
        let outcome = {
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            publish_generation(&root, 2)
        };
        let fault_fired = disk.fault.fail_write_in.get().is_none();
        if !fault_fired {
            // The armed index is beyond the publication's writes: the run
            // above was fault-free and must have succeeded.
            outcome.expect("fault-free run succeeds");
            assert!(fault_index > 0, "publication must write at least once");
            break;
        }
        assert!(
            outcome.is_err(),
            "write fault {fault_index} did not surface as an error"
        );
        assert_old_or_new(&disk, Some(1), 2, &format!("write fault {fault_index}"));
        fault_index += 1;
    }
}

#[test]
fn every_read_failure_preserves_authority() {
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, 1).expect("baseline");
    }
    let base = disk.snapshot();

    let mut fault_index = 0u32;
    loop {
        disk.restore_cut(&base, &[], 0, 0);
        disk.fault.fail_read_in.set(Some(fault_index));
        // Mounting itself reads, so the armed fault can fire inside the
        // harness's own `expect`s before publish starts; both shapes model
        // the same thing (a read failed mid-operation) and both must leave
        // committed authority intact.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            publish_generation(&root, 2)
        }));
        let fault_fired = disk.fault.fail_read_in.get().is_none();
        disk.fault.fail_read_in.set(None);
        if !fault_fired {
            outcome
                .expect("fault-free run must not panic")
                .expect("fault-free run succeeds");
            break;
        }
        // Whether the mount panicked (an `expect` in the harness) or publish
        // returned an error — or even tolerated the fault and succeeded —
        // committed authority must select as old or new, intact.
        assert_old_or_new(&disk, Some(1), 2, &format!("read fault {fault_index}"));
        fault_index += 1;
    }
}

// ---------------------------------------------------------------------------
// Simulated power cuts: every write boundary, plus torn sectors
// ---------------------------------------------------------------------------

/// Cut points around a *first* publication: authority must be absent or the
/// new generation — and never a prepared record promoted to authority.
#[test]
fn first_publication_survives_every_cut_point() {
    let disk = new_card();
    let base = disk.snapshot();
    disk.start_logging();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, 1).expect("logged publication");
    }
    let log = disk.take_log();
    assert!(log.len() > 4, "publication should write several blocks");

    for cut in 0..=log.len() {
        disk.restore_cut(&base, &log, cut, 0);
        assert_old_or_new(&disk, None, 1, &format!("clean cut after {cut} writes"));
        for torn in [1, 128, 511] {
            disk.restore_cut(&base, &log, cut, torn);
            assert_old_or_new(&disk, None, 1, &format!("cut {cut} + {torn} torn bytes"));
        }
    }
}

/// Cut points around a *replacement* publication: authority must be the old
/// or the new generation at every boundary — never absent, never mixed.
#[test]
fn replacement_survives_every_cut_point() {
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, 1).expect("baseline");
    }
    let base = disk.snapshot();
    disk.start_logging();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, 2).expect("logged publication");
    }
    let log = disk.take_log();

    for cut in 0..=log.len() {
        disk.restore_cut(&base, &log, cut, 0);
        assert_old_or_new(&disk, Some(1), 2, &format!("clean cut after {cut} writes"));
        for torn in [1, 128, 511] {
            disk.restore_cut(&base, &log, cut, torn);
            assert_old_or_new(&disk, Some(1), 2, &format!("cut {cut} + {torn} torn bytes"));
        }
    }
}

/// The full lifecycle under cuts: publication interrupted at every point,
/// then *completed by a retry* — the resume path a reboot takes. After the
/// retry, authority must be the retried generation.
#[test]
fn interrupted_publication_retries_to_completion() {
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, 1).expect("baseline");
    }
    let base = disk.snapshot();
    disk.start_logging();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, 2).expect("logged publication");
    }
    let log = disk.take_log();

    for cut in 0..=log.len() {
        disk.restore_cut(&base, &log, cut, 0);
        let before = reboot_authority(&disk).map(|(generation, _)| generation);
        let retry = {
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            if before == Some(2) {
                // The cut landed after the commit point; the next generation
                // continues from 2.
                publish_generation(&root, 3)
            } else {
                publish_generation(&root, 2)
            }
        };
        retry.unwrap_or_else(|error| panic!("retry after cut {cut} failed: {error:?}"));
        let expected = if before == Some(2) { 3 } else { 2 };
        let (generation, _) = reboot_authority(&disk).expect("retried authority");
        assert_eq!(generation, expected, "cut {cut}: retry did not converge");
    }
}

// ---------------------------------------------------------------------------
// Hostile on-card states
// ---------------------------------------------------------------------------

#[test]
fn oversized_slot_file_is_corrupt_and_recoverable() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    // A slot file larger than any legitimate record (and larger than the
    // scratch buffer) must summarize as corrupt without being read whole,
    // and publication must recover the slot by overwriting it.
    {
        let file = root
            .open_file_in_dir(PAIR.names[0], Mode::ReadWriteCreateOrTruncate)
            .expect("create oversized file");
        let junk = [0xAAu8; 1024];
        for _ in 0..8 {
            file.write(&junk).expect("write junk");
        }
        file.close().expect("close junk");
    }
    let mut scratch = [0u8; SCRATCH_BYTES];
    assert_eq!(
        summarize_slot(&root, PAIR.names[0], PAIR.magic, &mut scratch),
        Ok(SlotSummary::Corrupt)
    );

    publish_generation(&root, 1).expect("publication over corrupt slot");
    drop(root);
    drop(mgr);
    let (generation, _) = reboot_authority(&disk).expect("authority");
    assert_eq!(generation, 1);
}

#[test]
fn ambiguous_equal_generations_refuse_selection_and_publication() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    // Manufacture the state A/B publication can never produce: both slots
    // committed at the same generation.
    let (buf, sealed) = sealed_metadata(1);
    let sector = encode_commit_sector(&sealed, 0).expect("sector");
    for name in PAIR.names {
        let file = root
            .open_file_in_dir(name, Mode::ReadWriteCreateOrTruncate)
            .expect("create dup");
        file.write(&buf[..sealed.padded_len]).expect("body");
        file.write(&sector).expect("sector");
        file.close().expect("close dup");
    }

    let mut scratch = [0u8; SCRATCH_BYTES];
    assert_eq!(
        select_authority(&root, PAIR, &mut scratch),
        Err(PublishError::AmbiguousAuthority)
    );
    assert_eq!(
        publish_generation(&root, 2),
        Err(PublishError::AmbiguousAuthority)
    );
}
