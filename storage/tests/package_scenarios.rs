//! The EPUB package (OPF) a build reads its spine from.

mod support;

use app_core::AppView;
use support::{zip, Card, Device};

/// An EPUB whose package document is `padding` bytes of description longer
/// than it needs to be, with `items` short chapters.
fn epub_with_a_long_package(items: usize, padding: usize) -> (Vec<u8>, usize) {
    let mut manifest = String::new();
    let mut spine = String::new();
    for n in 0..items {
        manifest.push_str(&format!(
            r#"<item id="c{n}" href="{n}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
    }
    let description = "A long description. ".repeat(padding / 20);
    let opf = format!(
        r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="i">urn:long</dc:identifier><dc:title>Long</dc:title><dc:description>{description}</dc:description></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
    );
    let opf_len = opf.len();
    let names: Vec<String> = (0..items).map(|n| format!("O/{n}.xhtml")).collect();
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
            "<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><h1>Part {n}</h1>\
             <p>One page of a long book.</p></body></html>"
        );
        entries.push((name.as_str(), body.into_bytes()));
    }
    (zip(&entries), opf_len)
}

/// A package longer than the parser's buffer is read only as far as the
/// buffer goes, so its spine is cut short. The book built from it is missing
/// its later chapters, and has to say so: the index is partial, as it is
/// when the spine overflows its own cap, not a whole book that happens to be
/// short.
#[test]
fn a_package_longer_than_the_buffer_builds_a_partial_book() {
    let (book, opf_len) = epub_with_a_long_package(60, 14_000);
    assert!(
        opf_len > reader_cache::READER_OPF_SCRATCH,
        "the package outgrows the buffer: {opf_len} bytes"
    );
    let card = Card::blank();
    card.put("BOOKS/Long.epub", &book);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Long.epub");
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();
    assert!(
        device.store.book_section_count() < 60,
        "the spine was cut short: {} sections",
        device.store.book_section_count()
    );
    assert!(
        device.store.book_index_is_partial(),
        "a book missing chapters is partial"
    );
}

/// The same book with a package that fits is whole.
#[test]
fn a_package_that_fits_builds_a_whole_book() {
    let (book, opf_len) = epub_with_a_long_package(60, 0);
    assert!(opf_len < reader_cache::READER_OPF_SCRATCH);
    let card = Card::blank();
    card.put("BOOKS/Long.epub", &book);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Long.epub");
    device.settle();
    assert_eq!(device.store.book_section_count(), 60);
    assert!(!device.store.book_index_is_partial());
}
