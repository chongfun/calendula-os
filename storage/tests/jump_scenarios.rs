//! A chapter jump into a book still being built.
//!
//! The chapter list is the whole TOC from the first step on, so it offers
//! chapters the progressive walk has not reached. A jump to one cannot load
//! the chapter's page yet; it has to land somewhere honest and follow the
//! walk there, not send the reader to the start of the book.

mod support;

use app_core::{AppView, Button, LibraryEvent};
use support::{epub, zip, Card, Device};

const BOOK: &str = "86 - Volume 02.epub";
const HOME: &str = "BOOKS/86/86 - Volume 02.epub";

/// First open, so the walk is suspended after the first chapter, then
/// `turned` pages read and the chapter list, with no background step in
/// between.
fn on_the_chapter_list_of_a_book_being_built(card: &Card, turned: u32) -> Device {
    card.put(HOME, &epub("86 Volume 2", 6, 2));
    let mut device = Device::wake(card);
    device.open_library();
    device.choose("86");
    device.point_at(BOOK);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    assert!(
        device.task.background_build.is_some(),
        "the walk is suspended"
    );
    for _ in 0..turned {
        device.press_only(Button::PageNext);
        device.run_queued();
    }
    assert_eq!(device.app.page, turned);
    // Portrait: the first press shows the key sheet, the second acts on it.
    device.press_only(Button::Confirm);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Chapters);
    device
}

#[test]
fn a_jump_past_the_walk_follows_it_to_the_chapter() {
    let card = Card::blank();
    let mut device = on_the_chapter_list_of_a_book_being_built(&card, 0);
    for _ in 0..4 {
        device.press_only(Button::Next);
    }
    assert_eq!(device.app.selection, 4, "the fifth chapter");
    let before = device.log.len();
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    let landed_at_start = device.log[before..].iter().any(|event| {
        matches!(
            event,
            LibraryEvent::Loaded {
                position: Some(0),
                ..
            }
        )
    });
    assert!(
        !landed_at_start,
        "the jump did not send the reader to the start: {:?}",
        &device.log[before..]
    );

    device.settle();
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.chapter, 4, "{:?}", &device.log[before..]);
    assert!(device.app.page > 0);
    // The fifth spine item: the spine is the six chapters, the navigation
    // document is not in it.
    let built = device.store.first_page_of_spine(4);
    assert_eq!(Some(device.app.page), built);
}

/// The landing page is not one the reader chose, so a sleep before the walk
/// arrives keeps the page they were reading, even one the coalescer was still
/// holding when they jumped.
#[test]
fn a_sleep_before_the_walk_arrives_keeps_the_page_the_reader_chose() {
    let card = Card::blank();
    let mut device = on_the_chapter_list_of_a_book_being_built(&card, 1);
    for _ in 0..4 {
        device.press_only(Button::Next);
    }
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    assert!(
        device.app.page > 1,
        "landed at the frontier, past the page read"
    );
    device.sleep();

    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.page, 1);
}

/// Once the walk reaches the chapter, the reader is on a page they chose, and
/// a sleep with no page turn after it keeps that page, not the one they
/// jumped from.
#[test]
fn a_sleep_after_the_walk_arrives_keeps_the_chapter_the_reader_chose() {
    let card = Card::blank();
    let mut device = on_the_chapter_list_of_a_book_being_built(&card, 1);
    for _ in 0..4 {
        device.press_only(Button::Next);
    }
    device.press_only(Button::Confirm);
    device.run_queued();
    device.settle();
    assert_eq!(device.app.chapter, 4);
    let chosen = device.app.page;
    assert_eq!(Some(chosen), device.store.first_page_of_spine(4));
    device.sleep();

    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.page, chosen);
}

/// A card that will not take the page the reader was on refuses the jump
/// with it: landing at the frontier would replace that page in the
/// coalescer, and it would be gone before the next flush could retry it.
#[test]
fn a_jump_whose_page_will_not_save_leaves_the_reader_on_that_page() {
    let card = Card::blank();
    let mut device = on_the_chapter_list_of_a_book_being_built(&card, 1);
    for _ in 0..4 {
        device.press_only(Button::Next);
    }
    assert_eq!(
        device.task.pending_progress.map(|record| record.screen),
        Some(1),
        "the page read is still in the coalescer"
    );
    card.disk.refuse_next_writes(u32::MAX);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.page, 1, "back on the page the reader was on");
    assert_eq!(device.app.chapter, 0);
    // The card comes back in time for the sleep's flush to retry it.
    card.disk.refuse_next_writes(0);
    device.sleep();

    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("86");
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.page, 1);
}

/// Three chapters in the table of contents, the middle one a spine item with
/// nothing in it to render: a part title page, say.
fn book_with_an_empty_chapter() -> Vec<u8> {
    book_with_an_empty_chapter_then(&[30])
}

/// Chapter One, then an empty Part Two, then one chapter for each paragraph
/// count in `after`.
fn book_with_an_empty_chapter_then(after: &[usize]) -> Vec<u8> {
    parts_book(after, false)
}

/// As [`book_with_an_empty_chapter_then`], and with `empty_last` an empty
/// Afterword after the last chapter.
fn parts_book(after: &[usize], empty_last: bool) -> Vec<u8> {
    let long = |n: usize, paragraphs: usize| {
        let mut body =
            format!(r#"<html xmlns="http://www.w3.org/1999/xhtml"><body><h1>Chapter {n}</h1>"#);
        for p in 0..paragraphs {
            body.push_str(&format!(
                "<p>Paragraph {p} of chapter {n}, long enough to wrap over a few lines \
                 and so fill the page the way a novel's paragraphs do.</p>"
            ));
        }
        body.push_str("</body></html>");
        body.into_bytes()
    };
    let mut manifest = String::from(
        r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/><item id="p2" href="p2.xhtml" media-type="application/xhtml+xml"/>"#,
    );
    let mut spine = String::from(r#"<itemref idref="c1"/><itemref idref="p2"/>"#);
    let mut toc = String::from(
        r#"<li><a href="c1.xhtml">One</a></li><li><a href="p2.xhtml">Part Two</a></li>"#,
    );
    let mut chapters = Vec::new();
    for (offset, paragraphs) in after.iter().enumerate() {
        let n = offset + 3;
        manifest.push_str(&format!(
            r#"<item id="c{n}" href="c{n}.xhtml" media-type="application/xhtml+xml"/>"#
        ));
        spine.push_str(&format!(r#"<itemref idref="c{n}"/>"#));
        toc.push_str(&format!(r#"<li><a href="c{n}.xhtml">Chapter {n}</a></li>"#));
        chapters.push((format!("O/c{n}.xhtml"), long(n, *paragraphs)));
    }
    if empty_last {
        manifest
            .push_str(r#"<item id="end" href="end.xhtml" media-type="application/xhtml+xml"/>"#);
        spine.push_str(r#"<itemref idref="end"/>"#);
        toc.push_str(r#"<li><a href="end.xhtml">Afterword</a></li>"#);
        chapters.push((
            "O/end.xhtml".to_string(),
            br#"<html xmlns="http://www.w3.org/1999/xhtml"><body></body></html>"#.to_vec(),
        ));
    }
    let opf = format!(
        r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="i"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="i">urn:empty</dc:identifier><dc:title>Parts</dc:title></metadata><manifest>{manifest}</manifest><spine>{spine}</spine></package>"#
    );
    let nav = format!(
        r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol>{toc}</ol></nav></body></html>"#
    );
    let mut entries = vec![
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="O/p.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .to_vec(),
        ),
        ("O/p.opf", opf.into_bytes()),
        ("O/nav.xhtml", nav.into_bytes()),
        ("O/c1.xhtml", long(1, 30)),
        (
            "O/p2.xhtml",
            br#"<html xmlns="http://www.w3.org/1999/xhtml"><body></body></html>"#.to_vec(),
        ),
    ];
    for (name, body) in &chapters {
        entries.push((name.as_str(), body.clone()));
    }
    zip(&entries)
}

/// The empty chapter has no pages of its own, so a jump to it lands where
/// the text after it starts, not at the start of the book.
#[test]
fn a_jump_to_a_chapter_with_nothing_in_it_lands_on_the_next_text() {
    let card = Card::blank();
    card.put("BOOKS/Parts.epub", &book_with_an_empty_chapter());
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Parts.epub");
    device.settle();
    assert!(!device.store.book_index_is_partial(), "the book is whole");
    device.turn(3);
    let third = device
        .store
        .first_page_of_spine(2)
        .expect("chapter three is built");
    assert!(third > 0);
    // Portrait: the first Confirm shows the key sheet, the second acts on it.
    device.press(Button::Confirm);
    device.press(Button::Confirm);
    assert_eq!(device.app.view, AppView::Chapters);
    while device.app.selection != 1 {
        device.press(Button::Next);
    }
    let row = if device.store.text_holds_toc() {
        device.store.overview_row_page(1)
    } else {
        device.store.toc_item(1).expect("resident row").page
    };
    assert_eq!(row, third + 1, "the row shows the page the jump lands on");
    device.press(Button::Confirm);
    device.settle();
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.page, third, "{:?}", device.log);
}

/// The chapter list of the book with an empty chapter, opened for the first
/// time, so the walk is suspended after the first chapter.
fn on_the_chapter_list_of_parts_being_built() -> Device {
    let card = Card::blank();
    card.put("BOOKS/Parts.epub", &book_with_an_empty_chapter());
    let mut device = Device::wake(&card);
    device.open_library();
    device.point_at("Parts.epub");
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    assert!(
        device.store.book_index_is_partial(),
        "the walk is suspended"
    );
    device.press_only(Button::Confirm);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Chapters);
    device
}

/// A chapter the walk has not reached has no page to show on the chapter
/// list yet. The row's page is 0, which the list draws as no number, rather
/// than the last page built, which every unbuilt row would then share.
#[test]
fn the_chapter_list_shows_no_page_for_a_chapter_the_walk_has_not_reached() {
    let device = on_the_chapter_list_of_parts_being_built();
    assert_eq!(device.store.first_page_of_spine(2), None, "not built yet");
    assert_eq!(device.store.overview_page_at(2), 0);
}

/// An empty chapter in a book still being built: the reader follows the walk
/// to the text after it, and is not left on the page the jump landed on.
#[test]
fn a_jump_to_an_empty_chapter_the_walk_has_not_reached_follows_it_to_the_next_text() {
    let mut device = on_the_chapter_list_of_parts_being_built();
    device.press_only(Button::Next);
    assert_eq!(device.app.selection, 1, "Part Two");
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    device.settle();
    assert!(!device.store.book_index_is_partial(), "the book is whole");
    let third = device
        .store
        .first_page_of_spine(2)
        .expect("chapter three is built");
    assert_eq!(device.app.page, third, "{:?}", device.log);
}

/// An empty chapter the walk has passed, in a book it is still building: the
/// text after it is built, so the jump lands there at once rather than at the
/// furthest page built, waiting for a later slice to move the reader back.
#[test]
fn a_jump_to_an_empty_chapter_the_walk_has_passed_lands_on_the_next_text() {
    // Seventeen short chapters, so one background step builds enough of them
    // to write the index to the card, which the jump reads back, and a last
    // one long enough that the step stops before it and leaves the book
    // partial.
    let mut after = vec![2; 17];
    after.push(90);
    let card = Card::blank();
    card.put("BOOKS/Parts.epub", &book_with_an_empty_chapter_then(&after));
    let mut device = Device::wake(&card);
    device.open_library();
    device.point_at("Parts.epub");
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    while device.store.first_page_of_spine(2).is_none() {
        assert!(device.store.book_index_is_partial(), "the walk is going");
        device.step_background();
    }
    assert!(
        device.store.book_index_is_partial(),
        "the last chapter is not built"
    );
    let third = device
        .store
        .first_page_of_spine(2)
        .expect("chapter three is built");
    assert!(third > 0);
    device.press_only(Button::Confirm);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Chapters);
    device.press_only(Button::Next);
    assert_eq!(device.app.selection, 1, "Part Two");
    let before = device.log.len();
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.app.page, third, "{:?}", &device.log[before..]);
}

/// An empty last chapter in a book still being built: once the walk finishes
/// with no text after it, the reader follows it to the last page, not left
/// on the page the jump landed on.
#[test]
fn a_jump_to_an_empty_last_chapter_the_walk_has_not_reached_ends_on_the_last_page() {
    let card = Card::blank();
    card.put("BOOKS/Parts.epub", &parts_book(&[30], true));
    let mut device = Device::wake(&card);
    device.open_library();
    device.point_at("Parts.epub");
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    assert!(
        device.store.book_index_is_partial(),
        "the walk is suspended"
    );
    device.press_only(Button::Confirm);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Chapters);
    while device.app.selection != 3 {
        device.press_only(Button::Next);
    }
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading);
    let landed = device.app.page;
    device.settle();
    assert!(!device.store.book_index_is_partial(), "the book is whole");
    let last = device.store.advertised_page_count() - 1;
    assert!(last > landed, "chapter three was built after the jump");
    assert_eq!(device.app.page, last, "{:?}", device.log);
}
