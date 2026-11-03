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
        let mut device = Device::wake_queued_only(&card);
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
