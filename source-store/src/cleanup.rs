//! Restartable cleanup: reclaiming state that lost its authority without
//! ever being able to create any.
//!
//! Cleanup deletes; it never publishes. Every rule below is a pure
//! function of committed on-card state, so a run interrupted anywhere is
//! simply rerun — the PRD's restartable/idempotent requirement falls out
//! of having no transient state to lose. What it reclaims, and the proof
//! each reclamation demands:
//!
//! - **Spent staging markers**: the marker's transaction committed
//!   (matching metadata exists) — marker deleted, candidate kept, it *is*
//!   the source now. Or the transaction is abandoned (no matching
//!   metadata, and no operation is in flight under the storage-owner
//!   serialization this module is called under) — candidate deleted first,
//!   then marker, so a cut between the two leaves the candidate still
//!   explained.
//! - **Superseded and deleted generations**: their slot's EPUB and
//!   metadata pair. The authoritative generation lives elsewhere
//!   (superseded) or nowhere by design (deleted); either way selection
//!   already hides these, so deletion changes nothing a reader can see.
//! - **Orphan candidates**: an `.EPB` in a slot with no committed
//!   metadata and no marker naming it. Managed-namespace provenance says
//!   this can only be wreckage; it is never adoptable, only removable.
//! - **Spent tombstones**: only when no metadata for the book survives
//!   *and* delete-replay safety no longer needs the tombstone — its
//!   receipt is retained, or its epoch is no longer accepted (a delayed
//!   retry would be rejected as stale before reaching execution, which is
//!   a safe answer). This is the PRD's rule that receipt cleanup can
//!   never cause delayed re-execution.
//! - **Ambiguous slots are never touched**: equal-generation duplicates
//!   are corruption for *recovery* to resolve; cleanup deleting either
//!   side would be the arbitrary selection the selector refuses to make.
//!
//! Artifact and cache reclamation (render bundles, negative results, M3)
//! joins here in M0R+; v1 covers the source-transaction state.

use embedded_sdmmc::{BlockDevice, Directory, Mode, TimeSource};

use crate::bodies::StagingMarker;
use crate::layout;
use crate::ops::{load_catalog, IdempotencyStore, OpsWorkspace};
use crate::publish::{self, PublishError};
use crate::receipts::{OperationReceipt, ReceiptLookup, ReceiptOperation, RECEIPT_STATUS_SUCCESS};
use crate::record::RecordState;
use crate::select::{SlotDisposition, MAX_SOURCE_SLOTS};

/// What one cleanup pass reclaimed, for the caller's telemetry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub reclaimed_slots: usize,
    pub reclaimed_orphans: usize,
    pub reclaimed_tombstones: usize,
    pub reclaimed_markers: usize,
}

/// One bounded cleanup pass over the whole managed namespace. Loads the
/// catalog fresh at entry and leaves it reloaded at exit.
pub fn run_cleanup<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    idem: &IdempotencyStore,
    ws: &mut OpsWorkspace,
) -> Result<CleanupReport, PublishError>
where
    D: BlockDevice,
    T: TimeSource,
{
    let mut report = CleanupReport::default();
    load_catalog(dir, ws)?;

    // Staging marker.
    let marker_names = layout::marker_pair();
    let marker = match publish::read_committed(dir, marker_names.pair(), &mut ws.record_scratch) {
        Ok(Some((_, RecordState::Committed(view)))) => StagingMarker::decode(&view),
        Ok(_) => None,
        Err(_) => None, // An unreadable marker pair is left for a later pass.
    };
    if let Some(marker) = marker {
        let committed = crate::select::marker_disposition(&marker, ws.entries.iter().flatten())
            == crate::select::MarkerDisposition::TransactionCommitted;
        let mut remove_marker = committed;
        if !committed {
            // Abandoned: the candidate goes first, so a cut between the
            // two deletions never leaves an unexplained candidate. A slot
            // occupied by committed metadata is someone else's source now
            // and is never deleted here; an out-of-range slot has no
            // candidate to delete.
            let slot = usize::from(marker.candidate_physical_slot);
            if slot >= MAX_SOURCE_SLOTS {
                remove_marker = true;
            } else if ws.entries[slot].is_none() {
                if let Some(name) = layout::source_slot_name(marker.candidate_physical_slot) {
                    let _ = dir.delete_file_in_dir(name.as_str());
                }
                remove_marker = true;
            }
        }
        if remove_marker {
            for name in marker_names.pair().names {
                let _ = dir.delete_file_in_dir(name);
            }
            report.reclaimed_markers += 1;
        }
    }

    // Superseded and deleted generations. Ambiguous slots are skipped by
    // rule; authoritative ones by definition.
    for slot in 0..MAX_SOURCE_SLOTS {
        let Some(_) = ws.entries[slot] else { continue };
        let reclaim = matches!(
            ws.dispositions[slot],
            SlotDisposition::HiddenSuperseded | SlotDisposition::HiddenDeleted
        );
        if !reclaim {
            continue;
        }
        let slot = slot as u8;
        if let Some(pair) = layout::metadata_pair(slot) {
            for name in pair.pair().names {
                let _ = dir.delete_file_in_dir(name);
            }
        }
        if let Some(name) = layout::source_slot_name(slot) {
            let _ = dir.delete_file_in_dir(name.as_str());
        }
        report.reclaimed_slots += 1;
    }

    // Orphan candidates: reload first so slots just reclaimed above are
    // seen as empty, then delete any .EPB with neither metadata nor a
    // marker naming it (the marker was handled above; if it survives, it
    // still names its candidate).
    load_catalog(dir, ws)?;
    let marker_slot =
        match publish::read_committed(dir, marker_names.pair(), &mut ws.record_scratch) {
            Ok(Some((_, RecordState::Committed(view)))) => {
                StagingMarker::decode(&view).map(|marker| marker.candidate_physical_slot)
            }
            _ => None,
        };
    for slot in 0..MAX_SOURCE_SLOTS as u8 {
        if ws.entries[usize::from(slot)].is_some() || marker_slot == Some(slot) {
            continue;
        }
        let Some(name) = layout::source_slot_name(slot) else {
            continue;
        };
        match dir.open_file_in_dir(name.as_str(), Mode::ReadOnly) {
            Ok(file) => {
                let _ = file.close();
            }
            Err(_) => continue,
        }
        let _ = dir.delete_file_in_dir(name.as_str());
        report.reclaimed_orphans += 1;
    }

    // Spent tombstones.
    for slot in 0..layout::MAX_TOMBSTONE_SLOTS {
        let Some((tombstone_slot, stone)) = ws.tombstones[slot] else {
            continue;
        };
        let book_state_remains = ws
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.metadata.logical_book_id == stone.logical_book_id);
        if book_state_remains {
            continue;
        }
        // Replay safety: the receipt answers the retry, or the epoch is
        // retired and the retry is safely rejected as stale.
        let epoch = u64::from_le_bytes(stone.delete_request_id[..8].try_into().unwrap_or([0u8; 8]));
        let nonce: [u8; 16] = stone.delete_request_id[8..].try_into().unwrap_or([0u8; 16]);
        let probe = OperationReceipt {
            epoch,
            request_nonce: nonce,
            operation: ReceiptOperation::Delete,
            logical_book_id: stone.logical_book_id,
            base_book_token_or_zero: stone.deleted_book_token,
            source_generation: 0,
            source_length_or_zero: 0,
            source_sha256_or_zero: [0; 32],
            display_label_len: 0,
            display_label: [0; 64],
            result_book_token_or_zero: [0; 16],
            result_status: RECEIPT_STATUS_SUCCESS,
        };
        let receipt_retained = matches!(idem.state.lookup(&probe), ReceiptLookup::Replay(_));
        let epoch_retired = !idem.state.epoch_is_accepted(epoch);
        if !(receipt_retained || epoch_retired) {
            continue;
        }
        if let Some(pair) = layout::tombstone_pair(tombstone_slot) {
            for name in pair.pair().names {
                let _ = dir.delete_file_in_dir(name);
            }
            report.reclaimed_tombstones += 1;
        }
    }

    load_catalog(dir, ws)?;
    Ok(report)
}
