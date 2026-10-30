//! The EPUB package (OPF) a build reads its spine from.

mod support;

use app_core::{AppView, Button};
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
    epub_from_package_bytes(items, opf.into_bytes())
}

fn epub_from_package_bytes(items: usize, opf: Vec<u8>) -> Vec<u8> {
    let names: Vec<String> = (0..items).map(|n| format!("O/{n}.xhtml")).collect();
    let mut entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf),
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

/// A package document longer than the OPF scratch buffer streams through the
/// idle XHTML window without prefix truncation, so its chapters are not lost
/// and the book builds whole.
#[test]
fn a_package_longer_than_the_buffer_builds_a_whole_book() {
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
    assert_eq!(
        device.store.book_section_count(),
        60,
        "all chapters build whole"
    );
    assert!(
        !device.store.book_index_is_partial(),
        "a streamed package is whole"
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
    let mut manifest = String::new();
    let mut spine = String::new();
    for n in 0..items {
        manifest.push_str(&format!(
            r#"<item id="c{n}" href="{n}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="c{n}" id="ré{n}"/>"#));
    }
    let opf = format!(
        r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="i">urn:long</dc:identifier><dc:title>Long</dc:title></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
    );
    let spine_start = opf.find("<spine>").expect("the package has a spine");
    let cut = (spine_start + 1500..spine_start + 2000)
        .find(|&i| !opf.is_char_boundary(i))
        .expect("spine has a multi-byte character");
    let mut opf_bytes = opf.into_bytes();
    opf_bytes.truncate(cut);
    let card = Card::blank();
    card.put(
        "BOOKS/Long.epub",
        &epub_from_package_bytes(items, opf_bytes.clone()),
    );
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
    epub_with_a_text_reference(items, padding, "1.xhtml")
}

/// The same book with `href` as the text reference's href.
fn epub_with_a_text_reference(items: usize, padding: usize, href: &str) -> (Vec<u8>, usize, usize) {
    let mut manifest = String::new();
    let mut spine = String::new();
    for n in 0..items {
        manifest.push_str(&format!(
            r#"<item id="c{n}" href="{n}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
    }
    let other = r#"<reference type="other.x" title="Note" href="0.xhtml"/>"#;
    let text = format!(r#"<reference type="text" title="Start" href="{href}"/>"#);
    let guide = format!("{}{text}", other.repeat(padding / other.len()));
    let opf = format!(
        r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="i">urn:long</dc:identifier><dc:title>Long</dc:title></metadata><manifest>{manifest}</manifest><spine>{spine}</spine><guide>{guide}</guide></package>"#
    );
    let spine_end = opf.find("</spine>").expect("the package has a spine") + "</spine>".len();
    let text_at = opf.find(&text).expect("the guide names the text");
    (epub_from_package(items, opf), spine_end, text_at)
}

/// The guide's text reference sets where the book starts, so a cut that
/// falls before it, though after the spine, has lost something the build
/// reads. The build reads the rest of the package for it and starts where
/// the guide says, as it does when the whole package fits: the book has the
/// same sections either way, and is whole. That holds however far past the
/// spine the reference is, including further than any scratch buffer goes.
#[test]
fn a_package_cut_before_its_text_reference_starts_where_the_guide_says() {
    let (fits, _, _) = epub_with_a_late_text_reference(60, 0);
    let (near, spine_end, text_at) = epub_with_a_late_text_reference(60, 14_000);
    assert!(
        spine_end < reader_cache::READER_OPF_SCRATCH,
        "the spine fits in the buffer: it ends at {spine_end}"
    );
    assert!(
        text_at > reader_cache::READER_OPF_SCRATCH,
        "the text reference is past the buffer: it starts at {text_at}"
    );
    let (far, spine_end, text_at) = epub_with_a_late_text_reference(60, 60_000);
    assert!(
        text_at - spine_end > reader_cache::READER_XHTML_SCRATCH,
        "the text reference is further past the spine than the xhtml scratch \
         holds: {} bytes",
        text_at - spine_end
    );
    let mut sections = Vec::new();
    for book in [fits, near, far] {
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
    assert_eq!(sections[2], sections[0], "however far the reference is");
}

/// The start the first step read past the cut holds for the steps after it.
/// Progress counts only the items from the start on, so a continuation that
/// lost it would count the preface again: 60 items where the book has 59.
#[test]
fn a_continuation_keeps_the_start_read_past_the_cut() {
    let (book, _, _) = epub_with_a_late_text_reference(60, 60_000);
    let card = Card::blank();
    card.put("BOOKS/Long.epub", &book);
    let mut device = Device::wake(&card);
    device.open_library();
    device.point_at("Long.epub");
    // Opened with no background step, so the walk is where the first open left it.
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    let book = device.app.book_id;
    let opened = device
        .task
        .build_progress(book)
        .expect("the first open suspends with the rest to build");
    assert_eq!(opened.total, 59, "{opened:?}");
    // A step runs for a time slice, so on a fast host it can finish the walk
    // and leave no progress to read; when it suspends, the count holds.
    device.step_background();
    if let Some(stepped) = device.task.build_progress(book) {
        assert_eq!(stepped.total, 59, "{stepped:?}");
    }
    device.settle();
    assert_eq!(device.store.book_section_count(), 59);
    assert!(!device.store.book_index_is_partial());
}

/// The text reference's href is matched against the spine by its end, so a
/// long one still names chapter 1, and so does a short one whose fragment
/// makes its tag longer than any buffer. The book starts there whether its
/// package fits or is cut before the reference.
#[test]
fn a_long_text_reference_starts_where_the_guide_says() {
    let long_path = format!("{}/1.xhtml", "d".repeat(400));
    let long_fragment = format!("1.xhtml#{}", "f".repeat(2000));
    for href in [long_path, long_fragment] {
        let mut sections = Vec::new();
        for padding in [0, 14_000] {
            let (book, spine_end, text_at) = epub_with_a_text_reference(60, padding, &href);
            if padding > 0 {
                assert!(spine_end < reader_cache::READER_OPF_SCRATCH);
                assert!(text_at > reader_cache::READER_OPF_SCRATCH);
            }
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
        assert_eq!(sections, [59, 59], "{href:.40}");
    }
}

/// A spine item referencing a file missing from the ZIP archive is skipped,
/// preserving valid preceding and succeeding chapters, and marks the book
/// index as partial.
#[test]
fn a_missing_spine_item_between_valid_chapters_marks_the_book_partial() {
    let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="i">urn:missing-spine</dc:identifier>
    <dc:title>Missing Spine</dc:title>
  </metadata>
  <manifest>
    <item id="c0" href="0.xhtml" media-type="application/xhtml+xml"/>
    <item id="c1" href="1.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="2.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="c0"/>
    <itemref idref="c1"/>
    <itemref idref="c2"/>
  </spine>
</package>"#;
    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf.as_bytes().to_vec()),
        (
            "O/0.xhtml",
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Chapter 0.</p></body></html>".to_vec(),
        ),
        // O/1.xhtml is intentionally missing from the ZIP entries.
        (
            "O/2.xhtml",
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Chapter 2.</p></body></html>".to_vec(),
        ),
    ];
    let book = zip(&entries);
    let card = Card::blank();
    card.put("BOOKS/Missing.epub", &book);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Missing.epub");
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();
    // Chapters 0 and 2 were built.
    assert_eq!(device.store.book_section_count(), 2);
    // Missing chapter 1 marks the book partial.
    assert!(device.store.book_index_is_partial());
}

/// An OPF manifest item whose tag exceeds the tokenizer buffer (1,024 bytes)
/// is dropped, leaving an unresolved spine slot that marks the book partial.
#[test]
fn oversized_manifest_item_marks_book_partial() {
    let padding = "x".repeat(1100);
    let opf = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="i">urn:oversized-tag</dc:identifier>
    <dc:title>Oversized Tag</dc:title>
  </metadata>
  <manifest>
    <item id="c0" href="0.xhtml" media-type="application/xhtml+xml"/>
    <item id="c1" href="1.xhtml" media-type="application/xhtml+xml" data-extra="{padding}"/>
  </manifest>
  <spine>
    <itemref idref="c0"/>
    <itemref idref="c1"/>
  </spine>
</package>"#
    );
    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf.into_bytes()),
        (
            "O/0.xhtml",
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Chapter 0.</p></body></html>".to_vec(),
        ),
        (
            "O/1.xhtml",
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Chapter 1.</p></body></html>".to_vec(),
        ),
    ];
    let book = zip(&entries);
    let card = Card::blank();
    card.put("BOOKS/Oversized.epub", &book);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Oversized.epub");
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();
    // Chapter 0 resolved and was built; oversized chapter 1 was dropped.
    assert_eq!(device.store.book_section_count(), 1);
    // Dropping chapter 1 marks the book partial.
    assert!(device.store.book_index_is_partial());
}

/// An OPF metadata tag exceeding the tokenizer buffer (e.g. 2 KB Calibre user
/// metadata) is skipped without marking the book partial, as long as all manifest
/// and spine items resolve.
#[test]
fn oversized_metadata_tag_does_not_mark_book_partial() {
    let padding = "x".repeat(2048);
    let opf = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="i">urn:oversized-meta</dc:identifier>
    <dc:title>Oversized Meta</dc:title>
    <meta name="calibre:user_metadata:custom" content="{padding}"/>
  </metadata>
  <manifest>
    <item id="c0" href="0.xhtml" media-type="application/xhtml+xml"/>
    <item id="c1" href="1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="c0"/>
    <itemref idref="c1"/>
  </spine>
</package>"#
    );
    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf.into_bytes()),
        (
            "O/0.xhtml",
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Chapter 0.</p></body></html>".to_vec(),
        ),
        (
            "O/1.xhtml",
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Chapter 1.</p></body></html>".to_vec(),
        ),
    ];
    let book = zip(&entries);
    let card = Card::blank();
    card.put("BOOKS/MetaOversized.epub", &book);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("MetaOversized.epub");
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();
    // Both chapters resolved and were built.
    assert_eq!(device.store.book_section_count(), 2);
    // Oversized metadata does not mark the book partial.
    assert!(!device.store.book_index_is_partial());
}

/// An unreferenced OPF manifest item (e.g. an auxiliary image or stylesheet)
/// whose tag exceeds the tokenizer buffer does not mark the book partial, as
/// long as all spine items resolve.
#[test]
fn oversized_unreferenced_manifest_item_does_not_mark_book_partial() {
    let padding = "x".repeat(1100);
    let opf = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="i">urn:unref-meta</dc:identifier>
    <dc:title>Unref Manifest</dc:title>
  </metadata>
  <manifest>
    <item id="c0" href="0.xhtml" media-type="application/xhtml+xml"/>
    <item id="aux" href="aux.png" media-type="image/png" data-extra="{padding}"/>
  </manifest>
  <spine>
    <itemref idref="c0"/>
  </spine>
</package>"#
    );
    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf.into_bytes()),
        (
            "O/0.xhtml",
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Chapter 0.</p></body></html>".to_vec(),
        ),
    ];
    let book = zip(&entries);
    let card = Card::blank();
    card.put("BOOKS/UnrefOversized.epub", &book);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("UnrefOversized.epub");
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();
    assert_eq!(device.store.book_section_count(), 1);
    assert!(!device.store.book_index_is_partial());
}

/// An unreferenced OPF manifest item whose `id` attribute appears after an
/// oversized attribute (past the 1,024-byte tag buffer) does not mark the book
/// partial if all referenced spine items successfully resolve.
#[test]
fn oversized_unreferenced_manifest_item_late_id_does_not_mark_book_partial() {
    let padding = "x".repeat(1100);
    let opf = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="i">urn:unref-late-id</dc:identifier>
    <dc:title>Unref Late ID</dc:title>
  </metadata>
  <manifest>
    <item id="c0" href="0.xhtml" media-type="application/xhtml+xml"/>
    <item data-extra="{padding}" id="aux" href="aux.png" media-type="image/png"/>
  </manifest>
  <spine>
    <itemref idref="c0"/>
  </spine>
</package>"#
    );
    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf.into_bytes()),
        (
            "O/0.xhtml",
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Chapter 0.</p></body></html>".to_vec(),
        ),
    ];
    let book = zip(&entries);
    let card = Card::blank();
    card.put("BOOKS/LateIdOversized.epub", &book);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("LateIdOversized.epub");
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();
    assert_eq!(device.store.book_section_count(), 1);
    assert!(!device.store.book_index_is_partial());
}

/// In a fallback-spine EPUB (where the spine has only dangling itemrefs),
/// an oversized auxiliary image is skipped as non-reading content without
/// marking the book partial.
#[test]
fn oversized_auxiliary_image_in_fallback_spine_does_not_mark_book_partial() {
    let padding = "x".repeat(1100);
    let opf = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="i">urn:fallback-aux</dc:identifier>
    <dc:title>Fallback Aux</dc:title>
  </metadata>
  <manifest>
    <item id="c0" href="0.xhtml" media-type="application/xhtml+xml"/>
    <item id="aux" href="aux.png" media-type="image/png" data-extra="{padding}"/>
  </manifest>
  <spine>
    <itemref idref="dangling"/>
  </spine>
</package>"#
    );
    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf.into_bytes()),
        (
            "O/0.xhtml",
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Chapter 0.</p></body></html>".to_vec(),
        ),
    ];
    let book = zip(&entries);
    let card = Card::blank();
    card.put("BOOKS/FallbackAux.epub", &book);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("FallbackAux.epub");
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();
    assert_eq!(device.store.book_section_count(), 1);
    assert!(!device.store.book_index_is_partial());
}

/// An EPUB referencing an oversized SVG manifest item in the spine is marked
/// partial, rather than omitting the SVG chapter and reporting whole.
#[test]
fn oversized_referenced_svg_manifest_item_marks_book_partial() {
    let padding = "x".repeat(1100);
    let opf = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="i">urn:svg-spine</dc:identifier>
    <dc:title>SVG Spine</dc:title>
  </metadata>
  <manifest>
    <item id="c0" href="0.xhtml" media-type="application/xhtml+xml"/>
    <item id="c1" href="1.svg" media-type="image/svg+xml" data-extra="{padding}"/>
  </manifest>
  <spine>
    <itemref idref="c0"/>
    <itemref idref="c1"/>
  </spine>
</package>"#
    );
    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf.into_bytes()),
        (
            "O/0.xhtml",
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Chapter 0.</p></body></html>".to_vec(),
        ),
        (
            "O/1.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"><text>Chapter 1</text></svg>".to_vec(),
        ),
    ];
    let book = zip(&entries);
    let card = Card::blank();
    card.put("BOOKS/SvgSpine.epub", &book);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("SvgSpine.epub");
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();
    assert_eq!(device.store.book_section_count(), 1);
    assert!(device.store.book_index_is_partial());
}

/// In a fallback-spine EPUB (where the spine has only dangling itemrefs),
/// an oversized unreferenced SVG resource is skipped as non-reading content
/// without marking the recovered book partial.
#[test]
fn oversized_unreferenced_svg_in_fallback_spine_does_not_mark_book_partial() {
    let padding = "x".repeat(1100);
    let opf = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="i">urn:fallback-svg</dc:identifier>
    <dc:title>Fallback SVG</dc:title>
  </metadata>
  <manifest>
    <item id="c0" href="0.xhtml" media-type="application/xhtml+xml"/>
    <item id="art" href="cover.svg" media-type="image/svg+xml" data-extra="{padding}"/>
  </manifest>
  <spine>
    <itemref idref="missing"/>
  </spine>
</package>"#
    );
    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf.into_bytes()),
        (
            "O/0.xhtml",
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Chapter 0.</p></body></html>".to_vec(),
        ),
        (
            "O/cover.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"><text>Cover</text></svg>".to_vec(),
        ),
    ];
    let book = zip(&entries);
    let card = Card::blank();
    card.put("BOOKS/FallbackSvg.epub", &book);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("FallbackSvg.epub");
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();
    assert_eq!(device.store.book_section_count(), 1);
    assert!(!device.store.book_index_is_partial());
}

/// When a manifest tag overflows and truncates `media-type` between "image/"
/// and "svg+xml", an incomplete attribute value must not classify the item as a
/// non-reading raster image. Because the referenced spine item cannot resolve,
/// the book must be marked partial.
#[test]
fn oversized_referenced_svg_truncated_media_type_marks_book_partial() {
    let padding = "x".repeat(987);
    let opf = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="i">urn:svg-spine-trunc</dc:identifier>
    <dc:title>SVG Spine Truncated Media</dc:title>
  </metadata>
  <manifest>
    <item id="c0" href="0.xhtml" media-type="application/xhtml+xml"/>
    <item data-extra="{padding}" media-type="image/svg+xml" id="c1" href="1.svg"/>
  </manifest>
  <spine>
    <itemref idref="c0"/>
    <itemref idref="c1"/>
  </spine>
</package>"#
    );
    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf.into_bytes()),
        (
            "O/0.xhtml",
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>Chapter 0.</p></body></html>".to_vec(),
        ),
        (
            "O/1.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"><text>Chapter 1</text></svg>".to_vec(),
        ),
    ];
    let book = zip(&entries);
    let card = Card::blank();
    card.put("BOOKS/SvgTrunc.epub", &book);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("SvgTrunc.epub");
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();
    assert_eq!(device.store.book_section_count(), 1);
    assert!(device.store.book_index_is_partial());
}
