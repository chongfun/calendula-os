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
//! side that is not live, records first and header last, so a power cut
//! anywhere in it leaves the live side exactly as it was and the other side
//! holding a header that decodes as nothing. A reader picks the side with
//! the newer valid header, then checks every record on it before believing
//! it, and falls back to the older side when one fails. That extra pass is
//! what keeps a damaged record from being read as a shorter library, and it
//! is one sequential read of a file the join is about to read anyway.
//!
//! Rewriting the whole file rather than appending is deliberate. An append
//! that straddles a cluster boundary can leave the FAT chain and the
//! directory entry disagreeing about where the file ends, and the recovery
//! model in this crate rests on not having to interpret that state. A
//! rewrite only happens when a scan found copies the ledger does not name,
//! which after the first scan is the uploads since the last one, and it
//! costs about what the catalog rewrite beside it already costs.
//!
//! Nothing here reads book bytes. Adoption is by place and size: a fresh
//! catalog row whose root, locator and size a live record names is that
//! record's copy, and any other row is a copy this library has not seen,
//! which is minted a fresh id. A copy that was moved on a computer is a new
//! book to this milestone, and its old record stays in the ledger as a copy
//! that is missing. Recognising the move is reconciliation work that needs
//! the source digest, and it lands on top of this.

use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use proto::cache::{source_hash_at, CACHE_ROOT_DIR};
use proto::catalog::{
    catalog_record_at, catalog_record_book_id, catalog_record_identity, CATALOG_HEADER_BYTES,
    CATALOG_RECORD_BYTES, CATALOG_RECORD_ID_OFFSET,
};
use proto::durable::generation_is_newer;
use proto::identity::{
    decode_ledger_header, decode_ledger_record, encode_ledger_header,
    encode_ledger_placeholder_header, encode_ledger_record, ledger_file_len, rows_with_hash,
    sort_row_keys, stage_row_key, BookId, LedgerHeader, LedgerRecord, LEDGER_HEADER_BYTES,
    LEDGER_RECORD_BYTES, ROW_KEY_BYTES,
};

/// The two generations, under the cache root.
pub const LEDGER_FILES: [&str; 2] = ["LEDGERA.BIN", "LEDGERB.BIN"];

/// Why the ledger could not do what was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerFault {
    /// The card refused a read, a write, or an open for a reason other than
    /// the file being absent. Not evidence about the ledger: the caller
    /// leaves things as they are and the next mount tries again.
    Device,
    /// More adopted copies than a header can count.
    Full,
    /// A row that cannot be adopted: a root byte this build does not know,
    /// or a locator wider than a record. Neither is reachable from a
    /// catalog this build wrote.
    Record,
    /// The caller's scratch cannot hold even one row key.
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
/// Both headers are read and the newer is checked record by record before
/// it is trusted; a side that fails hands over to the other. A device fault
/// anywhere is an error rather than a fallback, because taking the older
/// generation over a side the card would not read could re-mint ids that
/// side holds.
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
    let headers = [
        read_committed_header(&cache_root, 0)?,
        read_committed_header(&cache_root, 1)?,
    ];
    let order: [Option<usize>; 2] = match (headers[0], headers[1]) {
        (Some(a), Some(b)) if generation_is_newer(b.generation, a.generation) => [Some(1), Some(0)],
        (Some(_), Some(_)) | (Some(_), None) => [Some(0), Some(1)],
        (None, Some(_)) => [Some(1), None],
        (None, None) => return Ok(None),
    };
    for side in order.into_iter().flatten() {
        let Some(header) = headers[side] else {
            continue;
        };
        if records_check_out(&cache_root, side, header.count)? {
            return Ok(Some(Ledger {
                side,
                generation: header.generation,
                count: header.count,
            }));
        }
    }
    Ok(None)
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

/// Write the next generation: every record of `previous`, then whatever
/// `fill` appends, committed by the header last.
///
/// The target is the side `previous` is not on, so `previous` stands
/// untouched until the new header has landed and read back. `fill` is
/// handed the writer once the carried records are down, and may read and
/// write other files meanwhile; the one it must not touch is the previous
/// side, which is already closed by then.
pub fn write_generation<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    previous: Option<&Ledger>,
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
    let count;
    {
        let file = cache_root
            .open_file_in_dir(LEDGER_FILES[target], Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| LedgerFault::Device)?;
        let mut header = [0u8; LEDGER_HEADER_BYTES];
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
            for _ in 0..live.count {
                // Carried forward only as read back intact. The side was
                // checked when it was opened, so anything else here is the
                // card, and a copy of it would be a copy of the damage.
                if !read_exact(&source, &mut bytes)? || decode_ledger_record(&bytes).is_none() {
                    return Err(LedgerFault::Device);
                }
                file.write(&bytes).map_err(|_| LedgerFault::Device)?;
                carried += 1;
            }
        }
        let mut writer = LedgerWriter {
            file: &file,
            count: carried,
        };
        fill(&mut writer)?;
        count = writer.count;
        encode_ledger_header(LedgerHeader { generation, count }, &mut header);
        file.seek_from_start(0).map_err(|_| LedgerFault::Device)?;
        file.write(&header).map_err(|_| LedgerFault::Device)?;
        file.flush().map_err(|_| LedgerFault::Device)?;
    }
    match read_committed_header(&cache_root, target)? {
        Some(header) if header.generation == generation && header.count == count => Ok(Ledger {
            side: target,
            generation,
            count,
        }),
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
}

/// Give every row of a freshly written catalog its [`BookId`].
///
/// `catalog` is the open `CATALOG.BIN` with `count` records written and its
/// header still the placeholder; rows carry no id yet. Each row a live
/// ledger record names by root, locator and size takes that record's id in
/// place. The rest are minted ids and appended to the ledger in one new
/// generation, and that generation is committed before this returns, so
/// the ids the catalog carries are durable by the time its own header is.
///
/// A join rather than a lookup per row, because a per-row lookup in a
/// thousand-book library is a thousand file opens. Row keys are staged in
/// `scratch` a slice at a time, the ledger is read once per slice, and only
/// rows whose 32-bit place hash agrees are compared in full. The scratch
/// bounds nothing but the number of slices.
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
) -> Result<Assignment, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut assigned = Assignment::default();
    if count == 0 {
        return Ok(assigned);
    }
    let per_slice = scratch.len() / ROW_KEY_BYTES;
    if per_slice == 0 {
        return Err(LedgerFault::Scratch);
    }
    let ledger = open(root)?;
    let mut record = [0u8; CATALOG_RECORD_BYTES];
    if let Some(live) = ledger.filter(|live| live.count > 0) {
        let mut slice_start = 0usize;
        while slice_start < count as usize {
            let slice_end = (slice_start + per_slice).min(count as usize);
            seek_row(catalog, slice_start)?;
            for row in slice_start..slice_end {
                if !read_exact(catalog, &mut record)? {
                    return Err(LedgerFault::Device);
                }
                let (hash, _) = catalog_record_identity(&record);
                stage_row_key(scratch, row - slice_start, hash, row as u16);
            }
            let staged = slice_end - slice_start;
            sort_row_keys(scratch, staged);
            for_each_record(root, &live, &mut |_, entry| {
                let hash = source_hash_at(entry.root, entry.locator, entry.byte_size);
                for row in rows_with_hash(scratch, staged, hash) {
                    seek_row(catalog, row as usize)?;
                    if !read_exact(catalog, &mut record)? {
                        return Err(LedgerFault::Device);
                    }
                    if catalog_record_at(&record)
                        != Some((entry.root, entry.locator, entry.byte_size))
                    {
                        continue;
                    }
                    if catalog_record_book_id(&record).is_some() {
                        assigned.duplicates = assigned.duplicates.saturating_add(1);
                        continue;
                    }
                    write_row_id(catalog, row as usize, entry.id)?;
                    assigned.matched += 1;
                }
                Ok(())
            })?;
            slice_start = slice_end;
        }
    }
    if (assigned.matched as usize) < count as usize {
        let minted = &mut assigned.minted;
        write_generation(root, ledger.as_ref(), |writer| {
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
                })?;
                // Leaves the cursor at the next row, where the read above
                // expects it.
                write_row_id(catalog, row, id)?;
                *minted += 1;
            }
            Ok(())
        })?;
    }
    Ok(assigned)
}

/// The committed header on `side`, or `None` for a side that is absent,
/// holds the placeholder, does not decode, or is not exactly as long as its
/// count says. Only a refused open or read is an error.
fn read_committed_header<D, T, const MD: usize, const MF: usize, const MV: usize>(
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
    let Some(header) = decode_ledger_header(&bytes) else {
        return Ok(None);
    };
    if file.length() as usize != ledger_file_len(header.count) {
        return Ok(None);
    }
    Ok(Some(header))
}

/// Whether every record on `side` decodes. `Ok(false)` is a damaged side;
/// `Err` is the card.
fn records_check_out<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
    side: usize,
    count: u16,
) -> Result<bool, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = cache_root
        .open_file_in_dir(LEDGER_FILES[side], Mode::ReadOnly)
        .map_err(|_| LedgerFault::Device)?;
    file.seek_from_start(LEDGER_HEADER_BYTES as u32)
        .map_err(|_| LedgerFault::Device)?;
    let mut bytes = [0u8; LEDGER_RECORD_BYTES];
    for _ in 0..count {
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
