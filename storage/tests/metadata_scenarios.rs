//! Idle EPUB probes update title sources without opening reading caches.
mod support;

use proto::catalog::TitleSource;
use proto::library_path::BookRoot;
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
        if next == cursor {
            break;
        }
        cursor = next;
    }
    assert_eq!(cursor, pending);

    // Refused reads must never advance the cursor past unverified entries.
    card.session(|root| {
        card.disk.refuse_read_in(Some(0));
        assert!(storage::library_sd::skip_settled_catalog_rows(root, 0, count).is_err());
    });
    card.disk.refuse_read_in(None);
}
