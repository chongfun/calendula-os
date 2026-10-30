//! Image sessions over the card: the passes that decode the open book's
//! images into its packs (`image_decode`), each in an SD session of its
//! own, between the storage task's commands.
//!
//! Two kinds of pass. A request's ([`decode_page_images`]) decodes the
//! images the page about to be announced shows, so the page never shows a
//! placeholder a picture would later replace; it gives way to a press, or
//! to a newer request, which then answers for itself. An idle pass
//! ([`decode_images_while_idle`]) runs in a background slice: the window
//! around the page on screen first, then the rest of the book nearest the
//! reader, until its budget, a press, or a card fault. What a pass settles
//! goes into the store's note, so a page whose images are known to need
//! nothing costs no session at all.

use crate::book_build::{book_locator, ReaderCacheScratch, SdFileReadAt};
use crate::book_images::{book_cache_dir, open_existing_book_images};
use crate::card::Card;
use crate::image_decode::{
    list_window_images, reader_place, resident_page_images, DecodeOutcome, ImageSession,
    PageImages, SessionStats,
};
use embassy_time::Instant;
use embedded_sdmmc::Mode;
use reader_cache::store::ReaderStore;

/// The most one idle pass spends in its session, so a page turn waits for
/// at most this plus one decode step's check.
pub const SESSION_BUDGET_MS: u64 = 4000;

/// Which images an idle pass decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImagePass {
    /// Those on the pages around the one on screen.
    Window,
    /// The rest of the book's, nearest the reader first.
    Rest,
}

/// How an idle image pass ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImagePassEnd {
    /// A press stopped it.
    pub pressed: bool,
    /// It ran to its end rather than out of the session budget: the window
    /// through its list, the rest pass until nothing was left or a card
    /// fault stopped it.
    pub finished: bool,
    /// It decoded or failed an image.
    pub worked: bool,
    /// It decoded an image the page on screen shows, so that page is worth
    /// painting again.
    pub screen_decoded: bool,
}

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

/// The pass's session, from inside the card's root: the book's cache
/// directory and index, and a session over them.
macro_rules! with_session {
    ($root:expr, $library:expr, $index:expr, $portrait:expr, |$dir:ident, $session:ident| $body:block) => {{
        let root = $root;
        let library: &ReaderStore = $library;
        let index: usize = $index;
        (|| {
            let entry = library.catalog_entry(index)?;
            let source = (entry.source_hash, entry.byte_size);
            let (at, path) = book_locator(library, index)?;
            let $dir = book_cache_dir(root, library.cache_key())?;
            let images = open_existing_book_images(&$dir, source, Mode::ReadWriteAppend)?;
            upload_store::library::with_book_at(root, at, &path, |book_dir, alias| {
                let open_archive = || {
                    let file = book_dir.open_file_in_dir(alias, Mode::ReadOnly).ok()?;
                    Some(SdFileReadAt::new(file))
                };
                let mut $session =
                    ImageSession::new(&$dir, images, open_archive, $portrait, now_ms);
                $body
            })
            .ok()
            .flatten()
            .flatten()
        })()
    }};
}

/// Decodes the open book's images while the reader is idle, for the
/// orientation the page on screen is laid out in: one pass, in an SD
/// session of its own, until it runs out of work, out of the session
/// budget, or gives way to a press (`gives_way`). Opens the book's EPUB
/// only once an image needs reading, so a pass that finds nothing reads
/// only the index (and, for the window, the section files of pages the
/// store does not hold). What it finds settled goes into the store's note:
/// the window's pages, or, when the rest pass finds nothing left, the whole
/// book. Callers ask only for books with images.
#[inline(never)]
pub fn decode_images_while_idle(
    card: &mut impl Card,
    library: &mut ReaderStore,
    index: usize,
    page: u32,
    scratch: &mut ReaderCacheScratch<'_>,
    pass: ImagePass,
    gives_way: &dyn Fn() -> bool,
) -> ImagePassEnd {
    let started = Instant::now();
    let out_of_time = || started.elapsed().as_millis() >= SESSION_BUDGET_MS;
    let (mut parts, list) = scratch.image_parts();
    let store: &ReaderStore = library;
    let portrait = store.portrait();
    // What the pass can vouch for: the window's pages, or the whole book.
    let mut settled = None;
    let mut out_of_budget = false;
    let mut screen_decoded = false;
    let session = card
        .with_root(|root| {
            with_session!(root, store, index, portrait, |dir, session| {
                let mut listed = 0;
                match pass {
                    ImagePass::Window => {
                        let mut window = PageImages::new(list);
                        let pages = list_window_images(
                            store,
                            index,
                            page,
                            &dir,
                            session.source(),
                            &mut window,
                        );
                        listed = window.len();
                        let screen_from = pages.map_or(usize::MAX, |listing| listing.screen_from);
                        let mut whole = true;
                        for (at, image) in
                            (0..window.len()).filter_map(|at| Some((at, window.get(at)?)))
                        {
                            if gives_way() || out_of_time() {
                                whole = false;
                                out_of_budget = !gives_way();
                                break;
                            }
                            match session.decode_page_image(image, &mut parts, gives_way) {
                                Some(DecodeOutcome::Cancelled | DecodeOutcome::Retry) => {
                                    whole = false;
                                    break;
                                }
                                Some(DecodeOutcome::Decoded) if at >= screen_from => {
                                    screen_decoded = true;
                                }
                                _ => {}
                            }
                        }
                        if whole {
                            settled = pages.map(|listing| (listing.from, listing.to));
                        }
                    }
                    ImagePass::Rest => {
                        let reader = reader_place(store, page, session.images());
                        if session.decode_nearest(reader, list, &mut parts, gives_way, &out_of_time)
                        {
                            settled = Some((0, u32::MAX));
                        } else {
                            out_of_budget = out_of_time();
                        }
                    }
                }
                Some((session.stats, listed))
            })
        })
        .ok()
        .flatten();
    let (stats, images) = session.unwrap_or_default();
    let pressed = gives_way();
    if stats.forgot_decodes() {
        library.forget_settled_images();
    }
    match settled {
        Some((0, u32::MAX)) if !stats.forgot_decodes() => library.note_book_images_settled(),
        Some((from, to)) if !stats.forgot_decodes() => library.note_settled_pages(from, to),
        _ => {}
    }
    log_image_session(
        match pass {
            ImagePass::Window => "window",
            ImagePass::Rest => "remaining",
        },
        images,
        stats,
        started,
        pressed,
    );
    ImagePassEnd {
        pressed,
        finished: !out_of_budget && !pressed,
        worked: stats.decoded > 0 || stats.failed > 0,
        screen_decoded,
    }
}

/// Decodes the images the page about to be shown draws, for its
/// orientation, before the request is answered, so the page never shows a
/// placeholder that a picture would later replace. The page is resident by
/// now; one without images, in a book without any, or that the store's
/// note says is settled, costs nothing (no card access); one found settled
/// is noted. False when the wait was abandoned (`gives_way`: a press, or a
/// newer request), the image in hand dropped, so the press is handled at
/// once.
#[inline(never)]
pub fn decode_page_images(
    card: &mut impl Card,
    library: &mut ReaderStore,
    index: usize,
    page: u32,
    scratch: &mut ReaderCacheScratch<'_>,
    gives_way: &dyn Fn() -> bool,
) -> bool {
    if !library.book_has_images()
        || !library.covers_global_page(index, page)
        || library.images_settled(page)
    {
        return true;
    }
    let (mut parts, list) = scratch.image_parts();
    let mut shown = PageImages::new(list);
    let whole = resident_page_images(library, page, &mut shown);
    if shown.is_empty() && whole {
        return true;
    }
    let started = Instant::now();
    if gives_way() {
        return false;
    }
    let store: &ReaderStore = library;
    let portrait = store.portrait();
    let mut settled = whole;
    let session = card
        .with_root(|root| {
            with_session!(root, store, index, portrait, |dir, session| {
                let _ = &dir;
                for image in (0..shown.len()).filter_map(|at| shown.get(at)) {
                    if gives_way() {
                        settled = false;
                        break;
                    }
                    if matches!(
                        session.decode_page_image(image, &mut parts, gives_way),
                        Some(DecodeOutcome::Cancelled | DecodeOutcome::Retry)
                    ) {
                        settled = false;
                        if gives_way() {
                            break;
                        }
                    }
                }
                Some((session.stats, shown.len()))
            })
        })
        .ok()
        .flatten();
    // No index to check against: nothing is vouched for.
    let (stats, images) = session
        .filter(|(stats, _)| stats.checked > 0 || shown.is_empty())
        .unwrap_or_else(|| {
            settled = false;
            (SessionStats::default(), 0)
        });
    let pressed = gives_way();
    if stats.forgot_decodes() {
        library.forget_settled_images();
    } else if settled && !pressed {
        library.note_settled_pages(page, page + 1);
    }
    log_image_session("request", images, stats, started, pressed);
    !pressed
}

fn log_image_session(
    pass: &str,
    images: usize,
    stats: SessionStats,
    started: Instant,
    pressed: bool,
) {
    bench_log!(
        "bench: image_session pass={} images={} checked={} decoded={} failed={} cancelled={} retry={} elapsed_ms={} pressed={} t_ms={}",
        pass,
        images,
        stats.checked,
        stats.decoded,
        stats.failed,
        stats.cancelled,
        stats.retry,
        started.elapsed().as_millis(),
        pressed,
        Instant::now().as_millis(),
    );
}
