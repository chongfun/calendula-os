//! End-to-end tests for unmanaged adoption and re-identification: a
//! direct-SD file becomes a logical book, changed bytes become a new
//! generation, and a deleted identity stays burned.

mod common;

use common::{new_card, open_mgr, open_root, Dir};
use embedded_sdmmc::Mode;
use source_store::bodies::{SourceOrigin, BOOK_TOKEN_BYTES, LOGICAL_BOOK_ID_BYTES};
use source_store::ops::{
    delete_book, find_authoritative_by_token, load_catalog, DeleteOutcome, DeleteRequest,
    IdempotencyStore, OpsWorkspace,
};
use source_store::publish::PublishError;
use source_store::unmanaged::{adopt_or_reidentify, AdoptOutcome};
use source_store::upload::FreshIdentity;

const BOOKS_DIR: &str = "BOOKS";

fn workspace() -> Box<OpsWorkspace> {
    Box::new(OpsWorkspace::new())
}

fn epub_bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|n| (n as u8).wrapping_mul(41) ^ seed)
        .collect()
}

fn fresh(id_seed: u8, token_seed: u8) -> FreshIdentity {
    FreshIdentity {
        logical_book_id: [id_seed; LOGICAL_BOOK_ID_BYTES],
        book_token: [token_seed; BOOK_TOKEN_BYTES],
    }
}

/// Put a user file into the books directory.
fn write_book(books: &Dir<'_>, name: &str, bytes: &[u8]) {
    let file = books
        .open_file_in_dir(name, Mode::ReadWriteCreateOrTruncate)
        .expect("create book file");
    file.write(bytes).expect("write book file");
    file.close().expect("close book file");
}

fn adopt(
    root: &Dir<'_>,
    books: &Dir<'_>,
    ws: &mut OpsWorkspace,
    name: &str,
    identity: FreshIdentity,
    nonce_seed: u8,
) -> AdoptOutcome {
    adopt_or_reidentify(root, books, name, identity, [nonce_seed; 16], ws, || true)
}

#[test]
fn adoption_reidentification_and_burned_identity() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    root.make_dir_in_dir(BOOKS_DIR).expect("mkdir books");
    let books = root.open_dir(BOOKS_DIR).expect("open books");
    let v1 = epub_bytes(5_000, 1);
    let v2 = epub_bytes(6_000, 2);

    write_book(&books, "MOBY.EPU", &v1);
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");

    // First sight: adopted at generation 1, labelled from the stem.
    let outcome = adopt(&root, &books, &mut ws, "MOBY.EPU", fresh(5, 10), 1);
    let AdoptOutcome::Adopted(result) = outcome else {
        panic!("adoption failed: {outcome:?}");
    };
    assert_eq!(result.source_generation, 1);
    let entry = find_authoritative_by_token(&ws, &[10; BOOK_TOKEN_BYTES]).expect("visible");
    assert_eq!(entry.metadata.source_origin, SourceOrigin::UnmanagedSd);
    assert_eq!(entry.metadata.unmanaged_name.as_str(), Some("MOBY.EPU"));
    assert_eq!(entry.metadata.display_label.as_bytes(), b"MOBY");

    // Second sight, unchanged bytes: a no-op, same token.
    assert_eq!(
        adopt(&root, &books, &mut ws, "MOBY.EPU", fresh(6, 11), 2),
        AdoptOutcome::AlreadyCurrent {
            book_token: [10; BOOK_TOKEN_BYTES]
        }
    );

    // Changed bytes: same book, next generation, new token, kept label.
    write_book(&books, "MOBY.EPU", &v2);
    let outcome = adopt(&root, &books, &mut ws, "MOBY.EPU", fresh(7, 12), 3);
    let AdoptOutcome::Reidentified(result) = outcome else {
        panic!("re-identification failed: {outcome:?}");
    };
    assert_eq!(result.logical_book_id, [5; LOGICAL_BOOK_ID_BYTES]);
    assert_eq!(result.book_token, [12; BOOK_TOKEN_BYTES]);
    assert_eq!(result.source_generation, 2);
    assert!(find_authoritative_by_token(&ws, &[10; BOOK_TOKEN_BYTES]).is_none());
    let entry = find_authoritative_by_token(&ws, &[12; BOOK_TOKEN_BYTES]).expect("visible");
    assert_eq!(entry.metadata.display_label.as_bytes(), b"MOBY");
    assert_eq!(entry.metadata.source_length, v2.len() as u64);

    // Delete the book; the same file adopts as a brand-new logical book
    // with a fresh identity — never a resurrection.
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    assert!(matches!(
        delete_book(
            &root,
            &mut idem,
            &DeleteRequest {
                epoch: 1,
                nonce: [50; 16],
                book_token: [12; BOOK_TOKEN_BYTES],
            },
            &mut ws,
        ),
        DeleteOutcome::Deleted { .. }
    ));
    // The burned id and token collide; fresh identity adopts.
    assert_eq!(
        adopt(&root, &books, &mut ws, "MOBY.EPU", fresh(5, 60), 4),
        AdoptOutcome::RejectedIdentityCollision,
        "the deleted logical id stays burned"
    );
    let outcome = adopt(&root, &books, &mut ws, "MOBY.EPU", fresh(90, 60), 5);
    let AdoptOutcome::Adopted(result) = outcome else {
        panic!("re-adoption failed: {outcome:?}");
    };
    assert_eq!(result.logical_book_id, [90; LOGICAL_BOOK_ID_BYTES]);
    assert_eq!(
        result.source_generation, 1,
        "a fresh book, not generation 3"
    );
}

#[test]
fn adoption_rejections() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    root.make_dir_in_dir(BOOKS_DIR).expect("mkdir books");
    let books = root.open_dir(BOOKS_DIR).expect("open books");
    let bytes = epub_bytes(3_000, 1);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");

    // Missing file.
    assert_eq!(
        adopt(&root, &books, &mut ws, "GHOST.EPU", fresh(5, 10), 1),
        AdoptOutcome::RejectedMissingFile
    );

    // Invalid names never reach the filesystem.
    assert_eq!(
        adopt(&root, &books, &mut ws, "NO SPACE.X", fresh(5, 10), 1),
        AdoptOutcome::RejectedNameInvalid
    );

    // Zero-length files are not books.
    write_book(&books, "EMPTY.EPU", &[]);
    assert_eq!(
        adopt(&root, &books, &mut ws, "EMPTY.EPU", fresh(5, 10), 1),
        AdoptOutcome::RejectedNameInvalid
    );

    // The container gate refuses; nothing is adopted.
    write_book(&books, "BAD.EPU", &bytes);
    assert_eq!(
        adopt_or_reidentify(
            &root,
            &books,
            "BAD.EPU",
            fresh(5, 10),
            [1; 16],
            &mut ws,
            || false
        ),
        AdoptOutcome::RejectedUnsupportedContainer
    );
    assert!(find_authoritative_by_token(&ws, &[10; BOOK_TOKEN_BYTES]).is_none());

    // A valid adoption, then a token collision on the next file.
    write_book(&books, "GOOD.EPU", &bytes);
    assert!(matches!(
        adopt(&root, &books, &mut ws, "GOOD.EPU", fresh(5, 10), 2),
        AdoptOutcome::Adopted(_)
    ));
    write_book(&books, "OTHER.EPU", &epub_bytes(2_000, 9));
    assert_eq!(
        adopt(&root, &books, &mut ws, "OTHER.EPU", fresh(6, 10), 3),
        AdoptOutcome::RejectedIdentityCollision
    );
}

#[test]
fn unmanaged_and_managed_coexist_in_the_catalog() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    root.make_dir_in_dir(BOOKS_DIR).expect("mkdir books");
    let books = root.open_dir(BOOKS_DIR).expect("open books");
    let unmanaged = epub_bytes(3_000, 1);

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    write_book(&books, "DIRECT.EPU", &unmanaged);
    assert!(matches!(
        adopt(&root, &books, &mut ws, "DIRECT.EPU", fresh(5, 10), 1),
        AdoptOutcome::Adopted(_)
    ));

    // A managed upload lands beside it, in a different slot.
    let mut idem = IdempotencyStore::load(&root, &mut ws).expect("idem");
    let managed = epub_bytes(4_000, 2);
    let req = source_store::upload::UploadRequest {
        epoch: 1,
        nonce: [9; 16],
        declared_length: managed.len() as u64,
        declared_sha256: source_store::validate::sha256_of(&managed),
        display_label: source_store::bodies::DisplayLabel::new(b"Managed").unwrap(),
        replace_token: None,
    };
    let mut txn =
        match source_store::upload::begin_upload(&root, &mut idem, &req, fresh(6, 20), &mut ws) {
            source_store::upload::UploadBeginOutcome::Started(txn) => txn,
            _ => panic!("begin"),
        };
    for chunk in managed.chunks(1500) {
        source_store::upload::upload_chunk(&root, &mut txn, chunk).expect("chunk");
    }
    source_store::upload::finish_upload(&root, &mut idem, txn, &mut ws, || true).expect("finish");

    // Both books are served, each with its own origin.
    let direct = find_authoritative_by_token(&ws, &[10; BOOK_TOKEN_BYTES]).expect("unmanaged");
    let uploaded = find_authoritative_by_token(&ws, &[20; BOOK_TOKEN_BYTES]).expect("managed");
    assert_eq!(direct.metadata.source_origin, SourceOrigin::UnmanagedSd);
    assert_eq!(uploaded.metadata.source_origin, SourceOrigin::ManagedUpload);
    assert_ne!(direct.physical_slot, uploaded.physical_slot);
    // The managed book's slot holds an EPUB file; the unmanaged one's
    // does not (its bytes live in BOOKS).
    let managed_slot = source_store::layout::source_slot_name(uploaded.physical_slot).unwrap();
    assert!(root
        .open_file_in_dir(managed_slot.as_str(), Mode::ReadOnly)
        .is_ok());
    let unmanaged_slot = source_store::layout::source_slot_name(direct.physical_slot).unwrap();
    assert!(root
        .open_file_in_dir(unmanaged_slot.as_str(), Mode::ReadOnly)
        .is_err());
}

/// Adoption vouches for an exact digest of a file the *user* owns, so its
/// last look before committing must rehash rather than re-measure. The
/// container gate runs inside that window and stands in for the edit.
#[test]
fn adoption_refuses_a_same_length_change_inside_the_commit_window() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    root.make_dir_in_dir(BOOKS_DIR).expect("mkdir books");
    let books = root.open_dir(BOOKS_DIR).expect("open books");
    let v1 = epub_bytes(5_000, 1);
    let swapped = epub_bytes(5_000, 9); // same length, different bytes

    write_book(&books, "MOBY.EPU", &v1);
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");

    let outcome = adopt_or_reidentify(
        &root,
        &books,
        "MOBY.EPU",
        fresh(5, 10),
        [1; 16],
        &mut ws,
        || {
            write_book(&books, "MOBY.EPU", &swapped);
            true
        },
    );
    assert_eq!(
        outcome,
        AdoptOutcome::Failed(PublishError::RevalidationRefused),
        "a same-length change inside the commit window was adopted"
    );
    assert!(find_authoritative_by_token(&ws, &[10; BOOK_TOKEN_BYTES]).is_none());

    // The settled bytes adopt on the next look, with their own identity.
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("catalog");
    let outcome = adopt(&root, &books, &mut ws, "MOBY.EPU", fresh(5, 11), 2);
    let AdoptOutcome::Adopted(result) = outcome else {
        panic!("retry after a refused window failed: {outcome:?}");
    };
    assert_eq!(result.book_token, [11; BOOK_TOKEN_BYTES]);
    let entry = find_authoritative_by_token(&ws, &[11; BOOK_TOKEN_BYTES]).expect("adopted");
    assert_eq!(
        entry.metadata.source_sha256,
        source_store::validate::sha256_of(&swapped)
    );
}
