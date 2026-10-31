//! The EPUB package (OPF) a build reads its spine from.

mod support;

use app_core::{AppView, Button};
use display::font::FontSize;
use support::{zip, Card, Device};
use ui::reading::ReadingBlocks;

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
/// preface, and `padding` bytes of other guide references before it. Also
/// returns where the spine's end tag ends and where the text reference starts
/// in the package.
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

/// A package cut after its spine, before its guide's text reference, builds
/// the same whole book as one that fits: the build starts at the first spine
/// item whatever the guide says, preface included. That holds however far
/// past the spine the reference is, including further than any scratch
/// buffer goes.
#[test]
fn a_package_cut_before_its_text_reference_builds_the_whole_book() {
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
    assert_eq!(
        sections[0], 60,
        "every chapter including the preface is in the book"
    );
    assert_eq!(
        sections[1], sections[0],
        "the cut book starts in the same place"
    );
    assert_eq!(sections[2], sections[0], "however far the reference is");
}

/// A cut package's continuation steps build every item the first step
/// counted, preface included.
#[test]
fn a_continuation_of_a_cut_package_builds_every_item() {
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
    assert_eq!(opened.total, 60, "{opened:?}");
    // A step runs for a time slice, so on a fast host it can finish the walk
    // and leave no progress to read; when it suspends, the count holds.
    device.step_background();
    if let Some(stepped) = device.task.build_progress(book) {
        assert_eq!(stepped.total, 60, "{stepped:?}");
    }
    device.settle();
    assert_eq!(device.store.book_section_count(), 60);
    assert!(!device.store.book_index_is_partial());
}

/// A long text reference, or a short one whose fragment makes its tag longer
/// than any buffer, changes nothing: the book is the same whole book whether
/// its package fits or is cut before the reference.
#[test]
fn a_long_text_reference_leaves_the_book_whole() {
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
        assert_eq!(sections, [60, 60], "{href:.40}");
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

/// A stale CONT.BIN from version 4 must be rejected with BadVersion, deleted,
/// and rebuilt cleanly from the EPUB with CONTENT_VERSION 5 and correct hyphenation.
#[test]
fn stale_v4_content_cache_is_rejected_and_rebuilt() {
    let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="i">urn:v4-stale</dc:identifier>
    <dc:title>V4 Stale</dc:title>
    <dc:language>en-US</dc:language>
  </metadata>
  <manifest>
    <item id="c0" href="0.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="c0"/>
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
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>English reading chapter.</p></body></html>".to_vec(),
        ),
    ];
    let book = zip(&entries);
    let card = Card::blank();
    card.put("BOOKS/V4.epub", &book);

    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("V4.epub");
    assert_eq!(device.app.view, AppView::Reading);
    device.settle();

    // Locate CONT.BIN on the card.
    let dirs = card.list("READER/CACHE2");
    assert!(!dirs.is_empty(), "cache dir created");
    let cont_path = format!("READER/CACHE2/{}/CONT.BIN", dirs[0]);
    let mut cont_bytes = card.read(&cont_path).expect("CONT.BIN exists");

    // Overwrite version to 4 and hyphenation byte 7 to 0 (as produced by v4).
    cont_bytes[4..6].copy_from_slice(&4u16.to_le_bytes());
    cont_bytes[7] = 0;
    card.delete(&cont_path);
    card.put_short(&cont_path, &cont_bytes);

    device.sleep();

    // Reopen under a different layout configuration to trigger replay.
    let mut device = Device::wake(&card);
    device.app.font_size = FontSize::Large;
    device.open_library();
    device.choose("V4.epub");
    assert_eq!(device.app.view, AppView::Reading);
    device.settle();

    // Replay must have failed on the v4 header, deleted the stale cache, and rebuilt it with v6.
    let updated_bytes = card.read(&cont_path).expect("rebuilt CONT.BIN exists");
    let version = u16::from_le_bytes([updated_bytes[4], updated_bytes[5]]);
    assert_eq!(version, proto::cache::CONTENT_VERSION);
    assert_eq!(version, 6);
    // English US hyphenation captured.
    assert_eq!(updated_bytes[7], 1);
}

/// Repaired lines in a paragraph must advance their logical source anchors strictly
/// monotonically based on each line's actual starting word source offset, rather than
/// by rendered byte lengths (which drift when styling markers or hyphens are involved).
#[test]
fn repaired_lines_keep_strictly_monotonic_logical_source_anchors() {
    let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="i">urn:repair-anchor</dc:identifier>
    <dc:title>Repair Anchor</dc:title>
    <dc:language>en-US</dc:language>
  </metadata>
  <manifest>
    <item id="c0" href="0.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="c0"/>
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
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body>\
              <p>The philosophical investigation concerning foundational epistemology \
              <em>demonstrates extraordinary perseverance</em> throughout comprehensive \
              multidisciplinary analysis of extraordinarily sophisticated phenomenological \
              hypotheses and architectural manifestations.</p>\
              </body></html>".to_vec(),
        ),
    ];
    let book = zip(&entries);
    let card = Card::blank();
    card.put("BOOKS/Repair.epub", &book);

    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Repair.epub");
    assert_eq!(device.app.view, AppView::Reading);
    device.settle();

    let count = device.store.block_count();
    assert!(
        count >= 3,
        "expected multiple lines in justified paragraph: {count}"
    );

    let mut prev_offset = None;
    for i in 0..count {
        let offset = device.store.block_offset(i).expect("block offset exists");
        if let Some(prev) = prev_offset {
            assert!(
                offset > prev,
                "block offsets must be strictly monotonic: block {i} offset {offset} <= prev {prev}"
            );
        }
        prev_offset = Some(offset);
    }
}

/// A package of `chapters` one-paragraph items whose guide names item
/// `text_start` as the start of the text.
fn epub_with_front_matter(chapters: &[&str], text_start: usize) -> Vec<u8> {
    let mut manifest = String::new();
    let mut spine = String::new();
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for (n, text) in chapters.iter().enumerate() {
        manifest.push_str(&format!(
            r#"<item id="c{n}" href="{n}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
        entries.push((
            format!("O/{n}.xhtml"),
            format!(
                r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><p>{text}</p></body></html>"#
            )
            .into_bytes(),
        ));
    }
    let opf = format!(
        r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="i">urn:front-matter</dc:identifier><dc:title>Front Matter</dc:title></metadata><manifest>{manifest}</manifest><spine>{spine}</spine><guide><reference type="text" title="Start" href="{text_start}.xhtml"/></guide></package>"#
    );
    let mut all: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf.into_bytes()),
    ];
    all.extend(
        entries
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.clone())),
    );
    zip(&all)
}

/// A first open starts at the book's first page, cover included, even when
/// the guide names a later item as the start of the text: few books name
/// their first chapter, and the cover is where the reader begins.
#[test]
fn a_first_open_starts_at_the_cover_whatever_the_guide_says() {
    let book = epub_with_front_matter(
        &[
            "The cover.",
            "A foreword before the story.",
            "Chapter one begins the story.",
            "Chapter two goes on.",
        ],
        2,
    );
    let card = Card::blank();
    card.put("BOOKS/Front.epub", &book);
    let mut device = Device::wake(&card);
    device.open_library();
    device.point_at("Front.epub");
    device.press_only(Button::Confirm);
    device.run_queued();

    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.page, 0);
    let shown: Vec<&str> = (0..device.store.block_count())
        .map(|i| device.store.block_text(i))
        .collect();
    assert!(
        shown.iter().any(|line| line.contains("The cover.")),
        "{shown:?}"
    );
    device.settle();
    assert_eq!(
        device.store.book_section_count(),
        4,
        "the front matter is part of the book"
    );
}

/// Each stored line's anchor is where its first word, or the tail of a word
/// split at the line before, starts in the source paragraph. A split word is
/// charged its separator once, and a soft hyphen it breaks at is skipped,
/// so later lines do not drift past their text however many words split.
#[test]
fn line_anchors_match_the_source_across_hyphenated_lines() {
    // No dashes, so every line-end hyphen is one the build drew; the soft
    // hyphens (U+00AD) fix some of the breaks, the patterns find the rest.
    // Under the parser's 384-byte block, so the paragraph is one block and
    // its stream offsets are its bytes.
    let source = "The philosophical investigation concerning foundational \
        episte\u{ad}mology demonstrates extraordinary perseverance through\u{ad}out \
        comprehensive multidisciplinary analysis of extraordinarily sophisticated \
        phenomeno\u{ad}logical hypotheses and architectural manifestations, \
        notwithstanding incomprehensible counterrevolutionary misunderstandings \
        regarding internationalization.";
    let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="i">urn:hyphen-anchor</dc:identifier>
    <dc:title>Hyphen Anchor</dc:title>
    <dc:language>en-US</dc:language>
  </metadata>
  <manifest>
    <item id="c0" href="0.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="c0"/>
  </spine>
</package>"#;
    let xhtml = format!(
        r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><p>{source}</p></body></html>"#
    );
    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf.as_bytes().to_vec()),
        ("O/0.xhtml", xhtml.into_bytes()),
    ];
    let card = Card::blank();
    card.put("BOOKS/Hyphens.epub", &zip(&entries));
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Hyphens.epub");
    assert_eq!(device.app.view, AppView::Reading);
    device.settle();

    let store = &*device.store;
    let count = store.block_count();
    let base = store.block_offset(0).expect("the paragraph has a line");
    let mut at = 0usize;
    let mut hyphenated = 0;
    for i in 0..count {
        let line = store.block_text(i);
        // A line breaks at a space, which belongs to neither line.
        while source[at..].starts_with(' ') {
            at += 1;
        }
        assert_eq!(
            store.block_offset(i),
            Some(base + at as u32),
            "line {i} {line:?} starts at source byte {at}"
        );
        let drawn = line.strip_suffix('-');
        hyphenated += usize::from(drawn.is_some());
        for ch in drawn.unwrap_or(line).chars() {
            // Lines never hold the soft hyphens a word carries.
            while source[at..].starts_with('\u{ad}') {
                at += '\u{ad}'.len_utf8();
            }
            assert!(
                source[at..].starts_with(ch),
                "line {i} {line:?} has {ch:?} where the source has {:?}",
                &source[at..]
            );
            at += ch.len_utf8();
        }
        // The tail of a word broken at a soft hyphen resumes after it.
        if drawn.is_some() && source[at..].starts_with('\u{ad}') {
            at += '\u{ad}'.len_utf8();
        }
    }
    assert!(source.len() < 384, "one parser block: {}", source.len());
    assert_eq!(at, source.len(), "every line was matched to the source");
    assert!(
        hyphenated >= 2,
        "the paragraph splits words at several line ends: {hyphenated} of {count} lines"
    );
}
