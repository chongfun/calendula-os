//! The EPUB package (OPF) a build reads its spine from.

mod support;

use app_core::AppView;
use support::{zip, Card, Device};

/// An EPUB whose package document is `padding` bytes of description longer
/// than it needs to be, with `items` short chapters.
fn epub_with_a_long_package(items: usize, padding: usize) -> (Vec<u8>, usize) {
    epub_with_package_padding(items, padding, 0).0
}

/// The same book with `guide_padding` bytes of guide references after the
/// spine as well. Also returns where the spine's end tag ends in the package.
fn epub_with_package_padding(
    items: usize,
    padding: usize,
    guide_padding: usize,
) -> ((Vec<u8>, usize), usize) {
    let mut manifest = String::new();
    let mut spine = String::new();
    for n in 0..items {
        manifest.push_str(&format!(
            r#"<item id="c{n}" href="{n}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
    }
    let description = "A long description. ".repeat(padding / 20);
    let reference = r#"<reference type="other.x" title="Note" href="0.xhtml"/>"#;
    let guide = reference.repeat(guide_padding / reference.len());
    let opf = format!(
        r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="i">urn:long</dc:identifier><dc:title>Long</dc:title><dc:description>{description}</dc:description></metadata><manifest>{manifest}</manifest><spine>{spine}</spine><guide>{guide}</guide></package>"#
    );
    let opf_len = opf.len();
    let spine_end = opf.find("</spine>").expect("the package has a spine") + "</spine>".len();
    ((epub_from_package(items, opf), opf_len), spine_end)
}

/// An EPUB of `items` short chapters, `O/0.xhtml` onward, around `opf`.
fn epub_from_package(items: usize, opf: String) -> Vec<u8> {
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
    zip(&entries)
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

/// A package cut after its spine has closed lost only what follows the spine
/// (here, guide references), not a chapter. Every section is built and the
/// book is whole: marking it partial would give up its content stream and
/// its progression restore for nothing.
#[test]
fn a_package_cut_after_its_spine_builds_a_whole_book() {
    let ((book, opf_len), spine_end) = epub_with_package_padding(60, 0, 14_000);
    assert!(
        spine_end < reader_cache::READER_OPF_SCRATCH,
        "the spine fits in the buffer: it ends at {spine_end}"
    );
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
    assert_eq!(device.store.book_section_count(), 60);
    assert!(
        !device.store.book_index_is_partial(),
        "a book with its whole spine is not partial"
    );
}

/// The buffer's end can fall inside a character. Each spine entry here
/// carries a non-ASCII id, and the description is sized so the cut lands in
/// the middle of one. The prefix is then not valid UTF-8 as it stands, but
/// everything before that character is: the book opens partial on the
/// chapters the prefix holds, as a cut between characters does, rather than
/// failing to open at all.
#[test]
fn a_package_cut_inside_a_character_builds_a_partial_book() {
    let items = 200;
    let scratch = reader_cache::READER_OPF_SCRATCH;
    let mut manifest = String::new();
    let mut spine = String::new();
    for n in 0..items {
        manifest.push_str(&format!(
            r#"<item id="c{n}" href="{n}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="c{n}" id="ré{n}"/>"#));
    }
    let opf = (0..64)
        .map(|pad| {
            let description = "x".repeat(pad);
            format!(
                r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="i">urn:long</dc:identifier><dc:title>Long</dc:title><dc:description>{description}</dc:description></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
            )
        })
        .find(|opf| opf.len() > scratch && !opf.is_char_boundary(scratch))
        .expect("some padding puts the buffer's end inside a character");
    let spine_start = opf.find("<spine>").expect("the package has a spine");
    assert!(spine_start < scratch, "the cut falls in the spine");
    let card = Card::blank();
    card.put("BOOKS/Long.epub", &epub_from_package(items, opf));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Long.epub");
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();
    let sections = device.store.book_section_count();
    assert!(
        sections > 0 && sections < items,
        "the spine was cut short: {sections} sections"
    );
    assert!(device.store.book_index_is_partial());
}

/// A book whose guide names chapter 1 as where the text starts, after a
/// preface the build would otherwise keep, and `padding` bytes of other
/// guide references before it. Also returns where the spine's end tag ends
/// and where the text reference starts in the package.
fn epub_with_a_late_text_reference(items: usize, padding: usize) -> (Vec<u8>, usize, usize) {
    let mut manifest = String::new();
    let mut spine = String::new();
    for n in 0..items {
        manifest.push_str(&format!(
            r#"<item id="c{n}" href="{n}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
    }
    let other = r#"<reference type="other.x" title="Note" href="0.xhtml"/>"#;
    let text = r#"<reference type="text" title="Start" href="1.xhtml"/>"#;
    let guide = format!("{}{text}", other.repeat(padding / other.len()));
    let opf = format!(
        r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="i">urn:long</dc:identifier><dc:title>Long</dc:title></metadata><manifest>{manifest}</manifest><spine>{spine}</spine><guide>{guide}</guide></package>"#
    );
    let spine_end = opf.find("</spine>").expect("the package has a spine") + "</spine>".len();
    let text_at = opf.find(text).expect("the guide names the text");
    (epub_from_package(items, opf), spine_end, text_at)
}

/// The guide's text reference sets where the book starts, so a cut that
/// falls before it, though after the spine, has lost something the build
/// reads. The build reads the rest of the package for it and starts where
/// the guide says, as it does when the whole package fits: the book has the
/// same sections either way, and is whole.
#[test]
fn a_package_cut_before_its_text_reference_starts_where_the_guide_says() {
    let (fits, _, _) = epub_with_a_late_text_reference(60, 0);
    let (cut, spine_end, text_at) = epub_with_a_late_text_reference(60, 14_000);
    assert!(
        spine_end < reader_cache::READER_OPF_SCRATCH,
        "the spine fits in the buffer: it ends at {spine_end}"
    );
    assert!(
        text_at > reader_cache::READER_OPF_SCRATCH,
        "the text reference is past the buffer: it starts at {text_at}"
    );
    let mut sections = Vec::new();
    for book in [fits, cut] {
        let card = Card::blank();
        card.put("BOOKS/Long.epub", &book);
        let mut device = Device::wake(&card);
        device.open_library();
        device.choose("Long.epub");
        assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
        device.settle();
        assert!(!device.store.book_index_is_partial());
        sections.push(device.store.book_section_count());
    }
    assert_eq!(sections[0], 59, "the preface before the text is left out");
    assert_eq!(
        sections[1], sections[0],
        "the cut book starts in the same place"
    );
}
