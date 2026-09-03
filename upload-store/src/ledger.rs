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
//! placeholder. That header is the last block of the generation to land, so
//! a generation with a header is a generation with all of its records.
//!
//! Which side is live is not something the two files can say on their own.
//! A side holding the placeholder, or nothing, looks the same whether a
//! rewrite of it was interrupted, which costs nothing, or it was the live
//! generation and lost its header, which is the loss of every id it added.
//! So a third file, `/READER/LEDGER.JNL`, keeps the fact that tells them
//! apart. While a rewrite lays the target's records down, the journal still
//! names the side that stands, so whatever the target held before is not
//! consulted, damaged or not. Once the records are down under the
//! placeholder it records which side is being written and what stood on the
//! other; after the new header has landed and read back it records which
//! side is live and what its header says.
//!
//! The journal has to survive its own interrupted write, so it is two
//! sector-sized slots written alternately, each entry with a sequence
//! number, the way `RECLAIM.JNL` is kept. A write torn by a power cut
//! damages the slot being written, and the entry before it still reads.
//! Falling back one entry is safe by construction: a generation's ids reach
//! a committed catalog only after the journal has named that generation
//! live, so the entry before names either the same live side or a rewrite
//! whose outcome the target's own header decides. A torn write of that
//! header, or of the placeholder before it, is read the same way: under a
//! journal that says a side is being written, a target that does not read
//! back as the committed, whole generation expected is a target whose commit
//! did not land, and the side that stood is live.
//!
//! A reader believes a side only when the journal accounts for it: the side
//! the journal names as live, with the header it recorded and every record
//! intact; or, during a rewrite, the target if it committed whole and
//! otherwise the side that stood, exactly as recorded. Anything else, a live
//! side that is not as the journal says, a journal with no entry that reads,
//! a header or journal of a version this build does not read, or ledger
//! files with no journal beside them, refuses. Nothing here repairs a ledger
//! by guessing which side to trust; the intact records stay where they are
//! for something explicit to salvage.
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
    carry_missing, classify_ledger_header, classify_ledger_journal, decode_ledger_record,
    encode_ledger_header, encode_ledger_journal, encode_ledger_placeholder_header,
    encode_ledger_record, ledger_file_len, rows_with_hash, sort_row_keys, stage_row_key, BookId,
    LedgerHeader, LedgerHeaderReading, LedgerJournal, LedgerJournalReading, LedgerRecord,
    LEDGER_HEADER_BYTES, LEDGER_JOURNAL_BYTES, LEDGER_JOURNAL_FILE_BYTES, LEDGER_JOURNAL_SLOTS,
    LEDGER_JOURNAL_SLOT_BYTES, LEDGER_RECORD_BYTES, ROW_KEY_BYTES,
};

/// The two generations, under the cache root.
pub const LEDGER_FILES: [&str; 2] = ["LEDGERA.BIN", "LEDGERB.BIN"];
/// The journal that says which of them is live, beside them.
pub const LEDGER_JOURNAL: &str = "LEDGER.JNL";

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
    /// The ledger is not as its journal says, or something in it does not
    /// read: the live side is missing, empty, or holds a header other than
    /// the one recorded; the live generation's records or length do not
    /// match its header; a header or the journal is bytes this build did not
    /// write; or there are ledger files with no journal to account for
    /// them. That is durable identity state damaged after it landed, and
    /// taking whatever else is on the card in its place would re-mint every
    /// id it held. The ledger is left exactly as it is and the operation is
    /// refused; a catalog already committed keeps serving, and only rebuilds
    /// stop, until the intact records are salvaged by something explicit.
    Damaged,
    /// A committed header, or a journal entry, of a format version this
    /// build does not read, written by another build. Refused for the same
    /// reason as damage: the ids it holds cannot come back from the card.
    Unreadable,
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

/// What one side's file holds, as far as its first sixteen bytes say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SideState {
    /// No file.
    Absent,
    /// An empty file, or one under the placeholder: a generation whose
    /// header has not landed, if the journal says one was being written
    /// here, and a lost one otherwise.
    Uncommitted,
    Committed(LedgerHeader),
    /// Bytes that are neither the placeholder nor a header: a torn header
    /// write, if the journal says one was under way here, and damage
    /// otherwise.
    Damaged,
    /// A header of a version this build does not read.
    Unreadable,
}

/// The live generation, or `None` for a card with no ledger yet.
///
/// The journal decides. When it names a live side, that side must hold the
/// header it recorded and read back whole. When it names a side being
/// rewritten, that side is live if its header landed with the generation
/// after the one that stood and it reads back whole; anything else there,
/// the placeholder, a torn header, a header of another generation, is a
/// commit that did not land, and the side that stood is live if it still
/// holds exactly the header recorded for it. A card with no journal has no
/// ledger, and ledger files beside no journal are [`LedgerFault::Damaged`].
/// Sides the journal does not point at are not read at all, so damage to a
/// generation that is no longer live costs nothing until the next rewrite
/// goes over it.
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
    let (side, header) = match journal_slots(&cache_root)? {
        None => {
            if side_state(&cache_root, 0)? == SideState::Absent
                && side_state(&cache_root, 1)? == SideState::Absent
            {
                return Ok(None);
            }
            return Err(LedgerFault::Damaged);
        }
        Some((LedgerJournal::Committed { side, header }, _, _)) => {
            let side = usize::from(side);
            match side_state(&cache_root, side)? {
                SideState::Committed(found) if found == header => {
                    if !reads_back_whole(&cache_root, side, header)? {
                        return Err(LedgerFault::Damaged);
                    }
                    (side, header)
                }
                SideState::Unreadable => return Err(LedgerFault::Unreadable),
                _ => return Err(LedgerFault::Damaged),
            }
        }
        Some((LedgerJournal::Rewriting { target, standing }, _, _)) => {
            let target = usize::from(target);
            let other = 1 - target;
            let expected = standing.map_or(1, |stood| stood.generation.wrapping_add(1));
            // The rewrite got as far as its commit and the power went before
            // the journal could say so. The generation number is what says
            // this is that commit, and the whole check is what says it
            // landed rather than tore.
            if let SideState::Committed(found) = side_state(&cache_root, target)? {
                if found.generation == expected && reads_back_whole(&cache_root, target, found)? {
                    return Ok(Some(Ledger {
                        side: target,
                        generation: found.generation,
                        count: found.count,
                    }));
                }
            }
            // The commit did not land. What stood must still stand, exactly
            // as recorded, or nothing does.
            match (standing, side_state(&cache_root, other)?) {
                (None, SideState::Absent) => return Ok(None),
                (Some(stood), SideState::Committed(found)) if found == stood => {
                    if !reads_back_whole(&cache_root, other, stood)? {
                        return Err(LedgerFault::Damaged);
                    }
                    (other, stood)
                }
                (Some(_), SideState::Unreadable) => return Err(LedgerFault::Unreadable),
                _ => return Err(LedgerFault::Damaged),
            }
        }
    };
    Ok(Some(Ledger {
        side,
        generation: header.generation,
        count: header.count,
    }))
}

/// The journal's newest entry that reads, or `None` for a card with no
/// journal. For a caller that reports on the ledger; [`open`] is what
/// decides from it.
pub fn read_journal<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Option<LedgerJournal>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(LedgerFault::Device),
    };
    Ok(journal_slots(&cache_root)?.map(|(entry, _, _)| entry))
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
/// the header that commits it, with the journal saying throughout what is
/// going on.
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
    let standing = previous.map(|live| LedgerHeader {
        generation: live.generation,
        count: live.count,
    });
    let rewriting = LedgerJournal::Rewriting {
        target: target as u8,
        standing,
    };

    // With nothing standing there is no journal yet, and a ledger file with
    // no journal beside it reads as damage, so the first generation is
    // announced before its file exists.
    if standing.is_none() {
        write_journal(&cache_root, rewriting)?;
    }

    // Records first, under a header that decodes as nothing, closed so the
    // directory entry holds the final length before anything says the
    // generation exists. The journal still names the side that stands, so a
    // cut anywhere in here leaves the target unread: whatever it held
    // before, damaged or not, is neither consulted nor a reason to refuse.
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

    // Now the target is known to hold the placeholder over this
    // generation's records and nothing else. Say it is being written: from
    // here until the header lands the side that stood is the one to
    // believe, and once the header has landed whole the target is.
    if standing.is_some() {
        write_journal(&cache_root, rewriting)?;
    }

    // Then the commit: one header, over the placeholder, in a file whose
    // length is already final.
    let committed = LedgerHeader { generation, count };
    {
        let file = cache_root
            .open_file_in_dir(LEDGER_FILES[target], Mode::ReadWriteAppend)
            .map_err(|_| LedgerFault::Device)?;
        file.seek_from_start(0).map_err(|_| LedgerFault::Device)?;
        encode_ledger_header(committed, &mut header);
        file.write(&header).map_err(|_| LedgerFault::Device)?;
        file.close().map_err(|_| LedgerFault::Device)?;
    }
    match side_state(&cache_root, target)? {
        SideState::Committed(found) if found == committed => {}
        _ => return Err(LedgerFault::Device),
    }
    if !reads_back_whole(&cache_root, target, committed)? {
        return Err(LedgerFault::Device);
    }

    // And say so. Until this lands the journal still explains the target as
    // being written, and a reader finds it committed whole with the
    // generation it expects, which is the same answer.
    write_journal(
        &cache_root,
        LedgerJournal::Committed {
            side: target as u8,
            header: committed,
        },
    )?;
    Ok(Ledger {
        side: target,
        generation,
        count,
    })
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
/// That includes a catalog with no rows at all, which ages every record.
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
    let live = ledger.filter(|live| live.count > 0);
    if count == 0 && live.is_none() {
        return Ok(assigned);
    }
    let bitmap_bytes = live.map_or(0, |live| (live.count as usize).div_ceil(8));
    if scratch.len() < bitmap_bytes {
        return Err(LedgerFault::Scratch);
    }
    let (named, keys) = scratch.split_at_mut(bitmap_bytes);
    named.fill(0);
    let per_slice = keys.len() / ROW_KEY_BYTES;
    if count > 0 && per_slice == 0 {
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
            match carry_missing(entry.misses, room.saturating_sub(carried_missing)) {
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

/// The journal's newest entry that reads, with the slot it is in and its
/// sequence number, or `None` for a card that has no journal: no file, or
/// an empty one, which is what a cut during its first creation leaves
/// before any side has been touched, or two blank slots.
///
/// Of two entries the newer by sequence wins. Beside an entry, a slot that
/// is blank or does not read is the slot a later write was torn in, and the
/// entry stands. A journal with no entry that reads, or of a length this
/// build did not write, is an error, not an absence; so is an entry of a
/// version this build does not read, in either slot.
fn journal_slots<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Option<(LedgerJournal, usize, u32)>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match cache_root.open_file_in_dir(LEDGER_JOURNAL, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(LedgerFault::Device),
    };
    if file.length() == 0 {
        return Ok(None);
    }
    if file.length() as usize != LEDGER_JOURNAL_FILE_BYTES {
        return Err(LedgerFault::Damaged);
    }
    let mut readings = [LedgerJournalReading::Blank; LEDGER_JOURNAL_SLOTS];
    let mut bytes = [0u8; LEDGER_JOURNAL_BYTES];
    for (index, reading) in readings.iter_mut().enumerate() {
        file.seek_from_start((index * LEDGER_JOURNAL_SLOT_BYTES) as u32)
            .map_err(|_| LedgerFault::Device)?;
        if !read_exact(&file, &mut bytes)? {
            return Err(LedgerFault::Damaged);
        }
        *reading = classify_ledger_journal(&bytes);
    }
    if readings
        .iter()
        .any(|reading| matches!(reading, LedgerJournalReading::UnknownVersion(_)))
    {
        return Err(LedgerFault::Unreadable);
    }
    let mut newest: Option<(LedgerJournal, usize, u32)> = None;
    for (index, reading) in readings.iter().enumerate() {
        let LedgerJournalReading::Entry { entry, sequence } = *reading else {
            continue;
        };
        newest = match newest {
            None => Some((entry, index, sequence)),
            Some((_, _, held)) if generation_is_newer(sequence, held) => {
                Some((entry, index, sequence))
            }
            // Two entries one sequence apart are the two most recent writes;
            // equal sequence numbers are nothing this writer produces.
            Some((_, _, held)) if held == sequence => return Err(LedgerFault::Damaged),
            Some(kept) => Some(kept),
        };
    }
    match newest {
        Some(found) => Ok(Some(found)),
        None if readings
            .iter()
            .all(|reading| *reading == LedgerJournalReading::Blank) =>
        {
            Ok(None)
        }
        None => Err(LedgerFault::Damaged),
    }
}

/// Publish `entry` into the slot the newest entry is not in, one sequence
/// past it. The file is created whole the first time and never truncated
/// after, so a torn write damages one slot and the entry before it stands.
fn write_journal<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
    entry: LedgerJournal,
) -> Result<(), LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let (slot, sequence) = match journal_slots(cache_root)? {
        Some((_, held, sequence)) => ((held + 1) % LEDGER_JOURNAL_SLOTS, sequence.wrapping_add(1)),
        None => (0, 1),
    };
    let mut block = [0u8; LEDGER_JOURNAL_SLOT_BYTES];
    let mut bytes = [0u8; LEDGER_JOURNAL_BYTES];
    encode_ledger_journal(entry, sequence, &mut bytes);
    block[..LEDGER_JOURNAL_BYTES].copy_from_slice(&bytes);
    let file = match cache_root.open_file_in_dir(LEDGER_JOURNAL, Mode::ReadWriteAppend) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => cache_root
            .open_file_in_dir(LEDGER_JOURNAL, Mode::ReadWriteCreate)
            .map_err(|_| LedgerFault::Device)?,
        Err(_) => return Err(LedgerFault::Device),
    };
    if file.length() == 0 {
        // A journal that does not exist yet, or one whose creation was cut
        // before it closed: both slots, the entry in the first.
        file.seek_from_start(0).map_err(|_| LedgerFault::Device)?;
        file.write(&block).map_err(|_| LedgerFault::Device)?;
        let blank = [0u8; LEDGER_JOURNAL_SLOT_BYTES];
        file.write(&blank).map_err(|_| LedgerFault::Device)?;
    } else {
        file.seek_from_start((slot * LEDGER_JOURNAL_SLOT_BYTES) as u32)
            .map_err(|_| LedgerFault::Device)?;
        file.write(&block).map_err(|_| LedgerFault::Device)?;
    }
    file.close().map_err(|_| LedgerFault::Device)
}

/// What the first sixteen bytes of `side` say. Only a refused open or read
/// is an error here; what a damaged or unreadable header means depends on
/// what the journal says about the side, and that is [`open`]'s call.
fn side_state<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
    side: usize,
) -> Result<SideState, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match cache_root.open_file_in_dir(LEDGER_FILES[side], Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(SideState::Absent),
        Err(_) => return Err(LedgerFault::Device),
    };
    if file.length() == 0 {
        return Ok(SideState::Uncommitted);
    }
    let mut bytes = [0u8; LEDGER_HEADER_BYTES];
    if !read_exact(&file, &mut bytes)? {
        return Ok(SideState::Damaged);
    }
    Ok(match classify_ledger_header(&bytes) {
        LedgerHeaderReading::Placeholder => SideState::Uncommitted,
        LedgerHeaderReading::Committed(header) => SideState::Committed(header),
        LedgerHeaderReading::UnknownVersion(_) => SideState::Unreadable,
        LedgerHeaderReading::Damaged => SideState::Damaged,
    })
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
