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
//!   already hides these, so deletion changes nothing a reader can see —
//!   *unless* the record is still someone's replay evidence, below.
//! - **Never: the last durable evidence that a request committed.** Every
//!   operation retains a receipt, but retention is allowed to fail (the
//!   operation is committed by then, and reporting failure would be a
//!   lie). Replay then rests on the committed record carrying the
//!   request's identity — source metadata for uploads and recoveries, the
//!   tombstone for deletes. Reclaiming that record while its epoch still
//!   accepts new requests would leave a retry resolving to nothing, and a
//!   request that resolves to nothing gets *executed again*: a duplicate
//!   book, or a delete that answers `RejectedUnknownToken`. So a hidden
//!   record whose receipt is absent and whose epoch is still accepted is
//!   kept; its bytes are reclaimed, its record waits for the epoch to
//!   retire. This is the PRD's rule that receipt cleanup can never cause
//!   delayed re-execution, applied to every operation rather than only to
//!   deletes.
//! - **Orphan candidates**: an `.EPB` in a slot with no committed
//!   metadata and no marker naming it. Managed-namespace provenance says
//!   this can only be wreckage; it is never adoptable, only removable.
//! - **Spent tombstones**: only when no metadata for the book survives
//!   *and* the retention rule above releases the delete's own request ID.
//! - **Ambiguous slots are never touched**: equal-generation duplicates
//!   are corruption for *recovery* to resolve; cleanup deleting either
//!   side would be the arbitrary selection the selector refuses to make.
//!
//! Artifact and cache reclamation (render bundles, negative results, M3)
//! joins here in M0R+; v1 covers the source-transaction state.

use embedded_sdmmc::{BlockDevice, Directory, Mode, TimeSource};

use crate::bodies::{StagingMarker, REQUEST_ID_BYTES};
use crate::layout;
use crate::ops::{load_catalog, IdempotencyStore, OpsWorkspace};
use crate::publish::{self, PublishError};
use crate::receipts::{IdempotencyState, REQUEST_NONCE_BYTES};
use crate::record::RecordState;
use crate::select::{SlotDisposition, MAX_SOURCE_SLOTS};

/// What one cleanup pass reclaimed, for the caller's telemetry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub reclaimed_slots: usize,
    pub reclaimed_orphans: usize,
    pub reclaimed_tombstones: usize,
    pub reclaimed_markers: usize,
    /// Hidden records left in place because they are the only durable
    /// evidence that their request committed. They cost a slot until their
    /// epoch retires; a non-zero count here with no progress across passes
    /// means the owner should rotate the epoch.
    pub retained_for_replay: usize,
}

/// One bounded cleanup pass over the whole managed namespace. Loads the
/// catalog fresh at entry and leaves it reloaded at exit.
///
/// The idempotency state is read from the card rather than taken from the
/// caller, and deliberately so: retention decisions are claims about what
/// survives a reboot, and a resident [`IdempotencyStore`] can be ahead of
/// the card — [`IdempotencyState::insert`] runs before the publication that
/// makes it durable, and that publication is allowed to fail. Judging
/// "the receipt is retained" from memory would let one failed receipt
/// publication delete both the receipt-less record and its tombstone.
pub fn run_cleanup<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    ws: &mut OpsWorkspace,
) -> Result<CleanupReport, PublishError>
where
    D: BlockDevice,
    T: TimeSource,
{
    let mut report = CleanupReport::default();
    // Fails closed on an unreadable idempotency record: without committed
    // idempotency state nothing here can prove a reclamation is safe.
    let idem = IdempotencyStore::load(dir, ws)?;
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
        let Some(entry) = ws.entries[slot] else {
            continue;
        };
        let reclaim = matches!(
            ws.dispositions[slot],
            SlotDisposition::HiddenSuperseded | SlotDisposition::HiddenDeleted
        );
        if !reclaim {
            continue;
        }
        let slot = slot as u8;
        // The bytes go either way — a hidden generation's EPUB is dead
        // weight, and it is the megabytes that matter on a small card.
        if let Some(name) = layout::source_slot_name(slot) {
            let _ = dir.delete_file_in_dir(name.as_str());
        }
        if replay_evidence_needed(ws, &idem.state, &entry.metadata.operation_request_id) {
            report.retained_for_replay += 1;
            continue;
        }
        if let Some(pair) = layout::metadata_pair(slot) {
            for name in pair.pair().names {
                let _ = dir.delete_file_in_dir(name);
            }
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
        let needed = replay_evidence_needed(ws, &idem.state, &stone.delete_request_id);
        if needed {
            report.retained_for_replay += 1;
        }
        let book_state_remains = ws
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.metadata.logical_book_id == stone.logical_book_id);
        if needed || book_state_remains {
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

/// Whether this committed record must be preserved for replay or conflict tracking.
///
/// True when the request's epoch is accepted *and* either no receipt exists for the ID
/// or the fallback evidence contradicts the receipt (or disagrees with itself).
///
/// A local (epoch-zero) request is never accepted as new by an operation,
/// so it is never retained — unmanaged adoption's idempotency is intrinsic,
/// not receipted.
fn replay_evidence_needed(
    ws: &OpsWorkspace,
    state: &IdempotencyState,
    request_id: &[u8; REQUEST_ID_BYTES],
) -> bool {
    let epoch = u64::from_le_bytes(request_id[..8].try_into().unwrap_or([0u8; 8]));
    let nonce: [u8; REQUEST_NONCE_BYTES] = request_id[8..].try_into().unwrap_or([0u8; 16]);
    if !state.epoch_is_accepted(epoch) {
        return false;
    }
    match state.get_receipt(epoch, &nonce) {
        Some(receipt) => {
            let trace = crate::ops::find_request_trace(ws, request_id);
            !crate::ops::receipt_is_consistent_with_trace(receipt, trace)
        }
        None => true,
    }
}
