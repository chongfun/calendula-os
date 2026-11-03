//! On-card title order is a rebuildable view of the exact catalog bytes.
mod support;

use proto::catalog::{
    encode_catalog_header, encode_catalog_record, CATALOG_HEADER_BYTES, CATALOG_RECORD_BYTES,
};
use proto::library_path::BookRoot;
use storage::title_browse::{compare_catalog_candidates, Candidate};
use storage::title_index_disk::{self, IndexError};
use support::Card;

#[test]
fn sliced_sort_handles_the_maximum_catalog_count() {
    use reader_cache::title_index::{IndexStorage, Sorter};
    use std::cmp::Ordering;
    struct Runs([Vec<u16>; 2]);
    impl IndexStorage for Runs {
        type Error = ();
        fn begin(&mut self, _: u16) -> Result<(), ()> {
            Ok(())
        }
        fn check(&mut self, _: u16) -> Result<(), ()> {
            Ok(())
        }
        fn read(&mut self, run: bool, position: u16) -> Result<u16, ()> {
            Ok(self.0[usize::from(run)][usize::from(position)])
        }
        fn write(&mut self, run: bool, position: u16, row: u16) -> Result<(), ()> {
            self.0[usize::from(run)][usize::from(position)] = row;
            Ok(())
        }
        fn compare(&mut self, left: u16, right: u16) -> Result<Ordering, ()> {
            Ok(right.cmp(&left))
        }
        fn publish(&mut self, _: bool, _: u16) -> Result<(), ()> {
            Ok(())
        }
    }
    let count = u16::MAX;
    let mut disk = Runs([vec![0; usize::from(count)], vec![0; usize::from(count)]]);
    let mut sorter = Sorter::new(count);
    let run = loop {
        if let Some(run) = sorter.step(&mut disk, 127).unwrap() {
            break run;
        }
    };
    assert!(disk.0[usize::from(run)]
        .iter()
        .copied()
        .eq((0..count).rev()));
}

fn catalog(card: &Card, count: u16) {
    card.make_short_folder("READER");
    let mut bytes = vec![0; CATALOG_HEADER_BYTES + usize::from(count) * CATALOG_RECORD_BYTES];
    let mut header = [0; CATALOG_HEADER_BYTES];
    encode_catalog_header(count, &mut header);
    bytes[..CATALOG_HEADER_BYTES].copy_from_slice(&header);
    for row in 0..count {
        let folder = if row % 3 == 0 { "" } else { "Fiction/" };
        let name = format!("{folder}Book {row:03}.epub");
        let title = format!("Title {:03}", count - row);
        let mut record = [0; CATALOG_RECORD_BYTES];
        let root = if folder.is_empty() && row % 2 == 0 {
            BookRoot::CardRoot
        } else {
            BookRoot::Library
        };
        encode_catalog_record(
            &mut record,
            &name,
            root,
            &name,
            &title,
            "B.EPU",
            u32::from(row) + 1,
            u32::from(row) + 10,
        );
        let start = CATALOG_HEADER_BYTES + usize::from(row) * CATALOG_RECORD_BYTES;
        bytes[start..start + CATALOG_RECORD_BYTES].copy_from_slice(&record);
    }
    card.put_short("READER/CATALOG.BIN", &bytes);
}

#[test]
fn cooperative_rebuild_and_validation_match_the_disk_format() {
    for count in [0, 1, 19, 257] {
        let card = Card::blank();
        catalog(&card, count);
        let mut job = title_index_disk::IndexJob::default();
        let mut slices = 0;
        let snapshot = loop {
            slices += 1;
            assert!(slices < 10_000);
            if let Some(snapshot) = card.session(|root| job.step(root)).unwrap() {
                break snapshot;
            }
        };
        assert!(slices > 1);
        assert_eq!(snapshot.count(), count);
        assert_eq!(card.session(title_index_disk::validate), Ok(snapshot));
        let mut validate = title_index_disk::IndexJob::default();
        let checked = loop {
            if let Some(snapshot) = card.session(|root| validate.step(root)).unwrap() {
                break snapshot;
            }
        };
        assert_eq!(checked, snapshot);
    }
}

#[test]
fn cooperative_rebuild_rejects_a_catalog_changed_between_slices() {
    let card = Card::blank();
    catalog(&card, 37);
    let mut job = title_index_disk::IndexJob::default();
    // Finish the initial catalog digest and begin work on the scratch file.
    for _ in 0..4 {
        assert_eq!(card.session(|root| job.step(root)), Ok(None));
    }
    let mut bytes = card.read("READER/CATALOG.BIN").unwrap();
    bytes[CATALOG_HEADER_BYTES + proto::catalog::CATALOG_RECORD_TITLE_OFFSET] ^= 1;
    card.delete("READER/CATALOG.BIN");
    card.put_short("READER/CATALOG.BIN", &bytes);
    let result = loop {
        match card.session(|root| job.step(root)) {
            Ok(None) => {}
            result => break result,
        }
    };
    assert_eq!(result, Err(IndexError::Invalid));
    assert!(matches!(
        card.session(title_index_disk::validate),
        Err(IndexError::Invalid | IndexError::Missing)
    ));
}

#[test]
fn interrupted_cooperative_rebuild_is_never_accepted_as_complete() {
    for slices in [1, 2, 4, 6, 8] {
        let card = Card::blank();
        catalog(&card, 37);
        card.session(title_index_disk::rebuild).unwrap();
        let mut bytes = card.read("READER/CATALOG.BIN").unwrap();
        bytes[CATALOG_HEADER_BYTES + proto::catalog::CATALOG_RECORD_TITLE_OFFSET] ^= 1;
        card.delete("READER/CATALOG.BIN");
        card.put_short("READER/CATALOG.BIN", &bytes);
        {
            let mut job = title_index_disk::IndexJob::default();
            for _ in 0..slices {
                assert_eq!(card.session(|root| job.step(root)), Ok(None));
            }
        }
        // Power loss discards only the RAM state.
        assert!(matches!(
            card.session(title_index_disk::validate),
            Err(IndexError::Invalid | IndexError::Missing)
        ));
        let mut retry = title_index_disk::IndexJob::default();
        let snapshot = loop {
            if let Some(snapshot) = card.session(|root| retry.step(root)).unwrap() {
                break snapshot;
            }
        };
        assert_eq!(card.session(title_index_disk::validate), Ok(snapshot));
    }
}

#[test]
fn cooperative_job_keeps_io_fault_distinct_from_invalid_index() {
    let card = Card::blank();
    catalog(&card, 4);
    card.session(title_index_disk::rebuild).unwrap();
    let before = card.read("READER/TITLEIDX.BIN").unwrap();
    let mut job = title_index_disk::IndexJob::default();
    card.session(|root| {
        card.disk.refuse_read_in(Some(0));
        assert_eq!(job.step(root), Err(IndexError::Device));
    });
    card.disk.refuse_read_in(None);
    assert_eq!(card.read("READER/TITLEIDX.BIN").unwrap(), before);
    assert!(card.session(title_index_disk::validate).is_ok());
}

#[test]
fn title_overlay_remains_sorted_while_the_other_slot_rebuilds() {
    let card = Card::blank();
    catalog(&card, 37);
    card.session(title_index_disk::rebuild).unwrap();
    let old = card.session(title_index_disk::validate).unwrap();
    let mut catalog_bytes = card.read("READER/CATALOG.BIN").unwrap();
    let start = CATALOG_HEADER_BYTES + proto::catalog::CATALOG_RECORD_TITLE_OFFSET;
    let mut title = [0; proto::catalog::CATALOG_TITLE_BYTES];
    proto::catalog::encode_catalog_title("Aardvark", &mut title);
    catalog_bytes[start..start + title.len()].copy_from_slice(&title);
    card.delete("READER/CATALOG.BIN");
    card.put_short("READER/CATALOG.BIN", &catalog_bytes);
    let overlay = card
        .session(|root| title_index_disk::title_overlay(root, old, &[], 0))
        .unwrap();
    let mut job = title_index_disk::IndexJob::away_from(old);
    let new = loop {
        card.session(|root| {
            title_index_disk::with_snapshot_overlay(root, old, &[overlay], |reader| {
                let mut previous = reader.row(0)?;
                for position in 1..old.count() {
                    let next = reader.row(position)?;
                    assert!(compare_catalog_candidates(&previous, &next).is_le());
                    previous = next;
                }
                Ok(())
            })
        })
        .unwrap();
        if let Some(snapshot) = card.session(|root| job.step(root)).unwrap() {
            break snapshot;
        }
    };
    assert_ne!(new.slot(), old.slot());
    assert_eq!(card.session(title_index_disk::validate), Ok(new));
    let mut boot = title_index_disk::IndexJob::default();
    let loaded = loop {
        if let Some(snapshot) = card.session(|root| boot.step(root)).unwrap() {
            break snapshot;
        }
    };
    assert_eq!(loaded, new);
}

#[test]
fn successive_title_overlays_remain_sorted_including_a_second_move_of_one_row() {
    let card = Card::blank();
    catalog(&card, 257);
    card.session(title_index_disk::rebuild).unwrap();
    let snapshot = card.session(title_index_disk::validate).unwrap();
    let mut overlays = Vec::new();
    for (row, title) in [
        (0u16, "Aardvark"),
        (255, "Zzzzz"),
        (0, "Middle"),
        (256, "Aardvark"),
    ] {
        let mut bytes = card.read("READER/CATALOG.BIN").unwrap();
        let start = CATALOG_HEADER_BYTES
            + usize::from(row) * CATALOG_RECORD_BYTES
            + proto::catalog::CATALOG_RECORD_TITLE_OFFSET;
        let mut field = [0; proto::catalog::CATALOG_TITLE_BYTES];
        proto::catalog::encode_catalog_title(title, &mut field);
        bytes[start..start + field.len()].copy_from_slice(&field);
        card.delete("READER/CATALOG.BIN");
        card.put_short("READER/CATALOG.BIN", &bytes);
        let next = card
            .session(|root| title_index_disk::title_overlay(root, snapshot, &overlays, row))
            .unwrap();
        overlays.push(next);
        card.session(|root| {
            title_index_disk::with_snapshot_overlay(root, snapshot, &overlays, |reader| {
                let mut previous = reader.row(0)?;
                let mut seen = vec![false; usize::from(snapshot.count())];
                seen[usize::from(previous.catalog_row)] = true;
                for position in 1..snapshot.count() {
                    let next = reader.row(position)?;
                    assert!(compare_catalog_candidates(&previous, &next).is_le());
                    assert!(!seen[usize::from(next.catalog_row)]);
                    seen[usize::from(next.catalog_row)] = true;
                    previous = next;
                }
                assert!(seen.into_iter().all(|seen| seen));
                Ok(())
            })
        })
        .unwrap();
    }
}

#[test]
fn interrupted_alternate_build_keeps_the_live_overlay_readable() {
    for (failure, landed) in [
        (0, false),
        (1, false),
        (5, false),
        (0, true),
        (1, true),
        (5, true),
    ] {
        let card = Card::blank();
        catalog(&card, 37);
        card.session(title_index_disk::rebuild).unwrap();
        let snapshot = card.session(title_index_disk::validate).unwrap();
        let before = card.read("READER/TITLEIDX.BIN").unwrap();
        let mut bytes = card.read("READER/CATALOG.BIN").unwrap();
        let start = CATALOG_HEADER_BYTES + proto::catalog::CATALOG_RECORD_TITLE_OFFSET;
        let mut title = [0; proto::catalog::CATALOG_TITLE_BYTES];
        proto::catalog::encode_catalog_title("Aardvark", &mut title);
        bytes[start..start + title.len()].copy_from_slice(&title);
        card.delete("READER/CATALOG.BIN");
        card.put_short("READER/CATALOG.BIN", &bytes);
        let overlay = card
            .session(|root| title_index_disk::title_overlay(root, snapshot, &[], 0))
            .unwrap();
        if landed {
            card.disk.land_then_fail_after(failure);
        } else {
            card.disk.fail_after_writes(failure);
        }
        let mut job = title_index_disk::IndexJob::away_from(snapshot);
        for _ in 0..1000 {
            match card.session(|root| job.step(root)) {
                Ok(None) => {}
                Ok(Some(_)) | Err(_) => break,
            }
        }
        card.disk.refuse_next_writes(0);
        assert_eq!(card.read("READER/TITLEIDX.BIN").unwrap(), before);
        card.session(|root| {
            title_index_disk::with_snapshot_overlay(root, snapshot, &[overlay], |reader| {
                assert_eq!(reader.row(overlay.new_position)?.catalog_row, 0);
                Ok(())
            })
        })
        .unwrap();
        let mut retry = title_index_disk::IndexJob::default();
        let verified = loop {
            if let Some(snapshot) = card.session(|root| retry.step(root)).unwrap() {
                break snapshot;
            }
        };
        assert_eq!(card.session(title_index_disk::validate), Ok(verified));
    }
}

#[test]
fn indexed_rows_are_complete_sorted_and_bound_to_catalog_contents() {
    let card = Card::blank();
    catalog(&card, 257);
    card.session(title_index_disk::rebuild)
        .expect("build index");
    let snapshot = card
        .session(title_index_disk::validate)
        .expect("validate index");
    let count = snapshot.count();
    assert_eq!(count, 257);
    let index = card.read("READER/TITLEIDX.BIN").expect("index bytes");
    let cat = card.read("READER/CATALOG.BIN").expect("catalog bytes");
    let start = title_index_disk::SNAPSHOT_BYTES
        + if index[6] != 0 {
            usize::from(count) * 2
        } else {
            0
        };
    let mut rows = Vec::new();
    for chunk in index[start..start + usize::from(count) * 2]
        .as_chunks::<2>()
        .0
    {
        let row = u16::from_le_bytes([chunk[0], chunk[1]]);
        let offset = CATALOG_HEADER_BYTES + usize::from(row) * CATALOG_RECORD_BYTES;
        let raw: &[u8; CATALOG_RECORD_BYTES] = cat[offset..offset + CATALOG_RECORD_BYTES]
            .try_into()
            .unwrap();
        rows.push(
            Candidate::from_record(&proto::catalog::decode_catalog_record(raw), row).unwrap(),
        );
    }
    assert_eq!(rows.len(), 257);
    let mut row_ids = rows.iter().map(|row| row.catalog_row).collect::<Vec<_>>();
    row_ids.sort_unstable();
    assert_eq!(row_ids, (0..count).collect::<Vec<_>>());
    assert!(rows
        .windows(2)
        .all(|pair| compare_catalog_candidates(&pair[0], &pair[1]).is_le()));
    assert!(rows[..86].iter().any(|row| row.root == BookRoot::CardRoot));
    assert!(rows[..86].iter().any(|row| row.root == BookRoot::Library));
    card.session(|root| {
        title_index_disk::with_snapshot(root, snapshot, |reader| {
            let root_path = proto::library_path::LibraryPath::root();
            let fiction = proto::library_path::LibraryPath::parse("Fiction").unwrap();
            assert_eq!(reader.folder_range(&root_path)?, (0, 86));
            assert_eq!(reader.folder_range(&fiction)?, (86, 257));
            assert_eq!(reader.row(0)?.catalog_row, rows[0].catalog_row);
            Ok(())
        })
        .unwrap();
    });

    // A title write changes no catalog header bytes. A previously published
    // order still has to be rejected after that rewrite and after a reboot.
    let mut changed = cat;
    changed[CATALOG_HEADER_BYTES + proto::catalog::CATALOG_RECORD_TITLE_OFFSET] ^= 1;
    card.delete("READER/CATALOG.BIN");
    card.put_short("READER/CATALOG.BIN", &changed);
    assert_eq!(
        card.session(title_index_disk::validate),
        Err(IndexError::Invalid)
    );
}

#[test]
fn corrupt_run_or_refused_read_never_validates() {
    let card = Card::blank();
    catalog(&card, 4);
    card.session(title_index_disk::rebuild)
        .expect("build index");
    let snapshot = card
        .session(title_index_disk::validate)
        .expect("valid index");
    let mut index = card.read("READER/TITLEIDX.BIN").expect("index");
    let run = index[6] != 0;
    index[title_index_disk::SNAPSHOT_BYTES + if run { 8 } else { 0 }] ^= 1;
    card.delete("READER/TITLEIDX.BIN");
    card.put_short("READER/TITLEIDX.BIN", &index);
    assert_eq!(
        card.session(title_index_disk::validate),
        Err(IndexError::Invalid)
    );
    card.session(|root| {
        assert_eq!(
            title_index_disk::with_snapshot(root, snapshot, |reader| { reader.row(0).map(|_| ()) }),
            Err(IndexError::Invalid)
        );
    });
    card.session(|root| {
        card.disk.refuse_read_in(Some(0));
        assert_eq!(title_index_disk::validate(root), Err(IndexError::Device));
    });
}

#[test]
fn corrupt_membership_prefix_cannot_authorize_a_folder() {
    let card = Card::blank();
    catalog(&card, 4);
    card.session(title_index_disk::rebuild).unwrap();
    let snapshot = card.session(title_index_disk::validate).unwrap();
    let mut index = card.read("READER/TITLEIDX.BIN").unwrap();
    let prefix_start =
        title_index_disk::SNAPSHOT_BYTES + 4 * 4 + if index[6] != 0 { 5 * 16 } else { 0 };
    index[prefix_start + 16] ^= 1;
    card.delete("READER/TITLEIDX.BIN");
    card.put_short("READER/TITLEIDX.BIN", &index);
    assert_eq!(
        card.session(title_index_disk::validate),
        Err(IndexError::Invalid)
    );
    let result = card.session(|root| {
        title_index_disk::with_snapshot(root, snapshot, |reader| reader.membership((0, 1)))
    });
    assert_eq!(result, Err(IndexError::Invalid));
}

#[test]
fn a_single_unaddressable_record_is_not_published() {
    let card = Card::blank();
    catalog(&card, 1);
    let mut cat = card.read("READER/CATALOG.BIN").unwrap();
    cat[CATALOG_HEADER_BYTES] = 0xff;
    card.delete("READER/CATALOG.BIN");
    card.put_short("READER/CATALOG.BIN", &cat);
    assert_eq!(
        card.session(title_index_disk::rebuild),
        Err(IndexError::Invalid)
    );
    assert_eq!(
        card.session(title_index_disk::validate),
        Err(IndexError::Invalid)
    );
}

#[test]
fn title_update_retires_proof_only_when_a_catalog_write_is_attempted() {
    let card = Card::blank();
    catalog(&card, 1);
    let invalidated = std::cell::Cell::new(false);
    // Matching title bytes still require one write to record BookCache
    // provenance. Only the next identical title-and-source update is a no-op.
    card.session(|root| {
        assert_eq!(
            storage::library_sd::update_catalog_title(root, 0, (10, 1), "Title 001", || {
                invalidated.set(true)
            }),
            storage::library_sd::CatalogTitleUpdate::Changed { row: 0 }
        );
    });
    assert!(invalidated.replace(false), "provenance promotion writes");
    card.session(|root| {
        assert_eq!(
            storage::library_sd::update_catalog_title(root, 0, (10, 1), "Title 001", || {
                invalidated.set(true)
            }),
            storage::library_sd::CatalogTitleUpdate::Unchanged
        );
    });
    assert!(
        !invalidated.get(),
        "unchanged title does not retire the index"
    );
    card.session(|root| {
        card.disk.refuse_next_writes(1);
        assert_eq!(
            storage::library_sd::update_catalog_title(root, 0, (10, 1), "New title", || {
                invalidated.set(true)
            }),
            storage::library_sd::CatalogTitleUpdate::Failed
        );
    });
    assert!(invalidated.get(), "even a failed write retires the proof");
    card.disk.refuse_next_writes(0);
}

#[test]
fn interrupted_rebuild_never_validates_a_partial_new_order() {
    for (failing_write, lands) in [
        (0, false),
        (1, false),
        (2, false),
        (5, false),
        (15, false),
        (40, false),
        (0, true),
        (1, true),
        (5, true),
        (40, true),
    ] {
        let card = Card::blank();
        catalog(&card, 9);
        card.session(title_index_disk::rebuild)
            .expect("initial index");
        // The old index is stale before the new build begins. It must never
        // validate after a failure merely because its old header survived.
        let mut changed = card.read("READER/CATALOG.BIN").unwrap();
        changed[CATALOG_HEADER_BYTES + proto::catalog::CATALOG_RECORD_TITLE_OFFSET] ^= 1;
        card.delete("READER/CATALOG.BIN");
        card.put_short("READER/CATALOG.BIN", &changed);
        if lands {
            card.disk.land_then_fail_after(failing_write);
        } else {
            card.disk.fail_after_writes(failing_write);
        }
        let result = card.session(title_index_disk::rebuild);
        card.disk.refuse_next_writes(0);
        if result.is_err() {
            // A completed new header may have landed before reporting an
            // error. Anything else must be rejected.
            let validation = card.session(title_index_disk::validate);
            assert!(
                validation
                    .as_ref()
                    .is_ok_and(|snapshot| snapshot.count() == 9)
                    || matches!(validation, Err(IndexError::Invalid | IndexError::Missing))
            );
        }
    }
}

#[test]
fn opening_a_book_refreshes_a_valid_index_after_learning_its_title() {
    let card = Card::blank();
    card.put("BOOKS/Zeta.epub", &support::epub("A learned title", 2, 23));
    card.put("BOOKS/Alpha.epub", &support::epub("Another book", 2, 24));
    let mut device = support::Device::wake(&card);
    card.session(title_index_disk::rebuild).unwrap();
    let snapshot = card.session(title_index_disk::validate).unwrap();
    device.store.set_title_index_snapshot(snapshot.into_bytes());
    device.open_library();
    device.choose("Zeta.epub");
    assert_eq!(device.app.view, app_core::AppView::Reading);
    let refreshed = device
        .store
        .title_index_snapshot()
        .and_then(title_index_disk::Snapshot::from_bytes)
        .expect("the title write refreshes the index");
    assert_eq!(card.session(title_index_disk::validate), Ok(refreshed));
    card.session(|root| {
        title_index_disk::with_snapshot(root, refreshed, |reader| {
            let first = reader.row(0)?;
            assert_eq!(first.locator.as_str(), "Zeta.epub");
            assert_eq!(first.label.as_str(), "A learned title");
            Ok(())
        })
        .unwrap()
    });
}

#[test]
fn interrupted_rebuild_can_be_interrupted_again_then_retried() {
    let card = Card::blank();
    catalog(&card, 19);
    card.session(title_index_disk::rebuild).unwrap();
    let mut cat = card.read("READER/CATALOG.BIN").unwrap();
    cat[CATALOG_HEADER_BYTES + proto::catalog::CATALOG_RECORD_TITLE_OFFSET] ^= 1;
    card.delete("READER/CATALOG.BIN");
    card.put_short("READER/CATALOG.BIN", &cat);
    for _ in 0..2 {
        card.disk.land_then_fail_after(1);
        assert!(card.session(title_index_disk::rebuild).is_err());
        card.disk.refuse_next_writes(0);
        assert_eq!(
            card.session(title_index_disk::validate),
            Err(IndexError::Invalid)
        );
    }
    card.session(title_index_disk::rebuild).unwrap();
    assert_eq!(
        card.session(title_index_disk::validate).unwrap().count(),
        19
    );
}
