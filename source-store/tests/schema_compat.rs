//! What happens to a card written by an older source-store build.
//!
//! The schema-1 source-metadata layout is reproduced here byte for byte
//! rather than referenced, precisely because the crate no longer contains
//! it: this fixture is the record of what v1 *was*, so the refusal below
//! cannot start silently accepting or misreading it.

mod common;

use common::{new_card, open_mgr, open_root};
use source_store::bodies::{
    SOURCE_METADATA_MAGIC, SOURCE_METADATA_SCHEMA, SOURCE_METADATA_SCHEMA_V1,
};
use source_store::layout;
use source_store::ops::{load_catalog, IdempotencyStore, OpsWorkspace};
use source_store::publish::{publish_record, PublishError};
use source_store::record::{
    classify_record, record_file_len, seal_body, RecordState, SealedBody, BODY_CRC_BYTES,
    BODY_PREFIX_BYTES,
};

/// The schema-1 field list, in order, as of commit `0eac2e5`. Identical to
/// schema 2 except that v2 inserts a 32-byte request-binding digest after
/// the operation request ID.
const V1_FIELD_BYTES: usize = 16 // logical_book_id
    + 8  // source_generation
    + 1  // source_origin
    + 1  // operation_kind
    + 24 // operation_request_id
    + 1  // externally_recovered
    + 1  // physical_slot
    + 8  // source_length
    + 32 // source_sha256
    + 2  // quick_fingerprint_policy_version
    + 32 // quick_fingerprint_sha256
    + 16 // book_token
    + 1  // display_label_length
    + 64 // display_label
    + 1  // unmanaged_name_length
    + 12; // unmanaged_name
const V1_LOGICAL_BYTES: usize = BODY_PREFIX_BYTES + V1_FIELD_BYTES + BODY_CRC_BYTES;

/// Seal a valid schema-1 metadata record: a managed book, generation 1.
fn sealed_v1_metadata() -> (Vec<u8>, SealedBody) {
    let mut buf = vec![0u8; record_file_len(V1_LOGICAL_BYTES).unwrap()];
    let mut at = BODY_PREFIX_BYTES;
    let put = |bytes: &[u8], buf: &mut Vec<u8>, at: &mut usize| {
        buf[*at..*at + bytes.len()].copy_from_slice(bytes);
        *at += bytes.len();
    };
    put(&[1u8; 16], &mut buf, &mut at); // logical_book_id
    put(&1u64.to_le_bytes(), &mut buf, &mut at); // source_generation
    put(&[1], &mut buf, &mut at); // source_origin = ManagedUpload
    put(&[1], &mut buf, &mut at); // operation_kind = ManagedUploadRequest
    put(&[2u8; 24], &mut buf, &mut at); // operation_request_id
    put(&[0], &mut buf, &mut at); // externally_recovered
    put(&[0], &mut buf, &mut at); // physical_slot
    put(&4096u64.to_le_bytes(), &mut buf, &mut at); // source_length
    put(&[5u8; 32], &mut buf, &mut at); // source_sha256
    put(&1u16.to_le_bytes(), &mut buf, &mut at); // quick fingerprint policy
    put(&[6u8; 32], &mut buf, &mut at); // quick_fingerprint_sha256
    put(&[7u8; 16], &mut buf, &mut at); // book_token
    put(&[6], &mut buf, &mut at); // display_label_length
    let mut label = [0u8; 64];
    label[..6].copy_from_slice(b"A Book");
    put(&label, &mut buf, &mut at); // display_label
    put(&[0], &mut buf, &mut at); // unmanaged_name_length
    put(&[0u8; 12], &mut buf, &mut at); // unmanaged_name
    assert_eq!(
        at,
        V1_LOGICAL_BYTES - BODY_CRC_BYTES,
        "the v1 fixture does not fill the v1 layout"
    );

    let sealed = seal_body(
        SOURCE_METADATA_MAGIC,
        SOURCE_METADATA_SCHEMA_V1,
        1,
        V1_LOGICAL_BYTES,
        &mut buf,
    )
    .expect("seal the v1 fixture");
    (buf, sealed)
}

#[test]
fn the_v1_layout_is_a_different_length_from_the_current_one() {
    assert_eq!(SOURCE_METADATA_SCHEMA, 2);
    assert_ne!(
        V1_LOGICAL_BYTES,
        source_store::bodies::SOURCE_METADATA_LOGICAL_BYTES,
        "v1 and v2 bodies are the same size, so the fixture proves nothing"
    );
}

/// A v1 record is structurally perfect — right magic, valid CRC, committed
/// commit sector — and still must not be read as a v2 record. It carries no
/// request binding, so adopting it as authority would mean serving replays
/// that cannot be bound to the request that made them.
#[test]
fn a_schema_1_card_is_refused_legibly_rather_than_misread() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let (buf, sealed) = sealed_v1_metadata();
    let pair = layout::metadata_pair(0).expect("slot 0");
    let mut scratch = [0u8; 4096];
    publish_record(&root, pair.pair(), &buf, &sealed, 0, &mut scratch, || true)
        .expect("a v1 record publishes: the framing is unchanged between schemas");

    // The framing layer still sees a committed record — this is a schema
    // question, not a corruption question.
    let mut whole = vec![0u8; record_file_len(V1_LOGICAL_BYTES).unwrap()];
    let file = root
        .open_file_in_dir(pair.pair().names[0], embedded_sdmmc::Mode::ReadOnly)
        .expect("open the published v1 record");
    let mut read = 0;
    while read < whole.len() {
        let n = file.read(&mut whole[read..]).expect("read");
        assert!(n > 0);
        read += n;
    }
    file.close().expect("close");
    let RecordState::Committed(view) = classify_record(&whole, SOURCE_METADATA_MAGIC) else {
        panic!("the v1 fixture should classify as committed");
    };
    assert_eq!(view.schema_version, SOURCE_METADATA_SCHEMA_V1);

    // The catalog refuses the card, and says why.
    let mut ws = Box::new(OpsWorkspace::new());
    assert_eq!(
        load_catalog(&root, &mut ws),
        Err(PublishError::UnsupportedSchema),
        "a v1 card must be refused as an old schema, not as corruption"
    );
    assert!(
        !ws.catalog_is_valid(),
        "a refused card must leave no usable catalog view"
    );

    // And nothing was rewritten in the process: refusing is a read-only
    // verdict, so the card stays exactly as recoverable as it was.
    let file = root
        .open_file_in_dir(pair.pair().names[0], embedded_sdmmc::Mode::ReadOnly)
        .expect("the v1 record survives the refusal");
    assert_eq!(file.length() as usize, whole.len());
    file.close().expect("close");

    // The idempotency store is independent of the metadata schema and still
    // loads, so the reset path has somewhere to start from.
    IdempotencyStore::load(&root, &mut ws).expect("idempotency state is unaffected");
}
