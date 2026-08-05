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

use common::{new_card, open_mgr, open_root, Dir, SharedDisk};
use source_store::bodies::{DisplayLabel, BOOK_TOKEN_BYTES, LOGICAL_BOOK_ID_BYTES};
use source_store::layout;
use source_store::ops::{
    delete_book, load_catalog, DeleteOutcome, DeleteRequest, IdempotencyStore, OpsWorkspace,
};
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
        UploadBeginOutcome::Failed(_) => "Failed",
    }
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
