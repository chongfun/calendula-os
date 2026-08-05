//! What a request ID promises once its receipt is gone.
//!
//! Two contracts, both invisible while receipts survive and both load-bearing
//! the moment one does not:
//!
//! - **A request ID stays spent across every endpoint.** `(epoch, nonce)`
//!   names a request, not a request to one endpoint. The receipt table
//!   enforces that by itself; the receipt-loss fallback reads durable
//!   records, which are per-operation files, so it has to search all of them.
//! - **Nobody claims a receipt is durable that the card has not taken.** A
//!   receipt staged in memory and lost to a failed publication must not make
//!   a same-session retry report `receipt_durable: true`.
//!
//! The cross-endpoint tests lose the receipt by deleting the idempotency
//! record — the whole-state version of a failed publication, and the only
//! part of it that matters here, since one operation's receipt is the only
//! one there is. The last two tests inject a real write fault instead, so the
//! state the others assume is proven reachable rather than assumed.

mod common;

use common::{new_card, open_mgr, open_root, publish_metadata, sample_metadata, Dir, SharedDisk};
use source_store::bodies::{
    DisplayLabel, Tombstone, BOOK_TOKEN_BYTES, DISPLAY_LABEL_MAX_BYTES, LOGICAL_BOOK_ID_BYTES,
    REQUEST_ID_BYTES, SHA256_BYTES, TOMBSTONE_LOGICAL_BYTES, TOMBSTONE_MAGIC, TOMBSTONE_SCHEMA,
    TOMBSTONE_STATUS_DELETED,
};
use source_store::cleanup::run_cleanup;
use source_store::layout;
use source_store::ops::{
    delete_book, find_request_trace, load_catalog, DeleteOutcome, DeleteRequest, IdempotencyStore,
    OpsWorkspace, RequestTrace,
};
use source_store::publish::publish_record;
use source_store::receipts::{OperationReceipt, ReceiptOperation, RECEIPT_STATUS_SUCCESS};
use source_store::record::{record_file_len, seal_body};
use source_store::recover::{recover_book, RecoveryOutcome, RecoveryRequest};
use source_store::select::SlotDisposition;
use source_store::upload::{
    begin_upload, finish_upload, upload_chunk, FreshIdentity, UploadBeginOutcome, UploadRequest,
    UploadResult,
};
use source_store::validate::sha256_of;

fn workspace() -> Box<OpsWorkspace> {
    Box::new(OpsWorkspace::new())
}

fn epub_bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|n| (n as u8).wrapping_mul(37) ^ seed)
        .collect()
}

fn fresh(id_seed: u8, token_seed: u8) -> FreshIdentity {
    FreshIdentity {
        logical_book_id: [id_seed; LOGICAL_BOOK_ID_BYTES],
        book_token: [token_seed; BOOK_TOKEN_BYTES],
    }
}

fn create_request(nonce_seed: u8, bytes: &[u8]) -> UploadRequest {
    UploadRequest {
        epoch: 1,
        nonce: [nonce_seed; 16],
        declared_length: bytes.len() as u64,
        declared_sha256: sha256_of(bytes),
        display_label: DisplayLabel::new(b"Book").unwrap(),
        replace_token: None,
    }
}

fn delete_request(nonce_seed: u8, token_seed: u8) -> DeleteRequest {
    DeleteRequest {
        epoch: 1,
        nonce: [nonce_seed; 16],
        book_token: [token_seed; BOOK_TOKEN_BYTES],
    }
}

fn recovery_request(nonce_seed: u8, token_seed: u8, bytes: &[u8]) -> RecoveryRequest {
    RecoveryRequest {
        epoch: 1,
        nonce: [nonce_seed; 16],
        book_token: [token_seed; BOOK_TOKEN_BYTES],
        observed_length: bytes.len() as u64,
        observed_sha256: sha256_of(bytes),
        display_label: None,
    }
}

/// Create a book through the real upload pipeline.
fn upload_book(
    root: &Dir<'_>,
    idem: &mut IdempotencyStore,
    ws: &mut OpsWorkspace,
    nonce_seed: u8,
    id_seed: u8,
    token_seed: u8,
    bytes: &[u8],
) -> UploadResult {
    let req = create_request(nonce_seed, bytes);
    let mut txn = match begin_upload(root, idem, &req, fresh(id_seed, token_seed), ws) {
        UploadBeginOutcome::Started(txn) => txn,
        outcome => panic!("begin: {}", name(&outcome)),
    };
    for chunk in bytes.chunks(1500) {
        upload_chunk(root, &mut txn, chunk).expect("chunk");
    }
    finish_upload(root, idem, txn, ws, || true).expect("finish")
}

/// Overwrite a managed slot's EPUB in place — the external modification that
/// makes a book recoverable.
fn tamper_slot(root: &Dir<'_>, slot: u8, bytes: &[u8]) {
    let name = layout::source_slot_name(slot).unwrap();
    let file = root
        .open_file_in_dir(
            name.as_str(),
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("tamper open");
    file.write(bytes).expect("tamper write");
    file.close().expect("tamper close");
}

/// The state a failed receipt publication leaves behind: the operation
/// committed, nothing in the idempotency record answers for it.
fn lose_every_receipt(root: &Dir<'_>) {
    for name in layout::idempotency_pair().pair().names {
        let _ = root.delete_file_in_dir(name);
    }
}

/// The reboot view's authoritative tokens.
fn visible_tokens(disk: &SharedDisk) -> Vec<[u8; BOOK_TOKEN_BYTES]> {
    let mgr = open_mgr(disk);
    let root = open_root(&mgr);
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog loads");
    let mut tokens = Vec::new();
    for (slot, entry) in ws.entries.iter().enumerate() {
        if let Some(entry) = entry {
            if ws.dispositions[slot] == SlotDisposition::Authoritative {
                tokens.push(entry.metadata.book_token);
            }
        }
    }
    tokens
}

fn name(outcome: &UploadBeginOutcome) -> &'static str {
    match outcome {
        UploadBeginOutcome::Started(_) => "Started",
        UploadBeginOutcome::Replayed(_) => "Replayed",
        UploadBeginOutcome::RejectedUnknownToken => "RejectedUnknownToken",
        UploadBeginOutcome::RejectedStaleEpoch => "RejectedStaleEpoch",
        UploadBeginOutcome::RejectedParameterMismatch => "RejectedParameterMismatch",
        UploadBeginOutcome::RejectedEpochExhausted => "RejectedEpochExhausted",
        UploadBeginOutcome::RejectedIdentityCollision => "RejectedIdentityCollision",
        UploadBeginOutcome::RejectedNoFreeSlot => "RejectedNoFreeSlot",
        UploadBeginOutcome::CatalogUnavailable => "CatalogUnavailable",
        UploadBeginOutcome::IdempotencyUnavailable => "IdempotencyUnavailable",
        UploadBeginOutcome::AmbiguousRequestEvidence => "AmbiguousRequestEvidence",
        UploadBeginOutcome::Failed(_) => "Failed",
    }
}

/// Seal and commit a tombstone into `slot` directly, bypassing `delete_book`.
/// The fixtures below need a tombstone carrying a request ID the current
/// build would never let a delete spend, which is the whole point.
fn publish_tombstone(root: &Dir<'_>, slot: u8, stone: &Tombstone) {
    let mut buf = vec![0u8; record_file_len(TOMBSTONE_LOGICAL_BYTES).unwrap()];
    let logical = stone.encode_into(&mut buf).expect("encode tombstone");
    let sealed =
        seal_body(TOMBSTONE_MAGIC, TOMBSTONE_SCHEMA, 1, logical, &mut buf).expect("seal tombstone");
    let pair = layout::tombstone_pair(slot).expect("tombstone slot");
    let mut scratch = [0u8; 4096];
    publish_record(root, pair.pair(), &buf, &sealed, 0, &mut scratch, || true)
        .expect("publish tombstone");
}

// ---------------------------------------------------------------------------
// The request-ID namespace is global
// ---------------------------------------------------------------------------

/// A create's ID, receipt lost. A delete reusing it must not delete the very
/// book that ID produced — the metadata carrying the ID is the answer, and
/// delete has to look at it.
#[test]
fn a_delete_cannot_reuse_an_uploads_request_id() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let bytes = epub_bytes(4_000, 1);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let created = upload_book(&root, &mut idem, &mut ws, 7, 10, 20, &bytes);
    lose_every_receipt(&root);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog after reboot");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem after reboot");
    let outcome = delete_book(&root, &mut idem, &delete_request(7, 20), &mut ws);
    assert_eq!(
        outcome,
        DeleteOutcome::RejectedParameterMismatch,
        "a delete answered a request ID the create already spent"
    );
    assert_eq!(
        visible_tokens(&disk),
        vec![created.book_token],
        "the book was deleted under a spent request ID"
    );
}

/// The same for recovery: its metadata record carries the ID, and a delete
/// that never opens metadata would call the reuse new.
#[test]
fn a_delete_cannot_reuse_a_recoverys_request_id() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let original = epub_bytes(4_000, 1);
    let changed = epub_bytes(3_000, 2);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    upload_book(&root, &mut idem, &mut ws, 5, 10, 20, &original);
    tamper_slot(&root, 0, &changed);
    let recovered = match recover_book(
        &root,
        &mut idem,
        &recovery_request(7, 20, &changed),
        [30; BOOK_TOKEN_BYTES],
        &mut ws,
        || true,
    ) {
        RecoveryOutcome::Recovered(result) => result,
        outcome => panic!("recover: {outcome:?}"),
    };
    lose_every_receipt(&root);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog after reboot");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem after reboot");
    let outcome = delete_book(&root, &mut idem, &delete_request(7, 30), &mut ws);
    assert_eq!(
        outcome,
        DeleteOutcome::RejectedParameterMismatch,
        "a delete answered a request ID the recovery already spent"
    );
    assert_eq!(
        visible_tokens(&disk),
        vec![recovered.book_token],
        "the book was deleted under a spent request ID"
    );
}

/// A delete's ID, receipt lost. A create reusing it must not run: the
/// tombstone is the durable answer, and create has to look at tombstones.
#[test]
fn a_create_cannot_reuse_a_deletes_request_id() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let first = epub_bytes(4_000, 1);
    let second = epub_bytes(3_000, 2);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    upload_book(&root, &mut idem, &mut ws, 5, 10, 20, &first);
    assert!(matches!(
        delete_book(&root, &mut idem, &delete_request(7, 20), &mut ws),
        DeleteOutcome::Deleted {
            replayed: false,
            ..
        }
    ));
    lose_every_receipt(&root);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog after reboot");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem after reboot");
    let outcome = begin_upload(
        &root,
        &mut idem,
        &create_request(7, &second),
        fresh(40, 50),
        &mut ws,
    );
    assert!(
        matches!(outcome, UploadBeginOutcome::RejectedParameterMismatch),
        "a create ran under a request ID the delete already spent: {}",
        name(&outcome)
    );
    assert!(
        visible_tokens(&disk).is_empty(),
        "a second operation executed under a spent request ID"
    );
}

/// And a recovery reusing a delete's ID. The recovery target is a *different*
/// book, so nothing but the namespace rule stands between the reuse and a
/// committed generation.
#[test]
fn a_recovery_cannot_reuse_a_deletes_request_id() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let doomed = epub_bytes(4_000, 1);
    let kept = epub_bytes(5_000, 2);
    let changed = epub_bytes(3_500, 3);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    upload_book(&root, &mut idem, &mut ws, 5, 10, 20, &doomed);
    let survivor = upload_book(&root, &mut idem, &mut ws, 6, 11, 21, &kept);
    assert!(matches!(
        delete_book(&root, &mut idem, &delete_request(7, 20), &mut ws),
        DeleteOutcome::Deleted {
            replayed: false,
            ..
        }
    ));
    tamper_slot(&root, 1, &changed);
    lose_every_receipt(&root);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog after reboot");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem after reboot");
    let outcome = recover_book(
        &root,
        &mut idem,
        &recovery_request(7, 21, &changed),
        [31; BOOK_TOKEN_BYTES],
        &mut ws,
        || true,
    );
    assert_eq!(
        outcome,
        RecoveryOutcome::RejectedParameterMismatch,
        "a recovery ran under a request ID the delete already spent"
    );
    assert_eq!(
        visible_tokens(&disk),
        vec![survivor.book_token],
        "the recovery committed a generation under a spent request ID"
    );
}

// ---------------------------------------------------------------------------
// Evidence that contradicts itself is not evidence
// ---------------------------------------------------------------------------

/// The request ID both an upload and a delete spent, on a card an older
/// build could leave behind.
const CONTESTED: u8 = 7;

/// Build that card. The sequence it comes from ran on `b095a80`, whose
/// delete fallback searched tombstones only:
///
/// 1. An upload commits metadata under this ID; its receipt does not land.
/// 2. A delete reuses the ID. Finding no receipt and no tombstone, that
///    build called it new and executed it — committing a tombstone under
///    the same ID.
/// 3. That receipt does not land either.
///
/// The metadata layout did not change across the upgrade, so the card loads
/// normally and neither record can be dismissed as stale. The delete is
/// published directly here because the current build, correctly, will not
/// perform step 2.
fn card_with_two_records_for_one_request(root: &Dir<'_>, disk: &SharedDisk) -> UploadResult {
    let bytes = epub_bytes(4_000, 1);
    let mut ws = workspace();
    load_catalog(root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(root, &mut ws).expect("idem");
    let created = upload_book(root, &mut idem, &mut ws, CONTESTED, 10, 20, &bytes);

    let mut request_id = [0u8; REQUEST_ID_BYTES];
    request_id[..8].copy_from_slice(&1u64.to_le_bytes());
    request_id[8..].copy_from_slice(&[CONTESTED; 16]);
    publish_tombstone(
        root,
        0,
        &Tombstone {
            logical_book_id: created.logical_book_id,
            deleted_source_generation: created.source_generation,
            deleted_book_token: created.book_token,
            delete_request_id: request_id,
            delete_result_status: TOMBSTONE_STATUS_DELETED,
        },
    );
    lose_every_receipt(root);

    // Both records are committed and both load: the epoch still accepts the
    // ID, so cleanup will not quietly repair this either.
    let mut ws = workspace();
    load_catalog(root, &mut ws).expect("catalog after reboot");
    assert_eq!(
        find_request_trace(&ws, &request_id),
        RequestTrace::Conflict,
        "the fixture did not produce two committed records for one request ID"
    );
    assert!(
        visible_tokens(disk).is_empty(),
        "the tombstone should hide the book it names"
    );
    created
}

/// Neither record may be served. Metadata-first would replay an upload
/// result for a book the tombstone says was deleted; tombstone-first would
/// replay a delete the upload metadata contradicts. Scan order is not an
/// adjudicator, so every endpoint refuses — and refuses without writing.
#[test]
fn contradictory_records_for_one_request_id_are_refused_by_every_endpoint() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let created = card_with_two_records_for_one_request(&root, &disk);
    let before = disk.snapshot();

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");

    // The original upload, retried with its original parameters.
    let retried = begin_upload(
        &root,
        &mut idem,
        &create_request(CONTESTED, &epub_bytes(4_000, 1)),
        fresh(10, 20),
        &mut ws,
    );
    assert!(
        matches!(retried, UploadBeginOutcome::AmbiguousRequestEvidence),
        "an upload replayed a result the card contradicts: {}",
        name(&retried)
    );

    // The delete, retried with the token its tombstone names.
    assert_eq!(
        delete_book(&root, &mut idem, &delete_request(CONTESTED, 20), &mut ws),
        DeleteOutcome::AmbiguousRequestEvidence,
        "a delete was refused as client misuse rather than as a card problem"
    );

    // And a recovery reusing the same ID.
    assert_eq!(
        recover_book(
            &root,
            &mut idem,
            &recovery_request(CONTESTED, 20, &epub_bytes(3_000, 2)),
            [31; BOOK_TOKEN_BYTES],
            &mut ws,
            || true,
        ),
        RecoveryOutcome::AmbiguousRequestEvidence,
        "a recovery ran against contradictory evidence"
    );

    assert!(
        disk.snapshot() == before,
        "refusing a contradictory request ID wrote to the card"
    );
    // The refusal is not a denial of service for other work: a different
    // request ID still resolves normally.
    assert_eq!(
        find_request_trace(&ws, &[9; REQUEST_ID_BYTES]),
        RequestTrace::None
    );
    let _ = created;
}

/// Identical to `card_with_two_records_for_one_request`, but retains a receipt
/// for the second operation (the delete).
fn card_with_two_records_and_receipt_for_one_request(
    root: &Dir<'_>,
    disk: &SharedDisk,
) -> UploadResult {
    let created = card_with_two_records_for_one_request(root, disk);
    let mut ws = workspace();
    load_catalog(root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(root, &mut ws).expect("idem");

    let mut request_id = [0u8; REQUEST_ID_BYTES];
    request_id[..8].copy_from_slice(&1u64.to_le_bytes());
    request_id[8..].copy_from_slice(&[CONTESTED; 16]);

    idem.state
        .insert(OperationReceipt {
            epoch: 1,
            request_nonce: [CONTESTED; 16],
            operation: ReceiptOperation::Delete,
            logical_book_id: created.logical_book_id,
            base_book_token_or_zero: created.book_token,
            source_generation: 0,
            source_length_or_zero: 0,
            source_sha256_or_zero: [0; SHA256_BYTES],
            display_label_len: 0,
            display_label: [0; DISPLAY_LABEL_MAX_BYTES],
            result_book_token_or_zero: [0; BOOK_TOKEN_BYTES],
            result_status: RECEIPT_STATUS_SUCCESS,
        })
        .expect("insert delete receipt");
    idem.publish(root, &mut ws).expect("publish receipt");

    created
}

/// Even when a receipt exists, contradictory fallback records (upload metadata
/// plus delete tombstone) must prevent the receipt from resolving the request.
/// Every endpoint must return `AmbiguousRequestEvidence`, cleanup must
/// preserve both conflicting records, and refusal must not mutate the card.
#[test]
fn durable_receipt_with_conflicting_fallback_records_is_refused_and_preserved() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let created = card_with_two_records_and_receipt_for_one_request(&root, &disk);
    let before = disk.snapshot();

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");

    // The upload retry returns AmbiguousRequestEvidence.
    let retried = begin_upload(
        &root,
        &mut idem,
        &create_request(CONTESTED, &epub_bytes(4_000, 1)),
        fresh(10, 20),
        &mut ws,
    );
    assert!(
        matches!(retried, UploadBeginOutcome::AmbiguousRequestEvidence),
        "an upload replayed despite contradictory fallback records: {}",
        name(&retried)
    );

    // The delete retry returns AmbiguousRequestEvidence.
    assert_eq!(
        delete_book(&root, &mut idem, &delete_request(CONTESTED, 20), &mut ws),
        DeleteOutcome::AmbiguousRequestEvidence,
        "a delete replayed despite contradictory fallback records"
    );

    // The recovery retry returns AmbiguousRequestEvidence.
    assert_eq!(
        recover_book(
            &root,
            &mut idem,
            &recovery_request(CONTESTED, 20, &epub_bytes(3_000, 2)),
            [31; BOOK_TOKEN_BYTES],
            &mut ws,
            || true,
        ),
        RecoveryOutcome::AmbiguousRequestEvidence,
        "a recovery ran despite contradictory fallback records"
    );

    assert!(
        disk.snapshot() == before,
        "refusing a contradictory request ID with a receipt wrote to the card"
    );

    // Cleanup must not remove either conflicting record.
    let mut ws = workspace();
    let report = run_cleanup(&root, &mut ws).expect("cleanup");
    assert_eq!(
        report.reclaimed_slots, 0,
        "cleanup removed metadata for a conflicting request"
    );
    assert_eq!(
        report.reclaimed_tombstones, 0,
        "cleanup removed tombstone for a conflicting request"
    );
    assert_eq!(
        report.retained_for_replay, 2,
        "cleanup failed to retain both conflicting records"
    );

    let _ = created;
}

/// Two metadata records carrying one request ID is the same failure with a
/// different shape — nothing legitimate writes an ID into two slots — and it
/// must not be resolved by whichever slot is scanned first either.
#[test]
fn duplicate_metadata_for_one_request_id_is_a_conflict_too() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let mut request_id = [0u8; REQUEST_ID_BYTES];
    request_id[..8].copy_from_slice(&1u64.to_le_bytes());
    request_id[8..].copy_from_slice(&[CONTESTED; 16]);
    for (slot, seed) in [(0u8, 40u8), (1, 41)] {
        let mut meta = sample_metadata(1);
        meta.physical_slot = slot;
        meta.logical_book_id = [seed; LOGICAL_BOOK_ID_BYTES];
        meta.book_token = [seed; BOOK_TOKEN_BYTES];
        meta.operation_request_id = request_id;
        let pair = layout::metadata_pair(slot).expect("slot");
        publish_metadata(&root, pair.pair(), &meta, 1).expect("publish");
    }

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    assert_eq!(find_request_trace(&ws, &request_id), RequestTrace::Conflict);

    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let before = disk.snapshot();
    let outcome = begin_upload(
        &root,
        &mut idem,
        &create_request(CONTESTED, &epub_bytes(2_000, 3)),
        fresh(50, 60),
        &mut ws,
    );
    assert!(
        matches!(outcome, UploadBeginOutcome::AmbiguousRequestEvidence),
        "a duplicated request ID resolved to one of its records: {}",
        name(&outcome)
    );
    assert!(
        disk.snapshot() == before,
        "refusing a duplicated request ID wrote to the card"
    );
}

// ---------------------------------------------------------------------------
// Durability is what the card took, not what memory holds
// ---------------------------------------------------------------------------

/// The receipt-less-but-committed state the tests above construct by hand,
/// reached instead by a real injected write fault — and then the reuse.
#[test]
fn an_injected_receipt_loss_still_spends_the_request_id() {
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let mut ws = workspace();
        load_catalog(&root, &mut ws).expect("catalog");
        let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
        upload_book(&root, &mut idem, &mut ws, 5, 10, 20, &epub_bytes(3_000, 1));
    }
    let base = disk.snapshot();
    let mut proved = 0u32;

    for fault in 0..400u32 {
        disk.restore_cut(&base, &[], 0, 0);
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let mut ws = workspace();
        load_catalog(&root, &mut ws).expect("catalog");
        let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");

        disk.fault.fail_write_in.set(Some(fault));
        let outcome = delete_book(&root, &mut idem, &delete_request(9, 20), &mut ws);
        let fired = disk.fault.fail_write_in.get().is_none();
        disk.fault.fail_write_in.set(None);
        if !fired {
            break;
        }
        // Only the runs where the tombstone committed and no receipt did.
        if !matches!(
            outcome,
            DeleteOutcome::Deleted {
                replayed: false,
                receipt_durable: false,
                ..
            }
        ) {
            continue;
        }
        if committed_receipt(&root, 9) != Some(false) {
            continue;
        }
        proved += 1;

        // Reboot, then spend the ID again on a create.
        let mut ws = workspace();
        load_catalog(&root, &mut ws).expect("catalog after reboot");
        let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem after reboot");
        let retry = begin_upload(
            &root,
            &mut idem,
            &create_request(9, &epub_bytes(2_000, 4)),
            fresh(40, 50),
            &mut ws,
        );
        assert!(
            matches!(retry, UploadBeginOutcome::RejectedParameterMismatch),
            "fault {fault}: a create ran under the delete's request ID: {}",
            name(&retry)
        );
    }

    assert!(
        proved > 0,
        "no write fault lost a receipt after a committed tombstone; the test proves nothing"
    );
}

/// A receipt inserted into resident state and then lost to a failed
/// publication is not durable, and a same-session retry must say so —
/// `receipt_durable: true` is a promise that a reboot mid-retry answers the
/// same way, which a memory-only receipt cannot keep.
#[test]
fn a_same_session_replay_never_calls_an_uncommitted_receipt_durable() {
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let mut ws = workspace();
        load_catalog(&root, &mut ws).expect("catalog");
        let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
        upload_book(&root, &mut idem, &mut ws, 5, 10, 20, &epub_bytes(3_000, 1));
    }
    let base = disk.snapshot();
    let mut proved = 0u32;

    for fault in 0..400u32 {
        disk.restore_cut(&base, &[], 0, 0);
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let mut ws = workspace();
        load_catalog(&root, &mut ws).expect("catalog");
        let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");

        disk.fault.fail_write_in.set(Some(fault));
        let outcome = delete_book(&root, &mut idem, &delete_request(9, 20), &mut ws);
        let fired = disk.fault.fail_write_in.get().is_none();
        disk.fault.fail_write_in.set(None);
        if !fired {
            break;
        }
        if !matches!(
            outcome,
            DeleteOutcome::Deleted {
                replayed: false,
                receipt_durable: false,
                ..
            }
        ) {
            continue;
        }
        // The card has to actually lack the receipt: a publication that
        // failed *after* its commit sector did persist it, and `true` would
        // then be the honest answer.
        if committed_receipt(&root, 9) != Some(false) {
            continue;
        }
        proved += 1;

        // Same session, same store — no reboot.
        match delete_book(&root, &mut idem, &delete_request(9, 20), &mut ws) {
            DeleteOutcome::Deleted {
                replayed: true,
                receipt_durable,
                ..
            } => assert!(
                !receipt_durable,
                "fault {fault}: a receipt that never reached the card replayed as durable"
            ),
            // The other honest answer: the store could not re-read the card
            // after the failure, so it refuses instead of guessing.
            DeleteOutcome::IdempotencyUnavailable => {}
            other => panic!("fault {fault}: same-session retry: {other:?}"),
        }
    }

    assert!(
        proved > 0,
        "no write fault left a receipt uncommitted; the test proves nothing"
    );
}

/// Whether the *committed* idempotency record answers for epoch 1 under this
/// nonce. `None` when the record cannot be read at all.
fn committed_receipt(root: &Dir<'_>, nonce_seed: u8) -> Option<bool> {
    let mut ws = workspace();
    IdempotencyStore::load(root, &mut ws)
        .ok()
        .map(|store| store.state.contains_request(1, &[nonce_seed; 16]))
}
