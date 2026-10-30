//! Books with pictures: the image index built beside the cache as the book
//! opens, the open page's pictures decoded before it is announced, the rest
//! of the book's while the reader is idle, and the index and packs carried
//! with a move.

mod support;

use app_core::{AppView, Button, LibraryEvent};
use proto::image::pack::pack_file;
use support::{zip, Card, Device};
use ui::reading::ReadingBlocks;

const BOOK: &str = "Pictures.epub";
const HOME: &str = "BOOKS/Pictures.epub";
const MOVED: &str = "BOOKS/Art/Pictures.epub";
/// 213 x 300 greyscale: a plate, drawn as an image block.
const PLATE_PNG: &[u8] = include_bytes!("../../fixtures/images/orientation-small.png");
/// 17 x 12 greyscale: a mark small enough to stand inside a line.
const MARK_PNG: &[u8] = include_bytes!("../../fixtures/images/inline-symbol.png");

/// A two-chapter EPUB: a plate at the start of the first chapter, a mark
/// inside a paragraph of the second, and enough text around them to run to
/// several pages. The entries are stored, as the harness's ZIP writes them.
fn picture_book() -> Vec<u8> {
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:test:pictures</dc:identifier><dc:title>Pictures</dc:title><dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="ch2.xhtml" media-type="application/xhtml+xml"/>
    <item id="plate" href="images/plate.png" media-type="image/png"/>
    <item id="mark" href="images/mark.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="ch1"/><itemref idref="ch2"/></spine>
</package>"#;
    let nav = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Pictures</title></head>
<body><nav epub:type="toc"><ol><li><a href="ch1.xhtml">One</a></li><li><a href="ch2.xhtml">Two</a></li></ol></nav></body></html>"#;
    let paragraphs = |chapter: usize| -> String {
        (0..12)
            .map(|p| {
                format!(
                    "<p>Paragraph {p} of chapter {chapter}, with enough ordinary words to \
                     wrap across several lines of the page and fill it the way a novel \
                     does, so that the chapter runs to a few pages.</p>"
                )
            })
            .collect()
    };
    let ch1 = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title></head><body>
<p><img src="images/plate.png" alt="A plate"/></p>{}</body></html>"#,
        paragraphs(1)
    );
    let ch2 = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Two</title></head><body>
<p>A mark <img src="images/mark.png" alt="*"/> stands inside this line of the second chapter.</p>{}</body></html>"#,
        paragraphs(2)
    );
    zip(&[
        ("mimetype", b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml",
            br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#
                .to_vec(),
        ),
        ("OEBPS/content.opf", opf.as_bytes().to_vec()),
        ("OEBPS/nav.xhtml", nav.as_bytes().to_vec()),
        ("OEBPS/ch1.xhtml", ch1.into_bytes()),
        ("OEBPS/ch2.xhtml", ch2.into_bytes()),
        ("OEBPS/images/plate.png", PLATE_PNG.to_vec()),
        ("OEBPS/images/mark.png", MARK_PNG.to_vec()),
    ])
}

/// Open the book from the library, running only what the open itself
/// queues: no background slice has run when this returns.
fn open_book(device: &mut Device) {
    device.open_library();
    device.point_at(BOOK);
    device.press_only(Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
}

/// The one book's cache directory.
fn cache_dir(card: &Card) -> String {
    let keys = card.list("READER/CACHE2");
    assert_eq!(keys.len(), 1, "one cache: {keys:?}");
    format!("READER/CACHE2/{}", keys[0])
}

/// The image records in a pack: each record's `(width, height)`.
fn pack_records(pack: &[u8]) -> Vec<(u16, u16)> {
    let mut records = Vec::new();
    let mut at = 0;
    while at + 16 <= pack.len() {
        let head: &[u8; 16] = pack[at..at + 16].try_into().expect("16 bytes");
        let record = proto::image::pack::PackRecord::decode(head)
            .unwrap_or_else(|| panic!("a record header at {at}"));
        records.push((record.width, record.height));
        at += record.record_len() as usize;
    }
    assert_eq!(at, pack.len(), "the pack is whole records");
    records
}

#[test]
fn a_picture_book_indexes_its_images_and_decodes_the_open_page_first() {
    let card = Card::blank();
    card.put(HOME, &picture_book());
    let mut device = Device::wake(&card);
    open_book(&mut device);
    let store = &*device.store;
    assert!(store.book_has_images(), "the book's images are indexed");
    let first = reader_cache::layout::ReaderPagePlan::new(store, 0).page();
    assert!(
        ui::reading::page_has_images(store, first),
        "the plate opens the book"
    );
    let plate = (0..store.block_count())
        .filter_map(|i| store.block(i))
        .find(|record| record.is_image())
        .expect("an image block");
    assert!(plate.image_height > 0, "layout fitted the plate: {plate:?}");

    let dir = cache_dir(&card);
    let index = card
        .read(&format!("{dir}/IMAGES.BIN"))
        .expect("the image index beside the cache");
    assert_eq!(&index[..4], b"X4IM");
    // The open decoded the page's plate before announcing it, for the
    // orientation the page is laid out in, and noted the page settled.
    let pack = card
        .read(&format!("{dir}/{}", pack_file(store.portrait())))
        .expect("the orientation's pack");
    assert_eq!(pack_records(&pack).len(), 1, "the plate, once");
    assert!(store.images_settled(0), "the open page needs no decode");
    assert!(
        !store.book_images_settled(),
        "the mark in chapter two is still to decode"
    );

    // Idle: the walk, then the rest of the book's images.
    device.settle();
    let store = &*device.store;
    assert!(
        store.book_images_settled(),
        "every image decoded while idle: {:?}",
        device.log
    );
    let pack = card
        .read(&format!("{dir}/{}", pack_file(store.portrait())))
        .expect("the pack");
    let records = pack_records(&pack);
    assert_eq!(records.len(), 2, "the plate and the mark: {records:?}");
    assert_eq!(records[1], (17, 12), "the mark at its own size");
    assert!(
        card.read(&format!("{dir}/{}", pack_file(!store.portrait())))
            .is_none(),
        "nothing decoded for the orientation the reader is not in"
    );
    assert!(
        !device.task.background_owed(&device.store, device.frame()),
        "nothing left owed"
    );
}

#[test]
fn a_press_leaves_the_open_page_to_the_idle_pass() {
    let card = Card::blank();
    card.put(HOME, &picture_book());
    let mut device = Device::wake(&card);
    open_book(&mut device);
    let dir = cache_dir(&card);
    let portrait = device.store.portrait();
    let pack = card
        .read(&format!("{dir}/{}", pack_file(portrait)))
        .expect("the pack");
    let before = pack_records(&pack).len();
    // The reader turns the page while the book is still being laid out;
    // the next page's images are decoded ahead of the frame, and the
    // picture page behind them stays decoded.
    device.press(Button::PageNext);
    device.settle();
    assert!(device.store.book_images_settled(), "{:?}", device.log);
    let pack = card
        .read(&format!("{dir}/{}", pack_file(portrait)))
        .expect("the pack");
    assert!(pack_records(&pack).len() >= before);
    assert!(
        device
            .log
            .iter()
            .any(|event| matches!(event, LibraryEvent::Loaded { .. })),
        "the book was announced"
    );
}

#[test]
fn a_moved_picture_book_keeps_its_index_and_its_decoded_pictures() {
    let card = Card::blank();
    card.put(HOME, &picture_book());
    let mut device = Device::wake(&card);
    open_book(&mut device);
    device.settle();
    assert!(device.store.book_images_settled());
    let dir = cache_dir(&card);
    let portrait = device.store.portrait();
    let pack_name = pack_file(portrait);
    let index_before = card.read(&format!("{dir}/IMAGES.BIN")).expect("index");
    let pack_before = card.read(&format!("{dir}/{pack_name}")).expect("pack");
    device.sleep();

    card.rename(HOME, MOVED);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Art");
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();

    // The cache followed the book: one directory, under the new key, with
    // the index restamped for the new identity and the packs as they were.
    let dir = cache_dir(&card);
    let index_after = card
        .read(&format!("{dir}/IMAGES.BIN"))
        .expect("the carried index");
    assert_eq!(
        index_after[64..],
        index_before[64..],
        "the entries are the ones decoded before the move"
    );
    assert_ne!(
        index_after[8..16],
        index_before[8..16],
        "the header names the new identity"
    );
    let pack_after = card.read(&format!("{dir}/{pack_name}")).expect("pack");
    assert_eq!(pack_after, pack_before, "nothing was decoded again");
    assert!(device.store.book_has_images());
    assert!(
        device.store.book_images_settled(),
        "the carried decodes count: {:?}",
        device.log
    );
}
