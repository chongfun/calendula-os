//! End-to-end tests for the create/replace upload transaction: streaming
//! and identity verification, idempotent replay, rejection paths, and the
//! power-cut invariant that a book is either absent or fully present —
//! never a phantom assembled from staging state.
//!
//! Card model shared from `tests/common/mod.rs`; records live in the card
//! root, as in the other suites.

mod common;

use common::{new_card, open_mgr, open_root, Dir, SharedDisk};
use embedded_sdmmc::Mode;
use source_store::bodies::{DisplayLabel, BOOK_TOKEN_BYTES, LOGICAL_BOOK_ID_BYTES};
use source_store::layout;
use source_store::ops::{
    delete_book, find_authoritative_by_token, load_catalog, DeleteOutcome, DeleteRequest,
    IdempotencyStore, OpsWorkspace,
};
use source_store::recover::{recover_book, RecoveryOutcome, RecoveryRequest};
use source_store::select::SlotDisposition;
use source_store::upload::{
    abort_upload, begin_upload, finish_upload, upload_chunk, FreshIdentity, UploadBeginOutcome,
    UploadError, UploadRequest, UploadResult,
};
use source_store::validate::sha256_of;

fn workspace() -> Box<OpsWorkspace> {
    Box::new(OpsWorkspace::new())
}

fn epub_bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|n| (n as u8).wrapping_mul(31) ^ seed)
        .collect()
}

fn request(epoch: u64, nonce_seed: u8, bytes: &[u8], replace: Option<u8>) -> UploadRequest {
    UploadRequest {
        epoch,
        nonce: [nonce_seed; 16],
        declared_length: bytes.len() as u64,
        declared_sha256: sha256_of(bytes),
        display_label: DisplayLabel::new(b"Uploaded Book").unwrap(),
        replace_token: replace.map(|seed| [seed; BOOK_TOKEN_BYTES]),
    }
}

fn fresh(id_seed: u8, token_seed: u8) -> FreshIdentity {
    FreshIdentity {
        logical_book_id: [id_seed; LOGICAL_BOOK_ID_BYTES],
        book_token: [token_seed; BOOK_TOKEN_BYTES],
    }
}

/// Drive one whole upload: begin, stream in 1000-byte chunks, finish.
fn run_upload(
    root: &Dir<'_>,
    idem: &mut IdempotencyStore,
    ws: &mut OpsWorkspace,
    req: &UploadRequest,
    identity: FreshIdentity,
    bytes: &[u8],
) -> Result<UploadResult, UploadError> {
    let mut txn = match begin_upload(root, idem, req, identity, ws) {
        UploadBeginOutcome::Started(txn) => txn,
        UploadBeginOutcome::Replayed(result) => return Ok(result),
        outcome => panic!("begin did not start: {}", begin_name(&outcome)),
    };
    for chunk in bytes.chunks(1000) {
        upload_chunk(root, &mut txn, chunk)?;
    }
    finish_upload(root, idem, txn, ws, || true)
}

fn begin_name(outcome: &UploadBeginOutcome) -> &'static str {
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

#[test]
fn create_streams_commits_and_serves_the_book() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let bytes = epub_bytes(10_000, 3);
    let req = request(1, 9, &bytes, None);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let result = run_upload(&root, &mut idem, &mut ws, &req, fresh(5, 10), &bytes).expect("upload");
    assert_eq!(
        result,
        UploadResult {
            logical_book_id: [5; LOGICAL_BOOK_ID_BYTES],
            book_token: [10; BOOK_TOKEN_BYTES],
            source_generation: 1,
            receipt_durable: true,
        }
    );

    // The catalog serves it, and the slot file holds the exact bytes.
    let entry = find_authoritative_by_token(&ws, &[10; BOOK_TOKEN_BYTES]).expect("visible");
    assert_eq!(entry.metadata.source_length, bytes.len() as u64);
    let name = layout::source_slot_name(entry.physical_slot).unwrap();
    let file = root
        .open_file_in_dir(name.as_str(), Mode::ReadOnly)
        .expect("slot file");
    let mut persisted = vec![0u8; bytes.len()];
    let mut at = 0;
    while at < persisted.len() {
        let n = file.read(&mut persisted[at..]).expect("read");
        assert!(n > 0);
        at += n;
    }
    file.close().expect("close");
    assert_eq!(persisted, bytes);

    // The marker is gone once the transaction committed.
    let marker_names = layout::marker_pair();
    for name in marker_names.pair().names {
        assert!(root.open_file_in_dir(name, Mode::ReadOnly).is_err());
    }

    // The minted token is live: it can delete the book.
    let outcome = delete_book(
        &root,
        &mut idem,
        &DeleteRequest {
            epoch: 1,
            nonce: [77; 16],
            book_token: [10; BOOK_TOKEN_BYTES],
        },
        &mut ws,
    );
    assert!(matches!(outcome, DeleteOutcome::Deleted { .. }));
}

#[test]
fn create_replays_from_receipt_and_from_metadata() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let bytes = epub_bytes(5_000, 3);
    let req = request(1, 9, &bytes, None);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let first = run_upload(&root, &mut idem, &mut ws, &req, fresh(5, 10), &bytes).expect("upload");

    // Replay via receipt: different fresh identity must be ignored.
    let replay = run_upload(&root, &mut idem, &mut ws, &req, fresh(6, 11), &bytes).expect("replay");
    assert_eq!(replay, first);
    assert_eq!(visible_tokens(&disk), vec![[10; BOOK_TOKEN_BYTES]]);

    // Replay via committed metadata when the receipt store is lost.
    let idem_names = layout::idempotency_pair();
    for name in idem_names.pair().names {
        let _ = root.delete_file_in_dir(name);
    }
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("fresh idem");
    let replay = run_upload(&root, &mut idem, &mut ws, &req, fresh(7, 12), &bytes).expect("replay");
    assert_eq!(
        (
            replay.logical_book_id,
            replay.book_token,
            replay.source_generation
        ),
        (
            first.logical_book_id,
            first.book_token,
            first.source_generation
        )
    );
    assert!(!replay.receipt_durable);

    // Same request ID with different declared bytes is misuse.
    let other = epub_bytes(5_000, 4);
    let mismatched = request(1, 9, &other, None);
    assert!(matches!(
        begin_upload(&root, &mut idem, &mismatched, fresh(8, 13), &mut ws),
        UploadBeginOutcome::RejectedParameterMismatch
    ));
}

/// A catalog that failed to load is not an empty catalog. The difference is
/// destructive: an empty catalog reports every slot free, and creating into
/// a "free" slot truncates whatever EPUB is there.
#[test]
fn a_failed_catalog_load_blocks_operations_instead_of_emptying_the_catalog() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let bytes = epub_bytes(5_000, 3);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let created = run_upload(
        &root,
        &mut idem,
        &mut ws,
        &request(1, 9, &bytes, None),
        fresh(5, 10),
        &bytes,
    )
    .expect("upload");
    let slot_name = layout::source_slot_name(
        find_authoritative_by_token(&ws, &created.book_token)
            .expect("visible")
            .physical_slot,
    )
    .unwrap();

    // A workspace nobody has loaded is not usable, even though its entries
    // array is all-`None`.
    let mut unloaded = workspace();
    assert!(!unloaded.catalog_is_valid());
    assert!(matches!(
        begin_upload(
            &root,
            &mut idem,
            &request(1, 30, &bytes, None),
            fresh(6, 20),
            &mut unloaded
        ),
        UploadBeginOutcome::CatalogUnavailable
    ));

    // Nor is one whose load failed part-way through.
    let mut failed = None;
    for fault in 0..16u32 {
        let mut ws = workspace();
        disk.fault.fail_read_in.set(Some(fault));
        let outcome = load_catalog(&root, &mut ws);
        disk.fault.fail_read_in.set(None);
        if outcome.is_err() {
            failed = Some(ws);
            break;
        }
    }
    let mut ws = failed.expect("a read fault somewhere in the catalog load");
    assert!(!ws.catalog_is_valid());
    assert!(matches!(
        begin_upload(
            &root,
            &mut idem,
            &request(1, 31, &bytes, None),
            fresh(7, 21),
            &mut ws
        ),
        UploadBeginOutcome::CatalogUnavailable
    ));

    // The committed book is untouched, and the catalog recovers on a
    // successful reload.
    let file = root
        .open_file_in_dir(slot_name.as_str(), Mode::ReadOnly)
        .expect("the book's bytes survive");
    assert_eq!(u64::from(file.length()), bytes.len() as u64);
    file.close().expect("close");
    load_catalog(&root, &mut ws).expect("reload");
    assert!(ws.catalog_is_valid());
    assert!(find_authoritative_by_token(&ws, &created.book_token).is_some());
}

/// Once receipts are gone, the committed metadata is the only thing left to
/// judge a retry by — so it must bind the *whole* original request, not the
/// handful of parameters that happen to be metadata fields. Every case here
/// agrees with the original on length, digest, and label, and differs only
/// in something metadata does not otherwise record.
#[test]
fn metadata_replay_demands_the_whole_original_request() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let bytes = epub_bytes(5_000, 3);
    let create = request(1, 9, &bytes, None);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let first =
        run_upload(&root, &mut idem, &mut ws, &create, fresh(5, 10), &bytes).expect("create");
    // A second book with byte-for-byte identical content, to stand in as a
    // wrong replace target below.
    let decoy = request(1, 40, &bytes, None);
    run_upload(&root, &mut idem, &mut ws, &decoy, fresh(6, 20), &bytes).expect("decoy");

    // Lose the receipts.
    let idem_names = layout::idempotency_pair();
    for name in idem_names.pair().names {
        let _ = root.delete_file_in_dir(name);
    }
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("fresh idem");

    // The original request still replays: the positive control.
    let replay =
        run_upload(&root, &mut idem, &mut ws, &create, fresh(7, 12), &bytes).expect("replay");
    assert_eq!(replay.book_token, first.book_token);

    // The same request ID reused as a *replace* — of the book it created,
    // or of the identical-bytes decoy. Neither is the request that
    // committed, and neither may be answered with its result.
    for target in [10u8, 20u8] {
        let as_replace = request(1, 9, &bytes, Some(target));
        assert!(
            matches!(
                begin_upload(&root, &mut idem, &as_replace, fresh(8, 13), &mut ws),
                UploadBeginOutcome::RejectedParameterMismatch
            ),
            "create replayed as a replace of {target}"
        );
    }

    // And reused by a different operation family entirely: a recovery
    // carrying the same epoch and nonce collides on request ID alone.
    let collision = RecoveryRequest {
        epoch: 1,
        nonce: [9; 16],
        book_token: [10; BOOK_TOKEN_BYTES],
        observed_length: bytes.len() as u64,
        observed_sha256: sha256_of(&bytes),
        display_label: None,
    };
    assert_eq!(
        recover_book(
            &root,
            &mut idem,
            &collision,
            [60; BOOK_TOKEN_BYTES],
            &mut ws,
            || true
        ),
        RecoveryOutcome::RejectedParameterMismatch
    );
}

#[test]
fn replace_supersedes_and_stales_the_old_token() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let v1 = epub_bytes(6_000, 1);
    let v2 = epub_bytes(7_000, 2);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let create = request(1, 9, &v1, None);
    run_upload(&root, &mut idem, &mut ws, &create, fresh(5, 10), &v1).expect("create");

    let replace = request(1, 20, &v2, Some(10));
    let result =
        run_upload(&root, &mut idem, &mut ws, &replace, fresh(99, 30), &v2).expect("replace");
    assert_eq!(
        (
            result.logical_book_id,
            result.book_token,
            result.source_generation
        ),
        ([5; LOGICAL_BOOK_ID_BYTES], [30; BOOK_TOKEN_BYTES], 2),
        "replace keeps the book id, mints a token, bumps the generation"
    );

    // Exactly one authoritative token — the new one.
    assert_eq!(visible_tokens(&disk), vec![[30; BOOK_TOKEN_BYTES]]);

    // The old token no longer replaces; the replay of the replace does.
    let stale = request(1, 40, &v1, Some(10));
    assert!(matches!(
        begin_upload(&root, &mut idem, &stale, fresh(50, 51), &mut ws),
        UploadBeginOutcome::RejectedUnknownToken
    ));
    let replayed =
        run_upload(&root, &mut idem, &mut ws, &replace, fresh(0, 0), &v2).expect("replay");
    assert_eq!(replayed.book_token, [30; BOOK_TOKEN_BYTES]);
}

#[test]
fn digest_mismatch_rejects_and_the_same_id_can_retry() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let bytes = epub_bytes(4_000, 3);
    let req = request(1, 9, &bytes, None);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");

    // Stream the wrong bytes under the right declaration.
    let wrong = epub_bytes(4_000, 4);
    let mut txn = match begin_upload(&root, &mut idem, &req, fresh(5, 10), &mut ws) {
        UploadBeginOutcome::Started(txn) => txn,
        outcome => panic!("begin: {}", begin_name(&outcome)),
    };
    for chunk in wrong.chunks(1000) {
        upload_chunk(&root, &mut txn, chunk).expect("chunk");
    }
    assert_eq!(
        finish_upload(&root, &mut idem, txn, &mut ws, || true),
        Err(UploadError::DigestMismatch)
    );
    assert!(visible_tokens(&disk).is_empty(), "nothing was adopted");

    // A failed operation is not receipted: the same request ID retries
    // fresh — and succeeds with the right bytes.
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let result = run_upload(&root, &mut idem, &mut ws, &req, fresh(5, 10), &bytes).expect("retry");
    assert_eq!(result.source_generation, 1);
    assert_eq!(visible_tokens(&disk), vec![[10; BOOK_TOKEN_BYTES]]);
}

#[test]
fn length_and_container_gates_reject() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let bytes = epub_bytes(4_000, 3);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");

    // Early EOF: fewer bytes than declared.
    let req = request(1, 9, &bytes, None);
    let mut txn = match begin_upload(&root, &mut idem, &req, fresh(5, 10), &mut ws) {
        UploadBeginOutcome::Started(txn) => txn,
        outcome => panic!("begin: {}", begin_name(&outcome)),
    };
    upload_chunk(&root, &mut txn, &bytes[..1000]).expect("chunk");
    assert_eq!(
        finish_upload(&root, &mut idem, txn, &mut ws, || true),
        Err(UploadError::LengthMismatch)
    );

    // Overrun: more bytes than declared, rejected at the chunk.
    let mut txn = match begin_upload(&root, &mut idem, &req, fresh(5, 10), &mut ws) {
        UploadBeginOutcome::Started(txn) => txn,
        outcome => panic!("begin: {}", begin_name(&outcome)),
    };
    for chunk in bytes.chunks(1000) {
        upload_chunk(&root, &mut txn, chunk).expect("chunk");
    }
    assert_eq!(
        upload_chunk(&root, &mut txn, &[0u8; 1]),
        Err(UploadError::LengthMismatch)
    );
    abort_upload(&root, txn);

    // Container gate refusal, after a byte-perfect stream.
    let mut txn = match begin_upload(&root, &mut idem, &req, fresh(5, 10), &mut ws) {
        UploadBeginOutcome::Started(txn) => txn,
        outcome => panic!("begin: {}", begin_name(&outcome)),
    };
    for chunk in bytes.chunks(1000) {
        upload_chunk(&root, &mut txn, chunk).expect("chunk");
    }
    assert_eq!(
        finish_upload(&root, &mut idem, txn, &mut ws, || false),
        Err(UploadError::UnsupportedContainer)
    );
    assert!(visible_tokens(&disk).is_empty());
}

#[test]
fn collisions_and_stale_epochs_reject_at_begin() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let bytes = epub_bytes(2_000, 3);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let req = request(1, 9, &bytes, None);
    run_upload(&root, &mut idem, &mut ws, &req, fresh(5, 10), &bytes).expect("create");

    // Colliding book id, then colliding token.
    let second = request(1, 20, &bytes, None);
    assert!(matches!(
        begin_upload(&root, &mut idem, &second, fresh(5, 40), &mut ws),
        UploadBeginOutcome::RejectedIdentityCollision
    ));
    assert!(matches!(
        begin_upload(&root, &mut idem, &second, fresh(41, 10), &mut ws),
        UploadBeginOutcome::RejectedIdentityCollision
    ));

    // A deleted book's identity stays burned: its id and token still
    // collide via the tombstone after deletion hides the metadata.
    let delete = DeleteRequest {
        epoch: 1,
        nonce: [70; 16],
        book_token: [10; BOOK_TOKEN_BYTES],
    };
    assert!(matches!(
        delete_book(&root, &mut idem, &delete, &mut ws),
        DeleteOutcome::Deleted { .. }
    ));
    assert!(matches!(
        begin_upload(&root, &mut idem, &second, fresh(5, 40), &mut ws),
        UploadBeginOutcome::RejectedIdentityCollision
    ));

    // Stale epoch after rotation.
    idem.state.rotate_epoch(2).expect("rotate");
    idem.publish(&root, &mut ws).expect("publish rotation");
    assert!(matches!(
        begin_upload(&root, &mut idem, &second, fresh(60, 61), &mut ws),
        UploadBeginOutcome::RejectedStaleEpoch
    ));
}

#[test]
fn power_cut_during_create_yields_absent_or_present_and_retry_converges() {
    let disk = new_card();
    let bytes = epub_bytes(6_000, 3);
    let req = request(1, 9, &bytes, None);

    let base = disk.snapshot();
    disk.start_logging();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let mut ws = workspace();
        load_catalog(&root, &mut ws).expect("catalog");
        let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
        run_upload(&root, &mut idem, &mut ws, &req, fresh(5, 10), &bytes).expect("upload");
    }
    let log = disk.take_log();
    assert!(log.len() > 10, "upload should write many blocks");

    // Every fourth boundary keeps the suite fast; the commit-adjacent
    // boundaries all fall inside the stride's coverage across runs, and
    // torn variants cover intra-sector cuts.
    for cut in (0..=log.len()).step_by(4) {
        for torn in [0, 257] {
            disk.restore_cut(&base, &log, cut, torn);
            let visible = visible_tokens(&disk);
            assert!(
                visible.is_empty() || visible == vec![[10; BOOK_TOKEN_BYTES]],
                "cut {cut}+{torn}: phantom catalog {visible:?}"
            );

            // Retry the identical request: replay or fresh execution, the
            // end state is exactly one committed copy of the book.
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            let mut ws = workspace();
            load_catalog(&root, &mut ws).expect("catalog after cut");
            let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem after cut");
            let result = run_upload(&root, &mut idem, &mut ws, &req, fresh(5, 10), &bytes)
                .unwrap_or_else(|error| panic!("cut {cut}+{torn}: retry failed: {error:?}"));
            assert_eq!(result.book_token, [10; BOOK_TOKEN_BYTES]);
            assert_eq!(result.source_generation, 1);
            drop(root);
            drop(mgr);
            assert_eq!(
                visible_tokens(&disk),
                vec![[10; BOOK_TOKEN_BYTES]],
                "cut {cut}+{torn}: retry did not converge"
            );
        }
    }
}

#[test]
fn abandoned_transaction_is_superseded_by_the_next() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let bytes = epub_bytes(5_000, 3);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");

    // Stream half an upload and walk away (no finish, no abort) — the
    // crash-shaped abandonment.
    let first = request(1, 9, &bytes, None);
    let mut txn = match begin_upload(&root, &mut idem, &first, fresh(5, 10), &mut ws) {
        UploadBeginOutcome::Started(txn) => txn,
        outcome => panic!("begin: {}", begin_name(&outcome)),
    };
    upload_chunk(&root, &mut txn, &bytes[..2000]).expect("chunk");
    drop(txn);
    assert!(visible_tokens(&disk).is_empty());

    // A different upload starts and completes; the abandoned candidate
    // never surfaces.
    let other = epub_bytes(3_000, 8);
    let second = request(1, 30, &other, None);
    let result =
        run_upload(&root, &mut idem, &mut ws, &second, fresh(6, 40), &other).expect("second");
    assert_eq!(result.source_generation, 1);
    assert_eq!(visible_tokens(&disk), vec![[40; BOOK_TOKEN_BYTES]]);

    // And the abandoned request itself can still run afterwards.
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let result = run_upload(&root, &mut idem, &mut ws, &first, fresh(5, 10), &bytes).expect("redo");
    assert_eq!(result.book_token, [10; BOOK_TOKEN_BYTES]);
    let mut tokens = visible_tokens(&disk);
    tokens.sort();
    assert_eq!(tokens, vec![[10; BOOK_TOKEN_BYTES], [40; BOOK_TOKEN_BYTES]]);
}
