//! End-to-end tests for the delete transaction: idempotency lookup order,
//! replay from receipts and from tombstones, epoch discipline, and the
//! power-cut invariant that a book is hidden exactly when its tombstone
//! committed.
//!
//! Card model shared from `tests/common/mod.rs`. Records live in the card
//! root rather than under `XTEINK/SRC` — the ops layer takes a directory
//! handle and never assumes where it is mounted, so the tests skip the
//! directory dance.

mod common;

use common::{new_card, open_mgr, open_root, publish_metadata, sample_metadata, Dir, SharedDisk};
use source_store::bodies::{BOOK_TOKEN_BYTES, LOGICAL_BOOK_ID_BYTES};
use source_store::layout;
use source_store::ops::{
    delete_book, find_authoritative_by_token, load_catalog, DeleteOutcome, DeleteRequest,
    IdempotencyStore, OpsWorkspace,
};
use source_store::receipts::MAX_RECEIPTS_PER_EPOCH;

fn workspace() -> Box<OpsWorkspace> {
    Box::new(OpsWorkspace::new())
}

/// Publish a book into source slot `slot`: metadata with the given book id
/// and token, at source generation `generation`.
fn setup_book(root: &Dir<'_>, slot: u8, book_seed: u8, token_seed: u8, generation: u64) {
    let mut meta = sample_metadata(generation);
    meta.logical_book_id = [book_seed; LOGICAL_BOOK_ID_BYTES];
    meta.book_token = [token_seed; BOOK_TOKEN_BYTES];
    meta.physical_slot = slot;
    let pair = layout::metadata_pair(slot).expect("slot in range");
    publish_metadata(root, pair.pair(), &meta, generation).expect("publish book");
}

fn request(epoch: u64, nonce_seed: u8, token_seed: u8) -> DeleteRequest {
    DeleteRequest {
        epoch,
        nonce: [nonce_seed; 16],
        book_token: [token_seed; BOOK_TOKEN_BYTES],
    }
}

/// The reboot view: fresh mount, fresh catalog, fresh idempotency load.
fn visible_tokens(disk: &SharedDisk) -> Vec<[u8; BOOK_TOKEN_BYTES]> {
    let mgr = open_mgr(disk);
    let root = open_root(&mgr);
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog loads");
    let mut tokens = Vec::new();
    for (slot, entry) in ws.entries.iter().enumerate() {
        if let Some(entry) = entry {
            if ws.dispositions[slot] == source_store::select::SlotDisposition::Authoritative {
                tokens.push(entry.metadata.book_token);
            }
        }
    }
    tokens
}

#[test]
fn delete_commits_hides_and_replays() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    setup_book(&root, 0, 5, 10, 1);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    assert!(find_authoritative_by_token(&ws, &[10; BOOK_TOKEN_BYTES]).is_some());

    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let req = request(1, 9, 10);
    let outcome = delete_book(&root, &mut idem, &req, &mut ws);
    assert_eq!(
        outcome,
        DeleteOutcome::Deleted {
            logical_book_id: [5; LOGICAL_BOOK_ID_BYTES],
            replayed: false,
            receipt_durable: true,
        }
    );
    // The workspace view already hides the book.
    assert!(find_authoritative_by_token(&ws, &[10; BOOK_TOKEN_BYTES]).is_none());
    drop(root);
    drop(mgr);

    // Reboot view: book hidden; replay returns the original result.
    assert!(visible_tokens(&disk).is_empty());
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem reload");
    let outcome = delete_book(&root, &mut idem, &req, &mut ws);
    assert_eq!(
        outcome,
        DeleteOutcome::Deleted {
            logical_book_id: [5; LOGICAL_BOOK_ID_BYTES],
            replayed: true,
            receipt_durable: true,
        }
    );

    // A *different* request against the now-deleted token is unknown.
    let fresh = request(1, 11, 10);
    assert_eq!(
        delete_book(&root, &mut idem, &fresh, &mut ws),
        DeleteOutcome::RejectedUnknownToken
    );
}

#[test]
fn replay_resolves_from_tombstone_when_receipt_lost() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    setup_book(&root, 0, 5, 10, 1);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let req = request(1, 9, 10);
    assert!(matches!(
        delete_book(&root, &mut idem, &req, &mut ws),
        DeleteOutcome::Deleted { .. }
    ));

    // Simulate the crash window between tombstone commit and receipt
    // publication: the idempotency record is simply gone.
    let idem_names = layout::idempotency_pair();
    for name in idem_names.pair().names {
        let _ = root.delete_file_in_dir(name);
    }

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("fresh idem");
    assert_eq!(idem.record_generation, 0, "receipt state really is gone");
    let outcome = delete_book(&root, &mut idem, &req, &mut ws);
    assert_eq!(
        outcome,
        DeleteOutcome::Deleted {
            logical_book_id: [5; LOGICAL_BOOK_ID_BYTES],
            replayed: true,
            receipt_durable: false,
        }
    );
}

#[test]
fn reused_request_id_with_different_token_is_rejected() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    setup_book(&root, 5, 5, 10, 1);
    setup_book(&root, 6, 6, 20, 1);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let original = request(1, 9, 10);
    assert!(matches!(
        delete_book(&root, &mut idem, &original, &mut ws),
        DeleteOutcome::Deleted { .. }
    ));

    // Same (epoch, nonce), different token: rejected via the receipt.
    let reused = request(1, 9, 20);
    assert_eq!(
        delete_book(&root, &mut idem, &reused, &mut ws),
        DeleteOutcome::RejectedParameterMismatch
    );

    // And rejected via the tombstone when the receipt is gone.
    let idem_names = layout::idempotency_pair();
    for name in idem_names.pair().names {
        let _ = root.delete_file_in_dir(name);
    }
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    assert_eq!(
        delete_book(&root, &mut idem, &reused, &mut ws),
        DeleteOutcome::RejectedParameterMismatch
    );
    // Book 20 was never deleted by any of this.
    assert!(find_authoritative_by_token(&ws, &[20; BOOK_TOKEN_BYTES]).is_some());
}

#[test]
fn stale_epoch_rejects_new_requests_but_replays_known_ones() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    setup_book(&root, 0, 5, 10, 1);
    setup_book(&root, 1, 6, 20, 1);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let original = request(1, 9, 10);
    assert!(matches!(
        delete_book(&root, &mut idem, &original, &mut ws),
        DeleteOutcome::Deleted { .. }
    ));

    // Rotate to epoch 2 and persist.
    idem.state.rotate_epoch(2).expect("rotate");
    idem.publish(&root, &mut ws).expect("publish rotation");

    // A genuinely new epoch-1 request is stale...
    let stale = request(1, 30, 20);
    assert_eq!(
        delete_book(&root, &mut idem, &stale, &mut ws),
        DeleteOutcome::RejectedStaleEpoch
    );
    assert!(find_authoritative_by_token(&ws, &[20; BOOK_TOKEN_BYTES]).is_some());

    // ...but the known epoch-1 request still replays.
    assert!(matches!(
        delete_book(&root, &mut idem, &original, &mut ws),
        DeleteOutcome::Deleted { replayed: true, .. }
    ));
}

#[test]
fn unknown_and_superseded_tokens_are_rejected() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    // One logical book across two generations: slot 0 holds superseded
    // generation 1 (token 10), slot 1 the authoritative generation 2
    // (token 20) — the state a completed replace leaves before cleanup.
    setup_book(&root, 0, 5, 10, 1);
    setup_book(&root, 1, 5, 20, 2);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");

    assert_eq!(
        delete_book(&root, &mut idem, &request(1, 9, 10), &mut ws),
        DeleteOutcome::RejectedUnknownToken,
        "a superseded generation's token is not a handle to the book"
    );
    assert_eq!(
        delete_book(&root, &mut idem, &request(1, 9, 99), &mut ws),
        DeleteOutcome::RejectedUnknownToken
    );
    assert!(find_authoritative_by_token(&ws, &[20; BOOK_TOKEN_BYTES]).is_some());
}

#[test]
fn epoch_exhaustion_rejects_before_any_commit() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    setup_book(&root, 0, 5, 10, 1);
    setup_book(&root, 1, 6, 20, 1);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");

    // One real delete, then synthetic receipts for the rest of the epoch
    // budget — filling the epoch without consuming tombstone slots, so
    // this test isolates the *epoch* limit from the tombstone limit.
    assert!(matches!(
        delete_book(&root, &mut idem, &request(1, 9, 10), &mut ws),
        DeleteOutcome::Deleted { .. }
    ));
    let mut filler = source_store::receipts::OperationReceipt {
        epoch: 1,
        request_nonce: [0; 16],
        operation: source_store::receipts::ReceiptOperation::Create,
        logical_book_id: [7; LOGICAL_BOOK_ID_BYTES],
        base_book_token_or_zero: [0; BOOK_TOKEN_BYTES],
        source_generation: 1,
        source_length_or_zero: 0,
        source_sha256_or_zero: [0; 32],
        display_label_len: 0,
        display_label: [0; 64],
        result_book_token_or_zero: [0; BOOK_TOKEN_BYTES],
        result_status: source_store::receipts::RECEIPT_STATUS_SUCCESS,
    };
    for n in 0..MAX_RECEIPTS_PER_EPOCH as u8 - 1 {
        filler.request_nonce = [50 + n; 16];
        idem.state.insert(filler).expect("filler receipt");
    }
    idem.publish(&root, &mut ws).expect("publish filled state");

    let exhausted = delete_book(&root, &mut idem, &request(1, 200, 20), &mut ws);
    assert_eq!(exhausted, DeleteOutcome::RejectedEpochExhausted);
    // The rejected book is untouched, and rotation unblocks it.
    assert!(find_authoritative_by_token(&ws, &[20; BOOK_TOKEN_BYTES]).is_some());
    idem.state.rotate_epoch(2).expect("rotate");
    idem.publish(&root, &mut ws).expect("publish rotation");
    assert!(matches!(
        delete_book(&root, &mut idem, &request(2, 200, 20), &mut ws),
        DeleteOutcome::Deleted { .. }
    ));
}

#[test]
fn tombstone_slot_exhaustion_rejects_cleanly() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    // MAX_TOMBSTONE_SLOTS deletions stand uncleaned; one more must be
    // refused without touching the book. Spread across two epochs to stay
    // inside the per-epoch budget.
    let total = layout::MAX_TOMBSTONE_SLOTS as u8 + 1;
    for n in 0..total {
        setup_book(&root, n, 100 + n, 100 + n, 1);
    }

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let mut epoch = 1u64;
    for n in 0..layout::MAX_TOMBSTONE_SLOTS as u8 {
        if !idem.state.has_epoch_headroom() {
            epoch += 1;
            idem.state.rotate_epoch(epoch).expect("rotate");
            idem.publish(&root, &mut ws).expect("publish rotation");
        }
        assert!(matches!(
            delete_book(&root, &mut idem, &request(epoch, n, 100 + n), &mut ws),
            DeleteOutcome::Deleted { .. }
        ));
    }

    // A fresh epoch, so the headroom check cannot mask the tombstone
    // limit this test is about.
    epoch += 1;
    idem.state.rotate_epoch(epoch).expect("final rotate");
    idem.publish(&root, &mut ws)
        .expect("publish final rotation");
    let last = 100 + layout::MAX_TOMBSTONE_SLOTS as u8;
    assert_eq!(
        delete_book(&root, &mut idem, &request(epoch, 200, last), &mut ws),
        DeleteOutcome::RejectedNoTombstoneSlot
    );
    assert!(find_authoritative_by_token(&ws, &[last; BOOK_TOKEN_BYTES]).is_some());
}

#[test]
fn power_cut_during_delete_hides_book_exactly_when_tombstone_committed() {
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        setup_book(&root, 0, 5, 10, 1);
    }
    let base = disk.snapshot();
    disk.start_logging();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let mut ws = workspace();
        load_catalog(&root, &mut ws).expect("catalog");
        let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
        assert!(matches!(
            delete_book(&root, &mut idem, &request(1, 9, 10), &mut ws),
            DeleteOutcome::Deleted { .. }
        ));
    }
    let log = disk.take_log();
    assert!(log.len() > 4, "delete should write several blocks");

    for cut in 0..=log.len() {
        for torn in [0, 1, 128, 511] {
            disk.restore_cut(&base, &log, cut, torn);
            // The reboot invariant: the book is visible exactly until the
            // tombstone's commit sector lands, hidden exactly after —
            // load_catalog itself asserts a prepared tombstone never hides
            // (it only decodes committed records).
            let visible = visible_tokens(&disk);
            let book_visible = visible.contains(&[10; BOOK_TOKEN_BYTES]);
            assert!(
                visible.len() <= 1,
                "cut {cut}+{torn}: phantom authority appeared"
            );

            // Whatever state the cut left, the retry converges: replay or
            // fresh execution, the book ends hidden.
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            let mut ws = workspace();
            load_catalog(&root, &mut ws).expect("catalog after cut");
            let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem after cut");
            match delete_book(&root, &mut idem, &request(1, 9, 10), &mut ws) {
                DeleteOutcome::Deleted { replayed, .. } => {
                    // A cut before the commit point leaves the book
                    // visible, so the retry executes fresh; after the
                    // commit point it must replay, not double-delete.
                    assert_eq!(
                        replayed, !book_visible,
                        "cut {cut}+{torn}: replay disagrees with visibility"
                    );
                }
                outcome => panic!("cut {cut}+{torn}: retry failed: {outcome:?}"),
            }
            drop(root);
            drop(mgr);
            assert!(
                visible_tokens(&disk).is_empty(),
                "cut {cut}+{torn}: retry did not hide the book"
            );
        }
    }
}

/// `publish_record` can fail *after* its commit sync — the record lands, the
/// call still reports failure. The idempotency store must not go on
/// believing the older generation is authority: proposing that same
/// generation again is refused as not-above-authority, and the store would
/// never persist another receipt for the rest of the session.
#[test]
fn an_uncertain_idempotency_commit_does_not_wedge_the_store() {
    let disk = new_card();
    let base = disk.snapshot();
    let mut commit_then_fail_seen = 0u32;

    // Sweep read faults for ones that land after the commit sync, in step
    // 8's verification read: exactly the "committed, but reported failure"
    // outcome. Faults that land earlier are the ordinary pre-commit failures
    // the other tests cover, and are skipped here.
    for fault in 0..400u32 {
        disk.restore_cut(&base, &[], 0, 0);
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let mut ws = workspace();
        let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
        let before = idem.record_generation;
        idem.state.insert(receipt(1, 50)).expect("insert");

        disk.fault.fail_read_in.set(Some(fault));
        let outcome = idem.publish(&root, &mut ws);
        let fired = disk.fault.fail_read_in.get().is_none();
        disk.fault.fail_read_in.set(None);
        if !fired {
            break;
        }
        if outcome.is_ok() {
            continue;
        }
        let committed = committed_generation(&root, &mut ws);
        if committed <= before {
            continue;
        }
        commit_then_fail_seen += 1;

        // The store has to have learned what is actually on the card.
        assert_eq!(
            idem.record_generation, committed,
            "fault {fault}: the store did not adopt the committed generation"
        );
        // And it has to be able to keep going: a second receipt must reach
        // the card rather than being refused forever.
        idem.state.insert(receipt(1, 51)).expect("second insert");
        idem.publish(&root, &mut ws)
            .unwrap_or_else(|error| panic!("fault {fault}: store wedged: {error:?}"));
        let reloaded = IdempotencyStore::load(&root, &mut ws).expect("reload");
        assert!(
            reloaded.state.contains_request(1, &[51; 16]),
            "fault {fault}: the recovered publication did not persist"
        );
    }

    assert!(
        commit_then_fail_seen > 0,
        "no read fault produced a commit-then-fail outcome; the test proves nothing"
    );
}

fn committed_generation(root: &Dir<'_>, ws: &mut OpsWorkspace) -> u64 {
    source_store::publish::select_authority(
        root,
        layout::idempotency_pair().pair(),
        &mut ws.record_scratch,
    )
    .expect("read idempotency authority")
    .map(|(_, generation)| generation)
    .unwrap_or(0)
}

/// A minimal valid success receipt for epoch `epoch` under `nonce_seed`.
fn receipt(epoch: u64, nonce_seed: u8) -> source_store::receipts::OperationReceipt {
    use source_store::bodies::{DISPLAY_LABEL_MAX_BYTES, SHA256_BYTES};
    use source_store::receipts::{ReceiptOperation, RECEIPT_STATUS_SUCCESS};
    source_store::receipts::OperationReceipt {
        epoch,
        request_nonce: [nonce_seed; 16],
        operation: ReceiptOperation::Delete,
        logical_book_id: [1; LOGICAL_BOOK_ID_BYTES],
        base_book_token_or_zero: [2; BOOK_TOKEN_BYTES],
        source_generation: 0,
        source_length_or_zero: 0,
        source_sha256_or_zero: [0; SHA256_BYTES],
        display_label_len: 0,
        display_label: [0; DISPLAY_LABEL_MAX_BYTES],
        result_book_token_or_zero: [0; BOOK_TOKEN_BYTES],
        result_status: RECEIPT_STATUS_SUCCESS,
    }
}
