//! Books with pictures: the image index built beside the cache as the book
//! opens, the open page's pictures decoded before it is announced, the rest
//! of the book's while the reader is idle, and the index and packs carried
//! with a move.

mod support;

use app_core::{AppView, Button, LibraryEvent};
use display::font::FontSize;
use proto::image::pack::{pack_file, SPOOL_FILE};
use support::{zip, Card, Device};
use ui::reading::ReadingBlocks;

const BOOK: &str = "Pictures.epub";
const HOME: &str = "BOOKS/Pictures.epub";
const MOVED: &str = "BOOKS/Art/Pictures.epub";
/// 213 x 300 greyscale: a plate, drawn as an image block.
const PLATE_PNG: &[u8] = include_bytes!("../../fixtures/images/orientation-small.png");
/// 17 x 12 greyscale: a mark small enough to stand inside a line.
const MARK_PNG: &[u8] = include_bytes!("../../fixtures/images/inline-symbol.png");

/// 75 x 75 greyscale: an initial, shown inside a line at 64 rows and as a
/// block at its own size.
const INITIAL_PNG: &[u8] = include_bytes!("../../fixtures/images/inline-initial.png");

/// A two-chapter EPUB: a plate at the start of the first chapter, a mark
/// inside a paragraph of the second, and enough text around them to run to
/// several pages. The entries are stored, as the harness's ZIP writes them.
fn picture_book() -> Vec<u8> {
    picture_book_with(MARK_PNG)
}

/// [`picture_book`], its mark drawn from `mark`.
fn picture_book_with(mark: &[u8]) -> Vec<u8> {
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
        ("OEBPS/images/mark.png", mark.to_vec()),
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
    // A spool a cut decode left, which the move does not carry: the
    // departed directory's reclaim frees it.
    card.put_short(&format!("{dir}/{SPOOL_FILE}"), b"cut short");
    device.sleep();

    card.rename(HOME, MOVED);
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Art");
    device.choose(BOOK);
    assert_eq!(device.app.view, AppView::Reading, "{:?}", device.log);
    device.settle();

    // The cache followed the book: one directory, under the new key, with
    // the index restamped for the new identity and the packs as they were,
    // and the departed directory reclaimed, spool and all.
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

/// A one-chapter book holding one paragraph, `text`, beside the mark.
fn marked_paragraph_book(text: &str) -> Vec<u8> {
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:test:marks</dc:identifier><dc:title>Marks</dc:title><dc:language>en-US</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="mark" href="images/mark.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#;
    let ch1 = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title></head><body><p>{text}</p></body></html>"#
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
        ("OEBPS/ch1.xhtml", ch1.into_bytes()),
        ("OEBPS/images/mark.png", MARK_PNG.to_vec()),
    ])
}

/// The lines of a justified paragraph that the page would leave ragged:
/// every line but its last, set as the drawer sets it, with an inline
/// image line judged by the inline rule.
fn ragged_lines(store: &reader_cache::store::ReaderStore) -> Vec<String> {
    let settings = store.type_settings();
    let page_box = store.page_box();
    (0..store.block_count())
        .filter(|&index| !store.paragraph_end(index))
        .filter_map(|index| {
            let record = store.block(index)?;
            let text = store.block_text(index);
            let style = store.block_style(index);
            let x =
                page_box.x_for(record.role) + ui::reading::block_first_line_indent(store, index);
            let width = ui::reading::inline_line_ink_width(text, settings, style);
            let gaps = ui::reading::justify_gap_count(text);
            if x + width >= page_box.right - ui::reading::READER_WRAP_SAFETY {
                return None;
            }
            let justified = if record.has_inline_images() {
                ui::reading::justify_inline_extra(x, width, page_box.right, gaps)
            } else {
                let space = ui::reading::word_space_width(settings, style);
                ui::reading::justify_extra(x, width, page_box.right, gaps, space)
            };
            justified.is_none().then(|| text.to_owned())
        })
        .collect()
}

/// A mark inside a justified paragraph is one of the line window's words:
/// the lines around it are broken again for the best fit, and the mark
/// comes through every repair, once, on a line marked as holding it. Each
/// paragraph here keeps a ragged line when the window lets go of its
/// words at the mark.
#[test]
fn a_mark_inside_a_justified_paragraph_rides_through_line_repair() {
    let img = r#"<img src="images/mark.png" alt="*"/>"#;
    let paragraphs = [
        (
            "They were indeed a queer-looking party that assembled on the bank, the birds \
             with draggled feathers, the animals with their fur clinging close to them, and \
             all dripping wet, cross, and uncomfortable. The first question of course was, \
             how to get dry again: they had a consultation about this, and after a few \
             minutes it seemed quite natural to Alice to find herself talking familiarly \
             with them, as if she had known them all her life. Indeed, she had quite a long \
             argument with the Lory, who at last turned sulky, and would only say, I am \
             older than you, and must know better.",
            20,
        ),
        (
            "Complete the parallelogram AEBG. Draw DH and CI parallel to AG and BG \
             respectively. Join IH, and produce it; then AB, CD and IH are concurrent, and \
             the parallelogram on the diagonal is equal to the parallelogram beside it, \
             which was to be shown. The same construction serves for every pair of \
             complements about the diagonal of any parallelogram whatever, and the proof \
             is unchanged when the figure is drawn on the other side of the line.",
            2,
        ),
    ];
    for (paragraph, at) in paragraphs {
        let mut words: Vec<&str> = paragraph.split(' ').collect();
        words.insert(at, img);
        let card = Card::blank();
        card.put(HOME, &marked_paragraph_book(&words.join(" ")));
        let mut device = Device::wake(&card);
        open_book(&mut device);
        device.settle();
        let store = &*device.store;
        assert!(store.book_has_images(), "the mark is indexed");
        let lines: Vec<&str> = (0..store.block_count())
            .map(|index| store.block_text(index))
            .collect();
        let holding: Vec<usize> = (0..lines.len())
            .filter(|&index| proto::image::token::inline_image_tokens(lines[index]).count() > 0)
            .collect();
        assert_eq!(holding.len(), 1, "the mark, on one line: {lines:?}");
        assert_eq!(
            proto::image::token::inline_image_tokens(lines[holding[0]]).count(),
            1,
            "the mark, once: {lines:?}"
        );
        for (index, line) in lines.iter().enumerate() {
            let record = store.block(index).expect("a record");
            assert_eq!(
                record.has_inline_images(),
                holding.contains(&index),
                "line {index} is marked as it holds images: {line:?}"
            );
        }
        let ragged = ragged_lines(store);
        assert!(
            ragged.is_empty(),
            "lines left ragged: {ragged:?} of {lines:?}"
        );
        let offsets: Vec<u32> = (0..lines.len())
            .map(|index| store.block_offset(index).expect("an offset"))
            .collect();
        assert!(
            offsets.windows(2).all(|pair| pair[0] < pair[1]),
            "anchors advance line by line: {offsets:?}"
        );
    }
}

/// A book with images drops a stray U+FFFC (the mark an image token starts
/// with) from its own text in the build that first indexes its images, as
/// every later layout of it does, so the only marks its lines hold start
/// tokens.
#[test]
fn a_picture_book_drops_stray_object_marks_from_its_first_build() {
    let img = r#"<img src="images/mark.png" alt="*"/>"#;
    let card = Card::blank();
    card.put(
        HOME,
        &marked_paragraph_book(&format!(
            "A stray \u{FFFC} object mark, and a picture {img} after it."
        )),
    );
    let mut device = Device::wake(&card);
    open_book(&mut device);
    device.settle();
    let store = &*device.store;
    assert!(store.book_has_images(), "the mark is indexed");
    let lines: Vec<&str> = (0..store.block_count())
        .map(|index| store.block_text(index))
        .collect();
    let marks: usize = lines
        .iter()
        .map(|line| line.matches('\u{FFFC}').count())
        .sum();
    let tokens: usize = lines
        .iter()
        .map(|line| proto::image::token::inline_image_tokens(line).count())
        .sum();
    assert_eq!(tokens, 1, "the picture: {lines:?}");
    assert_eq!(marks, tokens, "no stray mark survives: {lines:?}");
}

/// A card read refused while a picture book first opens is a storage
/// fault wherever it lands, the image index's lookups included: the open
/// does not report the chapter's text as too long.
#[test]
fn a_refused_read_during_a_picture_book_open_is_not_a_full_chapter() {
    let mut refused = 0usize;
    for probe in 0..4_000 {
        let card = Card::blank();
        card.put(HOME, &picture_book());
        let mut device = Device::wake(&card);
        device.open_library();
        device.point_at(BOOK);
        card.disk.refuse_read_in(Some(probe));
        device.press_only(Button::Confirm);
        device.run_queued();
        let fired = !card.disk.read_refusal_armed();
        card.disk.refuse_read_in(None);
        if !fired {
            break;
        }
        refused += 1;
        assert_ne!(
            device.store.reader_fault(),
            reader_cache::store::ReaderFault::TooLarge,
            "probe {probe}: {:?}",
            device.log
        );
    }
    assert!(refused > 100, "the sweep reached the open: {refused}");
}

/// An image a line shows smaller than its block size is decoded at the
/// line's size by the idle pass too, which no page asks for: layout marks
/// it inline as it puts it in the line.
#[test]
fn an_initial_inside_a_line_decodes_at_the_lines_size_while_idle() {
    let card = Card::blank();
    card.put(HOME, &picture_book_with(INITIAL_PNG));
    let mut device = Device::wake(&card);
    open_book(&mut device);
    device.settle();
    let store = &*device.store;
    assert!(store.book_images_settled(), "{:?}", device.log);
    let dir = cache_dir(&card);
    let pack = card
        .read(&format!("{dir}/{}", pack_file(store.portrait())))
        .expect("the pack");
    let records = pack_records(&pack);
    assert_eq!(records.len(), 2, "the plate and the initial: {records:?}");
    assert_eq!(records[1], (64, 64), "the initial at the line's size");
}

/// Whether the book's published index says it has images.
fn book_index_has_images(card: &Card) -> bool {
    let book = card
        .read(&format!("{}/BOOK.BIN", cache_dir(card)))
        .expect("BOOK.BIN");
    let header: &[u8; proto::cache::BOOK_V2_HEADER_BYTES] = book
        [..proto::cache::BOOK_V2_HEADER_BYTES]
        .try_into()
        .expect("a header");
    proto::cache::decode_book_v2_header(header)
        .expect("a header that decodes")
        .has_images
}

/// Back to a type size whose sections are still on the card, the book is
/// put back together from them (the reindex path) and its index keeps
/// saying it has images, in the store and on the card.
#[test]
fn a_picture_book_reindexed_for_an_earlier_layout_keeps_its_images() {
    let card = Card::blank();
    card.put(HOME, &picture_book());
    for (pass, size) in [FontSize::Medium, FontSize::Large, FontSize::Medium]
        .into_iter()
        .enumerate()
    {
        let mut device = Device::wake(&card);
        device.app.font_size = size;
        open_book(&mut device);
        device.settle();
        assert!(
            device.store.book_has_images(),
            "pass {pass} {size:?}: the store says the book has images: {:?}",
            device.log
        );
        assert!(
            book_index_has_images(&card),
            "pass {pass} {size:?}: BOOK.BIN says so"
        );
        device.sleep();
    }
}

/// Clearing a picture book's cache from the Library's actions sheet takes
/// its image index, both packs and a spool an interrupted decode left,
/// with the rest of the cache.
#[test]
fn clearing_a_picture_books_cache_takes_its_image_files() {
    let card = Card::blank();
    card.put(HOME, &picture_book());
    let mut device = Device::wake(&card);
    open_book(&mut device);
    device.settle();
    let dir = cache_dir(&card);
    let portrait = device.store.portrait();
    assert!(
        card.read(&format!("{dir}/IMAGES.BIN")).is_some(),
        "an index"
    );
    assert!(
        card.read(&format!("{dir}/{}", pack_file(portrait)))
            .is_some(),
        "a pack"
    );
    card.put_short(&format!("{dir}/{SPOOL_FILE}"), b"cut short");
    // The other orientation's pack, as reading turned would leave it.
    card.put_short(&format!("{dir}/{}", pack_file(!portrait)), b"X4PK");
    device.sleep();

    let mut device = Device::wake(&card);
    device.open_library();
    device.point_at(BOOK);
    device.press(Button::PagePrevious);
    device.press(Button::Confirm);
    device.settle();

    let left: Vec<String> = card
        .list("READER/CACHE2")
        .iter()
        .flat_map(|key| card.list(&format!("READER/CACHE2/{key}")))
        .collect();
    assert!(
        !left
            .iter()
            .any(|file| file.starts_with("IMG") || file.starts_with("SPOOL")),
        "image files survived the clear: {left:?}"
    );
    assert!(
        !left.iter().any(|file| file == "BOOK.BIN"),
        "the cache was cleared: {left:?} {:?}",
        device.log
    );
}

/// A card read refused anywhere in a picture book's first open, probing
/// its images included, costs that open and nothing after it: opened
/// again on a sound card, the book shows its mark as an image, not as the
/// alt text a cache built around the fault would keep for good.
#[test]
fn a_refused_read_during_the_first_open_leaves_no_alt_text_behind() {
    let img = r#"<img src="images/mark.png" alt="ALT"/>"#;
    let book = marked_paragraph_book(&format!("A mark {img} inside a line of text."));
    let mut refused = 0usize;
    for probe in 0..4_000 {
        let card = Card::blank();
        card.put(HOME, &book);
        let mut device = Device::wake(&card);
        device.open_library();
        device.point_at(BOOK);
        card.disk.refuse_read_in(Some(probe));
        device.press_only(Button::Confirm);
        device.run_queued();
        let fired = !card.disk.read_refusal_armed();
        card.disk.refuse_read_in(None);
        if !fired {
            break;
        }
        refused += 1;
        device.settle();
        device.sleep();

        let mut device = Device::wake(&card);
        open_book(&mut device);
        device.settle();
        let store = &*device.store;
        let lines: Vec<&str> = (0..store.block_count())
            .map(|index| store.block_text(index))
            .collect();
        let tokens: usize = lines
            .iter()
            .map(|line| proto::image::token::inline_image_tokens(line).count())
            .sum();
        assert_eq!(tokens, 1, "probe {probe}: the mark as an image: {lines:?}");
        assert!(
            !lines.iter().any(|line| line.contains("ALT")),
            "probe {probe}: no alt text: {lines:?}"
        );
    }
    assert!(refused > 50, "the sweep reached the open: {refused}");
}

/// 64×64 multi-scan progressive (`SOF2`) JPEG: DC = 0 across all blocks, AC[1] = +15 * 32.
/// In `Fast` mode (DC-only) it decodes as flat mid-gray; in `High` mode (multi-scan AC
/// reconstruction) every 8×8 block gets strong horizontal white-to-black bars.
const PROGRESSIVE_BARS_JPG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x20, 0x08, 0x08, 0x08, 0x08, 0x08,
    0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08,
    0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08,
    0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08,
    0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0xFF, 0xC2, 0x00, 0x0B, 0x08, 0x00, 0x40,
    0x00, 0x40, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x14, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xC4, 0x00, 0x15,
    0x10, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x04, 0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xDA, 0x00,
    0x08, 0x01, 0x01, 0x10, 0x01, 0x3F, 0x00, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C,
    0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C,
    0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C,
    0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C,
    0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0x7C, 0xFF, 0xD9,
];

fn progressive_jpg_book() -> Vec<u8> {
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/dc/elements/1.1/" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:test:prog-bars</dc:identifier><dc:title>Bars</dc:title><dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="bars" href="images/bars.jpg" media-type="image/jpeg"/>
  </manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#;
    let ch1 = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>One</title></head><body>
<p><img src="images/bars.jpg" alt="BARS"/></p></body></html>"#;
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
        ("OEBPS/ch1.xhtml", ch1.to_vec()),
        ("OEBPS/images/bars.jpg", PROGRESSIVE_BARS_JPG.to_vec()),
    ])
}

fn cycle_images_setting(device: &mut Device, expected: app_core::ImageMode) {
    while device.app.view != AppView::Home {
        device.press(Button::Back);
    }
    device.press(Button::Next);
    assert_eq!(
        device.app.view,
        AppView::Settings,
        "Home Next opens Settings"
    );
    for _ in 0..8 {
        device.press(Button::Next);
    }
    assert_eq!(device.app.selection, 8, "row 8 is Images");
    device.press(Button::Confirm);
    assert_eq!(device.app.image_mode, expected);
    device.press(Button::Back);
    assert_eq!(device.app.view, AppView::Home);
}

fn read_single_image_status(device: &mut Device, path: &str) -> proto::image::index::DecodeStatus {
    use storage::card::Card as _;
    let portrait = device.store.portrait();
    let index = device.store.loaded_index.expect("loaded index");
    let entry = device.store.catalog_entry(index).expect("catalog entry");
    let source = (entry.source_hash, entry.byte_size);
    let key = device.store.cache_key();
    let hash = proto::image::path::path_hash(path);
    device
        .card
        .with_root(|root| {
            let dir = storage::book_images::book_cache_dir(root, key).expect("cache dir");
            let images = storage::book_images::open_existing_book_images(
                &dir,
                source,
                embedded_sdmmc::Mode::ReadOnly,
            )
            .expect("image index");
            let mut found = None;
            let mut scratch = [0u8; 4096];
            images
                .scan(&mut scratch, |_slot, entry| {
                    if entry.path_hash == hash {
                        found = Some(entry);
                        false
                    } else {
                        true
                    }
                })
                .expect("scan image index");
            found.expect("image entry present").decode(portrait).status
        })
        .expect("card root")
}

fn render_page_bytes(device: &mut Device, page_num: u32) -> Vec<u8> {
    use storage::card::Card as _;
    let mut fb = display::fb::Framebuffer::new();
    fb.set_frame(display::fb::FbFrame::Portrait);
    fb.clear(true);
    let page = reader_cache::layout::ReaderPagePlan::new(&device.store, page_num).page();
    let book_id = device.app.book_id;
    let mut band = [0u8; 4096];
    let store = &*device.store;
    device
        .card
        .with_root(|root| {
            storage::page_images::draw_page(
                root,
                store,
                book_id,
                page,
                &mut band,
                || 0,
                |draw_img| {
                    ui::reading::draw_reading_page_body(&mut fb, store, page, |fb, image| {
                        draw_img(fb, image)
                    });
                },
            );
        })
        .expect("card root");
    fb.bytes().to_vec()
}

#[test]
fn image_mode_display_uses_progressive_ac_and_placeholder_skips_decode() {
    let book = progressive_jpg_book();
    let card = Card::blank();
    card.put(HOME, &book);
    let mut device = Device::wake(&card);
    assert_eq!(device.app.image_mode, app_core::ImageMode::Display);

    // 1. Default Display mode: progressive JPEG decodes straight to Decoded
    // via multi-scan AC reconstruction.
    open_book(&mut device);
    device.settle();
    assert_eq!(device.store.image_mode(), 0);
    assert_eq!(
        read_single_image_status(&mut device, "OEBPS/images/bars.jpg"),
        proto::image::index::DecodeStatus::Decoded
    );
    let dir = cache_dir(&card);
    let pack = card
        .read(&format!("{dir}/{}", pack_file(device.store.portrait())))
        .expect("display pack");
    assert!(!pack.is_empty());
    let display_fb = render_page_bytes(&mut device, 0);
    let display_pages = device.store.advertised_page_count();

    // 2. Switch Display -> Placeholder: keeps identical pagination and layout
    // key, skips decoding/reading the pack, and renders a placeholder instead
    // of the decoded bitmap.
    cycle_images_setting(&mut device, app_core::ImageMode::Placeholder);
    device.press(Button::Confirm);
    device.settle();
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.store.image_mode(), 1);
    assert_eq!(device.store.advertised_page_count(), display_pages);
    let placeholder_fb = render_page_bytes(&mut device, 0);
    assert_ne!(
        placeholder_fb, display_fb,
        "Placeholder mode must draw placeholder outline instead of decoded pack bitmap"
    );

    // 3. Fresh card opened under Placeholder skips decoding altogether.
    let card_ph = Card::blank();
    card_ph.put(HOME, &book);
    let mut device_ph = Device::wake(&card_ph);
    cycle_images_setting(&mut device_ph, app_core::ImageMode::Placeholder);
    open_book(&mut device_ph);
    device_ph.settle();
    assert_eq!(device_ph.store.image_mode(), 1);
    assert_eq!(
        read_single_image_status(&mut device_ph, "OEBPS/images/bars.jpg"),
        proto::image::index::DecodeStatus::NotDecoded
    );
    let dir_ph = cache_dir(&card_ph);
    assert!(
        card_ph
            .read(&format!(
                "{dir_ph}/{}",
                pack_file(device_ph.store.portrait())
            ))
            .is_none(),
        "Placeholder mode must not decode an image pack"
    );

    // 4. DC-only fallbacks remain usable and do not requeue indefinitely
    // when the full AC reconstruction exceeds fixed resource limits.
    let mut entry = proto::image::index::ImageEntry {
        probe: proto::image::index::ProbeStatus::Supported,
        width: 64,
        height: 64,
        ..Default::default()
    };
    entry.portrait.status = proto::image::index::DecodeStatus::DecodedFast;
    assert!(storage::image_decode::pending_size(&entry, true).is_none());
    entry.portrait.status = proto::image::index::DecodeStatus::NotDecoded;
    assert!(storage::image_decode::pending_size(&entry, true).is_some());
    entry.portrait.status = proto::image::index::DecodeStatus::Decoded;
    assert!(storage::image_decode::pending_size(&entry, true).is_none());
}

#[test]
fn image_mode_suppress_omits_images_reflows_and_preserves_anchor() {
    let book = picture_book();
    let card = Card::blank();
    card.put(HOME, &book);
    let mut device = Device::wake(&card);
    open_book(&mut device);
    device.settle();

    let display_pages = device.store.advertised_page_count();
    let illustrated_layout = device.store.layout_key();

    // Advance into chapter 2 so a saved place exists after the chapter-1 plate.
    let mut found_ch2_para = false;
    while device.app.page + 1 < display_pages {
        device.press(Button::Next);
        device.settle();
        let store: &reader_cache::store::ReaderStore = &device.store;
        let p = reader_cache::layout::ReaderPagePlan::new(store, device.app.page).page();
        let page_text: String = (p.first_block as usize
            ..p.first_block as usize + p.block_count as usize)
            .map(|i| store.block_text(i))
            .collect::<Vec<_>>()
            .join(" ");
        if page_text.contains("Paragraph 0 of chapter 2") {
            found_ch2_para = true;
            break;
        }
    }
    assert!(found_ch2_para, "located chapter 2 prose page under Display");

    // Display -> Placeholder keeps identical total pages and layout key.
    cycle_images_setting(&mut device, app_core::ImageMode::Placeholder);
    device.press(Button::Confirm);
    device.settle();
    assert_eq!(device.store.advertised_page_count(), display_pages);
    assert_eq!(device.store.layout_key(), illustrated_layout);

    // Placeholder -> Suppress removes images from layout, reduces total pages,
    // reflows text without placeholders or U+FFFC tokens, and preserves the
    // chapter-2 anchor on the active page.
    cycle_images_setting(&mut device, app_core::ImageMode::Suppress);
    device.press(Button::Confirm);
    device.settle();
    assert_eq!(device.app.view, AppView::Reading);
    assert_eq!(device.store.image_mode(), 2);
    assert!(device.store.type_settings().suppress_images);
    let suppressed_layout = device.store.layout_key();
    assert_ne!(suppressed_layout, illustrated_layout);
    assert_ne!(suppressed_layout & 0x80, 0);
    let suppress_pages = device.store.advertised_page_count();
    assert!(
        suppress_pages < display_pages,
        "omitting PLATE_PNG must reduce total pages: suppress={suppress_pages} display={display_pages}"
    );
    let store: &reader_cache::store::ReaderStore = &device.store;
    let active_page = reader_cache::layout::ReaderPagePlan::new(store, device.app.page).page();
    assert!(!ui::reading::page_has_images(store, active_page));
    let active_text: String = (active_page.first_block as usize
        ..active_page.first_block as usize + active_page.block_count as usize)
        .map(|i| store.block_text(i))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        active_text.contains("Paragraph 0 of chapter 2"),
        "ContentAnchor restore must keep chapter 2 prose on screen after Display -> Suppress; got: {active_text}"
    );

    // Inspect all loaded blocks across every page under Suppress: no image
    // blocks, no inline image tokens, and inline image word separation is
    // preserved ("A mark stands inside").
    let mut saw_clean_inline_prose = false;
    while device.app.page > 0 {
        device.press(Button::Previous);
        device.settle();
    }
    loop {
        let store: &reader_cache::store::ReaderStore = &device.store;
        for i in 0..store.block_count() {
            let block = store.block(i).expect("block");
            assert!(!block.is_image(), "no image blocks under Suppress");
            assert!(
                !block.has_inline_images(),
                "no inline image blocks under Suppress"
            );
            let text = store.block_text(i);
            assert!(
                !text.contains('\u{FFFC}'),
                "no U+FFFC tokens under Suppress: {text:?}"
            );
            if text.contains("A mark stands inside") {
                saw_clean_inline_prose = true;
            }
        }
        if device.app.page + 1 >= suppress_pages {
            break;
        }
        device.press(Button::Next);
        device.settle();
    }
    assert!(
        saw_clean_inline_prose,
        "suppressed inline image must preserve surrounding space between 'A mark' and 'stands inside'"
    );

    // Switching Suppress -> Display restores the full illustrated pagination cleanly.
    cycle_images_setting(&mut device, app_core::ImageMode::Display);
    device.press(Button::Confirm);
    device.settle();
    assert_eq!(device.store.advertised_page_count(), display_pages);
    assert_eq!(device.store.layout_key(), illustrated_layout);

    // Fresh card opened under Suppress skips image decoding and writes no pack.
    let card_sup = Card::blank();
    card_sup.put(HOME, &book);
    let mut device_sup = Device::wake(&card_sup);
    cycle_images_setting(&mut device_sup, app_core::ImageMode::Placeholder);
    cycle_images_setting(&mut device_sup, app_core::ImageMode::Suppress);
    open_book(&mut device_sup);
    device_sup.settle();
    assert_eq!(device_sup.store.image_mode(), 2);
    assert_eq!(
        read_single_image_status(&mut device_sup, "OEBPS/images/plate.png"),
        proto::image::index::DecodeStatus::NotDecoded
    );
    let dir_sup = cache_dir(&card_sup);
    assert!(
        card_sup
            .read(&format!(
                "{dir_sup}/{}",
                pack_file(device_sup.store.portrait())
            ))
            .is_none(),
        "Suppress mode must not decode an image pack"
    );
}
