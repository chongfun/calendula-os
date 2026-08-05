//! Unmanaged direct-SD sources: adopting a discovered EPUB as a logical
//! book, and re-identifying it when its bytes change.
//!
//! Unmanaged books live outside the managed namespace — the user's own
//! files in the books directory — so the storage layer owns none of their
//! bytes, only their *identity*. Adoption computes that identity in full
//! (length, SHA-256, quick fingerprint), mints a logical book, and
//! publishes metadata whose `unmanaged_name` points back at the file.
//! When a later look finds different bytes under the same name, the same
//! entry point re-identifies: a new source generation with a new token,
//! `LocalUnmanagedOperation` provenance, same logical book — the unmanaged
//! analogue of explicit recovery, automatic because there is no committed
//! upload contract to protect. What is *never* automatic is resurrection:
//! a deleted logical book's identity stays burned, so re-copying a deleted
//! file adopts a brand-new logical book.
//!
//! Metadata records share the managed slot-pair pool (`M<NN>`); an
//! unmanaged book's pair simply has no `S<NN>.EPB` beside it. Local
//! operations carry a request ID of epoch zero plus caller entropy —
//! epoch zero is never a valid HTTP epoch, so local and remote provenance
//! cannot collide — and are not receipted: idempotency here is intrinsic
//! (re-running adoption over unchanged bytes is a no-op by identity
//! comparison, not by replay).

use embedded_sdmmc::{BlockDevice, Directory, TimeSource};

use crate::bodies::{
    DisplayLabel, OperationKind, SourceMetadata, SourceOrigin, UnmanagedName, BOOK_TOKEN_BYTES,
    REQUEST_ID_BYTES, SOURCE_METADATA_MAGIC, SOURCE_METADATA_SCHEMA,
};
use crate::layout;
use crate::ops::{hash_file_identity, load_catalog, OpsWorkspace};
use crate::publish::{self, PublishError};
use crate::record::{self, RecordState};
use crate::select::{SlotDisposition, MAX_SOURCE_SLOTS};
use crate::upload::UploadResult;
use crate::validate::QUICK_FINGERPRINT_POLICY_V1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdoptOutcome {
    /// A new logical book now serves this file.
    Adopted(UploadResult),
    /// The file's identity matches its committed metadata: nothing to do.
    AlreadyCurrent {
        book_token: [u8; BOOK_TOKEN_BYTES],
    },
    /// The file changed; a new generation of the same logical book now
    /// serves it.
    Reidentified(UploadResult),
    /// The file is gone from the books directory. The committed metadata
    /// stands (the book reads as unavailable); deletion is the caller's
    /// separate decision.
    RejectedMissingFile,
    /// The container gate refused the bytes; nothing was adopted.
    RejectedUnsupportedContainer,
    /// The minted identity collided; re-mint and retry.
    RejectedIdentityCollision,
    /// Every metadata slot pair is in use.
    RejectedNoFreeSlot,
    /// The name is not a valid 8.3 books-directory name.
    RejectedNameInvalid,
    Failed(PublishError),
}

/// Adopt a discovered unmanaged EPUB, or re-identify it if its bytes no
/// longer match its committed identity. `books_dir` holds the file;
/// records live in `records_dir` as usual. `fresh` supplies RNG-minted
/// identity (the book id is used only on first adoption; the token on
/// both paths); `local_nonce` tags the operation's provenance.
///
/// The caller passes a freshly [`load_catalog`]ed workspace and runs under
/// the storage-owner serialization, like every operation.
pub fn adopt_or_reidentify<D, T, F, const MD: usize, const MF: usize, const MV: usize>(
    records_dir: &Directory<'_, D, T, MD, MF, MV>,
    books_dir: &Directory<'_, D, T, MD, MF, MV>,
    name: &str,
    fresh: crate::upload::FreshIdentity,
    local_nonce: [u8; 16],
    ws: &mut OpsWorkspace,
    validate_container: F,
) -> AdoptOutcome
where
    D: BlockDevice,
    T: TimeSource,
    F: FnOnce() -> bool,
{
    let Some(unmanaged_name) = UnmanagedName::new(name) else {
        return AdoptOutcome::RejectedNameInvalid;
    };

    // The file's actual identity, hashed in full before any decision.
    let identity = match hash_file_identity(books_dir, name, ws) {
        Ok(Some(identity)) => identity,
        Ok(None) => return AdoptOutcome::RejectedMissingFile,
        Err(error) => return AdoptOutcome::Failed(error),
    };
    if identity.length == 0 {
        // A zero-length file is not a book and not adoptable.
        return AdoptOutcome::RejectedNameInvalid;
    }

    // The live entry for this name, if any. Only the authoritative
    // generation counts: a deleted book's lingering records never make its
    // name "known", so a re-copied file adopts fresh.
    let existing = ws
        .entries
        .iter()
        .enumerate()
        .filter_map(|(slot, entry)| entry.as_ref().map(|entry| (slot, entry)))
        .find(|(slot, entry)| {
            ws.dispositions[*slot] == SlotDisposition::Authoritative
                && entry.metadata.source_origin == SourceOrigin::UnmanagedSd
                && entry.metadata.unmanaged_name == unmanaged_name
        })
        .map(|(_, entry)| *entry);

    if let Some(entry) = existing {
        if entry.metadata.source_length == identity.length
            && entry.metadata.source_sha256 == identity.sha256
        {
            return AdoptOutcome::AlreadyCurrent {
                book_token: entry.metadata.book_token,
            };
        }
    }

    // New bytes — either a new book or a new generation. Both go through
    // the container gate and identity minting.
    if !validate_container() {
        return AdoptOutcome::RejectedUnsupportedContainer;
    }
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
        return AdoptOutcome::RejectedIdentityCollision;
    }

    let (logical_book_id, source_generation, physical_slot, reidentifying) = match existing {
        Some(entry) => {
            let Some(next) = entry.metadata.source_generation.checked_add(1) else {
                return AdoptOutcome::Failed(PublishError::BadInput);
            };
            (
                entry.metadata.logical_book_id,
                next,
                entry.physical_slot,
                true,
            )
        }
        None => {
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
                return AdoptOutcome::RejectedIdentityCollision;
            }
            let free_slot =
                (0..MAX_SOURCE_SLOTS as u8).find(|slot| ws.entries[usize::from(*slot)].is_none());
            let Some(slot) = free_slot else {
                return AdoptOutcome::RejectedNoFreeSlot;
            };
            (fresh.logical_book_id, 1, slot, false)
        }
    };

    // Local operation identity: epoch zero plus caller entropy.
    let mut operation_request_id = [0u8; REQUEST_ID_BYTES];
    operation_request_id[8..].copy_from_slice(&local_nonce);

    // Initial label from the filename stem; once committed metadata
    // exists, its embedded label is authoritative, so re-identification
    // keeps the committed one.
    let display_label = match existing {
        Some(entry) => entry.metadata.display_label,
        None => stem_label(name),
    };

    let metadata = SourceMetadata {
        logical_book_id,
        source_generation,
        source_origin: SourceOrigin::UnmanagedSd,
        operation_kind: OperationKind::LocalUnmanagedOperation,
        operation_request_id,
        externally_recovered: false,
        physical_slot,
        source_length: identity.length,
        source_sha256: identity.sha256,
        quick_fingerprint_policy_version: QUICK_FINGERPRINT_POLICY_V1,
        quick_fingerprint_sha256: identity.quick_fingerprint,
        book_token: fresh.book_token,
        display_label,
        unmanaged_name,
    };
    let Some(meta_names) = layout::metadata_pair(physical_slot) else {
        return AdoptOutcome::Failed(PublishError::BadInput);
    };
    let record_generation =
        match publish::select_authority(records_dir, meta_names.pair(), &mut ws.record_scratch) {
            Ok(committed) => {
                let last = committed.map(|(_, generation)| generation).unwrap_or(0);
                match last.checked_add(1) {
                    Some(next) => next,
                    None => return AdoptOutcome::Failed(PublishError::BadInput),
                }
            }
            Err(error) => return AdoptOutcome::Failed(error),
        };
    let Some(logical) = metadata.encode_into(&mut ws.seal_scratch) else {
        return AdoptOutcome::Failed(PublishError::BadInput);
    };
    let Some(sealed) = record::seal_body(
        SOURCE_METADATA_MAGIC,
        SOURCE_METADATA_SCHEMA,
        record_generation,
        logical,
        &mut ws.seal_scratch,
    ) else {
        return AdoptOutcome::Failed(PublishError::BadInput);
    };
    let observed_length = identity.length;
    let base_token = existing.map(|entry| entry.metadata.book_token);
    let revalidate = || {
        // Re-identification must still be superseding the generation it
        // resolved; first adoption must still find its pair unclaimed.
        let mut scratch = [0u8; META_REVALIDATE_SCRATCH];
        let pair_state = publish::read_committed(records_dir, meta_names.pair(), &mut scratch);
        let base_ok = match (&base_token, pair_state) {
            (Some(token), Ok(Some((_, RecordState::Committed(view))))) => {
                SourceMetadata::decode(&view).is_some_and(|meta| meta.book_token == *token)
            }
            (None, Ok(None)) => true,
            _ => false,
        };
        if !base_ok {
            return false;
        }
        // The cheap changed-again tripwire, as in recovery.
        match books_dir.open_file_in_dir(name, embedded_sdmmc::Mode::ReadOnly) {
            Ok(file) => {
                let same = u64::from(file.length()) == observed_length;
                let _ = file.close();
                same
            }
            Err(_) => false,
        }
    };
    match publish::publish_record(
        records_dir,
        meta_names.pair(),
        &ws.seal_scratch,
        &sealed,
        0,
        &mut ws.record_scratch,
        revalidate,
    ) {
        Ok(_) => {}
        Err(error) => return AdoptOutcome::Failed(error),
    }

    if load_catalog(records_dir, ws).is_err() {
        ws.entries = [None; MAX_SOURCE_SLOTS];
    }
    let result = UploadResult {
        logical_book_id,
        book_token: fresh.book_token,
        source_generation,
        // Local operations are not receipted; intrinsic idempotency.
        receipt_durable: false,
    };
    if reidentifying {
        AdoptOutcome::Reidentified(result)
    } else {
        AdoptOutcome::Adopted(result)
    }
}

/// The initial label for a first adoption: the filename stem, or
/// `"Untitled"` when the stem does not survive label validation.
fn stem_label(name: &str) -> DisplayLabel {
    let stem = name.split('.').next().unwrap_or(name);
    DisplayLabel::new(stem.as_bytes())
        .or_else(|| DisplayLabel::new(b"Untitled"))
        .unwrap_or_else(DisplayLabel::placeholder)
}

/// Metadata record files are one padded sector plus the commit sector.
const META_REVALIDATE_SCRATCH: usize =
    match record::record_file_len(crate::bodies::SOURCE_METADATA_LOGICAL_BYTES) {
        Some(len) => len,
        None => 0,
    };
const _: () = assert!(META_REVALIDATE_SCRATCH > 0);
