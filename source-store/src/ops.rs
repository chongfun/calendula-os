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
    REQUEST_ID_BYTES, SHA256_BYTES, SOURCE_METADATA_SCHEMA, TOMBSTONE_MAGIC, TOMBSTONE_SCHEMA,
    TOMBSTONE_STATUS_DELETED,
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
    /// Whether `entries`/`dispositions`/`tombstones` came from a *complete*
    /// [`load_catalog`]. See [`catalog_is_valid`][Self::catalog_is_valid].
    catalog_valid: bool,
}

impl OpsWorkspace {
    pub fn new() -> Self {
        Self {
            record_scratch: [0; OPS_SCRATCH_BYTES],
            seal_scratch: [0; OPS_SEAL_BYTES],
            entries: [None; MAX_SOURCE_SLOTS],
            dispositions: [SlotDisposition::HiddenDeleted; MAX_SOURCE_SLOTS],
            tombstones: [None; layout::MAX_TOMBSTONE_SLOTS],
            catalog_valid: false,
        }
    }

    /// Whether the catalog view may be acted on.
    ///
    /// A partial or failed load is *not* an empty catalog, and the
    /// difference is destructive: an empty catalog says every slot is free,
    /// and the create path opens a free slot's EPUB with
    /// `ReadWriteCreateOrTruncate`. A workspace whose reload failed would
    /// therefore truncate a committed book whose records merely could not
    /// be read. Operations refuse to run until a complete load succeeds; a
    /// fresh workspace starts invalid for the same reason.
    pub fn catalog_is_valid(&self) -> bool {
        self.catalog_valid
    }
}

impl Default for OpsWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

/// The resident idempotency state plus the committed record generation it
/// was loaded from — what [`publish`][Self::publish] increments.
///
/// ## The resident state is a copy of the card's, or the store is unusable
///
/// Between a [`load`][Self::load] and the next [`publish`][Self::publish],
/// callers stage exactly one receipt into `state` and publish it
/// immediately, so at every *operation boundary* the resident state is
/// meant to equal the committed record. That equality is what lets a replay
/// served from `state` report `receipt_durable: true`: the receipt answering
/// the retry is on the card, so a reboot mid-retry answers the same way.
///
/// An uncertain publication is the one thing that can break the equality —
/// the staged receipt may or may not have landed — so `publish` re-reads the
/// card on failure and adopts whatever it finds. When even that read fails,
/// the store cannot say what is committed and marks itself unusable
/// ([`is_usable`][Self::is_usable]); operations refuse rather than serve
/// durability claims from a state whose durability is unknown.
pub struct IdempotencyStore {
    pub state: IdempotencyState,
    pub record_generation: u64,
    /// Whether `state` is known to be a copy of the committed record. See
    /// the type docs; only an unrecoverable publication clears it.
    committed_state_known: bool,
}

impl IdempotencyStore {
    /// Whether this store may answer questions about what is committed.
    /// False after a publication failed *and* the card could not then be
    /// re-read; a fresh [`load`][Self::load] is the way back.
    pub fn is_usable(&self) -> bool {
        self.committed_state_known
    }

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
                committed_state_known: true,
            }),
            Some((_, RecordState::Committed(view))) => {
                if view.schema_version != IDEMPOTENCY_SCHEMA {
                    return Err(PublishError::UnsupportedSchema);
                }
                let state = IdempotencyState::decode(&view).ok_or(PublishError::Io)?;
                Ok(Self {
                    state,
                    record_generation: view.generation,
                    committed_state_known: true,
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
        match publish::publish_record(
            dir,
            names.pair(),
            &ws.seal_scratch,
            &sealed,
            0,
            &mut ws.record_scratch,
            || true,
        ) {
            Ok(_) => {
                self.record_generation = generation;
                Ok(())
            }
            Err(error) => {
                // A failure after the commit sync still leaves the record
                // committed (`publish_record` says so explicitly), so
                // "publish failed" means neither "generation N is still
                // authority" nor "the staged receipt is not on the card" —
                // both are now unknown, and both matter. Keeping the
                // resident counter at N would make every future publication
                // propose N+1 again, which `publish_record` refuses as not
                // above authority; keeping the resident *state* would let a
                // same-session retry replay a receipt that may exist only in
                // memory and call it durable. Ask the card for both.
                self.resync_from_card(dir, ws);
                Err(error)
            }
        }
    }

    /// Re-read the committed record and adopt it wholesale — state and
    /// generation together, because after an uncertain publication both are
    /// in question and only the card can settle either.
    ///
    /// This deliberately discards a staged receipt that did not land. Losing
    /// it costs nothing but reclamation speed: the operation itself
    /// committed, its metadata or tombstone still carries the request ID, and
    /// the receipt-loss fallback answers the retry from there — reporting
    /// `receipt_durable: false`, which is the truth.
    ///
    /// A card that cannot be re-read leaves the store unusable rather than
    /// guessing; see [`is_usable`][Self::is_usable].
    fn resync_from_card<D, T, const MD: usize, const MF: usize, const MV: usize>(
        &mut self,
        dir: &Directory<'_, D, T, MD, MF, MV>,
        ws: &mut OpsWorkspace,
    ) where
        D: BlockDevice,
        T: TimeSource,
    {
        match Self::load(dir, ws) {
            Ok(committed) => {
                self.state = committed.state;
                self.record_generation = committed.record_generation;
                self.committed_state_known = true;
            }
            Err(_) => self.committed_state_known = false,
        }
    }
}

/// Load every committed source-metadata record and tombstone, then run
/// startup selection. Fills `ws.entries`, `ws.tombstones`, and
/// `ws.dispositions` (parallel to `entries`); this is the reboot view of
/// the catalog and the base state of every operation.
///
/// The workspace's catalog is marked invalid for the duration and valid
/// again only on a complete success, so a caller that ignores the `Err` —
/// or one that keeps using the workspace after it — still cannot act on a
/// half-loaded view. See [`OpsWorkspace::catalog_is_valid`].
pub fn load_catalog<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    ws: &mut OpsWorkspace,
) -> Result<(), PublishError>
where
    D: BlockDevice,
    T: TimeSource,
{
    ws.catalog_valid = false;
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
            // A schema this build does not read is a different answer from
            // corruption, and the caller can act on the difference.
            if view.schema_version != SOURCE_METADATA_SCHEMA {
                return Err(PublishError::UnsupportedSchema);
            }
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
            if view.schema_version != TOMBSTONE_SCHEMA {
                return Err(PublishError::UnsupportedSchema);
            }
            let stone = Tombstone::decode(&view).ok_or(PublishError::Io)?;
            ws.tombstones[usize::from(slot)] = Some((slot, stone));
        }
    }
    run_selection(ws).ok_or(PublishError::BadInput)?;
    ws.catalog_valid = true;
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
        // See `publish::confirm_absent`: a read-only open reports a card
        // that could not answer as a file that is not there, and "the
        // source is gone" is a conclusion this layer acts on.
        Err(embedded_sdmmc::Error::NotFound) => {
            publish::confirm_absent(dir, name)?;
            return Ok(None);
        }
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

/// A file's persisted `(length, SHA-256)` using caller-supplied scratch
/// instead of the workspace.
///
/// The final-revalidation closures need this: they run *inside*
/// [`publish::publish_record`], which already holds the workspace, and they
/// are the only place that can bind exact identity to the commit itself.
/// `scratch` may be small — it is a read chunk, not a whole-file buffer —
/// at the cost of more read calls; the callers use a stack buffer.
pub fn sha256_file_identity<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    name: &str,
    scratch: &mut [u8],
) -> Result<Option<(u64, [u8; SHA256_BYTES])>, PublishError>
where
    D: BlockDevice,
    T: TimeSource,
{
    if scratch.is_empty() {
        return Err(PublishError::BadInput);
    }
    let file = match dir.open_file_in_dir(name, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => {
            publish::confirm_absent(dir, name)?;
            return Ok(None);
        }
        Err(_) => return Err(PublishError::Io),
    };
    let length = u64::from(file.length());
    let mut sha = Sha256Job::new(length);
    let mut failed = false;
    while sha.remaining() > 0 {
        let want = (sha.remaining() as usize).min(scratch.len());
        if read_exact(&file, &mut scratch[..want]).is_err() || sha.update(&scratch[..want]).is_err()
        {
            failed = true;
            break;
        }
    }
    let closed = file.close();
    if failed || closed.is_err() {
        return Err(PublishError::Io);
    }
    match sha.finish() {
        Ok(sha256) => Ok(Some((length, sha256))),
        Err(_) => Err(PublishError::Io),
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

/// What committed state says about a request ID whose receipt is gone.
///
/// See [`find_request_trace`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestTrace {
    /// No committed record carries this request ID.
    None,
    /// A source-metadata record carries it: a create, replace, or recovery.
    Metadata(SlotEntry),
    /// A tombstone carries it: a delete.
    Tombstone(Tombstone),
    /// *More than one* committed record carries it, so the card holds
    /// contradictory accounts of what this request did. No answer can be
    /// served from evidence that disagrees with itself; every endpoint
    /// refuses.
    Conflict,
}

/// Find the committed record — of *any* type — that carries `request_id`.
///
/// The request-ID namespace is global. `(epoch, nonce)` names a request, not
/// a request-to-one-endpoint, and the receipt table enforces that on its own:
/// one sorted table across all operations, with
/// [`OperationReceipt::matches_parameters`] comparing the operation itself,
/// so a delete's ID reused by an upload resolves to `ParameterMismatch`.
///
/// The receipt-loss fallback has to enforce the same namespace, and it reads
/// durable records — which are per-operation files. An endpoint that
/// searched only the record type it writes would find nothing for an ID
/// spent on a different operation, call the request genuinely new, and
/// execute it: a second execution under a request ID that has already been
/// answered, with the evidence sitting in a file that endpoint never opens.
/// So every endpoint asks this one question instead.
///
/// Every record is scanned, not just up to the first hit, because a card
/// *can* carry two — and the scan order must not be what decides the answer.
/// A build whose delete fallback searched only tombstones would execute a
/// delete under an ID an upload had already spent, leaving metadata and a
/// tombstone both bearing it; the schema does not change across that upgrade,
/// so such a card loads normally. Returning the first hit would then replay
/// an upload result for a book that was subsequently deleted, or refuse a
/// delete whose own tombstone is sitting right there. Neither is an answer,
/// so a second match is [`RequestTrace::Conflict`] and every endpoint fails
/// closed. Duplicate metadata or duplicate tombstones count the same way:
/// whatever produced them, one request did not commit twice.
///
/// Receipts are still resolved first, ahead of this. A receipt is a stronger
/// and later record than either file — the operation that wrote it saw the
/// fallback state and was allowed to proceed — so a card with a receipt has
/// an unambiguous answer even when its older records disagree.
///
/// Epoch-zero IDs are outside the namespace and never match: they are local
/// unmanaged provenance (see [`crate::unmanaged`]), which no client request
/// can validly carry, and which is not receipted at all.
pub fn find_request_trace(ws: &OpsWorkspace, request_id: &[u8; REQUEST_ID_BYTES]) -> RequestTrace {
    if request_id[..8] == [0u8; 8] {
        return RequestTrace::None;
    }
    let mut found = RequestTrace::None;
    for entry in ws.entries.iter().flatten() {
        if entry.metadata.operation_request_id == *request_id {
            if !matches!(found, RequestTrace::None) {
                return RequestTrace::Conflict;
            }
            found = RequestTrace::Metadata(*entry);
        }
    }
    for (_, stone) in ws.tombstones.iter().flatten() {
        if stone.delete_request_id == *request_id {
            if !matches!(found, RequestTrace::None) {
                return RequestTrace::Conflict;
            }
            found = RequestTrace::Tombstone(*stone);
        }
    }
    found
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
    /// The workspace's catalog view is not a complete load of committed
    /// state, so no operation may act on it. Retryable once a reload
    /// succeeds; see [`OpsWorkspace::catalog_is_valid`].
    CatalogUnavailable,
    /// The idempotency store cannot say what is committed, so no request may
    /// be resolved against it. Retryable once a reload succeeds; see
    /// [`IdempotencyStore::is_usable`].
    IdempotencyUnavailable,
    /// Committed records disagree about what this request ID already did;
    /// see [`RequestTrace::Conflict`]. Not retryable as-is — the card needs
    /// attention, not another attempt.
    AmbiguousRequestEvidence,
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
    if !ws.catalog_is_valid() {
        return DeleteOutcome::CatalogUnavailable;
    }
    if !idem.is_usable() {
        return DeleteOutcome::IdempotencyUnavailable;
    }

    // 1. Receipts first: a known request ID answers from its receipt no
    //    matter how stale its token has become.
    let probe = req.receipt([0; LOGICAL_BOOK_ID_BYTES]);
    match idem.state.lookup(&probe) {
        ReceiptLookup::Replay(receipt) => {
            return DeleteOutcome::Deleted {
                logical_book_id: receipt.logical_book_id,
                replayed: true,
                // The guard above is what makes this true rather than
                // hopeful: a usable store's receipts are the card's.
                receipt_durable: true,
            };
        }
        ReceiptLookup::ParameterMismatch => return DeleteOutcome::RejectedParameterMismatch,
        ReceiptLookup::Unknown => {}
    }

    // 2. Committed records second, across every operation type: the
    //    tombstone carries the delete's request identity precisely so a
    //    retry that lost its receipt (crash between tombstone commit and
    //    receipt publication) still replays — and a metadata record
    //    carrying it means the ID was already spent on an upload or a
    //    recovery, which a delete result is no answer to.
    let request_id = req.request_id();
    match find_request_trace(ws, &request_id) {
        RequestTrace::Tombstone(stone) => {
            if stone.deleted_book_token == req.book_token {
                return DeleteOutcome::Deleted {
                    logical_book_id: stone.logical_book_id,
                    replayed: true,
                    receipt_durable: false,
                };
            }
            return DeleteOutcome::RejectedParameterMismatch;
        }
        RequestTrace::Metadata(_) => return DeleteOutcome::RejectedParameterMismatch,
        RequestTrace::Conflict => return DeleteOutcome::AmbiguousRequestEvidence,
        RequestTrace::None => {}
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

    // 8. Reload the catalog view so the caller sees the book hidden. The
    //    deletion stands either way; a failed reload leaves the workspace
    //    marked invalid, which blocks the *next* operation rather than
    //    corrupting it.
    let _ = load_catalog(dir, ws);

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
            request_binding_sha256: [0; SHA256_BYTES],
            externally_recovered: false,
            physical_slot: 0,
            source_length: 0,
            source_sha256: [0; SHA256_BYTES],
            quick_fingerprint_policy_version: 0,
            quick_fingerprint_sha256: [0; SHA256_BYTES],
            book_token: [0; BOOK_TOKEN_BYTES],
            display_label: crate::bodies::DisplayLabel::placeholder(),
            unmanaged_name: crate::bodies::UnmanagedName::none(),
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
