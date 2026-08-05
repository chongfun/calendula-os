//! End-to-end tests for explicit recovery and the cleanup engine:
//! adoption of externally changed bytes only under a confirmed observed
//! identity, and reclamation that can never destroy authority or replay
//! safety.

mod common;

use common::{new_card, open_mgr, open_root, publish_metadata, sample_metadata, Dir, SharedDisk};
use embedded_sdmmc::Mode;
use source_store::bodies::{DisplayLabel, BOOK_TOKEN_BYTES, LOGICAL_BOOK_ID_BYTES};
use source_store::cleanup::run_cleanup;
use source_store::layout;
use source_store::ops::{
    delete_book, find_authoritative_by_token, load_catalog, DeleteOutcome, DeleteRequest,
    IdempotencyStore, OpsWorkspace,
};
use source_store::publish::PublishError;
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
    let req = UploadRequest {
        epoch: idem.state.current_epoch,
        nonce: [nonce_seed; 16],
        declared_length: bytes.len() as u64,
        declared_sha256: sha256_of(bytes),
        display_label: DisplayLabel::new(b"Book").unwrap(),
        replace_token: None,
    };
    let mut txn = match begin_upload(root, idem, &req, fresh(id_seed, token_seed), ws) {
        UploadBeginOutcome::Started(txn) => txn,
        UploadBeginOutcome::Replayed(result) => return result,
        _ => panic!("begin failed"),
    };
    for chunk in bytes.chunks(1500) {
        upload_chunk(root, &mut txn, chunk).expect("chunk");
    }
    finish_upload(root, idem, txn, ws, || true).expect("finish")
}

fn fresh(id_seed: u8, token_seed: u8) -> FreshIdentity {
    FreshIdentity {
        logical_book_id: [id_seed; LOGICAL_BOOK_ID_BYTES],
        book_token: [token_seed; BOOK_TOKEN_BYTES],
    }
}

/// Overwrite a managed slot's EPUB file in place — the external
/// modification a card editor or another device performs.
fn tamper_slot(root: &Dir<'_>, slot: u8, bytes: &[u8]) {
    let name = layout::source_slot_name(slot).unwrap();
    let file = root
        .open_file_in_dir(name.as_str(), Mode::ReadWriteCreateOrTruncate)
        .expect("tamper open");
    file.write(bytes).expect("tamper write");
    file.close().expect("tamper close");
}

fn recovery(epoch: u64, nonce_seed: u8, token_seed: u8, bytes: &[u8]) -> RecoveryRequest {
    RecoveryRequest {
        epoch,
        nonce: [nonce_seed; 16],
        book_token: [token_seed; BOOK_TOKEN_BYTES],
        observed_length: bytes.len() as u64,
        observed_sha256: sha256_of(bytes),
        display_label: None,
    }
}

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

#[test]
fn recovery_adopts_confirmed_bytes_and_replays() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let original = epub_bytes(6_000, 1);
    let changed = epub_bytes(4_000, 2);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let created = upload_book(&root, &mut idem, &mut ws, 9, 5, 10, &original);
    let slot = find_authoritative_by_token(&ws, &created.book_token)
        .expect("visible")
        .physical_slot;

    tamper_slot(&root, slot, &changed);

    let req = recovery(1, 20, 10, &changed);
    let outcome = recover_book(
        &root,
        &mut idem,
        &req,
        [30; BOOK_TOKEN_BYTES],
        &mut ws,
        || true,
    );
    let RecoveryOutcome::Recovered(result) = outcome else {
        panic!("recovery failed: {outcome:?}");
    };
    assert_eq!(result.logical_book_id, [5; LOGICAL_BOOK_ID_BYTES]);
    assert_eq!(result.book_token, [30; BOOK_TOKEN_BYTES]);
    assert_eq!(result.source_generation, 2);
    assert!(result.receipt_durable);

    // The adopted generation is authoritative and marked as recovered.
    let entry = find_authoritative_by_token(&ws, &[30; BOOK_TOKEN_BYTES]).expect("recovered");
    assert!(entry.metadata.externally_recovered);
    assert_eq!(entry.metadata.source_length, changed.len() as u64);
    assert_eq!(entry.metadata.source_sha256, sha256_of(&changed));
    assert!(find_authoritative_by_token(&ws, &[10; BOOK_TOKEN_BYTES]).is_none());

    // Replay via receipt, then via metadata once receipts are gone.
    let replay = recover_book(
        &root,
        &mut idem,
        &req,
        [77; BOOK_TOKEN_BYTES],
        &mut ws,
        || true,
    );
    assert_eq!(replay, RecoveryOutcome::Recovered(result));
    let idem_names = layout::idempotency_pair();
    for name in idem_names.pair().names {
        let _ = root.delete_file_in_dir(name);
    }
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let replay = recover_book(
        &root,
        &mut idem,
        &req,
        [78; BOOK_TOKEN_BYTES],
        &mut ws,
        || true,
    );
    let RecoveryOutcome::Recovered(replayed) = replay else {
        panic!("metadata replay failed: {replay:?}");
    };
    assert_eq!(
        (replayed.book_token, replayed.source_generation),
        ([30; BOOK_TOKEN_BYTES], 2)
    );
}

#[test]
fn recovery_rejects_wrong_states() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let original = epub_bytes(6_000, 1);
    let changed = epub_bytes(4_000, 2);
    let changed_again = epub_bytes(3_000, 3);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let created = upload_book(&root, &mut idem, &mut ws, 9, 5, 10, &original);
    let slot = find_authoritative_by_token(&ws, &created.book_token)
        .expect("visible")
        .physical_slot;

    // Nothing changed: nothing to recover.
    let req = recovery(1, 20, 10, &original);
    assert_eq!(
        recover_book(
            &root,
            &mut idem,
            &req,
            [30; BOOK_TOKEN_BYTES],
            &mut ws,
            || true
        ),
        RecoveryOutcome::RejectedNotExternallyModified
    );

    // The client observed `changed`, but the card now holds
    // `changed_again`: adoption must refuse.
    tamper_slot(&root, slot, &changed_again);
    let stale = recovery(1, 21, 10, &changed);
    assert_eq!(
        recover_book(
            &root,
            &mut idem,
            &stale,
            [30; BOOK_TOKEN_BYTES],
            &mut ws,
            || true
        ),
        RecoveryOutcome::RejectedObservedMismatch
    );

    // Container gate refusal leaves the book quarantined, not adopted.
    let honest = recovery(1, 22, 10, &changed_again);
    assert_eq!(
        recover_book(
            &root,
            &mut idem,
            &honest,
            [30; BOOK_TOKEN_BYTES],
            &mut ws,
            || false
        ),
        RecoveryOutcome::RejectedUnsupportedContainer
    );

    // Unknown token.
    let unknown = recovery(1, 23, 99, &changed_again);
    assert_eq!(
        recover_book(
            &root,
            &mut idem,
            &unknown,
            [30; BOOK_TOKEN_BYTES],
            &mut ws,
            || true
        ),
        RecoveryOutcome::RejectedUnknownToken
    );

    // The original token still resolves (nothing was adopted), and the
    // catalog still lists exactly the original book.
    assert_eq!(visible_tokens(&disk), vec![[10; BOOK_TOKEN_BYTES]]);
}

#[test]
fn cleanup_reclaims_superseded_generation_and_frees_its_slot() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let v1 = epub_bytes(5_000, 1);
    let v2 = epub_bytes(5_000, 2);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    upload_book(&root, &mut idem, &mut ws, 9, 5, 10, &v1);
    let replace = UploadRequest {
        epoch: 1,
        nonce: [20; 16],
        declared_length: v2.len() as u64,
        declared_sha256: sha256_of(&v2),
        display_label: DisplayLabel::new(b"Book").unwrap(),
        replace_token: Some([10; BOOK_TOKEN_BYTES]),
    };
    let mut txn = match begin_upload(&root, &mut idem, &replace, fresh(99, 30), &mut ws) {
        UploadBeginOutcome::Started(txn) => txn,
        _ => panic!("replace begin"),
    };
    for chunk in v2.chunks(1500) {
        upload_chunk(&root, &mut txn, chunk).expect("chunk");
    }
    finish_upload(&root, &mut idem, txn, &mut ws, || true).expect("replace");

    let report = run_cleanup(&root, &mut ws).expect("cleanup");
    assert_eq!(report.reclaimed_slots, 1, "the superseded generation");

    // Slot 0's records and bytes are gone; the book still serves.
    let pair = layout::metadata_pair(0).unwrap();
    for name in pair.pair().names {
        assert!(root.open_file_in_dir(name, Mode::ReadOnly).is_err());
    }
    let slot_file = layout::source_slot_name(0).unwrap();
    assert!(root
        .open_file_in_dir(slot_file.as_str(), Mode::ReadOnly)
        .is_err());
    assert_eq!(visible_tokens(&disk), vec![[30; BOOK_TOKEN_BYTES]]);

    // A second pass reclaims nothing, and the freed slot is reused by the
    // next create.
    let report = run_cleanup(&root, &mut ws).expect("cleanup again");
    assert_eq!(report, Default::default());
    let another = upload_book(&root, &mut idem, &mut ws, 40, 6, 50, &v1);
    let entry = find_authoritative_by_token(&ws, &another.book_token).expect("new book");
    assert_eq!(entry.physical_slot, 0, "freed slot is allocated first");
}

#[test]
fn cleanup_reclaims_deleted_books_and_spent_tombstones_safely() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let bytes = epub_bytes(4_000, 1);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    upload_book(&root, &mut idem, &mut ws, 9, 5, 10, &bytes);
    let delete = DeleteRequest {
        epoch: 1,
        nonce: [30; 16],
        book_token: [10; BOOK_TOKEN_BYTES],
    };
    assert!(matches!(
        delete_book(&root, &mut idem, &delete, &mut ws),
        DeleteOutcome::Deleted { .. }
    ));

    // Receipt retained -> the tombstone is reclaimable along with the
    // book's records, and the delete still replays from the receipt.
    let report = run_cleanup(&root, &mut ws).expect("cleanup");
    assert_eq!(report.reclaimed_slots, 1);
    assert_eq!(report.reclaimed_tombstones, 1);
    assert!(matches!(
        delete_book(&root, &mut idem, &delete, &mut ws),
        DeleteOutcome::Deleted { replayed: true, .. }
    ));

    // Second book: delete, then retire the receipt's epoch entirely. The
    // tombstone must survive cleanup while its epoch is still accepted...
    upload_book(&root, &mut idem, &mut ws, 40, 6, 50, &bytes);
    let delete2 = DeleteRequest {
        epoch: 1,
        nonce: [41; 16],
        book_token: [50; BOOK_TOKEN_BYTES],
    };
    assert!(matches!(
        delete_book(&root, &mut idem, &delete2, &mut ws),
        DeleteOutcome::Deleted { .. }
    ));
    // Drop the receipts (lost idempotency store), keeping epoch 1 current.
    let idem_names = layout::idempotency_pair();
    for name in idem_names.pair().names {
        let _ = root.delete_file_in_dir(name);
    }
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("fresh idem");
    let report = run_cleanup(&root, &mut ws).expect("cleanup");
    assert_eq!(
        report.reclaimed_tombstones, 0,
        "no receipt and a live epoch: the tombstone is the only replay evidence"
    );
    assert!(matches!(
        delete_book(&root, &mut idem, &delete2, &mut ws),
        DeleteOutcome::Deleted {
            replayed: true,
            receipt_durable: false,
            ..
        }
    ));

    // ...and be reclaimable once the epoch is retired, after which the
    // delayed retry is rejected as stale — never re-executed.
    idem.state.rotate_epoch(2).expect("rotate");
    idem.state.rotate_epoch(3).expect("rotate again");
    idem.publish(&root, &mut ws).expect("publish rotations");
    let report = run_cleanup(&root, &mut ws).expect("cleanup");
    assert_eq!(report.reclaimed_tombstones, 1);
    assert_eq!(
        delete_book(&root, &mut idem, &delete2, &mut ws),
        DeleteOutcome::RejectedStaleEpoch
    );
}

#[test]
fn cleanup_handles_markers_orphans_and_leaves_ambiguity() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let bytes = epub_bytes(4_000, 1);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");

    // An abandoned mid-stream upload: marker plus partial candidate.
    let req = UploadRequest {
        epoch: 1,
        nonce: [9; 16],
        declared_length: bytes.len() as u64,
        declared_sha256: sha256_of(&bytes),
        display_label: DisplayLabel::new(b"Book").unwrap(),
        replace_token: None,
    };
    let mut txn = match begin_upload(&root, &mut idem, &req, fresh(5, 10), &mut ws) {
        UploadBeginOutcome::Started(txn) => txn,
        _ => panic!("begin"),
    };
    upload_chunk(&root, &mut txn, &bytes[..1500]).expect("chunk");
    drop(txn);

    // A bare orphan candidate in another slot, with no marker at all.
    tamper_slot(&root, 7, &bytes[..600]);

    // An ambiguous pair: the same book at the same generation in two
    // slots. Cleanup must not touch either side.
    let mut duplicate = sample_metadata(4);
    duplicate.logical_book_id = [200; LOGICAL_BOOK_ID_BYTES];
    for slot in [2u8, 3u8] {
        duplicate.physical_slot = slot;
        duplicate.book_token = [200 + slot; BOOK_TOKEN_BYTES];
        let pair = layout::metadata_pair(slot).unwrap();
        publish_metadata(&root, pair.pair(), &duplicate, 1).expect("duplicate");
    }

    let report = run_cleanup(&root, &mut ws).expect("cleanup");
    assert_eq!(report.reclaimed_markers, 1);
    assert_eq!(report.reclaimed_orphans, 1);
    assert_eq!(report.reclaimed_slots, 0);

    // The abandoned candidate and marker are gone.
    let candidate = layout::source_slot_name(0).unwrap();
    assert!(root
        .open_file_in_dir(candidate.as_str(), Mode::ReadOnly)
        .is_err());
    let marker_names = layout::marker_pair();
    for name in marker_names.pair().names {
        assert!(root.open_file_in_dir(name, Mode::ReadOnly).is_err());
    }

    // Both ambiguous records survive, still ambiguous.
    for slot in [2usize, 3usize] {
        assert!(ws.entries[slot].is_some());
        assert_eq!(ws.dispositions[slot], SlotDisposition::HiddenAmbiguous);
    }

    // The abandoned request can now run to completion in the freed slot.
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut txn = match begin_upload(&root, &mut idem, &req, fresh(5, 10), &mut ws) {
        UploadBeginOutcome::Started(txn) => txn,
        _ => panic!("rerun begin"),
    };
    for chunk in bytes.chunks(1500) {
        upload_chunk(&root, &mut txn, chunk).expect("chunk");
    }
    finish_upload(&root, &mut idem, txn, &mut ws, || true).expect("rerun finish");
    assert!(visible_tokens(&disk).contains(&[10; BOOK_TOKEN_BYTES]));
}

#[test]
fn power_cut_during_cleanup_preserves_authority_and_converges() {
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let bytes = epub_bytes(4_000, 1);
        let v2 = epub_bytes(4_000, 2);
        let mut ws = workspace();
        load_catalog(&root, &mut ws).expect("catalog");
        let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
        // A superseded generation, a deleted book, and an abandoned
        // marker — everything cleanup touches, all at once.
        upload_book(&root, &mut idem, &mut ws, 9, 5, 10, &bytes);
        let replace = UploadRequest {
            epoch: 1,
            nonce: [20; 16],
            declared_length: v2.len() as u64,
            declared_sha256: sha256_of(&v2),
            display_label: DisplayLabel::new(b"Book").unwrap(),
            replace_token: Some([10; BOOK_TOKEN_BYTES]),
        };
        let mut txn = match begin_upload(&root, &mut idem, &replace, fresh(99, 30), &mut ws) {
            UploadBeginOutcome::Started(txn) => txn,
            _ => panic!("replace begin"),
        };
        for chunk in v2.chunks(1500) {
            upload_chunk(&root, &mut txn, chunk).expect("chunk");
        }
        finish_upload(&root, &mut idem, txn, &mut ws, || true).expect("replace");
        upload_book(&root, &mut idem, &mut ws, 40, 6, 50, &bytes);
        let delete = DeleteRequest {
            epoch: 1,
            nonce: [41; 16],
            book_token: [50; BOOK_TOKEN_BYTES],
        };
        assert!(matches!(
            delete_book(&root, &mut idem, &delete, &mut ws),
            DeleteOutcome::Deleted { .. }
        ));
        let abandoned = UploadRequest {
            epoch: 1,
            nonce: [60; 16],
            declared_length: bytes.len() as u64,
            declared_sha256: sha256_of(&bytes),
            display_label: DisplayLabel::new(b"Book").unwrap(),
            replace_token: None,
        };
        let mut txn = match begin_upload(&root, &mut idem, &abandoned, fresh(7, 70), &mut ws) {
            UploadBeginOutcome::Started(txn) => txn,
            _ => panic!("abandoned begin"),
        };
        upload_chunk(&root, &mut txn, &bytes[..1500]).expect("chunk");
        drop(txn);
    }
    let base = disk.snapshot();
    disk.start_logging();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let mut ws = workspace();
        run_cleanup(&root, &mut ws).expect("cleanup");
    }
    let log = disk.take_log();

    for cut in (0..=log.len()).step_by(2) {
        disk.restore_cut(&base, &log, cut, 0);
        // Authority is untouchable by cleanup at any boundary.
        let mut tokens = visible_tokens(&disk);
        tokens.sort();
        assert_eq!(
            tokens,
            vec![[30; BOOK_TOKEN_BYTES]],
            "cut {cut}: cleanup disturbed authority"
        );
        // And the rerun converges to the fully clean state.
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let mut ws = workspace();
        run_cleanup(&root, &mut ws).expect("cleanup rerun");
        let rerun = run_cleanup(&root, &mut ws).expect("cleanup idempotent");
        assert_eq!(rerun, Default::default(), "cut {cut}: did not converge");
    }
}

/// Recovery hashes the slot, gates the container, then commits. Its last
/// look before the commit must bind the exact identity it is about to vouch
/// for — a length check would pass a same-length edit made in that window,
/// leaving metadata that describes bytes the card no longer holds. The
/// container gate is the injection point: it runs inside the window.
#[test]
fn recovery_refuses_a_same_length_change_inside_the_commit_window() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let original = epub_bytes(6_000, 1);
    let observed = epub_bytes(4_000, 2);
    let swapped = epub_bytes(4_000, 3); // same length, different bytes

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let created = upload_book(&root, &mut idem, &mut ws, 9, 5, 10, &original);
    let slot = find_authoritative_by_token(&ws, &created.book_token)
        .expect("visible")
        .physical_slot;

    tamper_slot(&root, slot, &observed);
    let req = recovery(1, 20, 10, &observed);
    let outcome = recover_book(
        &root,
        &mut idem,
        &req,
        [30; BOOK_TOKEN_BYTES],
        &mut ws,
        || {
            // The bytes change again, at the same length, after the device
            // hashed them and before the commit sector lands.
            tamper_slot(&root, slot, &swapped);
            true
        },
    );
    assert_eq!(
        outcome,
        RecoveryOutcome::Failed(PublishError::RevalidationRefused),
        "a same-length change inside the commit window was adopted"
    );

    // Nothing was adopted: the original generation still holds the book, and
    // the operation is retryable against a freshly observed identity.
    assert_eq!(visible_tokens(&disk), vec![[10; BOOK_TOKEN_BYTES]]);
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let honest = recovery(1, 21, 10, &swapped);
    let outcome = recover_book(
        &root,
        &mut idem,
        &honest,
        [31; BOOK_TOKEN_BYTES],
        &mut ws,
        || true,
    );
    let RecoveryOutcome::Recovered(result) = outcome else {
        panic!("retry after a refused window failed: {outcome:?}");
    };
    assert_eq!(result.book_token, [31; BOOK_TOKEN_BYTES]);
}

/// Cleanup may not reclaim the last durable evidence that a request
/// committed. When a receipt did not survive and its epoch still accepts new
/// requests, the committed record is all that stands between a retry and a
/// second execution.
#[test]
fn cleanup_keeps_the_last_replay_evidence_of_a_receiptless_request() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let v1 = epub_bytes(5_000, 1);
    let v2 = epub_bytes(5_000, 2);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let created = upload_book(&root, &mut idem, &mut ws, 9, 5, 10, &v1);
    let create_slot = find_authoritative_by_token(&ws, &created.book_token)
        .expect("visible")
        .physical_slot;

    // A replace hides the create's generation — the shape cleanup reclaims.
    let replace = UploadRequest {
        epoch: 1,
        nonce: [20; 16],
        declared_length: v2.len() as u64,
        declared_sha256: sha256_of(&v2),
        display_label: DisplayLabel::new(b"Book").unwrap(),
        replace_token: Some([10; BOOK_TOKEN_BYTES]),
    };
    let mut txn = match begin_upload(&root, &mut idem, &replace, fresh(99, 30), &mut ws) {
        UploadBeginOutcome::Started(txn) => txn,
        outcome => panic!(
            "replace begin: {outcome:?}",
            outcome = outcome_name(&outcome)
        ),
    };
    for chunk in v2.chunks(1500) {
        upload_chunk(&root, &mut txn, chunk).expect("chunk");
    }
    finish_upload(&root, &mut idem, txn, &mut ws, || true).expect("replace");

    // Lose every receipt: the superseded metadata is now the only durable
    // record that the create ran.
    let idem_names = layout::idempotency_pair();
    for name in idem_names.pair().names {
        let _ = root.delete_file_in_dir(name);
    }
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");

    let report = run_cleanup(&root, &mut ws).expect("cleanup");
    assert_eq!(report.reclaimed_slots, 0, "the record was reclaimed anyway");
    assert_eq!(report.retained_for_replay, 1);
    // The bytes go; the record stays.
    let slot_file = layout::source_slot_name(create_slot).unwrap();
    assert!(root
        .open_file_in_dir(slot_file.as_str(), Mode::ReadOnly)
        .is_err());
    let pair = layout::metadata_pair(create_slot).unwrap();
    assert!(
        pair.pair()
            .names
            .iter()
            .any(|name| root.open_file_in_dir(*name, Mode::ReadOnly).is_ok()),
        "no metadata record survived to answer the retry"
    );

    // Reboot: the original create replays instead of making a second book.
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let replayed = upload_book(&root, &mut idem, &mut ws, 9, 77, 78, &v1);
    assert_eq!(
        (replayed.logical_book_id, replayed.book_token),
        (created.logical_book_id, created.book_token),
        "the create was executed a second time"
    );

    // Once the epoch retires, a delayed retry is refused as stale, so the
    // evidence is no longer needed and the slot is reclaimed.
    idem.state.rotate_epoch(2).expect("rotate");
    idem.state.rotate_epoch(3).expect("rotate again");
    idem.publish(&root, &mut ws).expect("publish rotations");
    let report = run_cleanup(&root, &mut ws).expect("cleanup");
    assert_eq!(report.reclaimed_slots, 1);
    assert_eq!(report.retained_for_replay, 0);
    for name in pair.pair().names {
        assert!(root.open_file_in_dir(name, Mode::ReadOnly).is_err());
    }
}

fn outcome_name(outcome: &UploadBeginOutcome) -> &'static str {
    match outcome {
        UploadBeginOutcome::Started(_) => "Started",
        UploadBeginOutcome::Replayed(_) => "Replayed",
        UploadBeginOutcome::CatalogUnavailable => "CatalogUnavailable",
        UploadBeginOutcome::Failed(_) => "Failed",
        _ => "Rejected",
    }
}
