use crate::{catalog, AppView, DisplayOrientation, ReaderSource, RenderRequest};
use core::fmt::Write;
use display::fb::Framebuffer;
use display::font::{draw_text, literata, measure_text, FontFamily, FontStyle};
use display::render::{draw_ascii, fill_rect, stroke_rect};
use display::Rect;
use heapless::String;
use reader_cache::layout;
use reader_cache::store::{BookLoadStatus, LibraryScanStatus, ReaderStore, LIBRARY_WINDOW};
use ui::{
    app_render::{self, UiRenderModel},
    UiBook, UiCover, UiLibraryRow, UiLibraryStatus, UiTocItem,
};

const SHOW_INPUT_DEBUG: bool = false;
// Decoupled from the 128-entry event/resident caps: the overview reads the
// full chapter list from the card, so the render window must hold it.
const MAX_UI_CHAPTERS: usize = 256;

pub(crate) fn render(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    if request.view == AppView::Reading && ReaderSource::from_book_id(request.book_id).is_sd() {
        render_sd_reading_while(fb, request, sd_library, || true);
        return;
    }
    fb.set_frame(app_render::fb_frame(request.orientation));
    let mut library_entries = [UiLibraryRow {
        name: "",
        is_folder: false,
    }; LIBRARY_WINDOW];
    let mut chapters = [UiTocItem {
        title: "",
        level: 1,
        page: 0,
    }; MAX_UI_CHAPTERS];
    let model = ui_model(request, sd_library, &mut library_entries, &mut chapters);
    app_render::render_request(fb, request, &model);

    if SHOW_INPUT_DEBUG {
        draw_input_sample(fb, request);
    }
}

/// The reading page of a card book as [`render`] draws it from RAM, asking
/// `go_on` before each line and stopping part way when it says no. False
/// when it stopped, leaving the frame incomplete. [`render`] draws every
/// such page through here, so a page drawn ahead of a press
/// (`crate::next_page`) is the frame the press's own render would draw.
/// That holds because the page reads nothing from `request` beyond the
/// fields of `app_core::ReadingPageKey` (keep that list current when this
/// draw starts reading another field) and nothing else outside the store.
pub(crate) fn render_sd_reading_while(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    go_on: impl FnMut() -> bool,
) -> bool {
    fb.set_frame(app_render::fb_frame(request.orientation));
    fb.clear(true);
    let finished = draw_sd_reader_page(fb, request, sd_library, go_on);
    app_render::render_reading_sheet_overlay(fb, request);
    if SHOW_INPUT_DEBUG {
        draw_input_sample(fb, request);
    }
    finished
}

/// Whether `request`'s page can be drawn ahead of the press that will ask
/// for it: a reading page of the loaded card book whose page is resident
/// in the store, laid out as the request says, that [`render`] draws from
/// RAM alone. A page with images, or set in the custom face from the card,
/// draws inside an SD session and is left to its own render, as is any
/// page while the input debug overlay (which draws per-press fields) is
/// compiled in.
pub(crate) fn page_can_be_staged(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    let Some(index) = ReaderSource::from_book_id(request.book_id).sd_index() else {
        return false;
    };
    !SHOW_INPUT_DEBUG
        && request.view == AppView::Reading
        && !request.reading_sheet
        && sd_library.covers_global_page(usize::from(index), request.page)
        && reading_buffer_ready(request, sd_library)
        && !reader_reads_card(request, sd_library)
}

fn custom_face_on_card(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    request.view == AppView::Reading
        && request.font_family == FontFamily::Custom
        && sd_library.custom_font_available()
}

/// Whether a render of `request` draws from the card, inside an SD session:
/// a reading page set in a custom face read from the card, or one whose
/// blocks include images. Every other render draws from RAM ([`render`]).
pub(crate) fn reader_reads_card(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    custom_face_on_card(request, sd_library) || reader_page_has_images(request, sd_library)
}

/// Whether the frame `request` asks for shows an image: a resident reading
/// page of a book with images, whose blocks include an image or a line with
/// images inside it. Reads nothing from the card.
pub(crate) fn reader_page_has_images(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    resident_image_page(request, sd_library)
        .is_some_and(|page| ui::reading::page_has_images(sd_library, page))
}

/// Whether the frame `request` asks for shows a picture: an image block,
/// which takes the full refresh. An image inside a line is a symbol or an
/// initial, too small to show the ghosting a picture does.
pub(crate) fn reader_page_has_pictures(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    resident_image_page(request, sd_library)
        .is_some_and(|page| ui::reading::page_has_image_blocks(sd_library, page))
}

/// The page `request` asks for, when it is a resident reading page of a
/// book with images.
fn resident_image_page(
    request: RenderRequest,
    sd_library: &ReaderStore,
) -> Option<proto::cache::PageRecord> {
    (request.view == AppView::Reading
        && ReaderSource::from_book_id(request.book_id).is_sd()
        && sd_library.book_has_images()
        && sd_library.reader_status() == BookLoadStatus::Ready
        && reading_buffer_ready(request, sd_library))
    .then(|| layout::ReaderPagePlan::new(sd_library, request.page).page())
}

/// Renders a reading page from inside an SD session: its pictures stream
/// from the book's pack through `band` (an empty band draws their
/// placeholders), and with `custom` its text is drawn in the custom face,
/// falling back to the built-in one when the face cannot draw a line.
pub(crate) fn render_reader_from_root(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    root: &crate::sd_session::SdRoot<'_>,
    band: &mut [u8],
    custom: bool,
) {
    fb.set_frame(app_render::fb_frame(request.orientation));
    fb.clear(true);
    draw_sd_reader_page_with(fb, request, sd_library, |fb, page| {
        storage::page_images::draw_page(
            root,
            sd_library,
            request.book_id,
            page,
            band,
            clock_us,
            |draw_image| {
                let drawn_custom = custom
                    && crate::custom_font::draw_reading_page_body(
                        root,
                        fb,
                        sd_library,
                        font_metrics,
                        page,
                        |fb, image| draw_image(fb, image),
                    );
                if !drawn_custom {
                    if custom {
                        // The custom pass can fail after drawing some lines,
                        // and the fallback draws every line, so it starts on
                        // a clean frame. Nothing else is on it yet: the
                        // caller cleared it for this page, and the footer
                        // and sheet come after.
                        fb.clear(true);
                    }
                    ui::reading::draw_reading_page_body(fb, sd_library, page, |fb, image| {
                        draw_image(fb, image)
                    });
                }
            },
        );
        true
    });
    app_render::render_reading_sheet_overlay(fb, request);
}

fn clock_us() -> u64 {
    embassy_time::Instant::now().as_micros()
}

pub(crate) fn render_sleep(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    let mut library_entries = [UiLibraryRow {
        name: "",
        is_folder: false,
    }; LIBRARY_WINDOW];
    let mut chapters = [UiTocItem {
        title: "",
        level: 1,
        page: 0,
    }; MAX_UI_CHAPTERS];
    let model = ui_model(request, sd_library, &mut library_entries, &mut chapters);
    app_render::render_sleep(fb, request, &model);
}

pub(crate) fn render_sleep_blank(fb: &mut Framebuffer) {
    // Landscape, not Native: the X4 scans its rows bottom up, which only the
    // reader frames fold in, so a Native frame lands upside down there. On
    // the X3 the two are the same.
    fb.set_frame(display::fb::FbFrame::Landscape);
    fb.clear(true);
    draw_text_centered_truncated_local(
        fb,
        literata(FontStyle::Regular),
        "CalendulaOS",
        0,
        display::WIDTH as i16,
        display::HEIGHT as i16 / 2,
    );
}

fn ui_model<'a>(
    request: RenderRequest,
    sd_library: &'a ReaderStore,
    library_entries: &'a mut [UiLibraryRow<'a>; LIBRARY_WINDOW],
    chapters: &'a mut [UiTocItem<'a>; MAX_UI_CHAPTERS],
) -> UiRenderModel<'a> {
    // The resident page of the folder the reader is in: `library_entries[i]`
    // is row `window_start + i`, the folder's books followed by its folders.
    // The firmware refills this page from the card before each Library
    // render.
    let window = sd_library.folder_rows();
    let window_start = sd_library.folder_start();
    let library_count = window.len().min(library_entries.len());
    for (i, row) in window.iter().take(library_count).enumerate() {
        library_entries[i] = UiLibraryRow {
            name: row.name.as_str(),
            is_folder: row.is_dir,
        };
    }
    let (chapter_count, chapters_window_start, chapters_total) =
        fill_chapters(chapters, request, sd_library);

    let fallback_book = catalog::active_book(request.book_id);
    let (title, author) =
        sd_library.active_book_labels(request.book_id, fallback_book.title, fallback_book.author);

    // The firmware-resolved current-chapter title, shown only for the book it
    // was resolved for -- so a colophon never names another book's chapter.
    // Keyed on source identity (not the loaded book) so it survives boot
    // restore, where the title is read before the book is opened.
    let chapter_title = if !sd_library.current_chapter_title().is_empty()
        && sd_library.current_chapter_source()
            == sd_library.current_catalog_identity(request.book_id)
    {
        sd_library.current_chapter_title()
    } else {
        ""
    };

    UiRenderModel {
        active_book: UiBook {
            title,
            author,
            progress_permille: book_progress_permille(request),
            cover: sd_library
                .selected_cover(request.book_id)
                .map(|cover| UiCover {
                    width: cover.width,
                    height: cover.height,
                    stride: cover.stride,
                    bits: cover.bits,
                }),
        },
        library_status: ui_library_status(sd_library.status),
        library_entries: &library_entries[..library_count],
        library_folder: sd_library.browse().path().file_name().unwrap_or(""),
        library_window_start: window_start as u16,
        chapters: &chapters[..chapter_count],
        chapters_window_start: chapters_window_start as u16,
        chapters_total: chapters_total.min(u16::MAX as usize) as u16,
        chapter_title,
        custom_font_name: sd_library.custom_font_name(),
    }
}

/// Fill the UI chapter rows and return `(resident_len, window_start, total)`
/// — the Contents page draws absolute indices out of this resident window.
fn fill_chapters<'a>(
    chapters: &mut [UiTocItem<'a>; MAX_UI_CHAPTERS],
    request: RenderRequest,
    sd_library: &'a ReaderStore,
) -> (usize, usize, usize) {
    // The on-disk chapter list, windowed from the card into the section
    // buffer while the overview is open: row `i` is absolute chapter
    // `window_start + i`.
    if ReaderSource::from_book_id(request.book_id).is_sd() && sd_library.text_holds_toc() {
        let total = sd_library.overview_chapter_count();
        let window_start = sd_library.toc_window_start();
        let count = total.saturating_sub(window_start).min(chapters.len());
        for (offset, item) in chapters.iter_mut().take(count).enumerate() {
            let index = window_start + offset;
            *item = UiTocItem {
                title: sd_library.overview_title_at(index),
                level: sd_library.overview_level_at(index),
                page: sd_library.overview_row_page(index),
            };
        }
        return (count, window_start, total);
    }
    if ReaderSource::from_book_id(request.book_id).is_sd() && sd_library.toc_count() > 0 {
        let count = sd_library.toc_count().min(chapters.len());
        for (index, item) in chapters.iter_mut().take(count).enumerate() {
            if let Some(toc_item) = sd_library.toc_item(index) {
                *item = UiTocItem {
                    title: toc_item.title,
                    level: toc_item.level,
                    page: toc_item.page,
                };
            }
        }
        return (count, 0, count);
    }
    if ReaderSource::from_book_id(request.book_id).is_sd() {
        let count = sd_library
            .chapter_count_for_ui()
            .max(1)
            .min(chapters.len() as u16) as usize;
        for item in chapters.iter_mut().take(count) {
            // Empty titles render as numbered chapters in the contents view.
            *item = UiTocItem {
                title: "",
                level: 1,
                page: 0,
            };
        }
        return (count, 0, count);
    }

    let count = (catalog::chapter_count() as usize).min(chapters.len());
    for (index, item) in chapters.iter_mut().take(count).enumerate() {
        if let Some(chapter) = catalog::chapter_at(index) {
            *item = UiTocItem {
                title: chapter.title,
                level: 1,
                page: 0,
            };
        }
    }
    (count, 0, count)
}

fn ui_library_status(status: LibraryScanStatus) -> UiLibraryStatus {
    match status {
        LibraryScanStatus::NotScanned => UiLibraryStatus::NotScanned,
        LibraryScanStatus::Scanning => UiLibraryStatus::Scanning,
        LibraryScanStatus::Ready => UiLibraryStatus::Ready,
        LibraryScanStatus::Empty => UiLibraryStatus::Empty,
        LibraryScanStatus::Error => UiLibraryStatus::Error,
    }
}

/// Whether the store holds the page `request` asks for, laid out as the
/// request wants it.
fn reading_buffer_ready(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    let layout_current = sd_library.type_settings() == request.type_settings();
    layout_current
        && sd_library.portrait() == app_core::is_portrait(request.orientation)
        && sd_library.loaded_index == ReaderStore::selected_book_index(request.book_id)
        && !sd_library.text_holds_toc()
}

/// False when `go_on` stopped the page body part way.
fn draw_sd_reader_page(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    go_on: impl FnMut() -> bool,
) -> bool {
    draw_sd_reader_page_with(fb, request, sd_library, |fb, page| {
        ui::reading::draw_reading_page_body_while(
            fb,
            sd_library,
            page,
            ui::reading::draw_image_placeholder,
            go_on,
        )
    })
}

/// The reading page: its plate while the page is not ready, else `body`
/// drawing the page's blocks and the footer after it. False when `body`
/// stopped part way, before the footer.
fn draw_sd_reader_page_with(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    body: impl FnOnce(&mut Framebuffer, proto::cache::PageRecord) -> bool,
) -> bool {
    match (
        sd_library.reader_status(),
        reading_buffer_ready(request, sd_library),
    ) {
        (_, false) | (BookLoadStatus::Empty | BookLoadStatus::Loading, _) => {
            draw_sd_reader_loading(fb, request, sd_library);
        }
        (BookLoadStatus::Error, _) => {
            draw_sd_reader_error(fb, request, sd_library);
        }
        (BookLoadStatus::Ready, _) => {
            let plan = layout::ReaderPagePlan::new(sd_library, request.page);
            let page_count = plan.page_count().max(1);
            if !body(fb, plan.page()) {
                return false;
            }
            draw_reader_footer(fb, request, sd_library, page_count);
        }
    }
    true
}

/// The reading page is full bleed; its only resident furniture is the
/// page-in-chapter counter, set in the 16px apparatus size.
fn draw_reader_footer(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    page_count: u32,
) {
    // Page within the chapter (spine item), aggregated across its cache
    // sections. Falls back to the current section, then the whole book, when
    // no book index is loaded to aggregate from.
    let (chapter_current, chapter_total) = sd_library
        .chapter_page_position(request.page)
        .unwrap_or_else(|| {
            let total = if sd_library.current_section_page_count > 0 {
                sd_library.current_section_page_count as u32
            } else {
                page_count
            }
            .max(1);
            let current = if sd_library.current_section_page_count > 0 {
                request
                    .page
                    .saturating_sub(sd_library.current_section_start_page)
                    .saturating_add(1)
            } else {
                request.page.saturating_add(1)
            }
            .min(total);
            (current, total)
        });

    let mut label = String::<32>::new();
    let _ = write!(label, "{}/{}", chapter_current, chapter_total);
    ui::reading::draw_reading_page_counter_with_progress(
        fb,
        label.as_str(),
        request.orientation == DisplayOrientation::LandscapeButtonsTop,
        request.footer_percent,
    );
}

fn draw_sd_reader_loading(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    let title_font = literata(FontStyle::Bold);
    let author_font = literata(FontStyle::Italic);
    let fallback = catalog::active_book(request.book_id);
    let (title, author) =
        sd_library.active_book_labels(request.book_id, fallback.title, fallback.author);

    // Vertically center title + author block within the reader page region
    // (frame-relative, so the X3's taller page and the portrait hold both
    // carry the plate to their own middle).
    let left = 8;
    let right = fb.frame_width() as i16 - 8;
    let title_y = fb.frame_height() as i16 / 2 - 8;
    let author_y = title_y + 36;
    draw_text_centered_truncated_local(fb, title_font, title, left, right, title_y);
    if !author.is_empty() {
        draw_text_centered_truncated_local(fb, author_font, author, left, right, author_y);
    }
}

fn draw_sd_reader_error(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    let fallback = catalog::active_book(request.book_id);
    let (title, _) = sd_library.active_book_labels(request.book_id, fallback.title, "");

    let left = 8;
    let right = fb.frame_width() as i16 - 8;
    let title_y = fb.frame_height() as i16 / 2 - 16;
    let message_y = title_y + 40;
    draw_text_centered_truncated_local(fb, title_font, title, left, right, title_y);
    let error = sd_library.reader_error();
    let message: &str = if error.is_empty() {
        "Could not open this book."
    } else {
        error
    };
    draw_text_centered_truncated_local(fb, body_font, message, left, right, message_y);
}

fn draw_text_centered_truncated_local(
    fb: &mut Framebuffer,
    font: &'static display::font::BitmapFont,
    text: &str,
    left: i16,
    right: i16,
    y: i16,
) {
    let max_width = (right - left).max(0);
    if max_width == 0 {
        return;
    }
    let width = measure_text(font, text) as i16;
    if width <= max_width {
        draw_text(fb, font, text, left + (max_width - width) / 2, y, false);
        return;
    }

    let mut end = text.len();
    while end > 0 {
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = &text[..end];
        let candidate_width = measure_text(font, candidate) as i16;
        let ellipsis_width = measure_text(font, "...") as i16;
        if candidate_width + ellipsis_width <= max_width {
            let x = left + (max_width - candidate_width - ellipsis_width) / 2;
            draw_text(fb, font, candidate, x, y, false);
            draw_text(fb, font, "...", x + candidate_width, y, false);
            return;
        }
        end = end.saturating_sub(1);
    }
}

fn draw_input_sample(fb: &mut Framebuffer, request: RenderRequest) {
    fill_rect(fb, Rect::new(488, 104, 220, 64), true);
    stroke_rect(fb, Rect::new(488, 104, 220, 64), false);
    draw_ascii(fb, "LAST", 504, 120, false);
    draw_ascii(fb, button_label(request.last_button), 552, 120, false);
}

fn button_label(button: Option<crate::Button>) -> &'static str {
    match button {
        Some(crate::Button::Power) => "POWER",
        Some(crate::Button::Back) => "BACK",
        Some(crate::Button::Confirm) => "OK",
        Some(crate::Button::Previous) => "PREV",
        Some(crate::Button::Next) => "NEXT",
        Some(crate::Button::PagePrevious) => "PAGE-",
        Some(crate::Button::PageNext) => "PAGE+",
        None => "NONE",
    }
}

/// The Home and sleep progress when the request's page count is 0: a card
/// book not yet measured, or the built-in guide. The guide's chapter count
/// says nothing about a card book, so that reads as unknown.
fn book_progress_permille(request: RenderRequest) -> u16 {
    if ReaderSource::from_book_id(request.book_id).is_sd() {
        return 0;
    }
    let chapters = catalog::chapter_count().max(1) as u32;
    ((request.chapter as u32 * 1000) / chapters.saturating_sub(1).max(1)) as u16
}
