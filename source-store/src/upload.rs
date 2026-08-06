//! The create/replace upload transaction: how streamed EPUB bytes become a
//! committed source generation, and every way they fail to.
//!
//! Split from [`crate::ops`] for readability only — it shares the
//! workspace, idempotency store, and catalog helpers, and the same
//! storage-owner calling contract (one operation at a time, from the one
//! task that owns the SD).
//!
//! The transaction is caller-driven around the byte stream, because the
//! bytes arrive from the Wi-Fi task at network pace:
//!
//! ```text
//! begin_upload      resolve idempotency, authority, slot; commit the
//!                   staging marker; create the empty candidate
//! upload_chunk*     append bytes, hashing as they land
//! finish_upload     durably sync, reread and rehash every persisted
//!                   byte, gate the container, fingerprint, publish
//!                   metadata with final revalidation, retain the receipt,
//!                   remove the marker
//! abort_upload      delete the candidate; the committed marker keeps the
//!                   slot quarantined either way
//! ```
//!
//! The ordering that matters: the staging marker commits *before* the
//! candidate file exists (a reserved slot is always explained), and source
//! metadata commits only after the persisted bytes — not the received
//! bytes — have been independently reread and matched against the declared
//! identity. A power cut anywhere leaves either the previous catalog or
//! the new generation; the in-between states are all marker-plus-candidate
//! shapes that selection ignores and cleanup can reclaim.
//!
//! The classic-ZIP/ZIP64 container gate is a caller hook
//! (`validate_container`): the ZIP walker lives in `proto`, which this
//! crate deliberately does not depend on. Firmware passes the real
//! `ZipStream` bounds check; tests pass verdicts.

use embedded_sdmmc::{BlockDevice, Directory, Mode, TimeSource};
use heapless::String;

use crate::bodies::{
    DisplayLabel, OperationKind, RequestBinding, SourceMetadata, SourceOrigin, StagedOperation,
    StagingMarker, UnmanagedName, BINDING_TAG_UPLOAD, BOOK_TOKEN_BYTES, LOGICAL_BOOK_ID_BYTES,
    REQUEST_ID_BYTES, SHA256_BYTES, STAGING_MARKER_MAGIC, STAGING_MARKER_SCHEMA,
};
use crate::layout;
use crate::ops::{
    find_authoritative_by_token, find_request_trace, hash_file_identity, load_catalog,
    marker_claims_request, no_committed_tombstone_for, receipt_is_consistent_with_trace,
    IdempotencyStore, OpsWorkspace, RequestTrace,
};
use crate::publish::{self, PublishError};
use crate::receipts::{
    OperationReceipt, ReceiptLookup, ReceiptOperation, RECEIPT_STATUS_SUCCESS, REQUEST_NONCE_BYTES,
};
use crate::record::{self, RecordState};
use crate::select::MAX_SOURCE_SLOTS;
use crate::validate::{Sha256Job, QUICK_FINGERPRINT_POLICY_V1};

/// Whole-source byte ceiling: the PRD's `MAX_EPUB_BYTES`, enforced against
/// the declared length *before* any staging so an oversized upload never
/// costs a marker publication or a byte of streaming. The capabilities
/// handshake advertises this number, and
/// `proto::epub::SourceContainerLimits::V1.max_epub_bytes` must carry the
/// same value — this crate deliberately does not depend on `proto`, so the
/// firmware wiring that owns both is where the equality gets asserted.
pub const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;

/// A parsed create or replace request: the epoch-scoped request ID, the
/// declared source identity the device must independently confirm, the
/// validated label, and — for replace — the base token naming exactly
/// which committed generation is being replaced.
#[derive(Clone, Copy, Debug)]
pub struct UploadRequest {
    pub epoch: u64,
    pub nonce: [u8; REQUEST_NONCE_BYTES],
    pub declared_length: u64,
    pub declared_sha256: [u8; SHA256_BYTES],
    pub display_label: DisplayLabel,
    /// `None` creates a new logical book; `Some` replaces the generation
    /// this token names.
    pub replace_token: Option<[u8; BOOK_TOKEN_BYTES]>,
}

/// Device-generated identity for the candidate generation, minted by the
/// caller from hardware RNG. Collisions against surviving state are
/// checked here; on [`UploadBeginOutcome::RejectedIdentityCollision`] the
/// caller re-mints and retries.
#[derive(Clone, Copy, Debug)]
pub struct FreshIdentity {
    pub logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
    pub book_token: [u8; BOOK_TOKEN_BYTES],
}

/// The committed result an upload (or its replay) reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadResult {
    pub logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
    pub book_token: [u8; BOOK_TOKEN_BYTES],
    pub source_generation: u64,
    /// False when the receipt could not be persisted; replay then rests on
    /// the committed metadata's request identity until cleanup migrates it.
    pub receipt_durable: bool,
}

// The Started variant carries the ~350-byte transaction (request copy,
// resolved identity, hasher). Boxing is off the table in no-alloc firmware,
// and one transient move through a single Wi-Fi-path call frame is well
// inside budget — the KB-scale state (OpsWorkspace) already lives off-stack.
#[allow(clippy::large_enum_variant)]
pub enum UploadBeginOutcome {
    /// Marker committed, candidate created; stream chunks next.
    Started(UploadTransaction),
    /// This request already committed; the original result, re-served.
    Replayed(UploadResult),
    /// Replace with a token that names no authoritative generation.
    RejectedUnknownToken,
    /// The declared length exceeds [`MAX_SOURCE_BYTES`]. Checked before the
    /// idempotency search: an oversized request can never have committed,
    /// and the PRD requires the rejection to land before staging.
    RejectedTooLarge,
    /// Replace against a base generation whose committed bytes this mount
    /// has proved are gone or changed (`Mismatch`, `Unavailable`, or
    /// `UnsupportedContainer`). Ordinary replacement is not offered over a
    /// broken authority chain — explicit recovery and delete are the
    /// repairs, and both stay available.
    RejectedExternallyModified,
    RejectedStaleEpoch,
    RejectedParameterMismatch,
    RejectedEpochExhausted,
    /// The minted identity collided with surviving state; re-mint.
    RejectedIdentityCollision,
    /// Every managed slot holds committed metadata.
    RejectedNoFreeSlot,
    /// The workspace's catalog view is not a complete load of committed
    /// state; see [`OpsWorkspace::catalog_is_valid`].
    CatalogUnavailable,
    /// The idempotency store cannot say what is committed; see
    /// [`IdempotencyStore::is_usable`].
    IdempotencyUnavailable,
    /// Committed records disagree about what this request ID already did;
    /// see [`RequestTrace::Conflict`].
    AmbiguousRequestEvidence,
    Failed(PublishError),
}

/// Why a finish (or a chunk) refused the candidate. All of these leave the
/// previous catalog authoritative and the candidate quarantined behind its
/// marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadError {
    /// Received bytes disagree with `Content-Length` (early EOF or
    /// overrun).
    LengthMismatch,
    /// Received bytes hash differently than declared.
    DigestMismatch,
    /// The *persisted* bytes reread differently than declared — the
    /// received stream was fine, the card's copy is not.
    PersistMismatch,
    /// The container gate (classic-ZIP bounds, ZIP64 rejection) refused.
    UnsupportedContainer,
    /// The final authority revalidation refused the metadata commit.
    RevalidationRefused,
    /// The workspace's catalog view is not a complete load of committed
    /// state; see [`OpsWorkspace::catalog_is_valid`].
    CatalogUnavailable,
    /// The idempotency store cannot say what is committed; see
    /// [`IdempotencyStore::is_usable`].
    IdempotencyUnavailable,
    Io(PublishError),
}

/// One in-flight upload. Holds no file handle — each chunk reopens the
/// candidate in append mode, which keeps the transaction free of the
/// directory's lifetimes at the cost of a directory-entry lookup per
/// chunk (revisit if upload throughput measurements mind).
pub struct UploadTransaction {
    request: UploadRequest,
    logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
    book_token: [u8; BOOK_TOKEN_BYTES],
    source_generation: u64,
    physical_slot: u8,
    candidate_name: String<12>,
    hasher: Sha256Job,
}

impl UploadTransaction {
    /// The candidate's internal file name, for the caller's container-gate
    /// hook.
    pub fn candidate_name(&self) -> &str {
        self.candidate_name.as_str()
    }

    pub fn bytes_received(&self) -> u64 {
        self.hasher.processed()
    }
}

impl UploadRequest {
    fn operation(&self) -> ReceiptOperation {
        if self.replace_token.is_some() {
            ReceiptOperation::Replace
        } else {
            ReceiptOperation::Create
        }
    }

    fn staged_operation(&self) -> StagedOperation {
        if self.replace_token.is_some() {
            StagedOperation::Replace
        } else {
            StagedOperation::Create
        }
    }

    fn request_id(&self) -> [u8; REQUEST_ID_BYTES] {
        let mut id = [0u8; REQUEST_ID_BYTES];
        id[..8].copy_from_slice(&self.epoch.to_le_bytes());
        id[8..].copy_from_slice(&self.nonce);
        id
    }

    /// The digest of every client-bound parameter, committed into metadata
    /// so the receipt-less replay path can demand full agreement — including
    /// create-versus-replace and the exact base token, neither of which any
    /// other metadata field records.
    pub fn binding_digest(&self) -> [u8; SHA256_BYTES] {
        RequestBinding {
            tag: BINDING_TAG_UPLOAD,
            operation: self.operation() as u8,
            request_id: &self.request_id(),
            base_book_token_or_zero: &self.replace_token.unwrap_or([0; BOOK_TOKEN_BYTES]),
            declared_length: self.declared_length,
            declared_sha256: &self.declared_sha256,
            label: Some(&self.display_label),
        }
        .digest()
    }

    /// The receipt-shaped view: client-bound parameters filled, device
    /// results zero (for lookup) or filled by the committer.
    fn receipt(
        &self,
        logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
        source_generation: u64,
        result_token: [u8; BOOK_TOKEN_BYTES],
    ) -> OperationReceipt {
        let (label_len, label) = OperationReceipt::label_from(&self.display_label);
        OperationReceipt {
            epoch: self.epoch,
            request_nonce: self.nonce,
            operation: self.operation(),
            logical_book_id,
            base_book_token_or_zero: self.replace_token.unwrap_or([0; BOOK_TOKEN_BYTES]),
            source_generation,
            source_length_or_zero: self.declared_length,
            source_sha256_or_zero: self.declared_sha256,
            display_label_len: label_len,
            display_label: label,
            result_book_token_or_zero: result_token,
            result_status: RECEIPT_STATUS_SUCCESS,
        }
    }
}

/// Resolve idempotency and authority, commit the staging marker, create
/// the empty candidate. See the module docs for the sequence rationale.
pub fn begin_upload<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    idem: &mut IdempotencyStore,
    req: &UploadRequest,
    fresh: FreshIdentity,
    ws: &mut OpsWorkspace,
) -> UploadBeginOutcome
where
    D: BlockDevice,
    T: TimeSource,
{
    if !ws.catalog_is_valid() {
        return UploadBeginOutcome::CatalogUnavailable;
    }
    if !idem.is_usable() {
        return UploadBeginOutcome::IdempotencyUnavailable;
    }
    // 0. Declared-length ceiling, at request-syntax level. Nothing this
    //    large ever staged or committed, so there is no idempotency state
    //    it could shadow, and rejecting here keeps the marker pair and the
    //    candidate slot untouched.
    if req.declared_length > MAX_SOURCE_BYTES {
        return UploadBeginOutcome::RejectedTooLarge;
    }

    // 1. Receipts first, provided committed fallback evidence is absent or
    //    uniquely consistent with the receipt.
    let request_id = req.request_id();
    let trace = find_request_trace(ws, &request_id);
    if let Some(receipt) = idem.state.get_receipt(req.epoch, &req.nonce) {
        if !receipt_is_consistent_with_trace(receipt, trace) {
            return UploadBeginOutcome::AmbiguousRequestEvidence;
        }
        let probe = req.receipt([1; LOGICAL_BOOK_ID_BYTES], 1, [0; BOOK_TOKEN_BYTES]);
        match idem.state.lookup(&probe) {
            ReceiptLookup::Replay(receipt) => {
                return UploadBeginOutcome::Replayed(UploadResult {
                    logical_book_id: receipt.logical_book_id,
                    book_token: receipt.result_book_token_or_zero,
                    source_generation: receipt.source_generation,
                    // True by the guard above: a usable store's receipts are
                    // the card's, so a reboot answers this retry the same way.
                    receipt_durable: true,
                });
            }
            ReceiptLookup::ParameterMismatch => {
                return UploadBeginOutcome::RejectedParameterMismatch
            }
            ReceiptLookup::Unknown => {}
        }
    }

    match trace {
        RequestTrace::Metadata(entry) => {
            // Same request identity: full parameter agreement replays the
            // recorded result, anything else is misuse. The binding digest
            // is the whole comparison — the metadata fields alone could not
            // tell a create from a replace, nor which book a replace named.
            if entry.metadata.request_binding_sha256 != req.binding_digest() {
                return UploadBeginOutcome::RejectedParameterMismatch;
            }
            return UploadBeginOutcome::Replayed(UploadResult {
                logical_book_id: entry.metadata.logical_book_id,
                book_token: entry.metadata.book_token,
                source_generation: entry.metadata.source_generation,
                receipt_durable: false,
            });
        }
        // The ID was spent on a delete. A create's result is not an answer
        // to it, and the retry must not become a second execution.
        RequestTrace::Tombstone(_) => return UploadBeginOutcome::RejectedParameterMismatch,
        RequestTrace::Conflict => return UploadBeginOutcome::AmbiguousRequestEvidence,
        RequestTrace::None => {}
    }
    // The staging-marker station of the lookup order. A committed marker
    // carrying this ID is this request's own interrupted staging: with the
    // same bound parameters the transaction below *is* the resume — the
    // superseding marker restages the same request, and nothing had
    // committed, so single execution holds. Different parameters under the
    // spent ID are misuse, exactly as they would be against a receipt.
    if marker_claims_request(ws, &request_id) {
        let resumable = ws.marker.as_ref().is_some_and(|marker| {
            marker.operation == req.staged_operation()
                && marker.base_book_token_or_zero
                    == req.replace_token.unwrap_or([0; BOOK_TOKEN_BYTES])
                && marker.expected_source_length == req.declared_length
                && marker.expected_source_sha256 == req.declared_sha256
                && marker.display_label == req.display_label
        });
        if !resumable {
            return UploadBeginOutcome::RejectedParameterMismatch;
        }
    }

    // 2. Genuinely new: freshness and budget before anything commits.
    if !idem.state.epoch_is_current(req.epoch) {
        return UploadBeginOutcome::RejectedStaleEpoch;
    }
    if !idem.state.has_epoch_headroom() {
        return UploadBeginOutcome::RejectedEpochExhausted;
    }

    // 3. Authority: resolve or mint the logical identity.
    let (logical_book_id, source_generation) = match req.replace_token {
        Some(token) => match find_authoritative_by_token(ws, &token) {
            Some(entry) => {
                // A base this mount has proved broken — bytes changed,
                // vanished, or unsupported — is not replaceable through
                // the ordinary path; the client must go through explicit
                // recovery or delete, which both re-prove state.
                let level = ws.session.level(
                    &entry.metadata.logical_book_id,
                    entry.metadata.source_generation,
                );
                if matches!(
                    level,
                    crate::session::IntegrityLevel::Mismatch
                        | crate::session::IntegrityLevel::Unavailable
                        | crate::session::IntegrityLevel::UnsupportedContainer
                ) {
                    return UploadBeginOutcome::RejectedExternallyModified;
                }
                let Some(next) = entry.metadata.source_generation.checked_add(1) else {
                    return UploadBeginOutcome::Failed(PublishError::BadInput);
                };
                (entry.metadata.logical_book_id, next)
            }
            None => return UploadBeginOutcome::RejectedUnknownToken,
        },
        None => {
            // A deleted logical-book ID is never reused; a fresh create
            // must not collide with anything that ever existed and still
            // has a trace.
            let id_taken = ws
                .entries
                .iter()
                .flatten()
                .any(|entry| entry.metadata.logical_book_id == fresh.logical_book_id)
                || ws
                    .tombstones
                    .iter()
                    .flatten()
                    .any(|(_, stone)| stone.logical_book_id == fresh.logical_book_id);
            if id_taken {
                return UploadBeginOutcome::RejectedIdentityCollision;
            }
            (fresh.logical_book_id, 1)
        }
    };
    let token_taken = ws
        .entries
        .iter()
        .flatten()
        .any(|entry| entry.metadata.book_token == fresh.book_token)
        || ws
            .tombstones
            .iter()
            .flatten()
            .any(|(_, stone)| stone.deleted_book_token == fresh.book_token);
    if token_taken {
        return UploadBeginOutcome::RejectedIdentityCollision;
    }

    // 4. A physical slot free of committed metadata. Superseded and
    //    deleted generations still hold committed records until cleanup,
    //    so their slots are not free.
    let free_slot =
        (0..MAX_SOURCE_SLOTS as u8).find(|slot| ws.entries[usize::from(*slot)].is_none());
    let Some(physical_slot) = free_slot else {
        return UploadBeginOutcome::RejectedNoFreeSlot;
    };
    let Some(candidate_name) = layout::source_slot_name(physical_slot) else {
        return UploadBeginOutcome::Failed(PublishError::BadInput);
    };

    // 5. Commit the staging marker before the candidate exists. A newer
    //    marker generation supersedes any abandoned transaction's marker;
    //    the abandoned candidate stays quarantined by the namespace and
    //    is reclaimed by cleanup.
    let marker = StagingMarker {
        operation: req.staged_operation(),
        operation_request_id: request_id,
        logical_book_id,
        base_book_token_or_zero: req.replace_token.unwrap_or([0; BOOK_TOKEN_BYTES]),
        candidate_source_generation: source_generation,
        candidate_physical_slot: physical_slot,
        expected_source_length: req.declared_length,
        expected_source_sha256: req.declared_sha256,
        display_label: req.display_label,
    };
    let names = layout::marker_pair();
    let marker_generation =
        match publish::select_authority(dir, names.pair(), &mut ws.record_scratch) {
            Ok(committed) => {
                let last = committed.map(|(_, generation)| generation).unwrap_or(0);
                match last.checked_add(1) {
                    Some(next) => next,
                    None => return UploadBeginOutcome::Failed(PublishError::BadInput),
                }
            }
            Err(error) => return UploadBeginOutcome::Failed(error),
        };
    let Some(logical) = marker.encode_into(&mut ws.seal_scratch) else {
        return UploadBeginOutcome::Failed(PublishError::BadInput);
    };
    let Some(sealed) = record::seal_body(
        STAGING_MARKER_MAGIC,
        STAGING_MARKER_SCHEMA,
        marker_generation,
        logical,
        &mut ws.seal_scratch,
    ) else {
        return UploadBeginOutcome::Failed(PublishError::BadInput);
    };
    if let Err(error) = publish::publish_record(
        dir,
        names.pair(),
        &ws.seal_scratch,
        &sealed,
        0,
        &mut ws.record_scratch,
        || true,
    ) {
        return UploadBeginOutcome::Failed(error);
    }
    // Keep the workspace's marker view current with what was just
    // committed, so a retry arriving before any catalog reload still
    // resolves this request's staging.
    ws.marker = Some(marker);

    // 6. Create (or truncate a stale abandoned) candidate. Through
    //    `open_for_write`, so a dropped read cannot answer with a second
    //    directory entry under the same name — the chunks below would then
    //    append to whichever entry the scan reaches first.
    match publish::open_for_write(
        dir,
        candidate_name.as_str(),
        Mode::ReadWriteCreateOrTruncate,
    ) {
        Ok(file) => {
            if file.close().is_err() {
                return UploadBeginOutcome::Failed(PublishError::Io);
            }
        }
        Err(error) => return UploadBeginOutcome::Failed(error),
    }

    UploadBeginOutcome::Started(UploadTransaction {
        request: *req,
        logical_book_id,
        book_token: fresh.book_token,
        source_generation,
        physical_slot,
        candidate_name,
        hasher: Sha256Job::new(req.declared_length),
    })
}

/// Append one received chunk, hashing as it lands. Errors are terminal
/// for the transaction: the caller aborts.
pub fn upload_chunk<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    txn: &mut UploadTransaction,
    chunk: &[u8],
) -> Result<(), UploadError>
where
    D: BlockDevice,
    T: TimeSource,
{
    if txn.hasher.update(chunk).is_err() {
        return Err(UploadError::LengthMismatch);
    }
    // Plain append, never create: `begin_upload` made the candidate, so a
    // missing one is a failure to report, not a file to conjure — and a
    // create mode here would let a dropped read fork the name and scatter
    // the stream across two entries.
    let file = dir
        .open_file_in_dir(txn.candidate_name.as_str(), Mode::ReadWriteAppend)
        .map_err(|_| UploadError::Io(PublishError::Io))?;
    let write = file.write(chunk);
    let closed = file.close();
    if write.is_err() || closed.is_err() {
        return Err(UploadError::Io(PublishError::Io));
    }
    Ok(())
}

/// Verify, gate, fingerprint, and commit the streamed candidate. See the
/// module docs for the full sequence; `validate_container` is the
/// classic-ZIP/ZIP64 gate, invoked after the persisted reread passes.
pub fn finish_upload<D, T, F, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    idem: &mut IdempotencyStore,
    txn: UploadTransaction,
    ws: &mut OpsWorkspace,
    validate_container: F,
) -> Result<UploadResult, UploadError>
where
    D: BlockDevice,
    T: TimeSource,
    F: FnOnce() -> bool,
{
    if !ws.catalog_is_valid() {
        return Err(UploadError::CatalogUnavailable);
    }
    if !idem.is_usable() {
        return Err(UploadError::IdempotencyUnavailable);
    }

    // 1. Receive-time identity.
    if txn.hasher.remaining() != 0 {
        return Err(UploadError::LengthMismatch);
    }
    let request = txn.request;
    let received = txn
        .hasher
        .finish()
        .map_err(|_| UploadError::LengthMismatch)?;
    if received != request.declared_sha256 {
        return Err(UploadError::DigestMismatch);
    }

    // 2. Durably sync the candidate's data and length.
    {
        let file = dir
            .open_file_in_dir(txn.candidate_name.as_str(), Mode::ReadWriteAppend)
            .map_err(|_| UploadError::Io(PublishError::Io))?;
        let synced = publish::durable_sync(&file);
        let closed = file.close();
        if synced.is_err() || closed.is_err() {
            return Err(UploadError::Io(PublishError::Io));
        }
    }

    // 3–4. Independently reread and rehash every persisted byte — the
    //    card's copy, not the received stream, is what metadata will vouch
    //    for — and take the quick fingerprint from that same copy.
    let identity = match hash_file_identity(dir, txn.candidate_name.as_str(), ws) {
        Ok(Some(identity)) => identity,
        Ok(None) => return Err(UploadError::PersistMismatch),
        Err(error) => return Err(UploadError::Io(error)),
    };
    if identity.length != request.declared_length || identity.sha256 != request.declared_sha256 {
        return Err(UploadError::PersistMismatch);
    }
    let quick_fingerprint = identity.quick_fingerprint;

    // 5. The container gate.
    if !validate_container() {
        return Err(UploadError::UnsupportedContainer);
    }

    // 6. Publish source metadata with final authority revalidation.
    let metadata = SourceMetadata {
        logical_book_id: txn.logical_book_id,
        source_generation: txn.source_generation,
        source_origin: SourceOrigin::ManagedUpload,
        operation_kind: OperationKind::ManagedUploadRequest,
        operation_request_id: request.request_id(),
        request_binding_sha256: request.binding_digest(),
        externally_recovered: false,
        physical_slot: txn.physical_slot,
        source_length: request.declared_length,
        source_sha256: request.declared_sha256,
        quick_fingerprint_policy_version: QUICK_FINGERPRINT_POLICY_V1,
        quick_fingerprint_sha256: quick_fingerprint,
        book_token: txn.book_token,
        display_label: request.display_label,
        unmanaged_name: UnmanagedName::none(),
    };
    let Some(meta_names) = layout::metadata_pair(txn.physical_slot) else {
        return Err(UploadError::Io(PublishError::BadInput));
    };
    let record_generation =
        match publish::select_authority(dir, meta_names.pair(), &mut ws.record_scratch) {
            Ok(committed) => {
                let last = committed.map(|(_, generation)| generation).unwrap_or(0);
                match last.checked_add(1) {
                    Some(next) => next,
                    None => return Err(UploadError::Io(PublishError::BadInput)),
                }
            }
            Err(error) => return Err(UploadError::Io(error)),
        };
    let Some(logical) = metadata.encode_into(&mut ws.seal_scratch) else {
        return Err(UploadError::Io(PublishError::BadInput));
    };
    let Some(sealed) = record::seal_body(
        crate::bodies::SOURCE_METADATA_MAGIC,
        crate::bodies::SOURCE_METADATA_SCHEMA,
        record_generation,
        logical,
        &mut ws.seal_scratch,
    ) else {
        return Err(UploadError::Io(PublishError::BadInput));
    };
    let request_id = request.request_id();
    // Resolve the base generation's slot pair *before* building the
    // closure: the closure must not borrow the workspace the publish call
    // is already using, and under the storage-owner serialization rule the
    // base cannot move slots between here and the commit.
    let base = match request.replace_token {
        Some(token) => match base_slot_names(&token, ws) {
            Some(names) => Some((token, names)),
            None => return Err(UploadError::RevalidationRefused),
        },
        None => None,
    };
    let revalidate = || {
        // The marker must still name this transaction: a superseding
        // upload's marker means this one was abandoned mid-flight.
        let marker_names = layout::marker_pair();
        let mut scratch = [0u8; MARKER_REVALIDATE_SCRATCH];
        let marker_current = match publish::read_committed(dir, marker_names.pair(), &mut scratch) {
            Ok(Some((_, RecordState::Committed(view)))) => StagingMarker::decode(&view)
                .is_some_and(|marker| marker.operation_request_id == request_id),
            _ => false,
        };
        if !marker_current {
            return false;
        }
        // No committed tombstone may name this logical book: deleted
        // identities are burned, and committing a generation above a
        // tombstone would resurrect one.
        if !no_committed_tombstone_for(dir, &txn.logical_book_id) {
            return false;
        }
        // For replace: the base generation must still be authoritative.
        let Some((token, base_names)) = &base else {
            return true;
        };
        let mut scratch = [0u8; MARKER_REVALIDATE_SCRATCH];
        match publish::read_committed(dir, base_names.pair(), &mut scratch) {
            Ok(Some((_, RecordState::Committed(view)))) => {
                SourceMetadata::decode(&view).is_some_and(|meta| meta.book_token == *token)
            }
            _ => false,
        }
    };
    match publish::publish_record(
        dir,
        meta_names.pair(),
        &ws.seal_scratch,
        &sealed,
        0,
        &mut ws.record_scratch,
        revalidate,
    ) {
        Ok(_) => {}
        Err(PublishError::RevalidationRefused) => return Err(UploadError::RevalidationRefused),
        Err(error) => return Err(UploadError::Io(error)),
    }

    // 7. Seed this mount's exact-validation set: the persisted-file reread
    //    above proved exact identity, and the publish just confirmed the
    //    generation through startup selection — the PRD's condition for
    //    letting the new book open without immediately rehashing it.
    ws.session
        .seed_full_validation(&txn.logical_book_id, txn.source_generation);

    // 8. Retain the receipt; failure degrades replay to the committed
    //    metadata's request identity, never the outcome.
    let receipt = request.receipt(txn.logical_book_id, txn.source_generation, txn.book_token);
    let receipt_durable = idem.state.insert(receipt).is_ok() && idem.publish(dir, ws).is_ok();

    // 9. The marker's job is done; its removal is cleanup.
    let marker_names = layout::marker_pair();
    for name in marker_names.pair().names {
        let _ = dir.delete_file_in_dir(name);
    }

    // 10. Refresh the caller's catalog view. The generation is committed
    //    either way; a failed reload leaves the workspace invalid, which
    //    stops the next operation instead of misleading it.
    let _ = load_catalog(dir, ws);

    Ok(UploadResult {
        logical_book_id: txn.logical_book_id,
        book_token: txn.book_token,
        source_generation: txn.source_generation,
        receipt_durable,
    })
}

/// Abort a transaction: delete the candidate bytes. The committed marker
/// stays until the next transaction supersedes it or cleanup reclaims it —
/// either way the slot remains quarantined, never adoptable.
pub fn abort_upload<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    txn: UploadTransaction,
) where
    D: BlockDevice,
    T: TimeSource,
{
    let _ = dir.delete_file_in_dir(txn.candidate_name.as_str());
}

/// The metadata pair of the slot whose *loaded catalog entry* carries
/// `token`. Used by the replace revalidation to reread the base
/// generation's record from disk.
fn base_slot_names(token: &[u8; BOOK_TOKEN_BYTES], ws: &OpsWorkspace) -> Option<layout::PairNames> {
    let entry = find_authoritative_by_token(ws, token)?;
    layout::metadata_pair(entry.physical_slot)
}

/// Metadata and marker record files are exactly one padded sector plus the
/// commit sector; the revalidation closure rereads them inside the publish
/// call, where the shared workspace scratch is already borrowed.
const MARKER_REVALIDATE_SCRATCH: usize =
    match record::record_file_len(crate::bodies::STAGING_MARKER_LOGICAL_BYTES) {
        Some(len) => len,
        None => 0,
    };
const _: () = assert!(MARKER_REVALIDATE_SCRATCH > 0);
