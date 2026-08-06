//! End-to-end tests for the mount-session validation contract and the
//! list operation: the provisional cached open (quick check, bounded
//! background full validation), the quick-check false negative the PRD
//! explicitly trades for reopen speed, quarantine of externally modified
//! managed books, and the list's integrity states and allowed operations.

mod common;

use common::{new_card, open_mgr, open_root, Dir};
use embedded_sdmmc::Mode;
use source_store::bodies::{DisplayLabel, SourceOrigin, BOOK_TOKEN_BYTES, LOGICAL_BOOK_ID_BYTES};
use source_store::layout;
use source_store::list::{list_books, BookListEntry, ListError, SourceIntegrityStatus};
use source_store::ops::{load_catalog, IdempotencyStore, OpsWorkspace};
use source_store::recover::{recover_book, RecoveryOutcome, RecoveryRequest};
use source_store::select::MAX_SOURCE_SLOTS;
use source_store::session::{
    quick_check, FullValidationJob, IntegrityLevel, QuickCheckOutcome, ValidationStep,
};
use source_store::unmanaged::{adopt_or_reidentify, AdoptOutcome};
use source_store::upload::{
    begin_upload, finish_upload, upload_chunk, FreshIdentity, UploadBeginOutcome, UploadRequest,
    UploadResult,
};
use source_store::validate::{quick_regions_v1, sha256_of};

fn workspace() -> Box<OpsWorkspace> {
    Box::new(OpsWorkspace::new())
}

fn epub_bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|n| (n as u8).wrapping_mul(37) ^ seed)
        .collect()
}

fn upload_book(
    root: &Dir<'_>,
    idem: &mut IdempotencyStore,
    ws: &mut OpsWorkspace,
    bytes: &[u8],
    nonce_seed: u8,
    id_seed: u8,
    token_seed: u8,
) -> UploadResult {
    let req = UploadRequest {
        epoch: 1,
        nonce: [nonce_seed; 16],
        declared_length: bytes.len() as u64,
        declared_sha256: sha256_of(bytes),
        display_label: DisplayLabel::new(b"Session Book").unwrap(),
        replace_token: None,
    };
    let identity = FreshIdentity {
        logical_book_id: [id_seed; LOGICAL_BOOK_ID_BYTES],
        book_token: [token_seed; BOOK_TOKEN_BYTES],
    };
    let mut txn = match begin_upload(root, idem, &req, identity, ws) {
        UploadBeginOutcome::Started(txn) => txn,
        _ => panic!("begin did not start"),
    };
    for chunk in bytes.chunks(1000) {
        upload_chunk(root, &mut txn, chunk).expect("chunk");
    }
    finish_upload(root, idem, txn, ws, || true).expect("finish")
}

fn list_all(ws: &OpsWorkspace) -> Vec<BookListEntry> {
    let mut out = [placeholder_entry(); MAX_SOURCE_SLOTS];
    let count = list_books(ws, &mut out).expect("list");
    out[..count].to_vec()
}

fn placeholder_entry() -> BookListEntry {
    BookListEntry {
        display_label: DisplayLabel::placeholder(),
        logical_book_id: [0; LOGICAL_BOOK_ID_BYTES],
        book_token: [0; BOOK_TOKEN_BYTES],
        source_generation: 0,
        source_origin: SourceOrigin::UnmanagedSd,
        externally_recovered: false,
        source_integrity_status: SourceIntegrityStatus::UncheckedThisMount,
        source_length: 0,
        observed_source_length: None,
        observed_source_sha256: None,
        allowed_operations: source_store::list::AllowedOperations {
            replace: false,
            delete: false,
            recover_current_bytes: false,
        },
    }
}

/// Flip one byte of the slot file in place.
fn tamper_slot_byte(root: &Dir<'_>, name: &str, offset: u32) {
    let file = root
        .open_file_in_dir(name, Mode::ReadWriteAppend)
        .expect("open slot file");
    file.seek_from_start(offset).expect("seek for read");
    let mut byte = [0u8; 1];
    assert_eq!(file.read(&mut byte).expect("read byte"), 1);
    file.seek_from_start(offset).expect("seek for write");
    file.write(&[byte[0] ^ 0xFF]).expect("tamper");
    file.close().expect("close");
}

#[test]
fn provisional_open_false_negative_quarantine_and_recovery_round_trip() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    // 50 KB: large enough that the quick-fingerprint regions leave real
    // gaps — the false-negative window this test walks through.
    let bytes = epub_bytes(50_000, 3);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let result = upload_book(&root, &mut idem, &mut ws, &bytes, 9, 5, 10);
    let book = result.logical_book_id;

    // The commit seeded this mount's validation set: the fresh book lists
    // as validated with the healthy operations, no rehash needed.
    let listed = list_all(&ws);
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].source_integrity_status,
        SourceIntegrityStatus::ValidatedThisMount
    );
    assert!(listed[0].allowed_operations.replace);
    assert!(listed[0].allowed_operations.delete);
    assert!(!listed[0].allowed_operations.recover_current_bytes);
    assert_eq!(listed[0].observed_source_length, None);

    // A remount forgets the proof; nothing is checked yet.
    ws.session.reset();
    assert_eq!(
        list_all(&ws)[0].source_integrity_status,
        SourceIntegrityStatus::UncheckedThisMount
    );

    // Tamper one byte *outside* every quick-fingerprint region, keeping
    // the length. The quick check must pass anyway — that is the PRD's
    // explicit trade — and full validation must be what catches it.
    let outside = 10_000u32;
    let regions = quick_regions_v1(bytes.len() as u64);
    assert!(
        regions
            .iter()
            .all(|(offset, len)| u64::from(outside) < *offset
                || u64::from(outside) >= offset + len),
        "test byte must sit outside every quick region"
    );
    let slot_name = layout::source_slot_name(0).unwrap();
    tamper_slot_byte(&root, slot_name.as_str(), outside);

    let entry = ws.entries[0].expect("slot 0 holds the book");
    assert_eq!(
        quick_check(&root, slot_name.as_str(), &entry, &mut ws.session),
        QuickCheckOutcome::Match,
        "a change outside the quick regions must pass the quick check"
    );
    let level = ws.session.level(&book, 1);
    assert_eq!(level, IntegrityLevel::QuickChecked);
    assert!(
        level.may_display_cached(),
        "provisional display is the point"
    );
    assert!(
        !level.may_use_source(),
        "and it must never authorize source use"
    );
    // The client-visible status stays unchecked until the full pass.
    assert_eq!(
        list_all(&ws)[0].source_integrity_status,
        SourceIntegrityStatus::UncheckedThisMount
    );

    // Background full validation, in bounded steps. 8 KB per step over
    // 50 KB must take several turns — the job is genuinely resumable.
    let mut job = FullValidationJob::new(&entry);
    let mut scratch = [0u8; 2048];
    let mut steps = 0usize;
    let concluded = loop {
        steps += 1;
        assert!(steps < 100, "validation failed to conclude");
        match job.step(
            &root,
            slot_name.as_str(),
            &mut ws.session,
            8192,
            &mut scratch,
        ) {
            ValidationStep::Pending => {}
            ValidationStep::Concluded(level) => break level,
        }
    };
    assert!(steps >= 6, "expected a multi-step run, got {steps}");
    assert_eq!(concluded, IntegrityLevel::Mismatch);

    // Quarantine: display closed, observed identity recorded and listed,
    // recovery and delete offered, replacement withdrawn.
    assert!(!ws.session.level(&book, 1).may_display_cached());
    let mut tampered = bytes.clone();
    tampered[outside as usize] ^= 0xFF;
    let observed_sha = sha256_of(&tampered);
    assert_eq!(
        ws.session.observed_identity(&book, 1),
        Some((bytes.len() as u64, observed_sha))
    );
    let listed = list_all(&ws);
    assert_eq!(
        listed[0].source_integrity_status,
        SourceIntegrityStatus::ExternallyModified
    );
    assert_eq!(listed[0].observed_source_length, Some(bytes.len() as u64));
    assert_eq!(listed[0].observed_source_sha256, Some(observed_sha));
    assert!(!listed[0].allowed_operations.replace);
    assert!(listed[0].allowed_operations.delete);
    assert!(listed[0].allowed_operations.recover_current_bytes);

    // Ordinary replacement over the broken chain is refused outright.
    let replace_req = UploadRequest {
        epoch: 1,
        nonce: [21; 16],
        declared_length: 4,
        declared_sha256: sha256_of(b"nope"),
        display_label: DisplayLabel::new(b"Replacement").unwrap(),
        replace_token: Some(result.book_token),
    };
    let identity = FreshIdentity {
        logical_book_id: [6; LOGICAL_BOOK_ID_BYTES],
        book_token: [22; BOOK_TOKEN_BYTES],
    };
    assert!(matches!(
        begin_upload(&root, &mut idem, &replace_req, identity, &mut ws),
        UploadBeginOutcome::RejectedExternallyModified
    ));

    // Explicit recovery with the exact observed identity adopts the bytes
    // as generation 2 and re-seeds the validation set.
    let recovery = RecoveryRequest {
        epoch: 1,
        nonce: [23; 16],
        book_token: result.book_token,
        observed_length: bytes.len() as u64,
        observed_sha256: observed_sha,
        display_label: None,
    };
    let outcome = recover_book(
        &root,
        &mut idem,
        &recovery,
        [30; BOOK_TOKEN_BYTES],
        &mut ws,
        || true,
    );
    let RecoveryOutcome::Recovered(recovered) = outcome else {
        panic!("recovery failed: {outcome:?}");
    };
    assert_eq!(recovered.source_generation, 2);
    let listed = list_all(&ws);
    assert_eq!(listed.len(), 1, "only the authoritative generation lists");
    assert_eq!(listed[0].source_generation, 2);
    assert!(listed[0].externally_recovered);
    assert_eq!(
        listed[0].source_integrity_status,
        SourceIntegrityStatus::ValidatedThisMount
    );
    assert!(listed[0].allowed_operations.replace);
    assert_eq!(listed[0].observed_source_length, None);
}

#[test]
fn quick_check_closes_on_length_change_and_concludes_absence() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let bytes = epub_bytes(20_000, 4);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let result = upload_book(&root, &mut idem, &mut ws, &bytes, 9, 5, 10);
    let book = result.logical_book_id;
    ws.session.reset();

    // Grow the file by one byte: the quick check closes without concluding
    // anything — a length change is not yet a mismatch verdict.
    let slot_name = layout::source_slot_name(0).unwrap();
    {
        let file = root
            .open_file_in_dir(slot_name.as_str(), Mode::ReadWriteAppend)
            .expect("open slot file");
        file.write(&[0xEE]).expect("append");
        file.close().expect("close");
    }
    let entry = ws.entries[0].expect("slot 0 holds the book");
    assert_eq!(
        quick_check(&root, slot_name.as_str(), &entry, &mut ws.session),
        QuickCheckOutcome::RequiresFullValidation
    );
    assert_eq!(ws.session.level(&book, 1), IntegrityLevel::Unchecked);

    // Full validation concludes Mismatch and records the *grown* identity.
    let mut grown = bytes.clone();
    grown.push(0xEE);
    let mut job = FullValidationJob::new(&entry);
    let mut scratch = [0u8; 2048];
    let concluded = loop {
        match job.step(
            &root,
            slot_name.as_str(),
            &mut ws.session,
            8192,
            &mut scratch,
        ) {
            ValidationStep::Pending => {}
            ValidationStep::Concluded(level) => break level,
        }
    };
    assert_eq!(concluded, IntegrityLevel::Mismatch);
    assert_eq!(
        ws.session.observed_identity(&book, 1),
        Some((grown.len() as u64, sha256_of(&grown)))
    );

    // Delete the file: the quick check concludes absence, the list offers
    // delete only, and a validation job agrees.
    root.delete_file_in_dir(slot_name.as_str()).expect("delete");
    assert_eq!(
        quick_check(&root, slot_name.as_str(), &entry, &mut ws.session),
        QuickCheckOutcome::Unavailable
    );
    let listed = list_all(&ws);
    assert_eq!(
        listed[0].source_integrity_status,
        SourceIntegrityStatus::Unavailable
    );
    assert!(!listed[0].allowed_operations.replace);
    assert!(listed[0].allowed_operations.delete);
    assert!(!listed[0].allowed_operations.recover_current_bytes);

    let mut job = FullValidationJob::new(&entry);
    assert_eq!(
        job.step(
            &root,
            slot_name.as_str(),
            &mut ws.session,
            8192,
            &mut scratch
        ),
        ValidationStep::Concluded(IntegrityLevel::Unavailable)
    );
}

#[test]
fn unmanaged_adoption_seeds_and_lists_without_replace() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    root.make_dir_in_dir("BOOKS").expect("make books dir");
    let books = root.open_dir("BOOKS").expect("open books dir");
    let bytes = epub_bytes(9_000, 6);
    {
        let file = books
            .open_file_in_dir("MOBY.EPB", Mode::ReadWriteCreateOrTruncate)
            .expect("create book file");
        file.write(&bytes).expect("write");
        file.close().expect("close");
    }

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let identity = FreshIdentity {
        logical_book_id: [5; LOGICAL_BOOK_ID_BYTES],
        book_token: [10; BOOK_TOKEN_BYTES],
    };
    let outcome = adopt_or_reidentify(
        &root,
        &books,
        "MOBY.EPB",
        identity,
        [7; 16],
        &mut ws,
        || true,
    );
    assert!(matches!(outcome, AdoptOutcome::Adopted(_)));

    let listed = list_all(&ws);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].source_origin, SourceOrigin::UnmanagedSd);
    // The adoption hash seeded the session.
    assert_eq!(
        listed[0].source_integrity_status,
        SourceIntegrityStatus::ValidatedThisMount
    );
    // Unmanaged books never offer replacement or client recovery; byte
    // changes re-identify locally instead.
    assert!(!listed[0].allowed_operations.replace);
    assert!(listed[0].allowed_operations.delete);
    assert!(!listed[0].allowed_operations.recover_current_bytes);
}

#[test]
fn list_requires_a_valid_catalog_and_bounded_output() {
    let ws = workspace();
    let mut out = [placeholder_entry(); 1];
    assert_eq!(
        list_books(&ws, &mut out),
        Err(ListError::CatalogUnavailable),
        "a never-loaded workspace must not list as empty"
    );

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let bytes = epub_bytes(3_000, 5);
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    upload_book(&root, &mut idem, &mut ws, &bytes, 9, 5, 10);
    let more = epub_bytes(3_000, 6);
    upload_book(&root, &mut idem, &mut ws, &more, 11, 6, 12);

    let mut too_small = [placeholder_entry(); 1];
    assert_eq!(
        list_books(&ws, &mut too_small),
        Err(ListError::OutputTooSmall)
    );
    let mut out = [placeholder_entry(); MAX_SOURCE_SLOTS];
    assert_eq!(list_books(&ws, &mut out), Ok(2));
}
