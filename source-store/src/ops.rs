//! Logical-book operations over the durable record layer. First vertical
//! slice: **delete**, plus the loaded idempotency store and catalog view
//! every operation shares.
//!
//! Delete goes first deliberately: it exercises the whole transaction
//! discipline — idempotency lookup *before* token validation, replay from
//! a receipt or from the tombstone itself, epoch freshness and headroom,
//! tombstone publication through the commit-sector protocol with final
//! authority revalidation, and receipt retention — without needing the
//! upload streaming machinery. Create/replace/recover build on the same
//! pieces.
//!
//! ## Serialization
//!
//! The PRD's storage owner and mutation lease appear here as a *calling
//! contract*, not a lock type: every function in this module must be
//! called from the single storage-owner task, one operation at a time.
//! That is the same single-writer rule the rest of the firmware already
//! lives by (only the board I/O task touches the SD), so the lease's job —
//! preventing authority from changing between final revalidation and
//! commit — is done by the task model. The revalidation hooks are still
//! implemented and still consulted: they are what turns "should be
//! impossible" interleavings into refused commits instead of corruption,
//! and they are the seam the M2-era cooperative jobs will need when an
//! operation spans executor turns.
//!
//! ## Memory
//!
//! [`OpsWorkspace`] is ~30 KB. On the device it must live in the Wi-Fi
//! session's loaned memory or a static — never a stack frame (the same
//! rule as `ReaderStore`); host tests `Box` it.

use embedded_sdmmc::{BlockDevice, Directory, Mode, TimeSource};

use crate::bodies::{
    SourceMetadata, Tombstone, BOOK_TOKEN_BYTES, DISPLAY_LABEL_MAX_BYTES, LOGICAL_BOOK_ID_BYTES,
    REQUEST_ID_BYTES, SHA256_BYTES, TOMBSTONE_MAGIC, TOMBSTONE_SCHEMA, TOMBSTONE_STATUS_DELETED,
};
use crate::layout;
use crate::publish::{self, PublishError};
use crate::receipts::{
    IdempotencyState, OperationReceipt, ReceiptLookup, ReceiptOperation, IDEMPOTENCY_MAGIC,
    IDEMPOTENCY_MAX_LOGICAL_BYTES, IDEMPOTENCY_SCHEMA, RECEIPT_STATUS_SUCCESS, REQUEST_NONCE_BYTES,
};
use crate::record::{self, RecordState};
use crate::select::{self, SlotDisposition, SlotEntry, MAX_SOURCE_SLOTS};
use crate::validate::{QuickFingerprintJob, Sha256Job};

/// Largest record file any operation reads or verifies: the idempotency
/// state at full receipt capacity.
pub const OPS_SCRATCH_BYTES: usize = match record::record_file_len(IDEMPOTENCY_MAX_LOGICAL_BYTES) {
    Some(len) => len,
    None => 0,
};
/// Largest sealed body any operation builds (the same record, without its
/// commit sector).
pub const OPS_SEAL_BYTES: usize = match record::padded_body_len(IDEMPOTENCY_MAX_LOGICAL_BYTES) {
    Some(len) => len,
    None => 0,
};

// The `None => 0` arms above exist only to satisfy `match` in const
// context; a zero would mean the record layout overflowed usize, which
// these assertions rule out at compile time.
const _: () = assert!(OPS_SCRATCH_BYTES > 0 && OPS_SEAL_BYTES > 0);

/// Working memory for one operation. See the module docs for placement
/// rules.
pub struct OpsWorkspace {
    pub record_scratch: [u8; OPS_SCRATCH_BYTES],
    pub seal_scratch: [u8; OPS_SEAL_BYTES],
    pub entries: [Option<SlotEntry>; MAX_SOURCE_SLOTS],
    pub dispositions: [SlotDisposition; MAX_SOURCE_SLOTS],
    pub tombstones: [Option<(u8, Tombstone)>; layout::MAX_TOMBSTONE_SLOTS],
}

impl OpsWorkspace {
    pub fn new() -> Self {
        Self {
            record_scratch: [0; OPS_SCRATCH_BYTES],
            seal_scratch: [0; OPS_SEAL_BYTES],
            entries: [None; MAX_SOURCE_SLOTS],
            dispositions: [SlotDisposition::HiddenDeleted; MAX_SOURCE_SLOTS],
            tombstones: [None; layout::MAX_TOMBSTONE_SLOTS],
        }
    }
}

impl Default for OpsWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

/// The resident idempotency state plus the committed record generation it
/// was loaded from — what [`publish`][Self::publish] increments.
pub struct IdempotencyStore {
    pub state: IdempotencyState,
    pub record_generation: u64,
}

impl IdempotencyStore {
    /// Load the committed idempotency record, or start fresh when none
    /// exists. Corruption of *both* slots — or a committed record that no
    /// longer decodes — fails closed: idempotency state must never be
    /// silently reset while retries may be in flight, per the PRD.
    pub fn load<D, T, const MD: usize, const MF: usize, const MV: usize>(
        dir: &Directory<'_, D, T, MD, MF, MV>,
        ws: &mut OpsWorkspace,
    ) -> Result<Self, PublishError>
    where
        D: BlockDevice,
        T: TimeSource,
    {
        let names = layout::idempotency_pair();
        match publish::read_committed(dir, names.pair(), &mut ws.record_scratch)? {
            None => Ok(Self {
                state: IdempotencyState::initial(),
                record_generation: 0,
            }),
            Some((_, RecordState::Committed(view))) => {
                let state = IdempotencyState::decode(&view).ok_or(PublishError::Io)?;
                Ok(Self {
                    state,
                    record_generation: view.generation,
                })
            }
            Some(_) => Err(PublishError::Io),
        }
    }

    /// Publish the resident state as the next committed record generation.
    pub fn publish<D, T, const MD: usize, const MF: usize, const MV: usize>(
        &mut self,
        dir: &Directory<'_, D, T, MD, MF, MV>,
        ws: &mut OpsWorkspace,
    ) -> Result<(), PublishError>
    where
        D: BlockDevice,
        T: TimeSource,
    {
        let logical = self
            .state
            .encode_into(&mut ws.seal_scratch)
            .ok_or(PublishError::BadInput)?;
        let generation = self
            .record_generation
            .checked_add(1)
            .ok_or(PublishError::BadInput)?;
        let sealed = record::seal_body(
            IDEMPOTENCY_MAGIC,
            IDEMPOTENCY_SCHEMA,
            generation,
            logical,
            &mut ws.seal_scratch,
        )
        .ok_or(PublishError::BadInput)?;
        let names = layout::idempotency_pair();
        publish::publish_record(
            dir,
            names.pair(),
            &ws.seal_scratch,
            &sealed,
            0,
            &mut ws.record_scratch,
            || true,
        )?;
        self.record_generation = generation;
        Ok(())
    }
}

/// Load every committed source-metadata record and tombstone, then run
/// startup selection. Fills `ws.entries`, `ws.tombstones`, and
/// `ws.dispositions` (parallel to `entries`); this is the reboot view of
/// the catalog and the base state of every operation.
pub fn load_catalog<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    ws: &mut OpsWorkspace,
) -> Result<(), PublishError>
where
    D: BlockDevice,
    T: TimeSource,
{
    ws.entries = [None; MAX_SOURCE_SLOTS];
    ws.tombstones = [None; layout::MAX_TOMBSTONE_SLOTS];
    for slot in 0..MAX_SOURCE_SLOTS as u8 {
        let Some(names) = layout::metadata_pair(slot) else {
            break;
        };
        // A slot whose pair is ambiguous or unreadable poisons the whole
        // catalog load rather than being skipped: skipping would present a
        // book as absent while its records still exist, which is exactly
        // the misreading the selector rules prohibit.
        if let Some((_, RecordState::Committed(view))) =
            publish::read_committed(dir, names.pair(), &mut ws.record_scratch)?
        {
            let metadata = SourceMetadata::decode(&view).ok_or(PublishError::Io)?;
            ws.entries[usize::from(slot)] = Some(SlotEntry {
                physical_slot: slot,
                metadata,
            });
        }
    }
    for slot in 0..layout::MAX_TOMBSTONE_SLOTS as u8 {
        let Some(names) = layout::tombstone_pair(slot) else {
            break;
        };
        if let Some((_, RecordState::Committed(view))) =
            publish::read_committed(dir, names.pair(), &mut ws.record_scratch)?
        {
            let stone = Tombstone::decode(&view).ok_or(PublishError::Io)?;
            ws.tombstones[usize::from(slot)] = Some((slot, stone));
        }
    }
    run_selection(ws).ok_or(PublishError::BadInput)?;
    Ok(())
}

/// Re-run selection over the loaded records. Compacts entries and
/// tombstones into scratch arrays for [`select::select_sources`], then
/// scatters dispositions back parallel to `ws.entries`.
fn run_selection(ws: &mut OpsWorkspace) -> Option<()> {
    let mut flat_entries = [dummy_entry(); MAX_SOURCE_SLOTS];
    let mut count = 0usize;
    for entry in ws.entries.iter().flatten() {
        flat_entries[count] = *entry;
        count += 1;
    }
    let mut flat_stones = [dummy_stone(); layout::MAX_TOMBSTONE_SLOTS];
    let mut stone_count = 0usize;
    for (_, stone) in ws.tombstones.iter().flatten() {
        flat_stones[stone_count] = *stone;
        stone_count += 1;
    }
    let mut dispositions = [SlotDisposition::HiddenDeleted; MAX_SOURCE_SLOTS];
    select::select_sources(
        &flat_entries[..count],
        &flat_stones[..stone_count],
        &mut dispositions[..count],
    )?;
    // Scatter back: dispositions[i] corresponds to the i-th present entry.
    ws.dispositions = [SlotDisposition::HiddenDeleted; MAX_SOURCE_SLOTS];
    let mut at = 0usize;
    for (slot, entry) in ws.entries.iter().enumerate() {
        if entry.is_some() {
            ws.dispositions[slot] = dispositions[at];
            at += 1;
        }
    }
    Some(())
}

/// What one on-card file actually is: its length, full SHA-256, and
/// policy-v1 quick fingerprint, all computed from the persisted bytes in
/// one pass over the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    pub length: u64,
    pub sha256: [u8; SHA256_BYTES],
    pub quick_fingerprint: [u8; SHA256_BYTES],
}

/// Hash a file's persisted identity. `Ok(None)` when the file does not
/// exist. Reads in workspace-scratch-sized chunks; the caller compares the
/// result against whatever identity it needs to prove or disprove.
pub fn hash_file_identity<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    name: &str,
    ws: &mut OpsWorkspace,
) -> Result<Option<FileIdentity>, PublishError>
where
    D: BlockDevice,
    T: TimeSource,
{
    let file = match dir.open_file_in_dir(name, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(PublishError::Io),
    };
    let length = u64::from(file.length());
    let mut sha = Sha256Job::new(length);
    let mut quick = QuickFingerprintJob::new(length);
    let mut failed = false;
    while sha.remaining() > 0 {
        let want = (sha.remaining() as usize).min(ws.record_scratch.len());
        if read_exact(&file, &mut ws.record_scratch[..want]).is_err()
            || sha.update(&ws.record_scratch[..want]).is_err()
        {
            failed = true;
            break;
        }
    }
    while !failed {
        let Some((offset, remaining)) = quick.next_read() else {
            break;
        };
        let want = (remaining as usize).min(ws.record_scratch.len());
        let seek = u32::try_from(offset)
            .ok()
            .and_then(|offset| file.seek_from_start(offset).ok());
        if seek.is_none()
            || read_exact(&file, &mut ws.record_scratch[..want]).is_err()
            || quick.update(&ws.record_scratch[..want]).is_err()
        {
            failed = true;
        }
    }
    let closed = file.close();
    if failed || closed.is_err() {
        return Err(PublishError::Io);
    }
    match (sha.finish(), quick.finish()) {
        (Ok(sha256), Ok(quick_fingerprint)) => Ok(Some(FileIdentity {
            length,
            sha256,
            quick_fingerprint,
        })),
        _ => Err(PublishError::Io),
    }
}

fn read_exact<D, T, const MD: usize, const MF: usize, const MV: usize>(
    file: &embedded_sdmmc::File<'_, D, T, MD, MF, MV>,
    buf: &mut [u8],
) -> Result<(), ()>
where
    D: BlockDevice,
    T: TimeSource,
{
    let mut at = 0usize;
    while at < buf.len() {
        match file.read(&mut buf[at..]) {
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => at += n,
        }
    }
    Ok(())
}

/// The authoritative entry carrying `book_token`, if any. Superseded,
/// deleted, and ambiguous slots never match — a stale token is unknown,
/// not a handle to hidden state.
pub fn find_authoritative_by_token(
    ws: &OpsWorkspace,
    book_token: &[u8; BOOK_TOKEN_BYTES],
) -> Option<SlotEntry> {
    for (slot, entry) in ws.entries.iter().enumerate() {
        if let Some(entry) = entry {
            if ws.dispositions[slot] == SlotDisposition::Authoritative
                && entry.metadata.book_token == *book_token
            {
                return Some(*entry);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// A parsed `POST /delete-book`: the epoch-scoped request ID and the
/// authoritative token the client believes it is deleting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeleteRequest {
    pub epoch: u64,
    pub nonce: [u8; REQUEST_NONCE_BYTES],
    pub book_token: [u8; BOOK_TOKEN_BYTES],
}

impl DeleteRequest {
    /// The 24-byte binary request ID bound into tombstones and receipts.
    fn request_id(&self) -> [u8; REQUEST_ID_BYTES] {
        let mut id = [0u8; REQUEST_ID_BYTES];
        id[..8].copy_from_slice(&self.epoch.to_le_bytes());
        id[8..].copy_from_slice(&self.nonce);
        id
    }

    /// The receipt-shaped view of this request used for lookup and, on
    /// success, retention. `logical_book_id` is filled by the device once
    /// resolved; it is not a compared parameter.
    fn receipt(&self, logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES]) -> OperationReceipt {
        OperationReceipt {
            epoch: self.epoch,
            request_nonce: self.nonce,
            operation: ReceiptOperation::Delete,
            logical_book_id,
            base_book_token_or_zero: self.book_token,
            source_generation: 0,
            source_length_or_zero: 0,
            source_sha256_or_zero: [0; SHA256_BYTES],
            display_label_len: 0,
            display_label: [0; DISPLAY_LABEL_MAX_BYTES],
            result_book_token_or_zero: [0; BOOK_TOKEN_BYTES],
            result_status: RECEIPT_STATUS_SUCCESS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// Committed — now, or by the earlier operation this request replays.
    /// `receipt_durable` is false when the receipt could not be persisted
    /// (replay then rests on the tombstone until cleanup migrates it).
    Deleted {
        logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
        replayed: bool,
        receipt_durable: bool,
    },
    /// No authoritative book carries this token.
    RejectedUnknownToken,
    /// Genuinely new request against a non-current epoch.
    RejectedStaleEpoch,
    /// This request ID was already used with different parameters.
    RejectedParameterMismatch,
    /// The current epoch's operation budget is exhausted; retryable after
    /// rotation.
    RejectedEpochExhausted,
    /// Every tombstone slot is occupied; retryable after cleanup.
    RejectedNoTombstoneSlot,
    Failed(PublishError),
}

/// Execute one delete request end to end, per the PRD's transaction:
/// resolve the request ID through receipts and tombstones *before* token
/// validation, then — for a genuinely new request — require the current
/// epoch and headroom, validate the token against the loaded catalog,
/// durably publish the tombstone with final revalidation, and retain the
/// receipt.
///
/// The caller passes a freshly [`load_catalog`]ed workspace; on
/// `Deleted { replayed: false, .. }` the workspace's catalog view is
/// reloaded so the hidden book is already gone from it.
pub fn delete_book<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    idem: &mut IdempotencyStore,
    req: &DeleteRequest,
    ws: &mut OpsWorkspace,
) -> DeleteOutcome
where
    D: BlockDevice,
    T: TimeSource,
{
    // 1. Receipts first: a known request ID answers from its receipt no
    //    matter how stale its token has become.
    let probe = req.receipt([0; LOGICAL_BOOK_ID_BYTES]);
    match idem.state.lookup(&probe) {
        ReceiptLookup::Replay(receipt) => {
            return DeleteOutcome::Deleted {
                logical_book_id: receipt.logical_book_id,
                replayed: true,
                receipt_durable: true,
            }
        }
        ReceiptLookup::ParameterMismatch => return DeleteOutcome::RejectedParameterMismatch,
        ReceiptLookup::Unknown => {}
    }

    // 2. Tombstones second: the tombstone carries the delete's request
    //    identity precisely so a retry that lost its receipt (crash
    //    between tombstone commit and receipt publication) still replays.
    let request_id = req.request_id();
    for (_, stone) in ws.tombstones.iter().flatten() {
        if stone.delete_request_id == request_id {
            if stone.deleted_book_token == req.book_token {
                return DeleteOutcome::Deleted {
                    logical_book_id: stone.logical_book_id,
                    replayed: true,
                    receipt_durable: false,
                };
            }
            return DeleteOutcome::RejectedParameterMismatch;
        }
    }

    // 3. Genuinely new: epoch freshness, then budget — both checked before
    //    anything commits, so a rejection is always a clean no-op.
    if !idem.state.epoch_is_current(req.epoch) {
        return DeleteOutcome::RejectedStaleEpoch;
    }
    if !idem.state.has_epoch_headroom() {
        return DeleteOutcome::RejectedEpochExhausted;
    }

    // 4. Ordinary token validation against the loaded catalog.
    let Some(entry) = find_authoritative_by_token(ws, &req.book_token) else {
        return DeleteOutcome::RejectedUnknownToken;
    };

    // 5. A free tombstone slot.
    let occupied = &ws.tombstones;
    let free_slot =
        (0..layout::MAX_TOMBSTONE_SLOTS as u8).find(|slot| occupied[usize::from(*slot)].is_none());
    let Some(tombstone_slot) = free_slot else {
        return DeleteOutcome::RejectedNoTombstoneSlot;
    };

    // 6. Prepare and durably publish the tombstone. The revalidation hook
    //    rereads the book's metadata pair immediately before the commit
    //    sector lands: same token, same generation, still committed.
    let stone = Tombstone {
        logical_book_id: entry.metadata.logical_book_id,
        deleted_source_generation: entry.metadata.source_generation,
        deleted_book_token: req.book_token,
        delete_request_id: request_id,
        delete_result_status: TOMBSTONE_STATUS_DELETED,
    };
    let Some(logical) = stone.encode_into(&mut ws.seal_scratch) else {
        return DeleteOutcome::Failed(PublishError::BadInput);
    };
    let Some(sealed) = record::seal_body(
        TOMBSTONE_MAGIC,
        TOMBSTONE_SCHEMA,
        1,
        logical,
        &mut ws.seal_scratch,
    ) else {
        return DeleteOutcome::Failed(PublishError::BadInput);
    };
    let Some(names) = layout::tombstone_pair(tombstone_slot) else {
        return DeleteOutcome::Failed(PublishError::BadInput);
    };
    let expected_token = req.book_token;
    let expected_generation = entry.metadata.source_generation;
    let book_slot = entry.physical_slot;
    let revalidate = || {
        let Some(meta_names) = layout::metadata_pair(book_slot) else {
            return false;
        };
        let mut scratch = [0u8; TOMBSTONE_REVALIDATE_SCRATCH];
        match publish::read_committed(dir, meta_names.pair(), &mut scratch) {
            Ok(Some((_, RecordState::Committed(view)))) => SourceMetadata::decode(&view)
                .is_some_and(|meta| {
                    meta.book_token == expected_token
                        && meta.source_generation == expected_generation
                }),
            _ => false,
        }
    };
    if let Err(error) = publish::publish_record(
        dir,
        names.pair(),
        &ws.seal_scratch,
        &sealed,
        0,
        &mut ws.record_scratch,
        revalidate,
    ) {
        return DeleteOutcome::Failed(error);
    }

    // 7. The deletion is committed. Retain the receipt; a failure here
    //    degrades replay to the tombstone path, never the outcome.
    let receipt_durable = idem
        .state
        .insert(req.receipt(entry.metadata.logical_book_id))
        .is_ok()
        && idem.publish(dir, ws).is_ok();

    // 8. Reload the catalog view so the caller sees the book hidden.
    if load_catalog(dir, ws).is_err() {
        // The deletion stands; a failed reload only stales the view.
        ws.entries = [None; MAX_SOURCE_SLOTS];
    }

    DeleteOutcome::Deleted {
        logical_book_id: stone.logical_book_id,
        replayed: false,
        receipt_durable,
    }
}

/// Metadata record files are exactly one padded sector plus the commit
/// sector; the revalidation closure rereads one inside the publish call,
/// where the shared workspace scratch is already borrowed.
const TOMBSTONE_REVALIDATE_SCRATCH: usize =
    match record::record_file_len(crate::bodies::SOURCE_METADATA_LOGICAL_BYTES) {
        Some(len) => len,
        None => 0,
    };

fn dummy_entry() -> SlotEntry {
    SlotEntry {
        physical_slot: 0,
        metadata: SourceMetadata {
            logical_book_id: [0; LOGICAL_BOOK_ID_BYTES],
            source_generation: 0,
            source_origin: crate::bodies::SourceOrigin::UnmanagedSd,
            operation_kind: crate::bodies::OperationKind::LocalUnmanagedOperation,
            operation_request_id: [0; REQUEST_ID_BYTES],
            externally_recovered: false,
            physical_slot: 0,
            source_length: 0,
            source_sha256: [0; SHA256_BYTES],
            quick_fingerprint_policy_version: 0,
            quick_fingerprint_sha256: [0; SHA256_BYTES],
            book_token: [0; BOOK_TOKEN_BYTES],
            display_label: crate::bodies::DisplayLabel::placeholder(),
        },
    }
}

fn dummy_stone() -> Tombstone {
    Tombstone {
        logical_book_id: [0; LOGICAL_BOOK_ID_BYTES],
        deleted_source_generation: 0,
        deleted_book_token: [0; BOOK_TOKEN_BYTES],
        delete_request_id: [0; REQUEST_ID_BYTES],
        delete_result_status: TOMBSTONE_STATUS_DELETED,
    }
}
