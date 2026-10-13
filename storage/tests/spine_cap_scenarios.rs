//! A book with more chapters than the spine holds.

mod support;

use app_core::AppView;
use support::{zip, Card, Device};

/// More spine items than `MAX_SPINE_ITEMS`, with short ids and hrefs so the
/// package stays well inside the parser's buffer: the cap is the only limit
/// in play.
fn capped_epub(items: usize) -> Vec<u8> {
    let mut manifest = String::new();
    let mut spine = String::new();
    for n in 0..items {
        manifest.push_str(&format!(r#"<item id="c{n}" href="{n}.x"/>"#));
        spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
    }
    let opf = format!(
        r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="i">urn:capped</dc:identifier><dc:title>Capped</dc:title></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
    );
    assert!(
        opf.len() < reader_cache::READER_OPF_SCRATCH,
        "{} bytes",
        opf.len()
    );
    let names: Vec<String> = (0..items).map(|n| format!("O/{n}.x")).collect();
    let mut entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf.into_bytes()),
    ];
    for (n, name) in names.iter().enumerate() {
        let body = format!(
            "<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Part {n}.</p></body></html>"
        );
        entries.push((name.as_str(), body.into_bytes()));
    }
    zip(&entries)
}

/// The build keeps the first `MAX_SPINE_ITEMS` chapters and drops the rest,
/// and the book it publishes says it is partial rather than whole.
#[test]
fn a_book_over_the_spine_cap_is_built_partial() {
    let card = Card::blank();
    card.put(
        "BOOKS/Capped.epub",
        &capped_epub(proto::epub::MAX_SPINE_ITEMS + 4),
    );
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Capped.epub");
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();
    assert_eq!(
        device.store.book_section_count(),
        proto::epub::MAX_SPINE_ITEMS
    );
    assert!(device.store.book_index_is_partial());
}

/// Exactly at the cap is a whole book.
#[test]
fn a_book_at_the_spine_cap_is_whole() {
    let card = Card::blank();
    card.put(
        "BOOKS/Capped.epub",
        &capped_epub(proto::epub::MAX_SPINE_ITEMS),
    );
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Capped.epub");
    device.settle();
    assert_eq!(
        device.store.book_section_count(),
        proto::epub::MAX_SPINE_ITEMS
    );
    assert!(!device.store.book_index_is_partial());
}
