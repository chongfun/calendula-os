//! The list-logical-books operation: the catalog as clients are allowed
//! to see it.
//!
//! The entry shape is the PRD's list contract, and the exclusions matter
//! more than the fields: only authoritative, non-deleted generations
//! appear. Prepared records, staging-only candidates, deleted identities,
//! superseded generations, and ambiguous slots are all invisible — not
//! filtered out here, but never admitted, because the input is the
//! workspace's already-selected dispositions. Physical filenames stay
//! internal; the entry names books by token and logical ID only.
//!
//! `allowed_operations` is derived from current authority plus this
//! mount's integrity proofs, so the browser can only offer what the
//! device would accept: a healthy managed book replaces and deletes; an
//! externally modified one recovers or deletes; a book whose bytes are
//! gone deletes. The observed identity a `Mismatch` recorded is exposed
//! exactly so the client can quote it back to `/recover-book` — the
//! device re-proves it either way.
//!
//! `artifact_presence_by_profile_and_producer` from the PRD's entry list
//! is deliberately absent: render bundles, covers, and their namespaces
//! arrive with M0R, and inventing a placeholder shape now would just be a
//! schema to migrate later.

use crate::bodies::{
    DisplayLabel, SourceOrigin, BOOK_TOKEN_BYTES, LOGICAL_BOOK_ID_BYTES, SHA256_BYTES,
};
use crate::ops::OpsWorkspace;
use crate::select::{SlotDisposition, MAX_SOURCE_SLOTS};
use crate::session::IntegrityLevel;

/// The client-facing source-integrity states of the PRD's list contract.
/// Coarser than [`IntegrityLevel`] on purpose: a quick-check match changes
/// what the device may *display*, not what the client may be told is
/// proved, so `QuickChecked` still reads `UncheckedThisMount` here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceIntegrityStatus {
    UncheckedThisMount,
    ValidatedThisMount,
    Unavailable,
    ExternallyModified,
    UnsupportedSourceContainer,
}

/// What current authority lets a client do with this book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllowedOperations {
    pub replace: bool,
    pub delete: bool,
    pub recover_current_bytes: bool,
}

/// One listed book. All fields come from committed metadata or from this
/// mount's session — nothing here reads source bytes, which is what keeps
/// listing cheap enough to serve on every page load.
#[derive(Clone, Copy, Debug)]
pub struct BookListEntry {
    pub display_label: DisplayLabel,
    pub logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
    pub book_token: [u8; BOOK_TOKEN_BYTES],
    pub source_generation: u64,
    pub source_origin: SourceOrigin,
    pub externally_recovered: bool,
    pub source_integrity_status: SourceIntegrityStatus,
    pub source_length: u64,
    /// The observed identity behind an `ExternallyModified` status; `None`
    /// everywhere else. What a recovery request must quote back.
    pub observed_source_length: Option<u64>,
    pub observed_source_sha256: Option<[u8; SHA256_BYTES]>,
    pub allowed_operations: AllowedOperations,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListError {
    /// The workspace's catalog view is not a complete load of committed
    /// state; a list served from it could show deleted books or hide live
    /// ones. See [`OpsWorkspace::catalog_is_valid`].
    CatalogUnavailable,
    /// `out` cannot hold every authoritative book. Size it by
    /// [`MAX_SOURCE_SLOTS`] and this cannot happen.
    OutputTooSmall,
}

/// Fill `out` with every listable book, returning how many were written.
/// Slot order — stable across calls that see the same committed state.
pub fn list_books(ws: &OpsWorkspace, out: &mut [BookListEntry]) -> Result<usize, ListError> {
    if !ws.catalog_is_valid() {
        return Err(ListError::CatalogUnavailable);
    }
    let mut written = 0usize;
    for slot in 0..MAX_SOURCE_SLOTS {
        let Some(entry) = &ws.entries[slot] else {
            continue;
        };
        if ws.dispositions[slot] != SlotDisposition::Authoritative {
            continue;
        }
        let meta = &entry.metadata;
        let level = ws
            .session
            .level(&meta.logical_book_id, meta.source_generation);
        let status = integrity_status(level);
        let observed = ws
            .session
            .observed_identity(&meta.logical_book_id, meta.source_generation);
        let listed = BookListEntry {
            display_label: meta.display_label,
            logical_book_id: meta.logical_book_id,
            book_token: meta.book_token,
            source_generation: meta.source_generation,
            source_origin: meta.source_origin,
            externally_recovered: meta.externally_recovered,
            source_integrity_status: status,
            source_length: meta.source_length,
            observed_source_length: observed.map(|(length, _)| length),
            observed_source_sha256: observed.map(|(_, sha256)| sha256),
            allowed_operations: allowed_operations(meta.source_origin, status),
        };
        let Some(target) = out.get_mut(written) else {
            return Err(ListError::OutputTooSmall);
        };
        *target = listed;
        written += 1;
    }
    Ok(written)
}

fn integrity_status(level: IntegrityLevel) -> SourceIntegrityStatus {
    match level {
        // A quick match buys provisional display, not a client-visible
        // claim; the PRD keeps the UI in UncheckedThisMount until the full
        // pass lands.
        IntegrityLevel::Unchecked | IntegrityLevel::QuickChecked => {
            SourceIntegrityStatus::UncheckedThisMount
        }
        IntegrityLevel::FullyValidated => SourceIntegrityStatus::ValidatedThisMount,
        IntegrityLevel::Mismatch => SourceIntegrityStatus::ExternallyModified,
        IntegrityLevel::Unavailable => SourceIntegrityStatus::Unavailable,
        IntegrityLevel::UnsupportedContainer => SourceIntegrityStatus::UnsupportedSourceContainer,
    }
}

/// The PRD's derivation, verbatim in structure: a healthy managed book
/// replaces and deletes; an externally modified managed book recovers or
/// deletes (never ordinary replacement — its authority chain is broken);
/// a book whose bytes are gone or unsupported only deletes; an unmanaged
/// book deletes, with local re-identification handling its byte changes
/// without a client operation.
fn allowed_operations(origin: SourceOrigin, status: SourceIntegrityStatus) -> AllowedOperations {
    let healthy = matches!(
        status,
        SourceIntegrityStatus::UncheckedThisMount | SourceIntegrityStatus::ValidatedThisMount
    );
    AllowedOperations {
        replace: origin == SourceOrigin::ManagedUpload && healthy,
        delete: true,
        recover_current_bytes: origin == SourceOrigin::ManagedUpload
            && status == SourceIntegrityStatus::ExternallyModified,
    }
}
