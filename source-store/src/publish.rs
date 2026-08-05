//! The durable publication sequence: how a sealed record becomes committed
//! authority, and the only path that may claim it did.
//!
//! The sequence is the PRD's, verbatim in structure:
//!
//! 1. pick the slot that is *not* current authority;
//! 2. write body, canonical padding, and a **zeroed** commit sector;
//! 3. [`durable_sync`] — the prepared-body sync;
//! 4. reopen and verify the file classifies as exactly this prepared record;
//! 5. run the caller's final authority revalidation;
//! 6. overwrite the commit sector with the valid one;
//! 7. [`durable_sync`] — the commit sync, which *is* the commit point;
//! 8. reopen and verify the pair selects this record through the same
//!    selector startup uses.
//!
//! Step 8 matters as much as step 7: reporting success from anything other
//! than the startup selector's verdict would let "what publish thinks it
//! wrote" and "what reboot will select" drift, which is the exact bug class
//! the PRD's selector rules exist to kill.
//!
//! Nothing here retries, and a failure after step 7 still leaves a committed
//! record on disk — the caller decides whether a `CommittedVerify` failure
//! is a lost card or a bug, but the old generation was never touched either
//! way: A/B alternation means the previous committed record is in the other
//! slot and this module never opens it for writing.

use embedded_sdmmc::{BlockDevice, Directory, File, Mode, TimeSource};

use crate::record::{
    classify_record, encode_commit_sector, select_generations, RecordState, SealedBody, Selection,
    COMMIT_FOOTER_BYTES,
};

/// The one primitive behind every authoritative commit. Delegates to
/// [`File::flush`]; see the crate docs for the three block-device properties
/// (write-through, completion-on-return, metadata coverage) that make that
/// delegation sufficient, and for why hardware power-cut verification is
/// still required on top.
pub fn durable_sync<D, T, const MD: usize, const MF: usize, const MV: usize>(
    file: &File<'_, D, T, MD, MF, MV>,
) -> Result<(), embedded_sdmmc::Error<D::Error>>
where
    D: BlockDevice,
    T: TimeSource,
{
    file.flush()
}

/// One record slot pair: the A and B file names inside the record's
/// directory, plus the body magic that types the pair. Physical names stay
/// internal to the storage layout; nothing here interprets them.
#[derive(Clone, Copy, Debug)]
pub struct SlotPair<'n> {
    pub names: [&'n str; 2],
    pub magic: [u8; 4],
}

/// What one slot holds, reduced to what selection and target-picking need.
/// A copyable summary rather than a borrowed [`RecordState`] so both slots
/// can be compared while sharing one scratch buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotSummary {
    Absent,
    Prepared { generation: u64 },
    Committed { generation: u64 },
    Corrupt,
}

impl SlotSummary {
    fn from_state(state: &RecordState<'_>) -> Self {
        match state {
            RecordState::Absent => SlotSummary::Absent,
            RecordState::Prepared(view) => SlotSummary::Prepared {
                generation: view.generation,
            },
            RecordState::Committed(view) => SlotSummary::Committed {
                generation: view.generation,
            },
            RecordState::Corrupt => SlotSummary::Corrupt,
        }
    }

    pub fn committed_generation(&self) -> Option<u64> {
        match self {
            SlotSummary::Committed { generation } => Some(*generation),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishError {
    /// Caller-supplied lengths, buffers, or generation ordering are wrong;
    /// nothing was written.
    BadInput,
    /// Both slots are committed with equal generations. Publication refuses
    /// to guess which to destroy; recovery owns this state.
    AmbiguousAuthority,
    /// Card I/O failed. The previous committed record is intact; a partial
    /// candidate may exist and classifies as prepared or corrupt for
    /// cleanup.
    Io,
    /// The reread after the prepared-body sync did not classify as this
    /// exact prepared record.
    PreparedVerify,
    /// The caller's final authority revalidation refused the commit. The
    /// prepared record is left for cleanup; no commit sector was written.
    RevalidationRefused,
    /// The record failed startup selection after the commit sync. The
    /// commit may or may not have landed; the caller must treat the pair as
    /// needing the startup selector's verdict, not assume either way.
    CommittedVerify,
}

/// Read one slot's file and summarize it. A missing file is [`SlotSummary::Absent`];
/// a file larger than `scratch` is corrupt by fiat — every legitimate record
/// type has a compile-time size bound, and an oversized file is exactly as
/// untrustworthy as a bad checksum, with the added property that reading it
/// whole is impossible.
pub fn summarize_slot<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    name: &str,
    magic: [u8; 4],
    scratch: &mut [u8],
) -> Result<SlotSummary, PublishError>
where
    D: BlockDevice,
    T: TimeSource,
{
    match read_whole_file(dir, name, scratch)? {
        None => Ok(SlotSummary::Absent),
        Some(ReadFile::Oversized) => Ok(SlotSummary::Corrupt),
        Some(ReadFile::Read(len)) => Ok(SlotSummary::from_state(&classify_record(
            &scratch[..len],
            magic,
        ))),
    }
}

/// Startup selection over a slot pair, through the same classification the
/// publish path verifies against. `Ok(None)` means no committed authority.
pub fn select_authority<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    pair: SlotPair<'_>,
    scratch: &mut [u8],
) -> Result<Option<(usize, u64)>, PublishError>
where
    D: BlockDevice,
    T: TimeSource,
{
    let a = summarize_slot(dir, pair.names[0], pair.magic, scratch)?;
    let b = summarize_slot(dir, pair.names[1], pair.magic, scratch)?;
    match select_summaries(a, b) {
        Selection::None => Ok(None),
        Selection::Selected { slot, generation } => Ok(Some((slot, generation))),
        Selection::Ambiguous => Err(PublishError::AmbiguousAuthority),
    }
}

/// [`select_generations`] lifted onto summaries.
fn select_summaries(a: SlotSummary, b: SlotSummary) -> Selection {
    select_generations(a.committed_generation(), b.committed_generation())
}

/// Select and read the pair's committed authority in one step: the record
/// the startup selector chose, reread and classified, with its body
/// borrowed out of `scratch` for typed decoding. `Ok(None)` when no
/// committed authority exists. A record that selected but then failed to
/// reread as committed — a mid-call surprise the serialization rules
/// should make impossible — reports `Io` rather than pretending absence.
pub fn read_committed<'s, D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    pair: SlotPair<'_>,
    scratch: &'s mut [u8],
) -> Result<Option<(usize, RecordState<'s>)>, PublishError>
where
    D: BlockDevice,
    T: TimeSource,
{
    let Some((slot, generation)) = select_authority(dir, pair, scratch)? else {
        return Ok(None);
    };
    let len = match read_whole_file(dir, pair.names[slot], scratch)? {
        Some(ReadFile::Read(len)) => len,
        _ => return Err(PublishError::Io),
    };
    let state = classify_record(&scratch[..len], pair.magic);
    match state.committed_generation() {
        Some(read_generation) if read_generation == generation => Ok(Some((slot, state))),
        _ => Err(PublishError::Io),
    }
}

/// Publish `sealed` (built by [`crate::record::seal_body`] into
/// `sealed_buf`) as the pair's next committed generation.
///
/// `revalidate` runs between the prepared verification and the commit-sector
/// write: it is the caller's last look at authority (lease still held,
/// source still current, not cancelled) with the guarantee that nothing
/// after a `true` but the commit itself can change the outcome. Returns the
/// slot index that now holds the committed record.
///
/// `scratch` must be at least `sealed.padded_len + COMMIT_FOOTER_BYTES`
/// bytes: it rereads the whole candidate file for both verifications.
pub fn publish_record<D, T, F, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    pair: SlotPair<'_>,
    sealed_buf: &[u8],
    sealed: &SealedBody,
    commit_nonce: u64,
    scratch: &mut [u8],
    revalidate: F,
) -> Result<usize, PublishError>
where
    D: BlockDevice,
    T: TimeSource,
    F: FnOnce() -> bool,
{
    let file_len = sealed
        .padded_len
        .checked_add(COMMIT_FOOTER_BYTES)
        .ok_or(PublishError::BadInput)?;
    if sealed_buf.len() < sealed.padded_len || scratch.len() < file_len {
        return Err(PublishError::BadInput);
    }
    let padded_at = u32::try_from(sealed.padded_len).map_err(|_| PublishError::BadInput)?;

    // Step 1: pick the target slot — never current authority.
    let a = summarize_slot(dir, pair.names[0], pair.magic, scratch)?;
    let b = summarize_slot(dir, pair.names[1], pair.magic, scratch)?;
    let target = match select_summaries(a, b) {
        Selection::Ambiguous => return Err(PublishError::AmbiguousAuthority),
        Selection::None => 0,
        Selection::Selected { slot, generation } => {
            // Generations are assigned above the highest committed one and
            // never reused; a publish that violates that is a caller bug
            // caught here, before any bytes move.
            if sealed.generation <= generation {
                return Err(PublishError::BadInput);
            }
            1 - slot
        }
    };

    // Steps 2–3: body, padding, zeroed commit sector, prepared-body sync.
    {
        let file = dir
            .open_file_in_dir(pair.names[target], Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| PublishError::Io)?;
        let write = file
            .write(&sealed_buf[..sealed.padded_len])
            .and_then(|()| file.write(&[0u8; COMMIT_FOOTER_BYTES]))
            .and_then(|()| durable_sync(&file));
        let closed = file.close();
        if write.is_err() || closed.is_err() {
            return Err(PublishError::Io);
        }
    }

    // Step 4: the prepared record must read back as exactly what was
    // written — same generation, same body bytes — through the classifier,
    // not a bespoke comparison.
    match read_whole_file(dir, pair.names[target], scratch)? {
        Some(ReadFile::Read(len)) if len == file_len => {}
        _ => return Err(PublishError::PreparedVerify),
    }
    match classify_record(&scratch[..file_len], pair.magic) {
        RecordState::Prepared(view)
            if view.generation == sealed.generation
                && view.logical_body == &sealed_buf[..sealed.logical_len] => {}
        _ => return Err(PublishError::PreparedVerify),
    }

    // Step 5: last look at authority before the point of no return.
    if !revalidate() {
        return Err(PublishError::RevalidationRefused);
    }

    // Steps 6–7: the commit-sector overwrite and the commit sync. The
    // sector is 512-aligned by construction (padded_len is a multiple of
    // 512), so this write shares no logical sector with body bytes.
    let sector = encode_commit_sector(sealed, commit_nonce).ok_or(PublishError::BadInput)?;
    {
        let file = dir
            .open_file_in_dir(pair.names[target], Mode::ReadWriteAppend)
            .map_err(|_| PublishError::Io)?;
        let write = file
            .seek_from_start(padded_at)
            .and_then(|()| file.write(&sector))
            .and_then(|()| durable_sync(&file));
        let closed = file.close();
        if write.is_err() || closed.is_err() {
            return Err(PublishError::Io);
        }
    }

    // Step 8: only the startup selector's verdict counts as success.
    match select_authority(dir, pair, scratch)? {
        Some((slot, generation)) if slot == target && generation == sealed.generation => Ok(target),
        _ => Err(PublishError::CommittedVerify),
    }
}

enum ReadFile {
    Read(usize),
    Oversized,
}

/// Read a whole file into `scratch`. `Ok(None)` when the file does not
/// exist; oversized files are reported without being read.
fn read_whole_file<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    name: &str,
    scratch: &mut [u8],
) -> Result<Option<ReadFile>, PublishError>
where
    D: BlockDevice,
    T: TimeSource,
{
    let file = match dir.open_file_in_dir(name, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(PublishError::Io),
    };
    let len = file.length() as usize;
    if len == 0 {
        let _ = file.close();
        return Ok(Some(ReadFile::Read(0)));
    }
    if len > scratch.len() {
        let _ = file.close();
        return Ok(Some(ReadFile::Oversized));
    }
    let mut at = 0usize;
    while at < len {
        match file.read(&mut scratch[at..len]) {
            Ok(0) | Err(_) => {
                let _ = file.close();
                return Err(PublishError::Io);
            }
            Ok(n) => at += n,
        }
    }
    let closed = file.close();
    if closed.is_err() {
        return Err(PublishError::Io);
    }
    Ok(Some(ReadFile::Read(len)))
}
