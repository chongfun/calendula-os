//! Fault-injection and simulated power-cut tests for the durable publication
//! protocol, against a real embedded-sdmmc FAT16 filesystem on an in-memory
//! card.
//!
//! Two failure models, deliberately distinct:
//!
//! - **Injected write/read failures**: the card refuses the Nth I/O and the
//!   code under test keeps running. Exercises the error paths and the
//!   invariant that a failed publication never disturbs committed authority.
//! - **Power-cut replay**: every block write during a publication is logged;
//!   the test then reconstructs the card as it stood after *each prefix* of
//!   those writes — plus torn variants of the next write — and runs startup
//!   selection on the reconstruction. This is the software half of the PRD's
//!   power-cut gate: for every cut point, selection must yield the previous
//!   committed state or the new committed state, and nothing else. The
//!   hardware half (real cards, real power removal) cannot be simulated and
//!   is tracked as a separate M0S acceptance item.
//!
//! The card model lives in `tests/common/mod.rs`, shared with the
//! logical-book operation suite.

mod common;

use common::{
    new_card, open_mgr, open_root, publish_generation, sample_metadata, sealed_metadata, SharedDisk,
};
use embedded_sdmmc::Mode;
use source_store::bodies::{
    DisplayLabel, SourceMetadata, StagedOperation, StagingMarker, Tombstone, BOOK_TOKEN_BYTES,
    LOGICAL_BOOK_ID_BYTES, REQUEST_ID_BYTES, SHA256_BYTES, SOURCE_METADATA_MAGIC,
    SOURCE_METADATA_SCHEMA, STAGING_MARKER_MAGIC, STAGING_MARKER_SCHEMA, TOMBSTONE_MAGIC,
    TOMBSTONE_SCHEMA, TOMBSTONE_STATUS_DELETED,
};
use source_store::publish::{
    publish_record, select_authority, summarize_slot, PublishError, SlotPair, SlotSummary,
};
use source_store::receipts::{
    IdempotencyState, OperationReceipt, ReceiptOperation, IDEMPOTENCY_MAGIC, IDEMPOTENCY_SCHEMA,
    RECEIPT_STATUS_SUCCESS, REQUEST_NONCE_BYTES,
};
use source_store::record::{classify_record, encode_commit_sector, seal_body, RecordState};

const PAIR: SlotPair<'_> = SlotPair {
    names: ["SRCMETA.A", "SRCMETA.B"],
    magic: SOURCE_METADATA_MAGIC,
};
const SCRATCH_BYTES: usize = 4096;

/// Mount the card fresh — the reboot view — and run startup selection,
/// decoding the selected record's body. Panics on I/O; returns `None` when
/// no committed authority exists.
fn reboot_authority(disk: &SharedDisk) -> Option<(u64, SourceMetadata)> {
    let mgr = open_mgr(disk);
    let root = open_root(&mgr);
    let mut scratch = [0u8; SCRATCH_BYTES];
    let selected = select_authority(&root, PAIR, &mut scratch).expect("selection must not error");
    let (slot, generation) = selected?;
    // Reread the selected slot and decode: authority is the *content*, and a
    // selected record that does not decode is a protocol failure.
    let file = root
        .open_file_in_dir(PAIR.names[slot], Mode::ReadOnly)
        .expect("selected slot must open");
    let len = file.length() as usize;
    assert!(len <= SCRATCH_BYTES, "selected record impossibly large");
    let mut at = 0usize;
    while at < len {
        let n = file.read(&mut scratch[at..len]).expect("read selected");
        assert!(n > 0, "short read of selected record");
        at += n;
    }
    file.close().expect("close selected");
    let RecordState::Committed(view) = classify_record(&scratch[..len], PAIR.magic) else {
        panic!("selected slot must classify committed");
    };
    assert_eq!(view.generation, generation);
    let decoded = SourceMetadata::decode(&view).expect("selected record must decode");
    Some((generation, decoded))
}

/// The publication invariant every cut point must satisfy: authority is
/// `old` or `new`, intact either way.
fn assert_old_or_new(disk: &SharedDisk, old: Option<u64>, new: u64, context: &str) {
    match reboot_authority(disk) {
        None => assert_eq!(old, None, "{context}: authority vanished"),
        Some((generation, decoded)) => {
            assert!(
                Some(generation) == old || generation == new,
                "{context}: selected generation {generation}, expected {old:?} or {new}"
            );
            // Content must match the generation it claims: a mixed record
            // (old body, new generation or vice versa) can never appear.
            assert_eq!(
                decoded,
                sample_metadata(generation),
                "{context}: selected record content does not match its generation"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[test]
fn publish_then_replace_alternates_slots() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let first = publish_generation(&root, PAIR, 1).expect("first publication");
    let second = publish_generation(&root, PAIR, 2).expect("second publication");
    let third = publish_generation(&root, PAIR, 3).expect("third publication");
    assert_ne!(first, second, "A/B must alternate");
    assert_eq!(first, third, "third generation overwrites the oldest slot");

    drop(root);
    drop(mgr);
    let (generation, decoded) = reboot_authority(&disk).expect("authority exists");
    assert_eq!(generation, 3);
    assert_eq!(decoded.source_generation, 3);
}

#[test]
fn stale_or_equal_generation_is_refused() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    publish_generation(&root, PAIR, 2).expect("baseline");
    assert_eq!(
        publish_generation(&root, PAIR, 2),
        Err(PublishError::BadInput)
    );
    assert_eq!(
        publish_generation(&root, PAIR, 1),
        Err(PublishError::BadInput)
    );
}

#[test]
fn revalidation_refusal_leaves_prepared_not_committed() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    publish_generation(&root, PAIR, 1).expect("baseline");

    let (buf, sealed) = sealed_metadata(2);
    let mut scratch = [0u8; SCRATCH_BYTES];
    let refused = publish_record(&root, PAIR, &buf, &sealed, 0, &mut scratch, || false);
    assert_eq!(refused, Err(PublishError::RevalidationRefused));

    // The candidate slot holds a prepared generation 2; authority is still 1.
    drop(root);
    drop(mgr);
    let (generation, _) = reboot_authority(&disk).expect("authority survives refusal");
    assert_eq!(generation, 1);

    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut scratch = [0u8; SCRATCH_BYTES];
    let summaries = [
        summarize_slot(&root, PAIR.names[0], PAIR.magic, &mut scratch).unwrap(),
        summarize_slot(&root, PAIR.names[1], PAIR.magic, &mut scratch).unwrap(),
    ];
    assert!(
        summaries.contains(&SlotSummary::Prepared { generation: 2 }),
        "refused candidate stays prepared for cleanup, got {summaries:?}"
    );
}

// ---------------------------------------------------------------------------
// Injected I/O failures
// ---------------------------------------------------------------------------

#[test]
fn every_write_failure_preserves_authority() {
    // Arm an exactly-once write failure at every write index a publication
    // performs, one run per index; the publication must fail cleanly and a
    // reboot must still select the old generation.
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, PAIR, 1).expect("baseline");
    }
    let base = disk.snapshot();

    let mut fault_index = 0u32;
    loop {
        disk.restore_cut(&base, &[], 0, 0);
        disk.fault.fail_write_in.set(Some(fault_index));
        let outcome = {
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            publish_generation(&root, PAIR, 2)
        };
        let fault_fired = disk.fault.fail_write_in.get().is_none();
        if !fault_fired {
            // The armed index is beyond the publication's writes: the run
            // above was fault-free and must have succeeded.
            outcome.expect("fault-free run succeeds");
            assert!(fault_index > 0, "publication must write at least once");
            break;
        }
        assert!(
            outcome.is_err(),
            "write fault {fault_index} did not surface as an error"
        );
        assert_old_or_new(&disk, Some(1), 2, &format!("write fault {fault_index}"));
        fault_index += 1;
    }
}

#[test]
fn every_read_failure_preserves_authority() {
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, PAIR, 1).expect("baseline");
    }
    let base = disk.snapshot();

    let mut fault_index = 0u32;
    loop {
        disk.restore_cut(&base, &[], 0, 0);
        disk.fault.fail_read_in.set(Some(fault_index));
        // Mounting itself reads, so the armed fault can fire inside the
        // harness's own `expect`s before publish starts; both shapes model
        // the same thing (a read failed mid-operation) and both must leave
        // committed authority intact.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            publish_generation(&root, PAIR, 2)
        }));
        let fault_fired = disk.fault.fail_read_in.get().is_none();
        disk.fault.fail_read_in.set(None);
        if !fault_fired {
            outcome
                .expect("fault-free run must not panic")
                .expect("fault-free run succeeds");
            break;
        }
        // Whether the mount panicked (an `expect` in the harness) or publish
        // returned an error — or even tolerated the fault and succeeded —
        // committed authority must select as old or new, intact.
        assert_old_or_new(&disk, Some(1), 2, &format!("read fault {fault_index}"));
        fault_index += 1;
    }
}

/// A read that failed must never be reported as a record that is not there.
///
/// `embedded-sdmmc`'s read-only `open_file_in_dir` answers `NotFound` for a
/// device error as readily as for a missing file, and "absent" is the most
/// dangerous answer this layer can be given: an absent pair has no committed
/// authority, so it becomes a publication target *and* loses the
/// generation-monotonicity floor. One dropped read would be enough to
/// overwrite a live record with a lower generation — or, one layer up, to
/// take a book out of the catalog and free its slot to be truncated.
#[test]
fn a_read_fault_is_never_mistaken_for_an_absent_record() {
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, PAIR, 7).expect("baseline");
    }
    let base = disk.snapshot();

    let mut faults_seen = 0u32;
    for fault_index in 0.. {
        disk.restore_cut(&base, &[], 0, 0);
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let mut scratch = [0u8; SCRATCH_BYTES];
        // Armed after mounting, so the budget covers only the selection
        // itself and every index maps to a read the selector issued.
        disk.fault.fail_read_in.set(Some(fault_index));
        let selected = select_authority(&root, PAIR, &mut scratch);
        let fired = disk.fault.fail_read_in.get().is_none();
        disk.fault.fail_read_in.set(None);
        if !fired {
            break;
        }
        faults_seen += 1;
        match selected {
            Ok(None) => {
                panic!("read fault {fault_index}: committed generation 7 reported as absent")
            }
            Ok(Some(selection)) => assert_eq!(
                selection,
                (0, 7),
                "read fault {fault_index}: selected something other than the committed record"
            ),
            // Refusing to answer is the correct alternative.
            Err(_) => {}
        }
    }
    assert!(
        faults_seen > 0,
        "no read fault fired; the test proves nothing"
    );
}

/// A read that failed while *opening* a record must never answer with a
/// second file under the same name.
///
/// The pinned `embedded-sdmmc`'s create modes convert any directory-lookup
/// error into "not there" and create — so a dropped read while reopening an
/// existing slot returns `Ok` on a duplicate 8.3 entry, and the name stops
/// identifying one file: writes land on the new entry while lookups may
/// keep finding the old one. Publication reopens its target on every third
/// generation (A/B alternation), so this is on the hot path.
#[test]
fn a_read_fault_while_opening_never_forks_a_record_into_two_entries() {
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, PAIR, 1).expect("baseline");
        publish_generation(&root, PAIR, 2).expect("other slot");
    }
    let base = disk.snapshot();

    let mut faults_seen = 0u32;
    for fault_index in 0.. {
        disk.restore_cut(&base, &[], 0, 0);
        let fault_fired = {
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            // Generation 3 reuses the slot generation 1 wrote, so its open
            // is the reopen-an-existing-file case.
            disk.fault.fail_read_in.set(Some(fault_index));
            let _ = publish_generation(&root, PAIR, 3);
            let fired = disk.fault.fail_read_in.get().is_none();
            disk.fault.fail_read_in.set(None);
            fired
        };
        if !fault_fired {
            break;
        }
        faults_seen += 1;
        assert_no_duplicate_names(&disk, &format!("read fault {fault_index}"));
        assert_old_or_new(&disk, Some(2), 3, &format!("read fault {fault_index}"));
    }
    assert!(
        faults_seen > 0,
        "no read fault fired; the test proves nothing"
    );
}

/// Every directory entry name must appear at most once.
fn assert_no_duplicate_names(disk: &SharedDisk, context: &str) {
    let mgr = open_mgr(disk);
    let root = open_root(&mgr);
    let mut names: Vec<String> = Vec::new();
    root.iterate_dir(|entry| names.push(entry.name.to_string()))
        .expect("iterate root");
    let mut seen = names.clone();
    seen.sort();
    seen.dedup();
    assert_eq!(
        seen.len(),
        names.len(),
        "{context}: the directory holds a duplicated name: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Simulated power cuts: every write boundary, plus torn sectors
// ---------------------------------------------------------------------------

/// Cut points around a *first* publication: authority must be absent or the
/// new generation — and never a prepared record promoted to authority.
#[test]
fn first_publication_survives_every_cut_point() {
    let disk = new_card();
    let base = disk.snapshot();
    disk.start_logging();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, PAIR, 1).expect("logged publication");
    }
    let log = disk.take_log();
    assert!(log.len() > 4, "publication should write several blocks");

    for cut in 0..=log.len() {
        disk.restore_cut(&base, &log, cut, 0);
        assert_old_or_new(&disk, None, 1, &format!("clean cut after {cut} writes"));
        for torn in [1, 128, 511] {
            disk.restore_cut(&base, &log, cut, torn);
            assert_old_or_new(&disk, None, 1, &format!("cut {cut} + {torn} torn bytes"));
        }
    }
}

/// Cut points around a *replacement* publication: authority must be the old
/// or the new generation at every boundary — never absent, never mixed.
#[test]
fn replacement_survives_every_cut_point() {
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, PAIR, 1).expect("baseline");
    }
    let base = disk.snapshot();
    disk.start_logging();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, PAIR, 2).expect("logged publication");
    }
    let log = disk.take_log();

    for cut in 0..=log.len() {
        disk.restore_cut(&base, &log, cut, 0);
        assert_old_or_new(&disk, Some(1), 2, &format!("clean cut after {cut} writes"));
        for torn in [1, 128, 511] {
            disk.restore_cut(&base, &log, cut, torn);
            assert_old_or_new(&disk, Some(1), 2, &format!("cut {cut} + {torn} torn bytes"));
        }
    }
}

/// The full lifecycle under cuts: publication interrupted at every point,
/// then *completed by a retry* — the resume path a reboot takes. After the
/// retry, authority must be the retried generation.
#[test]
fn interrupted_publication_retries_to_completion() {
    let disk = new_card();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, PAIR, 1).expect("baseline");
    }
    let base = disk.snapshot();
    disk.start_logging();
    {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        publish_generation(&root, PAIR, 2).expect("logged publication");
    }
    let log = disk.take_log();

    for cut in 0..=log.len() {
        disk.restore_cut(&base, &log, cut, 0);
        let before = reboot_authority(&disk).map(|(generation, _)| generation);
        let retry = {
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            if before == Some(2) {
                // The cut landed after the commit point; the next generation
                // continues from 2.
                publish_generation(&root, PAIR, 3)
            } else {
                publish_generation(&root, PAIR, 2)
            }
        };
        retry.unwrap_or_else(|error| panic!("retry after cut {cut} failed: {error:?}"));
        let expected = if before == Some(2) { 3 } else { 2 };
        let (generation, _) = reboot_authority(&disk).expect("retried authority");
        assert_eq!(generation, expected, "cut {cut}: retry did not converge");
    }
}

// ---------------------------------------------------------------------------
// Hostile on-card states
// ---------------------------------------------------------------------------

#[test]
fn oversized_slot_file_is_corrupt_and_recoverable() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    // A slot file larger than any legitimate record (and larger than the
    // scratch buffer) must summarize as corrupt without being read whole,
    // and publication must recover the slot by overwriting it.
    {
        let file = root
            .open_file_in_dir(PAIR.names[0], Mode::ReadWriteCreateOrTruncate)
            .expect("create oversized file");
        let junk = [0xAAu8; 1024];
        for _ in 0..8 {
            file.write(&junk).expect("write junk");
        }
        file.close().expect("close junk");
    }
    let mut scratch = [0u8; SCRATCH_BYTES];
    assert_eq!(
        summarize_slot(&root, PAIR.names[0], PAIR.magic, &mut scratch),
        Ok(SlotSummary::Corrupt {
            claims_commit: false
        })
    );

    publish_generation(&root, PAIR, 1).expect("publication over corrupt slot");
    drop(root);
    drop(mgr);
    let (generation, _) = reboot_authority(&disk).expect("authority");
    assert_eq!(generation, 1);
}

#[test]
fn ambiguous_equal_generations_refuse_selection_and_publication() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    // Manufacture the state A/B publication can never produce: both slots
    // committed at the same generation.
    let (buf, sealed) = sealed_metadata(1);
    let sector = encode_commit_sector(&sealed, 0).expect("sector");
    for name in PAIR.names {
        let file = root
            .open_file_in_dir(name, Mode::ReadWriteCreateOrTruncate)
            .expect("create dup");
        file.write(&buf[..sealed.padded_len]).expect("body");
        file.write(&sector).expect("sector");
        file.close().expect("close dup");
    }

    let mut scratch = [0u8; SCRATCH_BYTES];
    assert_eq!(
        select_authority(&root, PAIR, &mut scratch),
        Err(PublishError::AmbiguousAuthority)
    );
    assert_eq!(
        publish_generation(&root, PAIR, 2),
        Err(PublishError::AmbiguousAuthority)
    );
}

// ---------------------------------------------------------------------------
// The cut sweep, run against every authoritative record class
// ---------------------------------------------------------------------------

/// One authoritative record class: its real layout pair and a
/// generation-dependent body, so a cut reconstruction proves not only that
/// selection lands on a whole generation but that the selected *content*
/// is that generation's.
struct RecordClass {
    label: &'static str,
    pair: SlotPair<'static>,
    schema: u16,
    encode: fn(u64, &mut [u8]) -> usize,
}

fn encode_metadata_body(generation: u64, buf: &mut [u8]) -> usize {
    sample_metadata(generation)
        .encode_into(buf)
        .expect("encode metadata body")
}

fn encode_tombstone_body(generation: u64, buf: &mut [u8]) -> usize {
    Tombstone {
        logical_book_id: [4; LOGICAL_BOOK_ID_BYTES],
        deleted_source_generation: generation,
        deleted_book_token: [generation as u8 + 1; BOOK_TOKEN_BYTES],
        delete_request_id: [2; REQUEST_ID_BYTES],
        delete_result_status: TOMBSTONE_STATUS_DELETED,
    }
    .encode_into(buf)
    .expect("encode tombstone body")
}

fn encode_idempotency_body(generation: u64, buf: &mut [u8]) -> usize {
    let mut state = IdempotencyState::initial();
    // One receipt per generation beyond the first, so bodies differ by
    // generation and a mixed old-body/new-sector record cannot hide.
    for n in 1..generation {
        state
            .insert(OperationReceipt {
                epoch: 1,
                request_nonce: [n as u8; REQUEST_NONCE_BYTES],
                operation: ReceiptOperation::Delete,
                logical_book_id: [n as u8; LOGICAL_BOOK_ID_BYTES],
                base_book_token_or_zero: [n as u8; BOOK_TOKEN_BYTES],
                source_generation: 0,
                source_length_or_zero: 0,
                source_sha256_or_zero: [0; SHA256_BYTES],
                display_label_len: 0,
                display_label: [0; 64],
                result_book_token_or_zero: [0; BOOK_TOKEN_BYTES],
                result_status: RECEIPT_STATUS_SUCCESS,
            })
            .expect("insert fixture receipt");
    }
    state.encode_into(buf).expect("encode idempotency body")
}

fn encode_marker_body(generation: u64, buf: &mut [u8]) -> usize {
    StagingMarker {
        operation: StagedOperation::Create,
        operation_request_id: [generation as u8 + 1; REQUEST_ID_BYTES],
        logical_book_id: [5; LOGICAL_BOOK_ID_BYTES],
        base_book_token_or_zero: [0; BOOK_TOKEN_BYTES],
        candidate_source_generation: generation,
        candidate_physical_slot: 3,
        expected_source_length: 100 + generation,
        expected_source_sha256: [generation as u8; SHA256_BYTES],
        display_label: DisplayLabel::new(b"m").unwrap(),
    }
    .encode_into(buf)
    .expect("encode marker body")
}

fn record_classes() -> [RecordClass; 4] {
    [
        RecordClass {
            label: "source metadata",
            pair: SlotPair {
                names: ["M00A.BIN", "M00B.BIN"],
                magic: SOURCE_METADATA_MAGIC,
            },
            schema: SOURCE_METADATA_SCHEMA,
            encode: encode_metadata_body,
        },
        RecordClass {
            label: "tombstone",
            pair: SlotPair {
                names: ["T00A.BIN", "T00B.BIN"],
                magic: TOMBSTONE_MAGIC,
            },
            schema: TOMBSTONE_SCHEMA,
            encode: encode_tombstone_body,
        },
        RecordClass {
            label: "idempotency state",
            pair: SlotPair {
                names: ["IDEMA.BIN", "IDEMB.BIN"],
                magic: IDEMPOTENCY_MAGIC,
            },
            schema: IDEMPOTENCY_SCHEMA,
            encode: encode_idempotency_body,
        },
        RecordClass {
            label: "staging marker",
            pair: SlotPair {
                names: ["MARKA.BIN", "MARKB.BIN"],
                magic: STAGING_MARKER_MAGIC,
            },
            schema: STAGING_MARKER_SCHEMA,
            encode: encode_marker_body,
        },
    ]
}

fn publish_class_generation(root: &common::Dir<'_>, class: &RecordClass, generation: u64) {
    let mut buf = vec![0u8; 16 * 1024];
    let logical = (class.encode)(generation, &mut buf);
    let sealed = seal_body(
        class.pair.magic,
        class.schema,
        generation,
        logical,
        &mut buf,
    )
    .expect("seal class body");
    let mut scratch = vec![0u8; 16 * 1024];
    publish_record(root, class.pair, &buf, &sealed, 0, &mut scratch, || true)
        .unwrap_or_else(|error| panic!("{}: publish gen {generation}: {error:?}", class.label));
}

/// Class-generic old-or-new assertion, with a byte-exact content check
/// against the generation's canonical body.
fn assert_class_old_or_new(
    disk: &SharedDisk,
    class: &RecordClass,
    old: Option<u64>,
    new: u64,
    context: &str,
) {
    let mgr = open_mgr(disk);
    let root = open_root(&mgr);
    let mut scratch = vec![0u8; 16 * 1024];
    let selected =
        select_authority(&root, class.pair, &mut scratch).expect("selection must not error");
    let Some((slot, generation)) = selected else {
        assert_eq!(old, None, "{}: {context}: authority vanished", class.label);
        return;
    };
    assert!(
        Some(generation) == old || generation == new,
        "{}: {context}: selected generation {generation}, expected {old:?} or {new}",
        class.label
    );
    let file = root
        .open_file_in_dir(class.pair.names[slot], Mode::ReadOnly)
        .expect("selected slot must open");
    let len = file.length() as usize;
    assert!(len <= scratch.len(), "selected record impossibly large");
    let mut at = 0usize;
    while at < len {
        let n = file.read(&mut scratch[at..len]).expect("read selected");
        assert!(n > 0, "short read of selected record");
        at += n;
    }
    file.close().expect("close selected");
    let RecordState::Committed(view) = classify_record(&scratch[..len], class.pair.magic) else {
        panic!(
            "{}: {context}: selected slot must classify committed",
            class.label
        );
    };
    assert_eq!(view.generation, generation);
    let mut expected = vec![0u8; 16 * 1024];
    let expected_len = (class.encode)(generation, &mut expected);
    // Stamp the framing prefix and CRC the encode helpers leave to
    // seal_body, so the comparison covers the exact committed bytes.
    seal_body(
        class.pair.magic,
        class.schema,
        generation,
        expected_len,
        &mut expected,
    )
    .expect("seal expected body");
    assert_eq!(
        view.logical_body,
        &expected[..expected_len],
        "{}: {context}: selected content does not match its generation",
        class.label
    );
}

/// The PRD's "run against all authoritative record classes": the full
/// cut-point-times-torn-sector sweep, for the first publication and a
/// replacement, over each class's real layout pair. One record protocol,
/// proved once per class rather than assumed to generalize.
#[test]
fn every_record_class_survives_every_cut_point() {
    for class in record_classes() {
        // First publication: absent or generation 1 at every cut.
        let disk = new_card();
        let base = disk.snapshot();
        disk.start_logging();
        {
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            publish_class_generation(&root, &class, 1);
        }
        let log = disk.take_log();
        assert!(
            log.len() > 4,
            "{}: publication should write several blocks",
            class.label
        );
        for cut in 0..=log.len() {
            for torn in [0, 1, 128, 511] {
                disk.restore_cut(&base, &log, cut, torn);
                assert_class_old_or_new(
                    &disk,
                    &class,
                    None,
                    1,
                    &format!("first publication, cut {cut} + {torn} torn bytes"),
                );
            }
        }

        // Replacement: old or new at every cut, never absent, never mixed.
        disk.restore_cut(&base, &log, log.len(), 0);
        let committed = disk.snapshot();
        disk.start_logging();
        {
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            publish_class_generation(&root, &class, 2);
        }
        let log = disk.take_log();
        for cut in 0..=log.len() {
            for torn in [0, 1, 128, 511] {
                disk.restore_cut(&committed, &log, cut, torn);
                assert_class_old_or_new(
                    &disk,
                    &class,
                    Some(1),
                    2,
                    &format!("replacement, cut {cut} + {torn} torn bytes"),
                );
            }
        }
    }
}
