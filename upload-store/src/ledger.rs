//! The library ledger: the durable record of which [`BookId`] each physical
//! copy on the card carries.
//!
//! `CATALOG.BIN` is rebuilt from the card whenever it stops matching it, and
//! that is fine for everything in it except an id, which the card cannot
//! give back. So ids live here, in `/READER/LEDGERA.BIN` and `LEDGERB.BIN`,
//! and the catalog only caches them.
//!
//! One generation is one whole file: a header, then one fixed-size record
//! per adopted copy, each carrying its own checksum. A rewrite goes to the
//! side that is not live. Records go down first under a placeholder header
//! and the file is closed, so its directory entry carries the final length;
//! then the file is opened again and the real header written over the
//! placeholder. That header is the commit, and it is the last block to land:
//! a power cut anywhere before it leaves the live side exactly as it was and
//! the other side holding a header that decodes as nothing, and a cut after
//! it leaves a generation that is already complete.
//!
//! A reader takes the newer committed header and checks the file's length and
//! every record before believing it. A committed generation that does not
//! read back whole is not an interrupted write, since the header could not
//! have landed before the records did. It is damage to durable identity
//! state, and it is refused rather than fallen back from: the older side is
//! missing every id the newer one added, so taking it would re-mint those
//! and orphan whatever comes to hang from them. Nothing here repairs a
//! damaged generation by guessing; the intact records on both sides are still
//! there for something explicit to salvage.
//!
//! Rewriting the whole file rather than appending is deliberate. An append
//! that straddles a cluster boundary can leave the FAT chain and the
//! directory entry disagreeing about where the file ends, and the recovery
//! model in this crate rests on not having to interpret that state. A
//! rewrite happens when a scan changes what the ledger says: copies to
//! adopt, copies gone missing, or copies come back. On a card that has not
//! changed, the scan does not run at all.
//!
//! Nothing here reads book bytes. Adoption is by place and size: a fresh
//! catalog row whose root, locator and size a live record names is that
//! record's copy, and any other row is a copy this library has not seen,
//! which is minted a fresh id. A copy that was moved on a computer is a new
//! book to this milestone, and its old record stays in the ledger as a
//! missing copy, for a bounded number of scans, so that the reconciliation
//! that recognises the move by digest has something to match when it lands.

use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use proto::cache::{source_hash_at, CACHE_ROOT_DIR};
use proto::catalog::{
    catalog_record_at, catalog_record_book_id, catalog_record_identity, CATALOG_HEADER_BYTES,
    CATALOG_RECORD_BYTES, CATALOG_RECORD_ID_OFFSET,
};
use proto::durable::generation_is_newer;
use proto::identity::{
    carry_missing, decode_ledger_header, decode_ledger_record, encode_ledger_header,
    encode_ledger_placeholder_header, encode_ledger_record, ledger_file_len, rows_with_hash,
    sort_row_keys, stage_row_key, BookId, LedgerHeader, LedgerRecord, LEDGER_HEADER_BYTES,
    LEDGER_RECORD_BYTES, ROW_KEY_BYTES,
};

/// The two generations, under the cache root.
pub const LEDGER_FILES: [&str; 2] = ["LEDGERA.BIN", "LEDGERB.BIN"];

/// The most records one generation can hold, because the header counts them
/// in two bytes. The same ceiling as the catalog, and for the same reason.
pub const LEDGER_MAX_RECORDS: usize = u16::MAX as usize;

/// Why the ledger could not do what was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerFault {
    /// The card refused a read, a write, or an open for a reason other than
    /// the file being absent. Not evidence about the ledger: the caller
    /// leaves things as they are and the next mount tries again.
    Device,
    /// The live generation was committed and does not read back whole: a
    /// record fails its checksum, or the file is not as long as its header
    /// says. That is durable identity state damaged after it landed, and
    /// falling back to the older generation would re-mint every id the live
    /// one added. The ledger is left exactly as it is and the operation is
    /// refused; a catalog already committed keeps serving, and only rebuilds
    /// stop, until the intact records are salvaged by something explicit.
    Damaged,
    /// More copies than a generation can hold once the live and newly
    /// adopted ones are counted. Missing records yield first, so this is a
    /// library past the catalog's own ceiling.
    Full,
    /// A row that cannot be adopted: a root byte this build does not know,
    /// or a locator wider than a record. Neither is reachable from a
    /// catalog this build wrote.
    Record,
    /// The caller's scratch cannot hold the join's working set.
    Scratch,
}

/// One validated generation: which side it is on, and what its header said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ledger {
    side: usize,
    pub generation: u32,
    pub count: u16,
}

impl Ledger {
    /// Which file this generation is in, for a caller that reports it.
    pub fn file_name(&self) -> &'static str {
        LEDGER_FILES[self.side]
    }
}

/// The live generation, or `None` for a card with no ledger yet.
///
/// Both headers are read; the newer committed one is the live generation,
/// and it is checked whole before it is handed back. A side whose header
/// does not decode was never committed, which is what an interrupted
/// rewrite leaves, and is skipped. A side whose header does decode and whose
/// records or length do not check out is [`LedgerFault::Damaged`], and the
/// older side is deliberately not consulted in its place.
pub fn open<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Option<Ledger>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(LedgerFault::Device),
    };
    let a = committed_header(&cache_root, 0)?;
    let b = committed_header(&cache_root, 1)?;
    let (side, header) = match (a, b) {
        (Some(a), Some(b)) if generation_is_newer(b.generation, a.generation) => (1, b),
        (Some(a), Some(_)) | (Some(a), None) => (0, a),
        (None, Some(b)) => (1, b),
        (None, None) => return Ok(None),
    };
    if !reads_back_whole(&cache_root, side, header)? {
        return Err(LedgerFault::Damaged);
    }
    Ok(Some(Ledger {
        side,
        generation: header.generation,
        count: header.count,
    }))
}

/// Every record of `ledger`, in order, to `visit`. A record that no longer
/// decodes is the card misbehaving between the check in [`open`] and now,
/// and is reported as such rather than skipped.
pub fn for_each_record<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    ledger: &Ledger,
    visit: &mut impl FnMut(u16, &LedgerRecord<'_>) -> Result<(), LedgerFault>,
) -> Result<(), LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = root
        .open_dir(CACHE_ROOT_DIR)
        .map_err(|_| LedgerFault::Device)?;
    let file = cache_root
        .open_file_in_dir(LEDGER_FILES[ledger.side], Mode::ReadOnly)
        .map_err(|_| LedgerFault::Device)?;
    file.seek_from_start(LEDGER_HEADER_BYTES as u32)
        .map_err(|_| LedgerFault::Device)?;
    let mut bytes = [0u8; LEDGER_RECORD_BYTES];
    for index in 0..ledger.count {
        if !read_exact(&file, &mut bytes)? {
            return Err(LedgerFault::Device);
        }
        let record = decode_ledger_record(&bytes).ok_or(LedgerFault::Device)?;
        visit(index, &record)?;
    }
    Ok(())
}

/// Appends records to the generation being written. See [`write_generation`].
pub struct LedgerWriter<'f, 'd, D, T, const MD: usize, const MF: usize, const MV: usize>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: &'f File<'d, D, T, MD, MF, MV>,
    count: u16,
}

impl<D, T, const MD: usize, const MF: usize, const MV: usize> LedgerWriter<'_, '_, D, T, MD, MF, MV>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    pub fn append(&mut self, record: &LedgerRecord<'_>) -> Result<(), LedgerFault> {
        let count = self.count.checked_add(1).ok_or(LedgerFault::Full)?;
        let mut bytes = [0u8; LEDGER_RECORD_BYTES];
        encode_ledger_record(record, &mut bytes).ok_or(LedgerFault::Record)?;
        self.file.write(&bytes).map_err(|_| LedgerFault::Device)?;
        self.count = count;
        Ok(())
    }

    /// Records in the generation so far, carried and appended.
    pub fn count(&self) -> u16 {
        self.count
    }
}

/// Write the next generation: the records of `previous` that `carry` keeps,
/// each with the `misses` it returns, then whatever `fill` appends, and then
/// the header that commits it.
///
/// The target is the side `previous` is not on, so `previous` stands
/// untouched until the new header has landed and read back. `fill` is
/// handed the writer once the carried records are down, and may read and
/// write other files meanwhile; the one it must not touch is the previous
/// side, which is already closed by then.
pub fn write_generation<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    previous: Option<&Ledger>,
    carry: &mut impl FnMut(u16, &LedgerRecord<'_>) -> Option<u8>,
    fill: impl FnOnce(&mut LedgerWriter<'_, '_, D, T, MD, MF, MV>) -> Result<(), LedgerFault>,
) -> Result<Ledger, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = open_or_make_cache_root(root)?;
    let (target, generation) = match previous {
        Some(live) => (1 - live.side, live.generation.wrapping_add(1)),
        None => (0, 1),
    };
    let mut header = [0u8; LEDGER_HEADER_BYTES];

    // Records first, under a header that decodes as nothing, closed so the
    // directory entry holds the final length before anything says the
    // generation exists.
    let count = {
        let file = cache_root
            .open_file_in_dir(LEDGER_FILES[target], Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| LedgerFault::Device)?;
        encode_ledger_placeholder_header(&mut header);
        file.write(&header).map_err(|_| LedgerFault::Device)?;
        let mut carried: u16 = 0;
        if let Some(live) = previous {
            let source = cache_root
                .open_file_in_dir(LEDGER_FILES[live.side], Mode::ReadOnly)
                .map_err(|_| LedgerFault::Device)?;
            source
                .seek_from_start(LEDGER_HEADER_BYTES as u32)
                .map_err(|_| LedgerFault::Device)?;
            let mut bytes = [0u8; LEDGER_RECORD_BYTES];
            for index in 0..live.count {
                // Carried forward only as read back intact. The side was
                // checked when it was opened, so anything else here is the
                // card, and a copy of it would be a copy of the damage.
                if !read_exact(&source, &mut bytes)? {
                    return Err(LedgerFault::Device);
                }
                let record = decode_ledger_record(&bytes).ok_or(LedgerFault::Device)?;
                let Some(misses) = carry(index, &record) else {
                    continue;
                };
                let kept = LedgerRecord { misses, ..record };
                let mut out = [0u8; LEDGER_RECORD_BYTES];
                encode_ledger_record(&kept, &mut out).ok_or(LedgerFault::Record)?;
                file.write(&out).map_err(|_| LedgerFault::Device)?;
                carried = carried.checked_add(1).ok_or(LedgerFault::Full)?;
            }
        }
        let mut writer = LedgerWriter {
            file: &file,
            count: carried,
        };
        fill(&mut writer)?;
        let count = writer.count;
        file.close().map_err(|_| LedgerFault::Device)?;
        count
    };

    // Then the commit: one header, over the placeholder, in a file whose
    // length is already final.
    {
        let file = cache_root
            .open_file_in_dir(LEDGER_FILES[target], Mode::ReadWriteAppend)
            .map_err(|_| LedgerFault::Device)?;
        file.seek_from_start(0).map_err(|_| LedgerFault::Device)?;
        encode_ledger_header(LedgerHeader { generation, count }, &mut header);
        file.write(&header).map_err(|_| LedgerFault::Device)?;
        file.close().map_err(|_| LedgerFault::Device)?;
    }

    match committed_header(&cache_root, target)? {
        Some(committed) if committed.generation == generation && committed.count == count => {
            let ledger = Ledger {
                side: target,
                generation,
                count,
            };
            if reads_back_whole(&cache_root, target, committed)? {
                Ok(ledger)
            } else {
                Err(LedgerFault::Device)
            }
        }
        _ => Err(LedgerFault::Device),
    }
}

/// What [`assign_book_ids`] did, for the caller that reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Assignment {
    /// Rows a live record named, which kept that record's id.
    pub matched: u16,
    /// Rows no record named, which were adopted under a fresh id.
    pub minted: u16,
    /// Live records that named a row another record had already claimed.
    /// The first in ledger order wins, deterministically. This writer
    /// appends one record per unnamed row, so a duplicate is a ledger
    /// written by something else.
    pub duplicates: u16,
    /// Records that named no row and were carried into the new generation
    /// as missing copies, one scan older.
    pub missing: u16,
    /// Records that named no row and were left behind: missing for longer
    /// than the ledger retains, or not fitting beside the live library.
    pub retired: u16,
}

/// Give every row of a freshly written catalog its [`BookId`].
///
/// `catalog` is the open `CATALOG.BIN` with `count` records written and its
/// header still the placeholder; rows carry no id yet. `ledger` is what
/// [`open`] returned for this card, opened by the caller so that a ledger
/// that refuses can refuse before the catalog is touched. Each row a live
/// record names by root, locator and size takes that record's id in place.
/// The rest are minted ids and appended in one new generation, which is
/// committed before this returns, so the ids the catalog carries are
/// durable by the time its own header is. The same generation carries
/// forward the records no row named, aged by one scan and dropped once
/// they pass the retention bound or would not fit beside the live library.
/// A scan that changes nothing writes nothing.
///
/// A join rather than a lookup per row, because a per-row lookup in a
/// thousand-book library is a thousand file opens. Row keys are staged in
/// `scratch` a slice at a time, the ledger is read once per slice, and only
/// rows whose 32-bit place hash agrees are compared in full. The head of
/// `scratch` holds one bit per ledger record, set when the record names a
/// row, which is what tells a live record from a missing one when the
/// generation is rewritten. The scratch bounds nothing but the number of
/// slices.
///
/// On a fault nothing is retracted. Ids written into rows are ids that the
/// ledger either committed, in which case they are right, or did not, in
/// which case the caller must not commit the catalog either, and the next
/// scan starts over from the generation that stands.
pub fn assign_book_ids<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    catalog: &File<'_, D, T, MD, MF, MV>,
    count: u16,
    scratch: &mut [u8],
    random: &mut impl FnMut() -> u32,
    ledger: Option<Ledger>,
) -> Result<Assignment, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut assigned = Assignment::default();
    if count == 0 {
        return Ok(assigned);
    }
    let live = ledger.filter(|live| live.count > 0);
    let bitmap_bytes = live.map_or(0, |live| (live.count as usize).div_ceil(8));
    if scratch.len() < bitmap_bytes {
        return Err(LedgerFault::Scratch);
    }
    let (named, keys) = scratch.split_at_mut(bitmap_bytes);
    named.fill(0);
    let per_slice = keys.len() / ROW_KEY_BYTES;
    if per_slice == 0 {
        return Err(LedgerFault::Scratch);
    }
    let mut record = [0u8; CATALOG_RECORD_BYTES];
    // Live records that had been missing, whose count has to go back to
    // zero: a change to the ledger even when nothing else moved.
    let mut returned = 0usize;
    if let Some(live) = live {
        let mut slice_start = 0usize;
        while slice_start < count as usize {
            let slice_end = (slice_start + per_slice).min(count as usize);
            seek_row(catalog, slice_start)?;
            for row in slice_start..slice_end {
                if !read_exact(catalog, &mut record)? {
                    return Err(LedgerFault::Device);
                }
                let (hash, _) = catalog_record_identity(&record);
                stage_row_key(keys, row - slice_start, hash, row as u16);
            }
            let staged = slice_end - slice_start;
            sort_row_keys(keys, staged);
            for_each_record(root, &live, &mut |index, entry| {
                let hash = source_hash_at(entry.root, entry.locator, entry.byte_size);
                let mut names_a_row = false;
                for row in rows_with_hash(keys, staged, hash) {
                    seek_row(catalog, row as usize)?;
                    if !read_exact(catalog, &mut record)? {
                        return Err(LedgerFault::Device);
                    }
                    if catalog_record_at(&record)
                        != Some((entry.root, entry.locator, entry.byte_size))
                    {
                        continue;
                    }
                    names_a_row = true;
                    if catalog_record_book_id(&record).is_some() {
                        assigned.duplicates = assigned.duplicates.saturating_add(1);
                        continue;
                    }
                    write_row_id(catalog, row as usize, entry.id)?;
                    assigned.matched += 1;
                }
                if names_a_row && !bit(named, index) {
                    set_bit(named, index);
                    if entry.misses > 0 {
                        returned += 1;
                    }
                }
                Ok(())
            })?;
            slice_start = slice_end;
        }
    }

    let live_records = named
        .iter()
        .map(|byte| byte.count_ones() as usize)
        .sum::<usize>();
    let missing_records = live.map_or(0, |live| live.count as usize) - live_records;
    let new_rows = count as usize - assigned.matched as usize;
    if new_rows == 0 && missing_records == 0 && returned == 0 {
        return Ok(assigned);
    }
    // Live and new copies are the library; a missing record takes a slot
    // only if one is left once they are all in.
    let room = LEDGER_MAX_RECORDS.saturating_sub(live_records + new_rows);
    let mut carried_missing = 0usize;
    let Assignment {
        minted,
        missing,
        retired,
        ..
    } = &mut assigned;
    write_generation(
        root,
        ledger.as_ref(),
        &mut |index, entry| {
            if bit(named, index) {
                return Some(0);
            }
            match carry_missing(entry.misses, room - carried_missing) {
                Some(misses) => {
                    carried_missing += 1;
                    *missing += 1;
                    Some(misses)
                }
                None => {
                    *retired += 1;
                    None
                }
            }
        },
        |writer| {
            seek_row(catalog, 0)?;
            for row in 0..count as usize {
                if !read_exact(catalog, &mut record)? {
                    return Err(LedgerFault::Device);
                }
                if catalog_record_book_id(&record).is_some() {
                    continue;
                }
                let (at, locator, byte_size) =
                    catalog_record_at(&record).ok_or(LedgerFault::Record)?;
                let id = BookId::mint(random);
                writer.append(&LedgerRecord {
                    id,
                    root: at,
                    locator,
                    byte_size,
                    misses: 0,
                })?;
                // Leaves the cursor at the next row, where the read above
                // expects it.
                write_row_id(catalog, row, id)?;
                *minted += 1;
            }
            Ok(())
        },
    )?;
    Ok(assigned)
}

fn bit(bits: &[u8], index: u16) -> bool {
    bits[index as usize / 8] & (1 << (index % 8)) != 0
}

fn set_bit(bits: &mut [u8], index: u16) {
    bits[index as usize / 8] |= 1 << (index % 8);
}

/// The committed header on `side`, or `None` for a side that is absent,
/// shorter than a header, or holding bytes that do not decode as one, which
/// is what the placeholder and a torn commit both look like. Only a refused
/// open or read is an error. Whether the generation behind a committed
/// header is whole is [`reads_back_whole`]'s question.
fn committed_header<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
    side: usize,
) -> Result<Option<LedgerHeader>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match cache_root.open_file_in_dir(LEDGER_FILES[side], Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(LedgerFault::Device),
    };
    let mut bytes = [0u8; LEDGER_HEADER_BYTES];
    if !read_exact(&file, &mut bytes)? {
        return Ok(None);
    }
    Ok(decode_ledger_header(&bytes))
}

/// Whether a committed generation is exactly as long as its header says and
/// every record on it decodes. `Ok(false)` is a damaged side; `Err` is the
/// card.
fn reads_back_whole<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
    side: usize,
    header: LedgerHeader,
) -> Result<bool, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = cache_root
        .open_file_in_dir(LEDGER_FILES[side], Mode::ReadOnly)
        .map_err(|_| LedgerFault::Device)?;
    if file.length() as usize != ledger_file_len(header.count) {
        return Ok(false);
    }
    file.seek_from_start(LEDGER_HEADER_BYTES as u32)
        .map_err(|_| LedgerFault::Device)?;
    let mut bytes = [0u8; LEDGER_RECORD_BYTES];
    for _ in 0..header.count {
        if !read_exact(&file, &mut bytes)? || decode_ledger_record(&bytes).is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn open_or_make_cache_root<'a, D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &'a Directory<'_, D, T, MD, MF, MV>,
) -> Result<Directory<'a, D, T, MD, MF, MV>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => Ok(dir),
        Err(embedded_sdmmc::Error::NotFound) => {
            root.make_dir_in_dir(CACHE_ROOT_DIR)
                .map_err(|_| LedgerFault::Device)?;
            root.open_dir(CACHE_ROOT_DIR)
                .map_err(|_| LedgerFault::Device)
        }
        Err(_) => Err(LedgerFault::Device),
    }
}

fn seek_row<D, T, const MD: usize, const MF: usize, const MV: usize>(
    catalog: &File<'_, D, T, MD, MF, MV>,
    row: usize,
) -> Result<(), LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    catalog
        .seek_from_start((CATALOG_HEADER_BYTES + row * CATALOG_RECORD_BYTES) as u32)
        .map_err(|_| LedgerFault::Device)
}

fn write_row_id<D, T, const MD: usize, const MF: usize, const MV: usize>(
    catalog: &File<'_, D, T, MD, MF, MV>,
    row: usize,
    id: BookId,
) -> Result<(), LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let at = CATALOG_HEADER_BYTES + row * CATALOG_RECORD_BYTES + CATALOG_RECORD_ID_OFFSET;
    catalog
        .seek_from_start(at as u32)
        .map_err(|_| LedgerFault::Device)?;
    catalog
        .write(&id.to_bytes())
        .map_err(|_| LedgerFault::Device)
}

/// Fill `out` from `file`. `Ok(false)` is a file that ended first; a refused
/// read is the card and is an error, since the two mean different things to
/// every caller here.
fn read_exact<D, T, const MD: usize, const MF: usize, const MV: usize>(
    file: &File<'_, D, T, MD, MF, MV>,
    mut out: &mut [u8],
) -> Result<bool, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    while !out.is_empty() {
        let read = file.read(out).map_err(|_| LedgerFault::Device)?;
        if read == 0 {
            return Ok(false);
        }
        let rest = out;
        out = &mut rest[read..];
    }
    Ok(true)
}
