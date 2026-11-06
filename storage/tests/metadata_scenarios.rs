//! Idle EPUB probes update title sources without opening reading caches.
mod support;

use proto::catalog::TitleSource;
use proto::library_path::BookRoot;
use storage::library_sd::SettledCatalogScan;
use support::{epub, zip, Card, Device};

fn catalog_row(card: &Card, at: BookRoot, path: &str) -> proto::catalog::CatalogRecord {
    card.session(|root| {
        (0..128)
            .filter_map(|index| storage::library_sd::read_catalog_record_at(root, index))
            .find(|entry| entry.root == Some(at) && entry.path.as_str() == path)
            .expect("book in committed catalog")
    })
}

#[test]
fn unopened_epubs_are_sorted_by_real_metadata_title() {
    let card = Card::blank();
    card.put("BOOKS/A-filename.epub", &epub("Zulu Title", 1, 1));
    card.put("BOOKS/Z-filename.epub", &epub("Alpha Title", 1, 2));
    let mut device = Device::wake(&card);
    device.open_library();
    assert_eq!(device.labels(), ["Alpha Title", "Zulu Title"]);
    for (name, title) in [
        ("A-filename.epub", "Zulu Title"),
        ("Z-filename.epub", "Alpha Title"),
    ] {
        let row = catalog_row(&card, BookRoot::Library, name);
        assert_eq!(row.title.as_str(), title);
        assert_eq!(row.title_source, TitleSource::Probed);
    }
    // An ordinary reboot retains the metadata source and indexed order.
    drop(device);
    let mut again = Device::wake(&card);
    again.open_library();
    assert_eq!(again.labels(), ["Alpha Title", "Zulu Title"]);
}

/// Volume numbers in titles sort by value when read from the title index,
/// and again after a reboot reloads it. The test names the files a, b and c
/// so that filename order puts volume 10 first.
#[test]
fn indexed_titles_order_volume_numbers_by_value() {
    let card = Card::blank();
    for (file, title) in [
        ("a.epub", "Saga Vol. 10"),
        ("b.epub", "Saga Vol. 2"),
        ("c.epub", "Saga Vol. 1"),
    ] {
        card.put(&format!("BOOKS/{file}"), &epub(title, 1, 3));
    }
    let mut first = Device::wake(&card);
    first.open_library();
    assert_eq!(
        first.labels(),
        ["Saga Vol. 1", "Saga Vol. 2", "Saga Vol. 10"]
    );
    drop(first);
    let mut reboot = Device::wake(&card);
    reboot.open_library();
    assert_eq!(
        reboot.labels(),
        ["Saga Vol. 1", "Saga Vol. 2", "Saga Vol. 10"]
    );
}

/// Two volumes of one series spell its name differently ("EIGHTY-SIX" and
/// "EIGHTY SIX"). Spaces and punctuation compare equal, so the volume number
/// decides. The test names the files so filename order disagrees with volume
/// order.
#[test]
fn indexed_titles_ignore_punctuation_differences_before_the_volume() {
    let card = Card::blank();
    let one = "86\u{2014}EIGHTY-SIX: Alter, Vol. 1: The Reaper";
    let two = "86\u{2014}EIGHTY SIX: Alter, Vol. 2: Fight";
    card.put("BOOKS/b.epub", &epub(one, 1, 3));
    card.put("BOOKS/a.epub", &epub(two, 1, 3));
    let mut device = Device::wake(&card);
    device.open_library();
    assert_eq!(device.labels(), [one, two]);
}

#[test]
fn title_sorted_rows_expose_the_persisted_series_and_volume_after_reboot() {
    let card = Card::blank();
    let opf = br#"<package><metadata>
        <dc:title>Aardvark Chronicles (Volume 12.5)</dc:title>
        <meta name="calibre:series" content="Galaxy Tales"/>
        <meta name="calibre:series_index" content="12.5"/>
        </metadata></package>"#;
    let volume = zip(&[
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<container><rootfile full-path="content.opf"/></container>"#.to_vec(),
        ),
        ("content.opf", opf.to_vec()),
    ]);
    card.put("BOOKS/Z-filename.epub", &volume);
    card.put("BOOKS/A-filename.epub", &epub("Zulu Other Book", 1, 3));
    let assert_page = |device: &mut Device| {
        device.open_library();
        assert_eq!(
            device.labels(),
            ["Aardvark Chronicles (Volume 12.5)", "Zulu Other Book"]
        );
        let rows = device.store.folder_rows();
        assert_eq!(
            rows[0].display_label.as_str(),
            "Aardvark Chronicles (Volume 12.5)"
        );
        assert_eq!(rows[0].series.as_str(), "Galaxy Tales");
        assert_eq!(rows[0].volume.as_str(), "12.5");
        assert!(rows[1].series.is_empty());
        assert!(rows[1].volume.is_empty());
    };
    let mut first = Device::wake(&card);
    assert_page(&mut first);
    assert_eq!(
        catalog_row(&card, BookRoot::Library, "Z-filename.epub")
            .volume
            .as_str(),
        "12.5"
    );
    drop(first);
    let mut reboot = Device::wake(&card);
    assert_page(&mut reboot);
}

#[test]
fn repeated_long_title_fragments_preserve_source_length_after_reboot() {
    let card = Card::blank();
    let title = format!("{}{}{}", "A".repeat(64), "B".repeat(40), "A".repeat(24));
    card.put("BOOKS/Repeated.epub", &epub(&title, 1, 171));
    let mut first = Device::wake(&card);
    first.open_library();
    let stored = catalog_row(&card, BookRoot::Library, "Repeated.epub");
    assert_eq!(stored.title_source, TitleSource::Probed);
    assert_eq!(stored.title_len as usize, title.len());
    assert_eq!(stored.title_suffix.as_str(), &title[title.len() - 24..]);
    assert_eq!(first.store.folder_rows()[0].title_len as usize, title.len());

    drop(first);
    let mut reboot = Device::wake(&card);
    reboot.open_library();
    assert_eq!(
        reboot.store.folder_rows()[0].title_len as usize,
        title.len(),
        "catalog hint must survive reboot and title-index loading"
    );
}

#[test]
fn nested_and_duplicate_copies_keep_distinct_book_ids() {
    let card = Card::blank();
    let bytes = epub("Shared Metadata", 1, 17);
    card.put("BOOKS/One.epub", &bytes);
    card.put("BOOKS/Folder/Two.epub", &bytes);
    let mut device = Device::wake(&card);
    let one = catalog_row(&card, BookRoot::Library, "One.epub");
    let two = catalog_row(&card, BookRoot::Library, "Folder/Two.epub");
    assert_ne!(one.book_id, two.book_id);
    assert_eq!(one.title_source, TitleSource::Probed);
    assert_eq!(two.title_source, TitleSource::Probed);
    assert_eq!(one.title, two.title);
    device.open_library();
    device.choose("Folder");
    assert_eq!(device.labels(), ["Shared Metadata"]);
}

#[test]
fn missing_title_and_invalid_zip_settle_without_fabricating_titles() {
    let card = Card::blank();
    let untitled = zip(&[
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<container><rootfile full-path="content.opf"/></container>"#.to_vec(),
        ),
        (
            "content.opf",
            b"<package><metadata><dc:creator>Someone</dc:creator></metadata></package>".to_vec(),
        ),
    ]);
    card.put("BOOKS/Untitled-file.epub", &untitled);
    card.put("BOOKS/Broken.epub", b"not a ZIP");
    let mut device = Device::wake(&card);
    device.open_library();
    assert_eq!(
        catalog_row(&card, BookRoot::Library, "Untitled-file.epub").title_source,
        TitleSource::NoMetadataTitle
    );
    assert_eq!(
        catalog_row(&card, BookRoot::Library, "Broken.epub").title_source,
        TitleSource::NoMetadataTitle
    );
    assert_eq!(device.labels(), ["Broken", "Untitled File"]);
}

#[test]
fn a_transient_sd_read_refusal_does_not_permanently_skip_metadata() {
    // Sweep faults across catalog lookup, the physical file open, and ZIP
    // reads. A single passing I/O refusal must not consume the row as a
    // permanent skip: the existing bounded retry can recover its real title.
    let mut fired = 0;
    for reads_before_fault in 0..32 {
        let card = Card::blank();
        card.put("BOOKS/Transient.epub", &epub("Recovered Metadata", 1, 30));
        let mut device = Device::wake(&card);
        // Rescan synchronously to recreate an eligible Unknown row, then
        // inject the fault only after catalog creation has completed.
        device.send(app_core::StorageCommand::RefreshCatalog);
        device.run_queued();
        assert_eq!(
            catalog_row(&card, BookRoot::Library, "Transient.epub").title_source,
            TitleSource::Unknown
        );
        card.disk.refuse_read_in(Some(reads_before_fault));
        device.settle();
        if !card.disk.read_refusal_armed() {
            fired += 1;
        }
        card.disk.refuse_read_in(None);
        assert_eq!(
            catalog_row(&card, BookRoot::Library, "Transient.epub").title_source,
            TitleSource::Probed,
            "transient read refusal after {reads_before_fault} reads"
        );
    }
    assert!(fired >= 4, "the injected faults never reached the probe");
}

#[test]
fn reboot_restores_existing_title_order_before_revisiting_all_metadata_rows() {
    let card = Card::blank();
    card.put("BOOKS/A-name.epub", &epub("Zulu Verified", 1, 70));
    card.put("BOOKS/Z-name.epub", &epub("Alpha Verified", 1, 71));
    // More rows than the early-validation budget. These have durable
    // NoMetadataTitle proofs and cannot require a fresh EPUB open on reboot.
    for n in 0..40 {
        card.put(&format!("BOOKS/M-{n:02}.epub"), b"not an EPUB");
    }
    let mut first = Device::wake(&card);
    first.open_library();
    // With more than 16 rows, the resident window is not necessarily the
    // index's first page. Check the published proof, not its cached window.
    assert!(first.store.title_index_snapshot().is_some());
    drop(first);

    let mut reboot = Device::wake_queued_only(&card);
    assert!(reboot.store.title_index_snapshot().is_none());
    for _ in 0..16 {
        if reboot.store.title_index_snapshot().is_some() {
            break;
        }
        reboot.step_background();
        reboot.run_queued();
    }
    assert!(
        reboot.store.title_index_snapshot().is_some(),
        "boot must validate and adopt the saved index before a 42-row pass finishes"
    );
    assert_eq!(
        catalog_row(&card, BookRoot::Library, "Z-name.epub").title_source,
        TitleSource::Probed
    );
}

#[test]
fn early_index_adoption_is_retired_before_the_first_metadata_write() {
    let card = Card::blank();
    card.put("BOOKS/A-name.epub", &epub("Zulu Verified", 1, 80));
    card.put("BOOKS/Z-name.epub", &epub("Alpha Verified", 1, 81));
    let mut initial = Device::wake(&card);
    // Recreate a catalog with Unknown titles, then publish an index that
    // matches those bytes. This models a reboot with outstanding probes.
    initial.send(app_core::StorageCommand::RefreshCatalog);
    initial.run_queued();
    assert_eq!(
        catalog_row(&card, BookRoot::Library, "A-name.epub").title_source,
        TitleSource::Unknown
    );
    card.session(storage::title_index_disk::rebuild).unwrap();
    drop(initial);

    let mut reboot = Device::wake_queued_only(&card);
    for _ in 0..20 {
        if reboot.store.title_index_snapshot().is_some() {
            break;
        }
        reboot.step_background();
        reboot.run_queued();
    }
    assert!(reboot.store.title_index_snapshot().is_some());
    reboot.step_background();
    assert!(
        reboot.store.title_index_snapshot().is_none(),
        "first metadata write must invalidate the adopted proof"
    );
    reboot.settle();
    reboot.open_library();
    assert_eq!(reboot.labels(), ["Alpha Verified", "Zulu Verified"]);
}

#[test]
fn discovery_during_reading_relands_the_library_after_fencing_old_rows() {
    let card = Card::blank();
    card.put("BOOKS/A.epub", &epub("Reading", 1, 80));
    card.put("BOOKS/Z.epub", &epub("Unopened", 1, 81));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("A.epub");
    assert_eq!(device.app.view, app_core::AppView::Reading);

    // Recreate the unfinished discovery work a newly scanned card has.
    device.send(app_core::StorageCommand::RefreshCatalog);
    device.run_queued();
    for _ in 0..100 {
        if device.store.title_fallback_pending() {
            break;
        }
        device.step_background();
        device.run_queued();
    }
    assert!(device.store.title_fallback_pending());
    assert!(device.store.folder_rows().is_empty());

    // The first Library render precedes the fallback slice and must not
    // read a new page through the old browse addresses.
    for _ in 0..6 {
        if device.app.view == app_core::AppView::Library {
            break;
        }
        device.press_only(app_core::Button::Back);
        device.run_queued();
    }
    assert_eq!(device.app.view, app_core::AppView::Library);
    storage::library_sd::ensure_folder_page(
        &mut device.card,
        &mut device.store,
        device.app.selection,
        true,
    );
    assert!(device.store.folder_rows().is_empty());
    device.step_background();
    assert!(!device.store.title_fallback_pending());
    assert_eq!(device.rows(), ["A.epub", "Z.epub"]);
    assert_eq!(device.app.library_count, 2);
    assert!(device.log.iter().any(|event| matches!(
        event,
        app_core::LibraryEvent::FolderListed {
            request_id: None,
            count: 2,
            ..
        }
    )));
}

#[test]
fn settled_metadata_rows_are_skipped_in_bounded_contiguous_batches() {
    let card = Card::blank();
    for n in 0..145 {
        card.put(&format!("BOOKS/Settled-{n:03}.epub"), b"invalid EPUB");
    }
    drop(Device::wake(&card));
    let count = 145;
    let mut cursor = 0;
    let mut slices = 0;
    while cursor < count {
        let next = card
            .session(|root| storage::library_sd::skip_settled_catalog_rows(root, cursor, count))
            .expect("read the committed catalog");
        let SettledCatalogScan::Next(next) = next else {
            panic!("settled catalog returned an unreadable row");
        };
        assert!(next > cursor);
        assert!(next - cursor <= storage::library_sd::SETTLED_SCAN_ROWS_PER_SLICE);
        cursor = next;
        slices += 1;
    }
    assert_eq!(slices, 3, "145 settled rows take three bounded sessions");

    // A newly pending row in the middle stops the fast scan; later rows
    // cannot be silently classified as processed by a cached cursor.
    let mut bytes = card.read("READER/CATALOG.BIN").unwrap();
    let pending = 138;
    bytes[proto::catalog::CATALOG_HEADER_BYTES
        + pending * proto::catalog::CATALOG_RECORD_BYTES
        + proto::catalog::CATALOG_RECORD_TITLE_SOURCE_OFFSET] = TitleSource::Unknown as u8;
    card.delete("READER/CATALOG.BIN");
    card.put_short("READER/CATALOG.BIN", &bytes);
    let mut cursor = 0;
    for _ in 0..4 {
        let next = card
            .session(|root| storage::library_sd::skip_settled_catalog_rows(root, cursor, count))
            .unwrap();
        let SettledCatalogScan::Next(next) = next else {
            panic!("pending catalog unexpectedly unreadable");
        };
        if next == cursor {
            break;
        }
        cursor = next;
    }
    assert_eq!(cursor, pending);

    // Refused reads must never advance the cursor past unverified entries.
    card.session(|root| {
        card.disk.refuse_read_in(Some(0));
        // Refusing the first read may prevent opening or validating the
        // catalog itself; unlike a failed record it cannot be stepped over.
        assert!(storage::library_sd::skip_settled_catalog_rows(root, 0, count).is_err());
    });
    card.disk.refuse_read_in(None);
}

#[test]
fn persistent_mid_batch_refusal_skips_only_the_bad_row_and_discovers_later_books() {
    let card = Card::blank();
    const COUNT: usize = 40;
    const FAILED_ROW: usize = 12;
    const LATER_ROW: usize = 35;
    for n in 0..COUNT {
        let bytes = if n == LATER_ROW {
            epub("After Unreadable Row", 1, 191)
        } else {
            b"not an EPUB".to_vec()
        };
        card.put(&format!("BOOKS/Book-{n:02}.epub"), &bytes);
    }
    // Start with durable settled proofs, then simulate an interrupted pass
    // that left two entries unprobed. The failed row must remain Unknown.
    drop(Device::wake(&card));
    let mut bytes = card.read("READER/CATALOG.BIN").unwrap();
    for row in [FAILED_ROW, LATER_ROW] {
        let offset = proto::catalog::CATALOG_HEADER_BYTES
            + row * proto::catalog::CATALOG_RECORD_BYTES
            + proto::catalog::CATALOG_RECORD_TITLE_SOURCE_OFFSET;
        bytes[offset] = TitleSource::Unknown as u8;
    }
    card.delete("READER/CATALOG.BIN");
    card.put_short("READER/CATALOG.BIN", &bytes);

    // Find a deterministic disk read command within the chosen record. The
    // test card refuses one command after a configurable number of successful
    // reads; rearming this fault models a persistent bad sector without
    // stopping the FAT root/header from being read.
    let fault_after = |start: usize| {
        (0..160)
            .find(|&reads| {
                card.session(|root| {
                    card.disk.refuse_read_in(Some(reads));
                    let result = storage::library_sd::skip_settled_catalog_rows(root, start, COUNT);
                    card.disk.refuse_read_in(None);
                    result == Ok(SettledCatalogScan::UnreadableRow(FAILED_ROW))
                })
            })
            .expect("a read command for the failing catalog record")
    };

    let mut device = Device::wake_queued_only(&card);
    assert_eq!(device.store.catalog_count(), COUNT);
    device.store.stop_title_index_request();

    // The first batch verifies the prefix despite the fault.
    device.card.refuse_read_in_after_mount(fault_after(0));
    device.step_background();

    // The next scans start at the exact failing row, not at zero. Four
    // sustained failures exhaust this row's budget; no provenance is written.
    let failing_read = fault_after(FAILED_ROW);
    for _ in 0..4 {
        device.card.refuse_read_in_after_mount(failing_read);
        device.step_background();
    }
    device.settle();
    assert_eq!(
        card.session(|root| storage::library_sd::read_catalog_record_at(root, FAILED_ROW))
            .unwrap()
            .title_source,
        TitleSource::Unknown,
        "an unreadable record must not be marked completed"
    );
    assert_eq!(
        card.session(|root| storage::library_sd::read_catalog_record_at(root, LATER_ROW))
            .unwrap()
            .title_source,
        TitleSource::Probed,
        "discovery must reach the later EPUB after isolating the bad row"
    );
}

#[test]
fn catalog_open_failures_do_not_consume_the_next_rows_retry_budget() {
    const COUNT: usize = 40;
    const FAILED_ROW: usize = 12;
    const LATER_ROW: usize = 35;
    let card = Card::blank();
    for n in 0..COUNT {
        let bytes = if n == LATER_ROW {
            epub("Found After Recovery", 1, 220)
        } else {
            b"not an EPUB".to_vec()
        };
        card.put(&format!("BOOKS/Book-{n:02}.epub"), &bytes);
    }
    drop(Device::wake(&card));

    // Reopen a previously settled catalog with two eligible rows. The
    // first is across a sector boundary, so the harness can independently
    // refuse its record read after the catalog header has been accepted.
    let mut bytes = card.read("READER/CATALOG.BIN").unwrap();
    for row in [FAILED_ROW, LATER_ROW] {
        bytes[proto::catalog::CATALOG_HEADER_BYTES
            + row * proto::catalog::CATALOG_RECORD_BYTES
            + proto::catalog::CATALOG_RECORD_TITLE_SOURCE_OFFSET] = TitleSource::Unknown as u8;
    }
    card.delete("READER/CATALOG.BIN");
    card.put_short("READER/CATALOG.BIN", &bytes);

    let record_read_fault = (0..160)
        .find(|&reads| {
            card.session(|root| {
                card.disk.refuse_read_in(Some(reads));
                let result =
                    storage::library_sd::skip_settled_catalog_rows(root, FAILED_ROW, COUNT);
                card.disk.refuse_read_in(None);
                result == Ok(SettledCatalogScan::UnreadableRow(FAILED_ROW))
            })
        })
        .expect("a read command inside the failing catalog record");

    let mut device = Device::wake_queued_only(&card);
    device.store.stop_title_index_request();
    assert_eq!(device.store.catalog_count(), COUNT);

    // The first successful batch moves past verified settled rows.
    device.step_background();

    // Three catalog-open refusals raise backoff, but must not count as
    // failures reading the next row.
    for attempts in 1u8..=3 {
        device.card.refuse_read_in_after_mount(0);
        device.step_background();
        assert_eq!(
            device.task.background_attempts(&device.store, None),
            attempts
        );
        assert_eq!(
            catalog_row(&card, BookRoot::Library, "Book-12.epub").title_source,
            TitleSource::Unknown
        );
    }

    // The catalog is now readable. The same row must still get three
    // refused record reads followed by a fourth, successful attempt.
    for attempts in 1u8..=3 {
        device.card.refuse_read_in_after_mount(record_read_fault);
        device.step_background();
        assert_eq!(
            device.task.background_attempts(&device.store, None),
            attempts,
            "catalog-level retry state should reset on successful open"
        );
        assert_eq!(
            catalog_row(&card, BookRoot::Library, "Book-12.epub").title_source,
            TitleSource::Unknown,
            "row must not be skipped after {attempts} record failures"
        );
    }
    device.step_background();
    assert_eq!(
        catalog_row(&card, BookRoot::Library, "Book-12.epub").title_source,
        TitleSource::NoMetadataTitle,
        "the fourth row attempt must still reach the EPUB probe"
    );
    device.settle();
    assert_eq!(
        catalog_row(&card, BookRoot::Library, "Book-35.epub").title_source,
        TitleSource::Probed,
        "discovery must continue to subsequent rows"
    );
}

#[test]
fn metadata_retry_backoff_never_delays_unsettled_source_evidence() {
    let card = Card::blank();
    card.put("BOOKS/A-Pending.epub", &epub("Pending Title", 1, 210));
    card.put("BOOKS/Z-Reading.epub", &epub("Reading Title", 1, 211));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Z-Reading.epub");
    let evidence = storage::book_build::evidence_job(&device.store)
        .expect("the open book has source evidence to collect");

    // A refresh can clear the store's active-book cursor, but the previously
    // armed evidence job still represents the physical copy being read.
    device.send(app_core::StorageCommand::RefreshCatalog);
    device.run_queued();
    device.store.stop_title_index_request();
    assert_eq!(
        catalog_row(&card, BookRoot::Library, "A-Pending.epub").title_source,
        TitleSource::Unknown
    );

    // Only metadata is owed, and its next catalog read refuses the card.
    // That failure must not become the retry delay of source evidence.
    card.disk.refuse_read_in(Some(0));
    device.step_background();
    card.disk.refuse_read_in(None);
    assert_eq!(device.task.background_attempts(&device.store, None), 1);

    // An already-armed evidence job outranks the failed metadata probe
    // even when catalog refresh has cleared the active-book cursor.
    device.task.pending_evidence = Some(evidence);
    assert_eq!(device.task.background_attempts(&device.store, None), 0);
}

#[test]
fn finishing_source_evidence_does_not_probe_metadata_in_same_slice() {
    let card = Card::blank();
    card.put("BOOKS/A-Pending.epub", &epub("Pending Title", 1, 212));
    card.put("BOOKS/Z-Reading.epub", &epub("Reading Title", 1, 213));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Z-Reading.epub");
    let evidence = storage::book_build::evidence_job(&device.store)
        .expect("the open book has source evidence to collect");

    // Make just one record due for discovery without a refresh: a refresh
    // clears the active book and would discard the evidence job before it
    // executes. Reset the worker so the next slice starts discovery at row 0.
    let mut catalog = card.read("READER/CATALOG.BIN").unwrap();
    let pending = (0..device.store.catalog_count())
        .find(|&row| {
            card.session(|root| storage::library_sd::read_catalog_record_at(root, row))
                .is_some_and(|entry| entry.path.as_str() == "A-Pending.epub")
        })
        .expect("pending book in catalog");
    catalog[proto::catalog::CATALOG_HEADER_BYTES
        + pending * proto::catalog::CATALOG_RECORD_BYTES
        + proto::catalog::CATALOG_RECORD_TITLE_SOURCE_OFFSET] = TitleSource::Unknown as u8;
    card.delete("READER/CATALOG.BIN");
    card.put_short("READER/CATALOG.BIN", &catalog);
    device.task = storage::task::StorageTask::default();
    device.store.stop_title_index_request();
    assert_eq!(
        catalog_row(&card, BookRoot::Library, "A-Pending.epub").title_source,
        TitleSource::Unknown
    );

    // Resume the evidence job armed before the catalog refresh. A small
    // EPUB completes evidence in this slice; metadata must remain untouched.
    device.task.pending_evidence = Some(evidence);
    device.step_background();
    assert!(device.task.pending_evidence.is_none());
    assert_eq!(
        catalog_row(&card, BookRoot::Library, "A-Pending.epub").title_source,
        TitleSource::Unknown,
        "an evidence-completion slice must not also perform metadata discovery"
    );

    device.step_background();
    assert_eq!(
        catalog_row(&card, BookRoot::Library, "A-Pending.epub").title_source,
        TitleSource::Probed,
        "the next eligible slice should still discover the metadata title"
    );
}
