//! A book whose spine names nothing, with more chapters than the spine holds.
//!
//! When no itemref resolves, the parser reads the spine from the manifest's
//! reading candidates instead. That fallback is capped like the spine itself.

mod support;

use app_core::AppView;
use support::{zip, Card, Device};

/// `items` chapters in the manifest and a spine whose one itemref names none
/// of them. Short hrefs keep the package well inside the parser's buffer, so
/// the cap is the only limit in play.
fn loose_epub(items: usize) -> Vec<u8> {
    let mut manifest = String::new();
    for n in 0..items {
        manifest.push_str(&format!(r#"<item id="c{n}" href="{n}.html"/>"#));
    }
    let opf = format!(
        r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="i">urn:loose</dc:identifier><dc:title>Loose</dc:title></metadata><manifest>{manifest}</manifest><spine><itemref idref="missing"/></spine></package>"#
    );
    assert!(
        opf.len() < reader_cache::READER_OPF_SCRATCH,
        "{} bytes",
        opf.len()
    );
    let names: Vec<String> = (0..items).map(|n| format!("O/{n}.html")).collect();
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

/// The build keeps the first `MAX_SPINE_ITEMS` chapters, and the book it
/// publishes says it is partial rather than whole.
#[test]
fn a_fallback_book_over_the_spine_cap_is_built_partial() {
    let card = Card::blank();
    card.put(
        "BOOKS/Loose.epub",
        &loose_epub(proto::epub::MAX_SPINE_ITEMS + 4),
    );
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Loose.epub");
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
fn a_fallback_book_at_the_spine_cap_is_whole() {
    let card = Card::blank();
    card.put(
        "BOOKS/Loose.epub",
        &loose_epub(proto::epub::MAX_SPINE_ITEMS),
    );
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Loose.epub");
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();
    assert_eq!(
        device.store.book_section_count(),
        proto::epub::MAX_SPINE_ITEMS
    );
    assert!(!device.store.book_index_is_partial());
}
