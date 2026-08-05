//! Shared harness for the source-store integration suites: the
//! fault-injecting, write-logging in-memory card, the FAT16 image it
//! carries, and the metadata-record fixtures both suites publish.
//!
//! The card model follows `reader-cache/tests/publish_faults.rs` (itself
//! copied from `upload-store/tests/transaction.rs`); within this crate the
//! two integration-test binaries share it through this module instead of a
//! fourth copy.

#![allow(dead_code)] // Each test binary uses a different subset.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Directory, Timestamp, VolumeIdx, VolumeManager,
};
use source_store::bodies::{
    DisplayLabel, OperationKind, SourceMetadata, SourceOrigin, UnmanagedName, BOOK_TOKEN_BYTES,
    LOGICAL_BOOK_ID_BYTES, REQUEST_ID_BYTES, SHA256_BYTES, SOURCE_METADATA_LOGICAL_BYTES,
    SOURCE_METADATA_MAGIC, SOURCE_METADATA_SCHEMA,
};
use source_store::publish::{publish_record, PublishError, SlotPair};
use source_store::record::{record_file_len, seal_body, SealedBody};

pub const BLOCK_BYTES: usize = 512;
/// 16 MiB card: big enough that fatfs picks FAT16 and small enough to stay fast.
pub const DISK_BLOCKS: u32 = 32 * 1024;
pub const PART_START_BLOCK: u32 = 64;

// ---------------------------------------------------------------------------
// Fault-injecting, write-logging in-memory block device
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskError;

/// Arms exactly-once faults: `fail_write_in = Some(n)` fails the (n+1)th
/// subsequent write call, then disarms. Exactly-once because the paths under
/// test issue their own I/O after the fault; a sticky fault would conflate
/// "this write failed" with "the card is gone".
#[derive(Default)]
pub struct FaultPlan {
    pub fail_write_in: Cell<Option<u32>>,
    pub fail_read_in: Cell<Option<u32>>,
}

impl FaultPlan {
    pub fn take_fault(counter: &Cell<Option<u32>>) -> bool {
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
pub struct LoggedWrite {
    pub block: u32,
    pub bytes: [u8; BLOCK_BYTES],
}

pub struct FaultyDisk {
    pub data: RefCell<Vec<u8>>,
    pub fault: FaultPlan,
    pub writes: Cell<u32>,
    /// When present, every block write is appended here (after the fault
    /// check, so only writes that actually landed are replayable).
    pub log: RefCell<Option<Vec<LoggedWrite>>>,
}

#[derive(Clone)]
pub struct SharedDisk(pub Rc<FaultyDisk>);

impl std::ops::Deref for SharedDisk {
    type Target = FaultyDisk;
    fn deref(&self) -> &FaultyDisk {
        &self.0
    }
}

impl SharedDisk {
    pub fn snapshot(&self) -> Vec<u8> {
        self.data.borrow().clone()
    }

    pub fn start_logging(&self) {
        *self.log.borrow_mut() = Some(Vec::new());
    }

    pub fn take_log(&self) -> Vec<LoggedWrite> {
        self.log.borrow_mut().take().expect("logging was started")
    }

    /// Rebuild the card as it stood after `prefix` writes of `log` landed on
    /// `base`, then optionally merge the first `torn_bytes` of the next
    /// write into its old block — a torn sector.
    pub fn restore_cut(&self, base: &[u8], log: &[LoggedWrite], prefix: usize, torn_bytes: usize) {
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

pub fn format_disk() -> Vec<u8> {
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

pub struct StaticTime;

impl embedded_sdmmc::TimeSource for StaticTime {
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

pub type Mgr = VolumeManager<SharedDisk, StaticTime, 8, 8, 1>;
pub type Dir<'a> = Directory<'a, SharedDisk, StaticTime, 8, 8, 1>;

pub fn new_card() -> SharedDisk {
    SharedDisk(Rc::new(FaultyDisk {
        data: RefCell::new(format_disk()),
        fault: FaultPlan::default(),
        writes: Cell::new(0),
        log: RefCell::new(None),
    }))
}

pub fn open_mgr(disk: &SharedDisk) -> Mgr {
    VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000)
}

pub fn open_root(mgr: &Mgr) -> Dir<'_> {
    let volume = mgr.open_volume(VolumeIdx(0)).expect("open volume");
    let raw_root = mgr
        .open_root_dir(volume.to_raw_volume())
        .expect("open root");
    Directory::new(raw_root, mgr)
}

// ---------------------------------------------------------------------------
// Metadata-record fixtures
// ---------------------------------------------------------------------------

/// A metadata body whose source generation tracks the caller's `generation`
/// and whose token defaults to a generation-derived pattern, so a selected
/// record proves which publication it came from by content.
pub fn sample_metadata(source_generation: u64) -> SourceMetadata {
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
        unmanaged_name: UnmanagedName::none(),
    }
}

/// Seal an arbitrary metadata body into a publishable buffer under the
/// given record (framing) generation.
pub fn sealed_record(meta: &SourceMetadata, record_generation: u64) -> (Vec<u8>, SealedBody) {
    let mut buf = vec![0u8; record_file_len(SOURCE_METADATA_LOGICAL_BYTES).unwrap()];
    let logical = meta.encode_into(&mut buf).expect("encode fixture metadata");
    let sealed = seal_body(
        SOURCE_METADATA_MAGIC,
        SOURCE_METADATA_SCHEMA,
        record_generation,
        logical,
        &mut buf,
    )
    .expect("seal fixture metadata");
    (buf, sealed)
}

/// Seal `sample_metadata(generation)` with record generation equal to the
/// source generation, for legibility in selector assertions.
pub fn sealed_metadata(generation: u64) -> (Vec<u8>, SealedBody) {
    sealed_record(&sample_metadata(generation), generation)
}

/// Publish a metadata record into `pair` with matched record and source
/// generations.
pub fn publish_generation(
    root: &Dir<'_>,
    pair: SlotPair<'_>,
    generation: u64,
) -> Result<usize, PublishError> {
    let (buf, sealed) = sealed_metadata(generation);
    let mut scratch = [0u8; 4096];
    publish_record(root, pair, &buf, &sealed, 0, &mut scratch, || true)
}

/// Publish an arbitrary metadata body into `pair`.
pub fn publish_metadata(
    root: &Dir<'_>,
    pair: SlotPair<'_>,
    meta: &SourceMetadata,
    record_generation: u64,
) -> Result<usize, PublishError> {
    let (buf, sealed) = sealed_record(meta, record_generation);
    let mut scratch = [0u8; 4096];
    publish_record(root, pair, &buf, &sealed, 0, &mut scratch, || true)
}
