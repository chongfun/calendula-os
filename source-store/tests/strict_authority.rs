//! Fail-closed tests for strict authority loading: a decayed committed
//! record — corrupt bytes still carrying a CRC-valid commit sector — must
//! never read as "absent", because absence silently resets the idempotency
//! epoch and frees metadata slots for truncation. Sector-less debris, the
//! normal residue of a cut publication, must keep reading as absent or the
//! crash-recovery semantics die instead.

mod common;

use common::{new_card, open_mgr, open_root, publish_metadata, sample_metadata, Dir};
use embedded_sdmmc::Mode;
use source_store::layout;
use source_store::ops::{load_catalog, IdempotencyStore, OpsWorkspace};
use source_store::publish::{publish_record, PublishError};
use source_store::receipts::{IdempotencyState, IDEMPOTENCY_MAGIC, IDEMPOTENCY_SCHEMA};
use source_store::record::{record_file_len, seal_body};

fn workspace() -> Box<OpsWorkspace> {
    Box::new(OpsWorkspace::new())
}

/// Flip one byte of a record file in place — decay, as opposed to a cut.
fn flip_byte(root: &Dir<'_>, name: &str, offset: u32) {
    let file = root
        .open_file_in_dir(name, Mode::ReadWriteAppend)
        .expect("open record file");
    file.seek_from_start(offset).expect("seek for read");
    let mut byte = [0u8; 1];
    assert_eq!(file.read(&mut byte).expect("read byte"), 1);
    file.seek_from_start(offset).expect("seek for write");
    file.write(&[byte[0] ^ 0xFF]).expect("flip");
    file.close().expect("close");
}

#[test]
fn a_decayed_idempotency_record_fails_closed_instead_of_resetting_the_epoch() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    // Commit a real idempotency record, then decay one body byte. The
    // commit sector stays valid; the body CRC no longer holds.
    let state = IdempotencyState::initial();
    let mut buf = vec![0u8; record_file_len(8192).unwrap()];
    let logical = state.encode_into(&mut buf).expect("encode state");
    let sealed =
        seal_body(IDEMPOTENCY_MAGIC, IDEMPOTENCY_SCHEMA, 1, logical, &mut buf).expect("seal state");
    let names = layout::idempotency_pair();
    let mut scratch = vec![0u8; 16 * 1024];
    let slot = publish_record(&root, names.pair(), &buf, &sealed, 0, &mut scratch, || true)
        .expect("publish state");

    let mut ws = workspace();
    IdempotencyStore::load(&root, &mut ws).expect("intact record loads");

    flip_byte(&root, names.pair().names[slot], 25);
    assert!(
        matches!(
            IdempotencyStore::load(&root, &mut ws),
            Err(PublishError::CorruptAuthority)
        ),
        "a decayed committed record must not read as a fresh epoch-1 store"
    );
}

#[test]
fn a_decayed_metadata_record_fails_closed_instead_of_freeing_the_slot() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let names = layout::metadata_pair(0).unwrap();
    let meta = sample_metadata(1);
    let slot = publish_metadata(&root, names.pair(), &meta, 1).expect("publish metadata");

    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("intact catalog loads");
    assert!(ws.entries[0].is_some());

    flip_byte(&root, names.pair().names[slot], 25);
    assert_eq!(
        load_catalog(&root, &mut ws),
        Err(PublishError::CorruptAuthority),
        "a decayed committed book must poison the catalog, not vanish from it"
    );
    assert!(!ws.catalog_is_valid(), "operations must refuse the view");
}

#[test]
fn sectorless_debris_still_reads_as_absent() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    // Garbage with no plausible commit sector — what a cut body write
    // leaves behind. It must stay invisible, not wedge the catalog.
    let names = layout::metadata_pair(0).unwrap();
    {
        let file = root
            .open_file_in_dir(names.pair().names[0], Mode::ReadWriteCreateOrTruncate)
            .expect("create debris");
        file.write(&[0xAB; 700]).expect("write debris");
        file.close().expect("close");
    }
    let mut ws = workspace();
    load_catalog(&root, &mut ws).expect("debris must not poison the catalog");
    assert!(ws.entries[0].is_none(), "debris is not a book");

    let idem = IdempotencyStore::load(&root, &mut ws).expect("fresh store");
    assert!(idem.is_usable());
}

#[test]
fn metadata_in_the_wrong_slot_pair_fails_closed() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    // A record whose body names slot 0, committed into slot 5's pair:
    // authority whose provenance cannot be explained.
    let names = layout::metadata_pair(5).unwrap();
    let meta = sample_metadata(1);
    assert_eq!(meta.physical_slot, 0);
    publish_metadata(&root, names.pair(), &meta, 1).expect("publish misplaced metadata");

    let mut ws = workspace();
    assert_eq!(
        load_catalog(&root, &mut ws),
        Err(PublishError::CorruptAuthority),
        "slot identity must match the pair the record sits in"
    );
}
