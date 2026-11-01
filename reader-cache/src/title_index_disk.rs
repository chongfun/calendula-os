//! Rebuildable on-card order of catalog row references.
//!
//! The file has two scratch runs of two-byte catalog row numbers. Its header
//! is invalid while either run is being written. Publication flushes the
//! finished run before writing the header, which binds that run to a SHA-256
//! of the complete catalog and a SHA-256 of the run itself.

use core::cmp::Ordering;
use embedded_sdmmc::{BlockDevice, Directory, File, Mode, TimeSource};
use proto::catalog::{
    catalog_file_len, decode_catalog_header, decode_catalog_record, CATALOG_HEADER_BYTES,
    CATALOG_RECORD_BYTES,
};
use proto::source::SourceHasher;

use crate::title_browse::{compare_catalog_candidates, Candidate, Membership};
use crate::title_index::{self, IndexStorage};

const INDEX_FILE: &str = "TITLEIDX.BIN";
const INDEX_FILE_ALT: &str = "TITLEID2.BIN";
const MAGIC: &[u8; 4] = b"TIX3";
pub const SNAPSHOT_BYTES: usize = 104;
const HEADER_BYTES: usize = SNAPSHOT_BYTES;
const HASH_BYTES: usize = 32;
const PREFIX_BYTES: usize = 16;
const IO_BYTES: usize = 256;

struct ReadCache {
    run: bool,
    start: u16,
    rows: u16,
    bytes: [u8; IO_BYTES],
}

impl Default for ReadCache {
    fn default() -> Self {
        Self {
            run: false,
            start: 0,
            rows: 0,
            bytes: [0; IO_BYTES],
        }
    }
}

/// A derived index was absent, malformed or no longer matches the catalog.
/// A card I/O failure is kept separate from those rebuildable outcomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexError {
    Missing,
    Invalid,
    Device,
}

/// A previously checked index and the exact catalog bytes it described.
/// Consumers must retire this token before any catalog mutation is attempted,
/// including a write that can land even if it reports failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    header: [u8; HEADER_BYTES],
}

impl Snapshot {
    pub fn count(&self) -> u16 {
        u16::from_le_bytes([self.header[4], self.header[5]])
    }

    fn run(&self) -> bool {
        self.header[6] != 0
    }

    pub fn slot(&self) -> u8 {
        self.header[7]
    }

    /// Opaque storage for the reader store's bounded, in-memory token.
    pub fn into_bytes(self) -> [u8; HEADER_BYTES] {
        self.header
    }

    /// Restore a token previously obtained from [`validate`]. This does not
    /// validate the catalog; the owner is responsible for retiring stale
    /// tokens before catalog writes or replacement.
    pub fn from_bytes(header: [u8; HEADER_BYTES]) -> Option<Self> {
        (&header[..4] == MAGIC && header[6] <= 1 && header[7] <= 1).then_some(Self { header })
    }
}

fn index_file(slot: u8) -> &'static str {
    if slot == 0 {
        INDEX_FILE
    } else {
        INDEX_FILE_ALT
    }
}

fn read_exact<D: BlockDevice, T: TimeSource, const MD: usize, const MF: usize, const MV: usize>(
    file: &File<'_, D, T, MD, MF, MV>,
    mut bytes: &mut [u8],
) -> Result<(), IndexError> {
    while !bytes.is_empty() {
        let n = file.read(bytes).map_err(|_| IndexError::Device)?;
        if n == 0 {
            return Err(IndexError::Invalid);
        }
        let remaining = bytes;
        bytes = &mut remaining[n..];
    }
    Ok(())
}

fn digest_range<
    D: BlockDevice,
    T: TimeSource,
    const MD: usize,
    const MF: usize,
    const MV: usize,
>(
    file: &File<'_, D, T, MD, MF, MV>,
    offset: u32,
    len: usize,
) -> Result<[u8; HASH_BYTES], IndexError> {
    file.seek_from_start(offset)
        .map_err(|_| IndexError::Device)?;
    let mut hash = SourceHasher::new();
    let mut buffer = [0u8; 256];
    let mut left = len;
    while left != 0 {
        let take = left.min(buffer.len());
        read_exact(file, &mut buffer[..take])?;
        hash.update(&buffer[..take]);
        left -= take;
    }
    Ok(*hash.finish().sha256())
}

fn run_offset(run: bool, count: u16) -> u32 {
    HEADER_BYTES as u32 + if run { u32::from(count) * 2 } else { 0 }
}

fn prefix_offset(run: bool, count: u16) -> u32 {
    HEADER_BYTES as u32
        + u32::from(count) * 4
        + if run {
            (u32::from(count) + 1) * PREFIX_BYTES as u32
        } else {
            0
        }
}

fn index_len(count: u16) -> usize {
    HEADER_BYTES + usize::from(count) * 4 + (usize::from(count) + 1) * PREFIX_BYTES * 2
}

struct DiskIndex<
    'a,
    'f,
    D: BlockDevice,
    T: TimeSource,
    const MD: usize,
    const MF: usize,
    const MV: usize,
> {
    catalog: &'a File<'f, D, T, MD, MF, MV>,
    index: &'a File<'f, D, T, MD, MF, MV>,
    count: u16,
    catalog_hash: [u8; HASH_BYTES],
    pending: [u8; IO_BYTES],
    pending_start: u16,
    pending_len: usize,
    pending_run: bool,
    reads: [ReadCache; 2],
    next_read: usize,
    left: Option<Candidate>,
    right: Option<Candidate>,
}

impl<D: BlockDevice, T: TimeSource, const MD: usize, const MF: usize, const MV: usize>
    DiskIndex<'_, '_, D, T, MD, MF, MV>
{
    fn new<'a, 'f>(
        catalog: &'a File<'f, D, T, MD, MF, MV>,
        index: &'a File<'f, D, T, MD, MF, MV>,
        count: u16,
        catalog_hash: [u8; HASH_BYTES],
    ) -> DiskIndex<'a, 'f, D, T, MD, MF, MV> {
        DiskIndex {
            catalog,
            index,
            count,
            catalog_hash,
            pending: [0; IO_BYTES],
            pending_start: 0,
            pending_len: 0,
            pending_run: false,
            reads: [ReadCache::default(), ReadCache::default()],
            next_read: 0,
            left: None,
            right: None,
        }
    }

    fn flush_pending(&mut self) -> Result<(), IndexError> {
        if self.pending_len != 0 {
            self.index
                .seek_from_start(
                    run_offset(self.pending_run, self.count) + u32::from(self.pending_start) * 2,
                )
                .map_err(|_| IndexError::Device)?;
            self.index
                .write(&self.pending[..self.pending_len])
                .map_err(|_| IndexError::Device)?;
            self.pending_len = 0;
            self.reads[0].rows = 0;
            self.reads[1].rows = 0;
        }
        Ok(())
    }

    fn candidate(&self, row: u16) -> Result<Candidate, IndexError> {
        if row >= self.count {
            return Err(IndexError::Invalid);
        }
        let offset = CATALOG_HEADER_BYTES + usize::from(row) * CATALOG_RECORD_BYTES;
        self.catalog
            .seek_from_start(offset as u32)
            .map_err(|_| IndexError::Device)?;
        let mut bytes = [0u8; CATALOG_RECORD_BYTES];
        read_exact(self.catalog, &mut bytes)?;
        Candidate::from_record(&decode_catalog_record(&bytes), row).ok_or(IndexError::Invalid)
    }
}

impl<D: BlockDevice, T: TimeSource, const MD: usize, const MF: usize, const MV: usize> IndexStorage
    for DiskIndex<'_, '_, D, T, MD, MF, MV>
{
    type Error = IndexError;

    fn begin(&mut self, count: u16) -> Result<(), IndexError> {
        if count != self.count {
            return Err(IndexError::Invalid);
        }
        let previous_len = self.index.length() as usize;
        if previous_len > index_len(u16::MAX) {
            return Err(IndexError::Invalid);
        }
        self.index
            .seek_from_start(0)
            .map_err(|_| IndexError::Device)?;
        self.index
            .write(&[0u8; HEADER_BYTES])
            .map_err(|_| IndexError::Device)?;
        // Retire the old header before touching either run. Never truncate an
        // existing file: an interrupted FAT truncate can leave its directory
        // entry naming clusters that have already been freed and reused.
        self.index.flush().map_err(|_| IndexError::Device)?;
        let zeros = [0u8; 256];
        let needed = index_len(count);
        let current = self.index.length() as usize;
        self.index
            .seek_from_start(current as u32)
            .map_err(|_| IndexError::Device)?;
        let mut left = needed.saturating_sub(current);
        while left > 0 {
            let take = left.min(zeros.len());
            self.index
                .write(&zeros[..take])
                .map_err(|_| IndexError::Device)?;
            left -= take;
        }
        self.index.flush().map_err(|_| IndexError::Device)
    }

    fn read(&mut self, run: bool, position: u16) -> Result<u16, IndexError> {
        if position >= self.count {
            return Err(IndexError::Invalid);
        }
        if self.pending_len != 0 && self.pending_run == run {
            self.flush_pending()?;
        }
        let found = self.reads.iter().position(|cache| {
            cache.rows != 0
                && cache.run == run
                && position >= cache.start
                && u32::from(position) < u32::from(cache.start) + u32::from(cache.rows)
        });
        let slot = if let Some(slot) = found {
            slot
        } else {
            let slot = self.next_read;
            self.next_read = (self.next_read + 1) % self.reads.len();
            let start = position / (IO_BYTES as u16 / 2) * (IO_BYTES as u16 / 2);
            let rows = (self.count - start).min(IO_BYTES as u16 / 2);
            self.index
                .seek_from_start(run_offset(run, self.count) + u32::from(start) * 2)
                .map_err(|_| IndexError::Device)?;
            read_exact(
                self.index,
                &mut self.reads[slot].bytes[..usize::from(rows) * 2],
            )?;
            self.reads[slot].run = run;
            self.reads[slot].start = start;
            self.reads[slot].rows = rows;
            slot
        };
        let offset = usize::from(position - self.reads[slot].start) * 2;
        let bytes = [
            self.reads[slot].bytes[offset],
            self.reads[slot].bytes[offset + 1],
        ];
        let row = u16::from_le_bytes(bytes);
        if row >= self.count {
            return Err(IndexError::Invalid);
        }
        Ok(row)
    }

    fn check(&mut self, row: u16) -> Result<(), IndexError> {
        self.candidate(row).map(|_| ())
    }

    fn write(&mut self, run: bool, position: u16, row: u16) -> Result<(), IndexError> {
        if position >= self.count || row >= self.count {
            return Err(IndexError::Invalid);
        }
        if self.pending_len != 0
            && (self.pending_run != run
                || u32::from(self.pending_start) + (self.pending_len / 2) as u32
                    != u32::from(position))
        {
            self.flush_pending()?;
        }
        if self.pending_len == 0 {
            self.pending_run = run;
            self.pending_start = position;
        }
        self.pending[self.pending_len..self.pending_len + 2].copy_from_slice(&row.to_le_bytes());
        self.pending_len += 2;
        if self.pending_len == self.pending.len() {
            self.flush_pending()?;
        }
        Ok(())
    }

    fn compare(&mut self, left: u16, right: u16) -> Result<Ordering, IndexError> {
        if self
            .left
            .as_ref()
            .is_none_or(|candidate| candidate.catalog_row != left)
        {
            self.left = Some(self.candidate(left)?);
        }
        if self
            .right
            .as_ref()
            .is_none_or(|candidate| candidate.catalog_row != right)
        {
            self.right = Some(self.candidate(right)?);
        }
        match (&self.left, &self.right) {
            (Some(left), Some(right)) => Ok(compare_catalog_candidates(left, right)),
            _ => Err(IndexError::Invalid),
        }
    }

    fn publish(&mut self, run: bool, count: u16) -> Result<(), IndexError> {
        if count != self.count {
            return Err(IndexError::Invalid);
        }
        self.flush_pending()?;
        self.index.flush().map_err(|_| IndexError::Device)?;
        // Prefix sums are a derived freshness hint, not identity. They let a
        // folder compare its live membership with a range of the index in
        // constant reads instead of rereading every catalog record on entry.
        let mut members = Membership::default();
        let mut buffer = [0u8; IO_BYTES];
        let mut used = PREFIX_BYTES; // The zero prefix precedes the first row.
        let mut written = 0usize;
        for position in 0..count {
            let row = self.read(run, position)?;
            let candidate = self.candidate(row)?;
            members.add(
                candidate.root,
                candidate.locator.as_str(),
                candidate.byte_size,
            );
            if used == buffer.len() {
                self.index
                    .seek_from_start(prefix_offset(run, count) + written as u32)
                    .map_err(|_| IndexError::Device)?;
                self.index
                    .write(&buffer[..used])
                    .map_err(|_| IndexError::Device)?;
                written += used;
                used = 0;
            }
            let (sum, xor) = members.totals();
            buffer[used..used + 8].copy_from_slice(&sum.to_le_bytes());
            buffer[used + 8..used + PREFIX_BYTES].copy_from_slice(&xor.to_le_bytes());
            used += PREFIX_BYTES;
        }
        self.index
            .seek_from_start(prefix_offset(run, count) + written as u32)
            .map_err(|_| IndexError::Device)?;
        self.index
            .write(&buffer[..used])
            .map_err(|_| IndexError::Device)?;
        self.index.flush().map_err(|_| IndexError::Device)?;
        let run_hash = digest_range(self.index, run_offset(run, count), usize::from(count) * 2)?;
        let prefix_hash = digest_range(
            self.index,
            prefix_offset(run, count),
            (usize::from(count) + 1) * PREFIX_BYTES,
        )?;
        let catalog_hash = digest_range(self.catalog, 0, catalog_file_len(count))?;
        if catalog_hash != self.catalog_hash {
            return Err(IndexError::Invalid);
        }
        let mut header = [0u8; HEADER_BYTES];
        header[..4].copy_from_slice(MAGIC);
        header[4..6].copy_from_slice(&count.to_le_bytes());
        header[6] = u8::from(run);
        header[8..40].copy_from_slice(&catalog_hash);
        header[40..72].copy_from_slice(&run_hash);
        header[72..104].copy_from_slice(&prefix_hash);
        self.index
            .seek_from_start(0)
            .map_err(|_| IndexError::Device)?;
        self.index.write(&header).map_err(|_| IndexError::Device)?;
        self.index.flush().map_err(|_| IndexError::Device)
    }
}

fn open_fault<E: core::error::Error>(error: embedded_sdmmc::Error<E>) -> IndexError {
    match error {
        embedded_sdmmc::Error::NotFound => IndexError::Missing,
        _ => IndexError::Device,
    }
}

fn catalog_count<
    D: BlockDevice,
    T: TimeSource,
    const MD: usize,
    const MF: usize,
    const MV: usize,
>(
    file: &File<'_, D, T, MD, MF, MV>,
) -> Result<u16, IndexError> {
    let mut header = [0; CATALOG_HEADER_BYTES];
    read_exact(file, &mut header)?;
    let count = decode_catalog_header(&header).ok_or(IndexError::Invalid)?;
    if file.length() as usize != catalog_file_len(count) {
        return Err(IndexError::Invalid);
    }
    Ok(count)
}

/// Build an index over a validated catalog. A failed build leaves a cache
/// with an invalid header; the catalog and physical library are untouched.
pub fn rebuild<D: BlockDevice, T: TimeSource, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<(), IndexError> {
    let dir = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .map_err(open_fault)?;
    let catalog = dir
        .open_file_in_dir(proto::cache::CATALOG_FILE, Mode::ReadOnly)
        .map_err(open_fault)?;
    let count = catalog_count(&catalog)?;
    let catalog_hash = digest_range(&catalog, 0, catalog_file_len(count))?;
    let index = dir
        .open_file_in_dir(INDEX_FILE, Mode::ReadWriteCreateOrAppend)
        .map_err(open_fault)?;
    let result = title_index::build(
        &mut DiskIndex::new(&catalog, &index, count, catalog_hash),
        count,
    );
    result?;
    index.close().map_err(|_| IndexError::Device)
}

/// Progress of a cooperative validation or rebuild. It owns no open files;
/// every slice closes its card session before yielding to the display task.
/// The owner must discard it before any catalog write, even a failed write.
pub struct IndexJob {
    phase: JobPhase,
    count: u16,
    header: [u8; HEADER_BYTES],
    catalog_hash: [u8; HASH_BYTES],
    offset: usize,
    hasher: SourceHasher,
    sorter: title_index::Sorter,
    run: bool,
    prefix_row: u16,
    prefix_started: bool,
    members: Membership,
    slot: u8,
    search_other: bool,
    fallback_slot: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobPhase {
    Inspect,
    InitialCatalog,
    ValidateRun,
    ValidatePrefix,
    Retire,
    Grow,
    Sort,
    Prefix,
    FinalCatalog,
    FinalRun,
    FinalPrefix,
    Publish,
}

impl Default for IndexJob {
    fn default() -> Self {
        Self {
            phase: JobPhase::Inspect,
            count: 0,
            header: [0; HEADER_BYTES],
            catalog_hash: [0; HASH_BYTES],
            offset: 0,
            hasher: SourceHasher::new(),
            sorter: title_index::Sorter::new(0),
            run: false,
            prefix_row: 0,
            prefix_started: false,
            members: Membership::default(),
            slot: 0,
            search_other: true,
            fallback_slot: 0,
        }
    }
}

impl IndexJob {
    /// Build away from an index that still serves a live title overlay.
    pub fn away_from(snapshot: Snapshot) -> Self {
        let slot = 1 - snapshot.slot();
        Self {
            slot,
            search_other: false,
            fallback_slot: slot,
            ..Self::default()
        }
    }

    fn try_other_or_rebuild(&mut self) {
        self.header = [0; HEADER_BYTES];
        if self.search_other {
            self.slot = 1;
            self.search_other = false;
            self.phase = JobPhase::Inspect;
        } else {
            self.slot = self.fallback_slot;
            self.phase = JobPhase::Retire;
        }
    }
    fn hash_slice<
        D: BlockDevice,
        T: TimeSource,
        const MD: usize,
        const MF: usize,
        const MV: usize,
    >(
        &mut self,
        file: &File<'_, D, T, MD, MF, MV>,
        start: u32,
        len: usize,
    ) -> Result<Option<[u8; HASH_BYTES]>, IndexError> {
        file.seek_from_start(start + self.offset as u32)
            .map_err(|_| IndexError::Device)?;
        let mut buffer = [0u8; IO_BYTES];
        let mut remaining = len.saturating_sub(self.offset).min(16 * 1024);
        while remaining != 0 {
            let take = remaining.min(buffer.len());
            read_exact(file, &mut buffer[..take])?;
            self.hasher.update(&buffer[..take]);
            self.offset += take;
            remaining -= take;
        }
        if self.offset == len {
            self.offset = 0;
            Ok(Some(*core::mem::take(&mut self.hasher).finish().sha256()))
        } else {
            Ok(None)
        }
    }

    /// Run one bounded card session. A successful result is a fully checked
    /// snapshot; `None` means more slices remain. Failure retires this job.
    pub fn step<
        D: BlockDevice,
        T: TimeSource,
        const MD: usize,
        const MF: usize,
        const MV: usize,
    >(
        &mut self,
        root: &Directory<'_, D, T, MD, MF, MV>,
    ) -> Result<Option<Snapshot>, IndexError> {
        let dir = root
            .open_dir(proto::cache::CACHE_ROOT_DIR)
            .map_err(open_fault)?;
        let catalog = dir
            .open_file_in_dir(proto::cache::CATALOG_FILE, Mode::ReadOnly)
            .map_err(open_fault)?;
        let count = catalog_count(&catalog)?;
        if self.phase == JobPhase::Inspect {
            self.count = count;
            self.sorter = title_index::Sorter::new(count);
            match dir.open_file_in_dir(index_file(self.slot), Mode::ReadOnly) {
                Ok(index) => {
                    let length_ok = (index.length() as usize) >= index_len(count)
                        && (index.length() as usize) <= index_len(u16::MAX);
                    if length_ok {
                        match read_exact(&index, &mut self.header) {
                            Ok(()) => {}
                            Err(IndexError::Invalid) => self.header = [0; HEADER_BYTES],
                            Err(error) => return Err(error),
                        }
                    }
                    if length_ok
                        && &self.header[..4] == MAGIC
                        && u16::from_le_bytes([self.header[4], self.header[5]]) == count
                        && self.header[6] <= 1
                        && self.header[7] == self.slot
                    {
                        self.run = self.header[6] != 0;
                    } else {
                        self.header = [0; HEADER_BYTES];
                    }
                }
                Err(embedded_sdmmc::Error::NotFound) => self.header = [0; HEADER_BYTES],
                Err(_) => return Err(IndexError::Device),
            }
            if &self.header[..4] != MAGIC && self.search_other {
                self.try_other_or_rebuild();
            } else {
                self.phase = JobPhase::InitialCatalog;
            }
            return Ok(None);
        }
        if count != self.count {
            return Err(IndexError::Invalid);
        }
        match self.phase {
            JobPhase::InitialCatalog | JobPhase::FinalCatalog => {
                if let Some(hash) = self.hash_slice(&catalog, 0, catalog_file_len(count))? {
                    if self.phase == JobPhase::InitialCatalog {
                        self.catalog_hash = hash;
                        if &self.header[..4] == MAGIC && self.header[8..40] == hash {
                            self.phase = JobPhase::ValidateRun;
                        } else {
                            self.try_other_or_rebuild();
                        }
                    } else if hash == self.catalog_hash {
                        self.header[8..40].copy_from_slice(&hash);
                        self.phase = JobPhase::FinalRun;
                    } else {
                        return Err(IndexError::Invalid);
                    }
                }
            }
            JobPhase::ValidateRun
            | JobPhase::ValidatePrefix
            | JobPhase::FinalRun
            | JobPhase::FinalPrefix => {
                let index = dir
                    .open_file_in_dir(index_file(self.slot), Mode::ReadOnly)
                    .map_err(open_fault)?;
                let is_run = matches!(self.phase, JobPhase::ValidateRun | JobPhase::FinalRun);
                let (offset, len) = if is_run {
                    (run_offset(self.run, count), usize::from(count) * 2)
                } else {
                    (
                        prefix_offset(self.run, count),
                        (usize::from(count) + 1) * PREFIX_BYTES,
                    )
                };
                if let Some(hash) = self.hash_slice(&index, offset, len)? {
                    self.phase = match self.phase {
                        JobPhase::ValidateRun if self.header[40..72] == hash => {
                            JobPhase::ValidatePrefix
                        }
                        JobPhase::ValidatePrefix if self.header[72..104] == hash => {
                            return Ok(Some(Snapshot {
                                header: self.header,
                            }));
                        }
                        JobPhase::ValidateRun | JobPhase::ValidatePrefix => {
                            self.try_other_or_rebuild();
                            self.phase
                        }
                        JobPhase::FinalRun => {
                            self.header[40..72].copy_from_slice(&hash);
                            JobPhase::FinalPrefix
                        }
                        JobPhase::FinalPrefix => {
                            self.header[72..104].copy_from_slice(&hash);
                            JobPhase::Publish
                        }
                        _ => return Err(IndexError::Invalid),
                    };
                }
            }
            JobPhase::Retire
            | JobPhase::Grow
            | JobPhase::Sort
            | JobPhase::Prefix
            | JobPhase::Publish => {
                let index = dir
                    .open_file_in_dir(index_file(self.slot), Mode::ReadWriteCreateOrAppend)
                    .map_err(open_fault)?;
                match self.phase {
                    JobPhase::Retire => {
                        if index.length() as usize > index_len(u16::MAX) {
                            return Err(IndexError::Invalid);
                        }
                        index.seek_from_start(0).map_err(|_| IndexError::Device)?;
                        index
                            .write(&[0; HEADER_BYTES])
                            .map_err(|_| IndexError::Device)?;
                        index.flush().map_err(|_| IndexError::Device)?;
                        self.phase = JobPhase::Grow;
                    }
                    JobPhase::Grow => {
                        let current = index.length() as usize;
                        let mut left = index_len(count).saturating_sub(current).min(4096);
                        index
                            .seek_from_start(current as u32)
                            .map_err(|_| IndexError::Device)?;
                        let zeros = [0; IO_BYTES];
                        while left != 0 {
                            let take = left.min(zeros.len());
                            index
                                .write(&zeros[..take])
                                .map_err(|_| IndexError::Device)?;
                            left -= take;
                        }
                        index.flush().map_err(|_| IndexError::Device)?;
                        if index.length() as usize >= index_len(count) {
                            self.phase = JobPhase::Sort;
                        }
                    }
                    JobPhase::Sort => {
                        let mut disk = DiskIndex::new(&catalog, &index, count, self.catalog_hash);
                        let done = self.sorter.step(&mut disk, 128)?;
                        disk.flush_pending()?;
                        index.flush().map_err(|_| IndexError::Device)?;
                        if let Some(run) = done {
                            self.run = run;
                            self.phase = JobPhase::Prefix;
                        }
                    }
                    JobPhase::Prefix => {
                        let mut disk = DiskIndex::new(&catalog, &index, count, self.catalog_hash);
                        let mut buffer = [0u8; IO_BYTES];
                        for _ in 0..4 {
                            if self.prefix_started && self.prefix_row == count {
                                break;
                            }
                            let mut used = 0;
                            let start =
                                usize::from(self.prefix_row) + usize::from(self.prefix_started);
                            if !self.prefix_started {
                                used = PREFIX_BYTES;
                                self.prefix_started = true;
                            }
                            while self.prefix_row < count && used + PREFIX_BYTES <= buffer.len() {
                                let row = disk.read(self.run, self.prefix_row)?;
                                let candidate = disk.candidate(row)?;
                                self.members.add(
                                    candidate.root,
                                    candidate.locator.as_str(),
                                    candidate.byte_size,
                                );
                                let (sum, xor) = self.members.totals();
                                buffer[used..used + 8].copy_from_slice(&sum.to_le_bytes());
                                buffer[used + 8..used + PREFIX_BYTES]
                                    .copy_from_slice(&xor.to_le_bytes());
                                used += PREFIX_BYTES;
                                self.prefix_row += 1;
                            }
                            index
                                .seek_from_start(
                                    prefix_offset(self.run, count) + (start * PREFIX_BYTES) as u32,
                                )
                                .map_err(|_| IndexError::Device)?;
                            index
                                .write(&buffer[..used])
                                .map_err(|_| IndexError::Device)?;
                        }
                        index.flush().map_err(|_| IndexError::Device)?;
                        if self.prefix_row == count {
                            self.phase = JobPhase::FinalCatalog;
                        }
                    }
                    JobPhase::Publish => {
                        self.header[..4].copy_from_slice(MAGIC);
                        self.header[4..6].copy_from_slice(&count.to_le_bytes());
                        self.header[6] = u8::from(self.run);
                        self.header[7] = self.slot;
                        index.seek_from_start(0).map_err(|_| IndexError::Device)?;
                        index.write(&self.header).map_err(|_| IndexError::Device)?;
                        index.flush().map_err(|_| IndexError::Device)?;
                        index.close().map_err(|_| IndexError::Device)?;
                        return Ok(Some(Snapshot {
                            header: self.header,
                        }));
                    }
                    _ => return Err(IndexError::Invalid),
                }
                index.close().map_err(|_| IndexError::Device)?;
            }
            JobPhase::Inspect => return Err(IndexError::Invalid),
        }
        Ok(None)
    }
}

/// Validate the file and the catalog it was built against. The returned run
/// number and count may be used only while the catalog remains unchanged.
pub fn validate<
    D: BlockDevice,
    T: TimeSource,
    const MD: usize,
    const MF: usize,
    const MV: usize,
>(
    root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Snapshot, IndexError> {
    match validate_slot(root, 0) {
        Ok(snapshot) => Ok(snapshot),
        Err(first @ (IndexError::Missing | IndexError::Invalid)) => match validate_slot(root, 1) {
            Err(IndexError::Missing) => Err(first),
            result => result,
        },
        Err(error) => Err(error),
    }
}

fn validate_slot<
    D: BlockDevice,
    T: TimeSource,
    const MD: usize,
    const MF: usize,
    const MV: usize,
>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    slot: u8,
) -> Result<Snapshot, IndexError> {
    let dir = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .map_err(open_fault)?;
    let catalog = dir
        .open_file_in_dir(proto::cache::CATALOG_FILE, Mode::ReadOnly)
        .map_err(open_fault)?;
    let count = catalog_count(&catalog)?;
    let index = dir
        .open_file_in_dir(index_file(slot), Mode::ReadOnly)
        .map_err(open_fault)?;
    let mut header = [0; HEADER_BYTES];
    read_exact(&index, &mut header)?;
    if &header[..4] != MAGIC
        || u16::from_le_bytes([header[4], header[5]]) != count
        || header[6] > 1
        || header[7] != slot
        || (index.length() as usize) < index_len(count)
        || index.length() as usize > index_len(u16::MAX)
    {
        return Err(IndexError::Invalid);
    }
    let run = header[6] != 0;
    if header[8..40] != digest_range(&catalog, 0, catalog_file_len(count))?
        || header[40..72] != digest_range(&index, run_offset(run, count), usize::from(count) * 2)?
        || header[72..104]
            != digest_range(
                &index,
                prefix_offset(run, count),
                (usize::from(count) + 1) * PREFIX_BYTES,
            )?
    {
        return Err(IndexError::Invalid);
    }
    Ok(Snapshot { header })
}

/// A short-lived view over the sorted row references and catalog.
pub struct Reader<
    'a,
    'f,
    D: BlockDevice,
    T: TimeSource,
    const MD: usize,
    const MF: usize,
    const MV: usize,
> {
    catalog: &'a File<'f, D, T, MD, MF, MV>,
    index: &'a File<'f, D, T, MD, MF, MV>,
    snapshot: Snapshot,
    overlays: &'a [TitleOverlay],
}

/// One confirmed title write moves a row within the preceding order. The
/// physical locator and membership stay unchanged. These moves are volatile:
/// after reboot the old catalog digest does not validate and the index rebuilds.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct TitleOverlay {
    pub row: u16,
    pub old_position: u16,
    pub new_position: u16,
}

impl<D: BlockDevice, T: TimeSource, const MD: usize, const MF: usize, const MV: usize>
    Reader<'_, '_, D, T, MD, MF, MV>
{
    /// Read a catalog row by its absolute row number, checking the bound.
    pub fn catalog_row(&self, row: u16) -> Result<Candidate, IndexError> {
        if row >= self.snapshot.count() {
            return Err(IndexError::Invalid);
        }
        let offset = CATALOG_HEADER_BYTES + usize::from(row) * CATALOG_RECORD_BYTES;
        self.catalog
            .seek_from_start(offset as u32)
            .map_err(|_| IndexError::Device)?;
        let mut record = [0; CATALOG_RECORD_BYTES];
        read_exact(self.catalog, &mut record)?;
        Candidate::from_record(&decode_catalog_record(&record), row).ok_or(IndexError::Invalid)
    }

    /// Resolve one sorted position to its catalog candidate.
    pub fn row(&self, position: u16) -> Result<Candidate, IndexError> {
        let count = self.snapshot.count();
        if position >= count {
            return Err(IndexError::Invalid);
        }
        let mut physical = position;
        for overlay in self.overlays.iter().rev() {
            if physical == overlay.new_position {
                return self.catalog_row(overlay.row);
            }
            physical -= u16::from(physical > overlay.new_position);
            physical += u16::from(physical >= overlay.old_position);
        }
        self.index
            .seek_from_start(run_offset(self.snapshot.run(), count) + u32::from(physical) * 2)
            .map_err(|_| IndexError::Device)?;
        let mut row = [0; 2];
        read_exact(self.index, &mut row)?;
        let row = u16::from_le_bytes(row);
        if row >= count {
            return Err(IndexError::Invalid);
        }
        self.catalog_row(row)
    }

    /// Locate an exact catalog candidate inside an already validated folder
    /// range. Equal physical keys are refused rather than guessed between.
    pub fn position(
        &self,
        range: (u16, u16),
        target: &Candidate,
    ) -> Result<Option<u16>, IndexError> {
        if range.0 > range.1 || range.1 > self.snapshot.count() {
            return Err(IndexError::Invalid);
        }
        let (mut low, mut high) = (usize::from(range.0), usize::from(range.1));
        while low < high {
            let mid = low + (high - low) / 2;
            if compare_catalog_candidates(&self.row(mid as u16)?, target) == Ordering::Less {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        if low == usize::from(range.1) {
            return Ok(None);
        }
        let found = self.row(low as u16)?;
        if compare_catalog_candidates(&found, target) != Ordering::Equal
            || found.root != target.root
            || found.locator != target.locator
            || found.byte_size != target.byte_size
        {
            return Ok(None);
        }
        if low + 1 < usize::from(range.1)
            && compare_catalog_candidates(&self.row((low + 1) as u16)?, target) == Ordering::Equal
        {
            return Err(IndexError::Invalid);
        }
        Ok(Some(low as u16))
    }

    /// The half-open range of the exact folder, in catalog-wide sort order.
    pub fn folder_range(
        &self,
        path: &proto::library_path::LibraryPath,
    ) -> Result<(u16, u16), IndexError> {
        fn parent(candidate: &Candidate) -> &str {
            candidate
                .locator
                .rsplit_once('/')
                .map_or("", |(parent, _)| parent)
        }
        let mut low = 0usize;
        let mut high = usize::from(self.snapshot.count());
        while low < high {
            let mid = low + (high - low) / 2;
            if parent(&self.row(mid as u16)?) < path.as_str() {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let start = low;
        high = usize::from(self.snapshot.count());
        while low < high {
            let mid = low + (high - low) / 2;
            if parent(&self.row(mid as u16)?) <= path.as_str() {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok((start as u16, low as u16))
    }

    /// Membership of one contiguous folder range. Prefix bytes are checked
    /// before use, so a corrupt derived cache cannot bless a stale directory.
    pub fn membership(&self, range: (u16, u16)) -> Result<Membership, IndexError> {
        let count = self.snapshot.count();
        if range.0 > range.1 || range.1 > count {
            return Err(IndexError::Invalid);
        }
        let base = prefix_offset(self.snapshot.run(), count);
        if self.snapshot.header[72..104]
            != digest_range(self.index, base, (usize::from(count) + 1) * PREFIX_BYTES)?
        {
            return Err(IndexError::Invalid);
        }
        let mut first = [0u8; PREFIX_BYTES];
        let mut last = [0u8; PREFIX_BYTES];
        self.index
            .seek_from_start(base + u32::from(range.0) * PREFIX_BYTES as u32)
            .map_err(|_| IndexError::Device)?;
        read_exact(self.index, &mut first)?;
        self.index
            .seek_from_start(base + u32::from(range.1) * PREFIX_BYTES as u32)
            .map_err(|_| IndexError::Device)?;
        read_exact(self.index, &mut last)?;
        let word = |bytes: &[u8]| -> Result<u64, IndexError> {
            Ok(u64::from_le_bytes(
                bytes.try_into().map_err(|_| IndexError::Invalid)?,
            ))
        };
        Ok(Membership::from_totals(
            u32::from(range.1 - range.0),
            word(&last[..8])?.wrapping_sub(word(&first[..8])?),
            word(&last[8..])? ^ word(&first[8..])?,
        ))
    }
}

/// Open an index previously validated in this catalog generation. The
/// compact run is checked on every open; the caller must invalidate the token
/// before any catalog write, including writes that report failure.
pub fn with_snapshot<
    D: BlockDevice,
    T: TimeSource,
    R,
    const MD: usize,
    const MF: usize,
    const MV: usize,
>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    snapshot: Snapshot,
    f: impl FnOnce(&Reader<'_, '_, D, T, MD, MF, MV>) -> Result<R, IndexError>,
) -> Result<R, IndexError> {
    with_snapshot_overlay(root, snapshot, &[], f)
}

/// Read through moves established by the catalog's single writer after
/// confirmed title-field writes. Never reconstruct these moves from disk or
/// keep them after an unconfirmed write, a rescan, or another catalog change.
pub fn with_snapshot_overlay<
    D: BlockDevice,
    T: TimeSource,
    R,
    const MD: usize,
    const MF: usize,
    const MV: usize,
>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    snapshot: Snapshot,
    overlays: &[TitleOverlay],
    f: impl FnOnce(&Reader<'_, '_, D, T, MD, MF, MV>) -> Result<R, IndexError>,
) -> Result<R, IndexError> {
    let dir = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .map_err(open_fault)?;
    let catalog = dir
        .open_file_in_dir(proto::cache::CATALOG_FILE, Mode::ReadOnly)
        .map_err(open_fault)?;
    let count = catalog_count(&catalog)?;
    if count != snapshot.count() {
        return Err(IndexError::Invalid);
    }
    let index = dir
        .open_file_in_dir(index_file(snapshot.slot()), Mode::ReadOnly)
        .map_err(open_fault)?;
    let mut header = [0; HEADER_BYTES];
    read_exact(&index, &mut header)?;
    if header != snapshot.header
        || (index.length() as usize) < index_len(count)
        || index.length() as usize > index_len(u16::MAX)
        || header[40..72]
            != digest_range(
                &index,
                run_offset(snapshot.run(), count),
                usize::from(count) * 2,
            )?
    {
        return Err(IndexError::Invalid);
    }
    if overlays.iter().any(|overlay| {
        overlay.row >= count || overlay.old_position >= count || overlay.new_position >= count
    }) {
        return Err(IndexError::Invalid);
    }
    f(&Reader {
        catalog: &catalog,
        index: &index,
        snapshot,
        overlays,
    })
}

/// Recover the new rank after another successful title-field write. The
/// caller must own the catalog writer and know that all mutations since
/// `snapshot` are the supplied confirmed title moves plus this one. A failed
/// write cannot establish that proof.
pub fn title_overlay<
    D: BlockDevice,
    T: TimeSource,
    const MD: usize,
    const MF: usize,
    const MV: usize,
>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    snapshot: Snapshot,
    overlays: &[TitleOverlay],
    changed_row: u16,
) -> Result<TitleOverlay, IndexError> {
    with_snapshot_overlay(root, snapshot, overlays, |reader| {
        let count = snapshot.count();
        let target = reader.catalog_row(changed_row)?;
        let mut bytes = [0u8; IO_BYTES];
        let mut position = 0u16;
        let mut previous = None;
        reader
            .index
            .seek_from_start(run_offset(snapshot.run(), count))
            .map_err(|_| IndexError::Device)?;
        while position < count {
            let rows = usize::from(count - position).min(IO_BYTES / 2);
            read_exact(reader.index, &mut bytes[..rows * 2])?;
            for &pair in bytes[..rows * 2].as_chunks::<2>().0 {
                let row = u16::from_le_bytes(pair);
                if row >= count {
                    return Err(IndexError::Invalid);
                }
                if row == changed_row && previous.replace(position).is_some() {
                    return Err(IndexError::Invalid);
                }
                position += 1;
            }
        }
        let mut old_position = previous.ok_or(IndexError::Invalid)?;
        for overlay in overlays {
            if old_position == overlay.old_position {
                old_position = overlay.new_position;
            } else {
                old_position -= u16::from(old_position > overlay.old_position);
                old_position += u16::from(old_position >= overlay.new_position);
            }
        }
        let mut low = 0usize;
        let mut high = usize::from(count) - 1;
        while low < high {
            let middle = low + (high - low) / 2;
            let physical = middle + usize::from(middle >= usize::from(old_position));
            if compare_catalog_candidates(&reader.row(physical as u16)?, &target) == Ordering::Less
            {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(TitleOverlay {
            row: changed_row,
            old_position,
            new_position: low as u16,
        })
    })
}
