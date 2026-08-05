//! Authoritative source selection: which logical books exist after reboot,
//! which slots hide, and which staged transactions completed.
//!
//! Pure over already-classified, already-decoded records — the I/O layer
//! reads each slot's A/B pair through [`crate::publish::select_authority`]
//! and each tombstone and marker through the same classifier, then hands
//! the decoded survivors here. Keeping the *rules* free of I/O means the
//! startup selector, the publish path's post-commit verification, and the
//! host tests all run literally the same function, which is what the PRD's
//! "validate it through the exact startup selector" steps require.
//!
//! Selection order, per the PRD:
//!
//! 1. committed tombstones hide every generation of their book at or below
//!    the deleted generation;
//! 2. among surviving generations of one logical book, the highest is
//!    authoritative;
//! 3. equal surviving generations for one book are ambiguous — corruption
//!    to report for recovery, never a coin flip;
//! 4. everything else (lower generations) is hidden and cleanup-eligible.
//!
//! Prepared records never reach this module: the I/O layer's per-slot A/B
//! selection only surfaces committed metadata.

use crate::bodies::{SourceMetadata, StagingMarker, Tombstone};

/// Managed physical source slots. Provisional v1 constant (a PRD
/// measurement gate, alongside the catalog's own limits): bounds every
/// selection scratch array at a size the firmware can afford.
pub const MAX_SOURCE_SLOTS: usize = 64;

/// One physical slot's committed metadata, as the I/O layer found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotEntry {
    pub physical_slot: u8,
    pub metadata: SourceMetadata,
}

/// What startup selection decided about one slot's committed metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotDisposition {
    /// The highest surviving generation of its logical book.
    Authoritative,
    /// Covered by a committed tombstone. Hidden; cleanup-eligible once the
    /// tombstone's retention rules allow.
    HiddenDeleted,
    /// A lower generation than the book's authority. Hidden;
    /// cleanup-eligible.
    HiddenSuperseded,
    /// Another slot claims the same book at the same generation and neither
    /// is tombstoned. Duplicate authority is corruption: both slots hide
    /// and the book requires recovery, not arbitrary selection.
    HiddenAmbiguous,
}

/// Selection outcome summary. Dispositions land in the caller's array,
/// parallel to the input entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionReport {
    pub authoritative: usize,
    /// Slots hidden as ambiguous. Nonzero means at least one book needs
    /// recovery and must not be silently listed.
    pub ambiguous: usize,
}

/// The highest deleted generation any committed tombstone claims for this
/// book; generations at or below it are hidden.
fn tombstone_floor(tombstones: &[Tombstone], logical_book_id: &[u8; 16]) -> Option<u64> {
    tombstones
        .iter()
        .filter(|stone| stone.logical_book_id == *logical_book_id)
        .map(|stone| stone.deleted_source_generation)
        .max()
}

/// Apply the selection order to every entry. `dispositions[i]` describes
/// `entries[i]`; entries beyond `dispositions.len()` are an input error the
/// caller prevents by sizing both from [`MAX_SOURCE_SLOTS`]. Returns `None`
/// on mismatched lengths rather than panicking.
pub fn select_sources(
    entries: &[SlotEntry],
    tombstones: &[Tombstone],
    dispositions: &mut [SlotDisposition],
) -> Option<SelectionReport> {
    if entries.len() > MAX_SOURCE_SLOTS || dispositions.len() < entries.len() {
        return None;
    }
    let mut report = SelectionReport {
        authoritative: 0,
        ambiguous: 0,
    };
    for (i, entry) in entries.iter().enumerate() {
        let meta = &entry.metadata;
        let deleted = tombstone_floor(tombstones, &meta.logical_book_id)
            .is_some_and(|floor| meta.source_generation <= floor);
        if deleted {
            dispositions[i] = SlotDisposition::HiddenDeleted;
            continue;
        }
        // Compare against every other *surviving* generation of this book.
        let mut superseded = false;
        let mut duplicated = false;
        for (j, other) in entries.iter().enumerate() {
            if i == j || other.metadata.logical_book_id != meta.logical_book_id {
                continue;
            }
            let other_deleted = tombstone_floor(tombstones, &other.metadata.logical_book_id)
                .is_some_and(|floor| other.metadata.source_generation <= floor);
            if other_deleted {
                continue;
            }
            if other.metadata.source_generation > meta.source_generation {
                superseded = true;
            } else if other.metadata.source_generation == meta.source_generation {
                duplicated = true;
            }
        }
        dispositions[i] = if duplicated {
            report.ambiguous += 1;
            SlotDisposition::HiddenAmbiguous
        } else if superseded {
            SlotDisposition::HiddenSuperseded
        } else {
            report.authoritative += 1;
            SlotDisposition::Authoritative
        };
    }
    Some(report)
}

/// What a committed staging marker means now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerDisposition {
    /// Matching committed source metadata exists: the transaction reached
    /// its commit point. The marker is cleanup — removable.
    TransactionCommitted,
    /// No matching committed metadata. The candidate file (if any) in the
    /// marker's slot is staging-only: hidden, never adoptable as unmanaged,
    /// resumable or cleanable by the transaction rules.
    CandidateHidden,
}

/// A marker matches only the exact identity it staged: same book, same
/// candidate generation, same physical slot. Anything else — including
/// newer metadata for the same book — leaves the marker's own candidate
/// unexplained and therefore hidden.
///
/// Takes an iterator so callers holding `Option`-arrays can pass
/// `.iter().flatten()` without compacting entries through a stack copy.
pub fn marker_disposition<'a>(
    marker: &StagingMarker,
    entries: impl IntoIterator<Item = &'a SlotEntry>,
) -> MarkerDisposition {
    let committed = entries.into_iter().any(|entry| {
        entry.metadata.logical_book_id == marker.logical_book_id
            && entry.metadata.source_generation == marker.candidate_source_generation
            && entry.physical_slot == marker.candidate_physical_slot
    });
    if committed {
        MarkerDisposition::TransactionCommitted
    } else {
        MarkerDisposition::CandidateHidden
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;
    use std::vec::Vec;

    use crate::bodies::{
        DisplayLabel, OperationKind, SourceOrigin, StagedOperation, UnmanagedName,
        BOOK_TOKEN_BYTES, LOGICAL_BOOK_ID_BYTES, REQUEST_ID_BYTES, SHA256_BYTES,
        TOMBSTONE_STATUS_DELETED,
    };

    fn meta(book: u8, generation: u64) -> SourceMetadata {
        SourceMetadata {
            logical_book_id: [book; LOGICAL_BOOK_ID_BYTES],
            source_generation: generation,
            source_origin: SourceOrigin::ManagedUpload,
            operation_kind: OperationKind::ManagedUploadRequest,
            operation_request_id: [0; REQUEST_ID_BYTES],
            externally_recovered: false,
            physical_slot: 0,
            source_length: 10,
            source_sha256: [0; SHA256_BYTES],
            quick_fingerprint_policy_version: 1,
            quick_fingerprint_sha256: [0; SHA256_BYTES],
            book_token: [generation as u8 + 1; BOOK_TOKEN_BYTES],
            display_label: DisplayLabel::new(b"t").unwrap(),
            unmanaged_name: UnmanagedName::none(),
        }
    }

    fn entry(slot: u8, book: u8, generation: u64) -> SlotEntry {
        let mut metadata = meta(book, generation);
        metadata.physical_slot = slot;
        SlotEntry {
            physical_slot: slot,
            metadata,
        }
    }

    fn stone(book: u8, generation: u64) -> Tombstone {
        Tombstone {
            logical_book_id: [book; LOGICAL_BOOK_ID_BYTES],
            deleted_source_generation: generation,
            deleted_book_token: [1; BOOK_TOKEN_BYTES],
            delete_request_id: [2; REQUEST_ID_BYTES],
            delete_result_status: TOMBSTONE_STATUS_DELETED,
        }
    }

    fn run(
        entries: &[SlotEntry],
        tombstones: &[Tombstone],
    ) -> (Vec<SlotDisposition>, SelectionReport) {
        let mut dispositions = vec![SlotDisposition::HiddenDeleted; entries.len()];
        let report = select_sources(entries, tombstones, &mut dispositions).unwrap();
        (dispositions, report)
    }

    #[test]
    fn highest_generation_wins_lower_hides() {
        let entries = [entry(0, 1, 1), entry(1, 1, 2), entry(2, 2, 1)];
        let (dispositions, report) = run(&entries, &[]);
        assert_eq!(
            dispositions,
            vec![
                SlotDisposition::HiddenSuperseded,
                SlotDisposition::Authoritative,
                SlotDisposition::Authoritative,
            ]
        );
        assert_eq!(
            report,
            SelectionReport {
                authoritative: 2,
                ambiguous: 0
            }
        );
    }

    #[test]
    fn tombstone_hides_at_or_below_only() {
        let entries = [entry(0, 1, 1), entry(1, 1, 2), entry(2, 1, 3)];
        let (dispositions, report) = run(&entries, &[stone(1, 2)]);
        assert_eq!(
            dispositions,
            vec![
                SlotDisposition::HiddenDeleted,
                SlotDisposition::HiddenDeleted,
                SlotDisposition::Authoritative,
            ]
        );
        assert_eq!(report.authoritative, 1);
    }

    #[test]
    fn equal_generations_are_ambiguous_not_first_wins() {
        let entries = [entry(0, 1, 2), entry(1, 1, 2)];
        let (dispositions, report) = run(&entries, &[]);
        assert_eq!(
            dispositions,
            vec![
                SlotDisposition::HiddenAmbiguous,
                SlotDisposition::HiddenAmbiguous
            ]
        );
        assert_eq!(
            report,
            SelectionReport {
                authoritative: 0,
                ambiguous: 2
            }
        );
    }

    #[test]
    fn tombstoned_duplicate_does_not_create_ambiguity() {
        // Slot 0's copy of generation 2 is deleted; slot 1's generation 2
        // of a *different* book id is unrelated. The surviving book's only
        // live generation is authoritative.
        let entries = [entry(0, 1, 2), entry(1, 1, 3)];
        let (dispositions, _) = run(&entries, &[stone(1, 2)]);
        assert_eq!(
            dispositions,
            vec![
                SlotDisposition::HiddenDeleted,
                SlotDisposition::Authoritative
            ]
        );
    }

    #[test]
    fn multiple_tombstones_use_highest_floor() {
        let entries = [entry(0, 1, 3)];
        let (dispositions, _) = run(&entries, &[stone(1, 1), stone(1, 3)]);
        assert_eq!(dispositions, vec![SlotDisposition::HiddenDeleted]);
    }

    #[test]
    fn marker_dispositions() {
        let marker = StagingMarker {
            operation: StagedOperation::Create,
            operation_request_id: [1; REQUEST_ID_BYTES],
            logical_book_id: [5; LOGICAL_BOOK_ID_BYTES],
            base_book_token_or_zero: [0; BOOK_TOKEN_BYTES],
            candidate_source_generation: 1,
            candidate_physical_slot: 3,
            expected_source_length: 9,
            expected_source_sha256: [0; SHA256_BYTES],
            display_label: DisplayLabel::new(b"m").unwrap(),
        };
        // No metadata: the candidate stays hidden.
        assert_eq!(
            marker_disposition(&marker, &[]),
            MarkerDisposition::CandidateHidden
        );
        // Metadata for the same book at a different slot or generation is
        // not this transaction.
        assert_eq!(
            marker_disposition(&marker, &[entry(2, 5, 1)]),
            MarkerDisposition::CandidateHidden
        );
        assert_eq!(
            marker_disposition(&marker, &[entry(3, 5, 2)]),
            MarkerDisposition::CandidateHidden
        );
        // The exact staged identity: committed.
        assert_eq!(
            marker_disposition(&marker, &[entry(3, 5, 1)]),
            MarkerDisposition::TransactionCommitted
        );
    }

    #[test]
    fn oversized_input_is_refused() {
        let entries = vec![entry(0, 1, 1); MAX_SOURCE_SLOTS + 1];
        let mut dispositions = vec![SlotDisposition::HiddenDeleted; entries.len()];
        assert!(select_sources(&entries, &[], &mut dispositions).is_none());
        let mut short = vec![SlotDisposition::HiddenDeleted; 1];
        assert!(select_sources(&entries[..2], &[], &mut short).is_none());
    }
}
