//! Explicit recovery of an externally modified managed book: adopting the
//! bytes now present in a managed slot as the book's next source
//! generation — deliberately, never silently.
//!
//! The PRD's rule this module exists for: a managed source that no longer
//! matches its committed identity is *quarantined*, and the only
//! protocol-v1 way forward (short of deletion) is this endpoint, where the
//! client names the exact observed identity it inspected and the device
//! independently confirms those are still the bytes on the card. A file
//! that "just changed" never becomes a generation; a client that observed
//! stale bytes gets a retryable mismatch, not an adoption.
//!
//! The transaction rides the same machinery as upload — same idempotency
//! resolution order, same epoch discipline, same publication protocol —
//! but has no staging phase: the candidate bytes are already in the slot,
//! so recovery's whole job is proving what they are and committing
//! metadata that says so. The new metadata record supersedes the old one
//! inside the same slot pair (A/B alternation), carrying
//! `ExternalRecoveryRequest` provenance and the `externally_recovered`
//! flag.

use embedded_sdmmc::{BlockDevice, Directory, TimeSource};

use crate::bodies::{
    DisplayLabel, OperationKind, RequestBinding, SourceMetadata, SourceOrigin, UnmanagedName,
    BINDING_TAG_RECOVER, BOOK_TOKEN_BYTES, LOGICAL_BOOK_ID_BYTES, REQUEST_ID_BYTES, SHA256_BYTES,
    SOURCE_METADATA_MAGIC, SOURCE_METADATA_SCHEMA,
};
use crate::layout;
use crate::ops::{
    find_authoritative_by_token, find_request_trace, hash_file_identity, load_catalog,
    IdempotencyStore, OpsWorkspace, RequestTrace,
};
use crate::publish::{self, PublishError};
use crate::receipts::{
    OperationReceipt, ReceiptLookup, ReceiptOperation, RECEIPT_STATUS_SUCCESS, REQUEST_NONCE_BYTES,
};
use crate::record::{self, RecordState};
use crate::upload::UploadResult;
use crate::validate::QUICK_FINGERPRINT_POLICY_V1;

/// A parsed `POST /recover-book`: the request ID, the token naming the last
/// committed generation recovery is authorized *from*, the observed
/// identity of the changed bytes, and an optional replacement label.
#[derive(Clone, Copy, Debug)]
pub struct RecoveryRequest {
    pub epoch: u64,
    pub nonce: [u8; REQUEST_NONCE_BYTES],
    pub book_token: [u8; BOOK_TOKEN_BYTES],
    pub observed_length: u64,
    pub observed_sha256: [u8; SHA256_BYTES],
    /// `None` keeps the committed generation's label.
    pub display_label: Option<DisplayLabel>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// Committed — now, or by the earlier operation this request replays.
    Recovered(UploadResult),
    /// No authoritative generation carries this token.
    RejectedUnknownToken,
    /// The token names an unmanaged book; unmanaged sources re-identify
    /// locally and have no recovery operation.
    RejectedUnmanagedBook,
    /// The physical bytes still match the committed identity — there is
    /// nothing to recover.
    RejectedNotExternallyModified,
    /// The physical bytes match neither the committed nor the observed
    /// identity: the source changed again after the client looked.
    /// Retryable with a freshly observed identity and a new request ID.
    RejectedObservedMismatch,
    RejectedStaleEpoch,
    RejectedParameterMismatch,
    RejectedEpochExhausted,
    /// The minted replacement token collided; re-mint.
    RejectedIdentityCollision,
    /// The container gate refused the observed bytes.
    RejectedUnsupportedContainer,
    /// The workspace's catalog view is not a complete load of committed
    /// state; see [`OpsWorkspace::catalog_is_valid`].
    CatalogUnavailable,
    /// The idempotency store cannot say what is committed; see
    /// [`IdempotencyStore::is_usable`].
    IdempotencyUnavailable,
    Failed(PublishError),
}

impl RecoveryRequest {
    fn request_id(&self) -> [u8; REQUEST_ID_BYTES] {
        let mut id = [0u8; REQUEST_ID_BYTES];
        id[..8].copy_from_slice(&self.epoch.to_le_bytes());
        id[8..].copy_from_slice(&self.nonce);
        id
    }

    /// Every client-bound parameter of this recovery, digested — the base
    /// token it is authorized from and the label it may replace included,
    /// neither of which the observed identity alone pins down.
    pub fn binding_digest(&self) -> [u8; SHA256_BYTES] {
        RequestBinding {
            tag: BINDING_TAG_RECOVER,
            operation: ReceiptOperation::RecoverExternallyModified as u8,
            request_id: &self.request_id(),
            base_book_token_or_zero: &self.book_token,
            declared_length: self.observed_length,
            declared_sha256: &self.observed_sha256,
            label: self.display_label.as_ref(),
        }
        .digest()
    }

    fn receipt(
        &self,
        logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
        source_generation: u64,
        result_token: [u8; BOOK_TOKEN_BYTES],
    ) -> OperationReceipt {
        let (label_len, label) = match &self.display_label {
            Some(label) => OperationReceipt::label_from(label),
            None => (0, [0u8; 64]),
        };
        OperationReceipt {
            epoch: self.epoch,
            request_nonce: self.nonce,
            operation: ReceiptOperation::RecoverExternallyModified,
            logical_book_id,
            base_book_token_or_zero: self.book_token,
            source_generation,
            source_length_or_zero: self.observed_length,
            source_sha256_or_zero: self.observed_sha256,
            display_label_len: label_len,
            display_label: label,
            result_book_token_or_zero: result_token,
            result_status: RECEIPT_STATUS_SUCCESS,
        }
    }
}

/// Execute one recovery request end to end. `fresh_token` is the minted
/// replacement token; `validate_container` is the classic-ZIP/ZIP64 gate
/// over the observed bytes, invoked only after their identity is
/// confirmed.
pub fn recover_book<D, T, F, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    idem: &mut IdempotencyStore,
    req: &RecoveryRequest,
    fresh_token: [u8; BOOK_TOKEN_BYTES],
    ws: &mut OpsWorkspace,
    validate_container: F,
) -> RecoveryOutcome
where
    D: BlockDevice,
    T: TimeSource,
    F: FnOnce() -> bool,
{
    if !ws.catalog_is_valid() {
        return RecoveryOutcome::CatalogUnavailable;
    }
    if !idem.is_usable() {
        return RecoveryOutcome::IdempotencyUnavailable;
    }

    // 1. Request-ID resolution before ordinary token validation: receipts,
    //    then any committed record carrying this recovery's identity.
    let probe = req.receipt([1; LOGICAL_BOOK_ID_BYTES], 1, [0; BOOK_TOKEN_BYTES]);
    match idem.state.lookup(&probe) {
        ReceiptLookup::Replay(receipt) => {
            return RecoveryOutcome::Recovered(UploadResult {
                logical_book_id: receipt.logical_book_id,
                book_token: receipt.result_book_token_or_zero,
                source_generation: receipt.source_generation,
                // True by the guard above; see `IdempotencyStore`.
                receipt_durable: true,
            });
        }
        ReceiptLookup::ParameterMismatch => return RecoveryOutcome::RejectedParameterMismatch,
        ReceiptLookup::Unknown => {}
    }
    let request_id = req.request_id();
    match find_request_trace(ws, &request_id) {
        RequestTrace::Metadata(entry) => {
            if entry.metadata.request_binding_sha256 != req.binding_digest() {
                return RecoveryOutcome::RejectedParameterMismatch;
            }
            return RecoveryOutcome::Recovered(UploadResult {
                logical_book_id: entry.metadata.logical_book_id,
                book_token: entry.metadata.book_token,
                source_generation: entry.metadata.source_generation,
                receipt_durable: false,
            });
        }
        // The ID was spent on a delete; a recovery result is no answer to it.
        RequestTrace::Tombstone(_) => return RecoveryOutcome::RejectedParameterMismatch,
        RequestTrace::None => {}
    }

    // 2. Genuinely new: freshness and budget before anything commits.
    if !idem.state.epoch_is_current(req.epoch) {
        return RecoveryOutcome::RejectedStaleEpoch;
    }
    if !idem.state.has_epoch_headroom() {
        return RecoveryOutcome::RejectedEpochExhausted;
    }

    // 3. The token must name the authoritative generation of a *managed*
    //    book.
    let Some(entry) = find_authoritative_by_token(ws, &req.book_token) else {
        return RecoveryOutcome::RejectedUnknownToken;
    };
    if entry.metadata.source_origin != SourceOrigin::ManagedUpload {
        return RecoveryOutcome::RejectedUnmanagedBook;
    }

    // 4. Independently establish what the slot's bytes are now. This is
    //    the device's own proof — the client's observation authorizes,
    //    the rehash decides.
    let Some(slot_name) = layout::source_slot_name(entry.physical_slot) else {
        return RecoveryOutcome::Failed(PublishError::BadInput);
    };
    let identity = match hash_file_identity(dir, slot_name.as_str(), ws) {
        Ok(Some(identity)) => identity,
        // A vanished source is not recoverable to anything; deletion is
        // the remaining repair.
        Ok(None) => return RecoveryOutcome::RejectedObservedMismatch,
        Err(error) => return RecoveryOutcome::Failed(error),
    };
    if identity.length == entry.metadata.source_length
        && identity.sha256 == entry.metadata.source_sha256
    {
        return RecoveryOutcome::RejectedNotExternallyModified;
    }
    if identity.length != req.observed_length || identity.sha256 != req.observed_sha256 {
        return RecoveryOutcome::RejectedObservedMismatch;
    }

    // 5. The observed bytes must be an acceptable container.
    if !validate_container() {
        return RecoveryOutcome::RejectedUnsupportedContainer;
    }

    // 6. Mint the replacement token; identities are never reused.
    let token_taken = ws
        .entries
        .iter()
        .flatten()
        .any(|other| other.metadata.book_token == fresh_token)
        || ws
            .tombstones
            .iter()
            .flatten()
            .any(|(_, stone)| stone.deleted_book_token == fresh_token);
    if token_taken {
        return RecoveryOutcome::RejectedIdentityCollision;
    }

    // 7. Publish the adopting metadata into the same slot pair, with final
    //    revalidation that the base generation is still committed and the
    //    bytes have not changed length again since the rehash.
    let Some(next_generation) = entry.metadata.source_generation.checked_add(1) else {
        return RecoveryOutcome::Failed(PublishError::BadInput);
    };
    let metadata = SourceMetadata {
        logical_book_id: entry.metadata.logical_book_id,
        source_generation: next_generation,
        source_origin: SourceOrigin::ManagedUpload,
        operation_kind: OperationKind::ExternalRecoveryRequest,
        operation_request_id: request_id,
        request_binding_sha256: req.binding_digest(),
        externally_recovered: true,
        physical_slot: entry.physical_slot,
        source_length: req.observed_length,
        source_sha256: req.observed_sha256,
        quick_fingerprint_policy_version: QUICK_FINGERPRINT_POLICY_V1,
        quick_fingerprint_sha256: identity.quick_fingerprint,
        book_token: fresh_token,
        display_label: req.display_label.unwrap_or(entry.metadata.display_label),
        unmanaged_name: UnmanagedName::none(),
    };
    let Some(meta_names) = layout::metadata_pair(entry.physical_slot) else {
        return RecoveryOutcome::Failed(PublishError::BadInput);
    };
    let record_generation =
        match publish::select_authority(dir, meta_names.pair(), &mut ws.record_scratch) {
            Ok(committed) => {
                let last = committed.map(|(_, generation)| generation).unwrap_or(0);
                match last.checked_add(1) {
                    Some(next) => next,
                    None => return RecoveryOutcome::Failed(PublishError::BadInput),
                }
            }
            Err(error) => return RecoveryOutcome::Failed(error),
        };
    let Some(logical) = metadata.encode_into(&mut ws.seal_scratch) else {
        return RecoveryOutcome::Failed(PublishError::BadInput);
    };
    let Some(sealed) = record::seal_body(
        SOURCE_METADATA_MAGIC,
        SOURCE_METADATA_SCHEMA,
        record_generation,
        logical,
        &mut ws.seal_scratch,
    ) else {
        return RecoveryOutcome::Failed(PublishError::BadInput);
    };
    let base_token = req.book_token;
    let observed_length = req.observed_length;
    let observed_sha256 = req.observed_sha256;
    let revalidate = || {
        let mut scratch = [0u8; META_REVALIDATE_SCRATCH];
        let base_current = match publish::read_committed(dir, meta_names.pair(), &mut scratch) {
            Ok(Some((_, RecordState::Committed(view)))) => {
                SourceMetadata::decode(&view).is_some_and(|meta| meta.book_token == base_token)
            }
            _ => false,
        };
        if !base_current {
            return false;
        }
        // Rehash in full, here, immediately before the commit sector lands.
        // A length check would be the cheap version and is not enough: this
        // record is about to *vouch* for an exact digest, and a same-length
        // modification since the earlier hash would leave it vouching for
        // bytes that are no longer on the card — the "changed again" case
        // this endpoint exists to refuse. Exact identity is the standard
        // the PRD sets for mutation authority, so exact identity is what
        // the last look checks. The cost is a second full pass over the
        // source on an operation the user invokes by hand.
        let mut hash_scratch = [0u8; IDENTITY_RECHECK_SCRATCH];
        match crate::ops::sha256_file_identity(dir, slot_name.as_str(), &mut hash_scratch) {
            Ok(Some((length, sha256))) => length == observed_length && sha256 == observed_sha256,
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
        Err(error) => return RecoveryOutcome::Failed(error),
    }

    // 8. Receipt, then refresh the caller's view.
    let receipt = req.receipt(
        metadata.logical_book_id,
        metadata.source_generation,
        fresh_token,
    );
    let receipt_durable = idem.state.insert(receipt).is_ok() && idem.publish(dir, ws).is_ok();
    let _ = load_catalog(dir, ws);

    RecoveryOutcome::Recovered(UploadResult {
        logical_book_id: metadata.logical_book_id,
        book_token: fresh_token,
        source_generation: metadata.source_generation,
        receipt_durable,
    })
}

/// Metadata record files are one padded sector plus the commit sector.
const META_REVALIDATE_SCRATCH: usize =
    match record::record_file_len(crate::bodies::SOURCE_METADATA_LOGICAL_BYTES) {
        Some(len) => len,
        None => 0,
    };
const _: () = assert!(META_REVALIDATE_SCRATCH > 0);

/// Read chunk for the final rehash. One sector: the revalidation closure is
/// a stack frame inside the publish call, and the whole point is to bound
/// what it adds there.
const IDENTITY_RECHECK_SCRATCH: usize = 512;
