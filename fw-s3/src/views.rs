use core::fmt::Write;
use display::fb::Framebuffer;
use display::font::{draw_text, literata, measure_text, FontFamily, FontStyle};
use fw_common::{catalog, AppView, DisplayOrientation, ReaderSource, RenderRequest};
use heapless::String;
use reader_cache::layout;
use reader_cache::store::{
    BookLoadStatus, LibraryScanStatus, ReaderFault, ReaderStore, LIBRARY_WINDOW,
};
use ui::{
    app_render::{self, UiRenderModel},
    UiBook, UiCover, UiLibraryRow, UiLibraryStatus, UiTocItem,
};

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
        series: "",
        title_suffix: "",
        title_len: 0,
        volume: "",
        file_name: "",
    }; LIBRARY_WINDOW];
    let mut chapters = [UiTocItem {
        title: "",
        level: 1,
        page: 0,
        has_subsections: false,
        expanded: false,
        is_subsection: false,
        chapter: 0,
    }; MAX_UI_CHAPTERS];
    let model = ui_model(request, sd_library, &mut library_entries, &mut chapters);
    let levels = app_render::render_request(fb, request, &model);
    crate::input::set_chapter_disclosure_levels(levels);
}

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
    finished
}

#[allow(dead_code)]
pub(crate) fn page_can_be_staged(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    let Some(index) = ReaderSource::from_book_id(request.book_id).sd_index() else {
        return false;
    };
    request.view == AppView::Reading
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

pub(crate) fn reader_reads_card(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    custom_face_on_card(request, sd_library) || reader_page_has_images(request, sd_library)
}

pub(crate) fn reader_page_has_images(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    resident_image_page(request, sd_library)
        .is_some_and(|page| ui::reading::page_has_images(sd_library, page))
}

pub(crate) fn reader_page_has_pictures(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    resident_image_page(request, sd_library)
        .is_some_and(|page| ui::reading::page_has_image_blocks(sd_library, page))
}

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

pub(crate) fn render_reader_from_root(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    font_metrics: &mut storage::custom_font::MetricCache,
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
                    && storage::custom_font::draw_reading_page_body(
                        root,
                        fb,
                        sd_library,
                        font_metrics,
                        page,
                        |fb, image| draw_image(fb, image),
                    );
                if !drawn_custom {
                    if custom {
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
        series: "",
        title_suffix: "",
        title_len: 0,
        volume: "",
        file_name: "",
    }; LIBRARY_WINDOW];
    let mut chapters = [UiTocItem {
        title: "",
        level: 1,
        page: 0,
        has_subsections: false,
        expanded: false,
        is_subsection: false,
        chapter: 0,
    }; MAX_UI_CHAPTERS];
    let model = ui_model(request, sd_library, &mut library_entries, &mut chapters);
    app_render::render_sleep(fb, request, &model);
}

pub(crate) fn render_sleep_blank(fb: &mut Framebuffer) {
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
    let window = sd_library.folder_rows();
    let window_start = sd_library.folder_start();
    let library_count = window.len().min(library_entries.len());
    for (i, row) in window.iter().take(library_count).enumerate() {
        library_entries[i] = UiLibraryRow {
            name: if row.is_dir {
                row.name.as_str()
            } else {
                row.display_label.as_str()
            },
            is_folder: row.is_dir,
            series: row.series.as_str(),
            title_suffix: row.title_suffix.as_str(),
            title_len: row.title_len,
            volume: row.volume.as_str(),
            file_name: if row.is_dir { "" } else { row.name.as_str() },
        };
    }
    let (chapter_count, chapters_window_start, chapters_total) =
        fill_chapters(chapters, request, sd_library);

    let fallback_book = catalog::active_book(request.book_id);
    let (title, author) =
        sd_library.active_book_labels(request.book_id, fallback_book.title, fallback_book.author);

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
        library_status: ui_library_status(sd_library.status)
            .while_relisting(sd_library.title_fallback_pending()),
        library_entries: &library_entries[..library_count],
        library_folder: sd_library.browse().path().file_name().unwrap_or(""),
        library_window_start: window_start as u16,
        chapters: &chapters[..chapter_count],
        chapters_window_start: chapters_window_start as u16,
        chapters_total: chapters_total.min(u16::MAX as usize) as u16,
        chapter_subsections: sd_library.chapter_subsections(),
        chapter_title,
        custom_font_name: sd_library.custom_font_name(),
    }
}

fn fill_chapters<'a>(
    chapters: &mut [UiTocItem<'a>; MAX_UI_CHAPTERS],
    request: RenderRequest,
    sd_library: &'a ReaderStore,
) -> (usize, usize, usize) {
    if ReaderSource::from_book_id(request.book_id).is_sd() && sd_library.text_holds_toc() {
        let total = sd_library.overview_chapter_count();
        let window_start = sd_library.toc_window_start();

        let mut visible_total = 0usize;
        let mut selected_visible = 0usize;
        let mut current_parent = None;
        for c in 0..total {
            let is_sub = sd_library.is_chapter_subsection(c);
            if !is_sub {
                current_parent = Some(c as u16);
            }
            let is_vis = !is_sub
                || (request.chapters_expanded.is_some()
                    && request.chapters_expanded == current_parent);
            if is_vis {
                if c <= request.selection as usize {
                    selected_visible = visible_total;
                }
                visible_total += 1;
            }
        }

        let portrait = app_core::is_portrait(request.orientation);
        let selected = selected_visible.min(visible_total.saturating_sub(1));
        let start_visible = ui::render::toc_scroll_start(selected, visible_total, portrait);
        let max_rows = ui::render::toc_visible_rows(portrait);
        let end_visible = (start_visible + max_rows).min(visible_total);

        let mut current_visible = 0usize;
        let mut current_parent = None;
        let mut c_first = request.selection as usize;
        let mut c_last = request.selection as usize;
        let mut found_first = false;
        for c in 0..total {
            let is_sub = sd_library.is_chapter_subsection(c);
            if !is_sub {
                current_parent = Some(c as u16);
            }
            let is_vis = !is_sub
                || (request.chapters_expanded.is_some()
                    && request.chapters_expanded == current_parent);
            if is_vis {
                if current_visible >= start_visible && current_visible < end_visible {
                    if !found_first {
                        c_first = c;
                        found_first = true;
                    }
                    c_last = c;
                }
                current_visible += 1;
            }
        }

        if sd_library.toc_is_sparse() {
            let indices = sd_library.toc_sparse_indices();
            let count = indices.len().min(chapters.len());
            for (offset, &chapter) in indices.iter().take(count).enumerate() {
                let index = chapter as usize;
                let is_subsection = sd_library.is_chapter_subsection(index);
                let has_subsections = sd_library.chapter_has_subsections(index);
                let level = if is_subsection {
                    sd_library.overview_level_at(index).max(2)
                } else {
                    1
                };
                let expanded = if is_subsection {
                    sd_library
                        .find_parent_chapter(index)
                        .is_some_and(|parent| request.chapters_expanded == Some(parent as u16))
                } else {
                    request.chapters_expanded == Some(chapter)
                };
                chapters[offset] = UiTocItem {
                    title: sd_library.overview_title_at(index),
                    level,
                    page: sd_library.overview_row_page(index),
                    has_subsections,
                    expanded,
                    is_subsection,
                    chapter,
                };
            }
            return (count, indices.first().copied().unwrap_or(0) as usize, total);
        }

        if c_last.saturating_sub(c_first) < chapters.len()
            && c_first >= window_start
            && c_last < window_start + chapters.len()
        {
            let count = total.saturating_sub(window_start).min(chapters.len());
            for (offset, item) in chapters.iter_mut().take(count).enumerate() {
                let index = window_start + offset;
                let is_subsection = sd_library.is_chapter_subsection(index);
                let has_subsections = sd_library.chapter_has_subsections(index);
                let level = if is_subsection {
                    match sd_library.overview_level_at(index) {
                        lvl if lvl > 1 => lvl,
                        _ => 2,
                    }
                } else {
                    1
                };
                let expanded = if is_subsection {
                    if let Some(parent) = sd_library.find_parent_chapter(index) {
                        request.chapters_expanded == Some(parent as u16)
                    } else {
                        false
                    }
                } else {
                    request.chapters_expanded == Some(index as u16)
                };
                *item = UiTocItem {
                    title: sd_library.overview_title_at(index),
                    level,
                    page: sd_library.overview_row_page(index),
                    has_subsections,
                    expanded,
                    is_subsection,
                    chapter: index as u16,
                };
            }
            return (count, window_start, total);
        }

        let mut out_idx = 0;
        let mut current_visible = 0usize;
        let mut current_parent = None;
        for c in 0..total {
            let is_sub = sd_library.is_chapter_subsection(c);
            if !is_sub {
                current_parent = Some(c as u16);
            }
            let is_vis = !is_sub
                || (request.chapters_expanded.is_some()
                    && request.chapters_expanded == current_parent);
            if is_vis {
                if current_visible >= start_visible
                    && current_visible < end_visible
                    && out_idx < chapters.len()
                {
                    let is_subsection = is_sub;
                    let has_subsections = sd_library.chapter_has_subsections(c);
                    let level = if is_subsection {
                        match sd_library.overview_level_at(c) {
                            lvl if lvl > 1 => lvl,
                            _ => 2,
                        }
                    } else {
                        1
                    };
                    let expanded = if is_subsection {
                        if let Some(parent) = sd_library.find_parent_chapter(c) {
                            request.chapters_expanded == Some(parent as u16)
                        } else {
                            false
                        }
                    } else {
                        request.chapters_expanded == Some(c as u16)
                    };
                    chapters[out_idx] = UiTocItem {
                        title: sd_library.overview_title_at(c),
                        level,
                        page: sd_library.overview_row_page(c),
                        has_subsections,
                        expanded,
                        is_subsection,
                        chapter: c as u16,
                    };
                    out_idx += 1;
                }
                current_visible += 1;
            }
        }
        return (out_idx, c_first, total);
    }
    if ReaderSource::from_book_id(request.book_id).is_sd() && sd_library.toc_count() > 0 {
        let count = sd_library.toc_count().min(chapters.len());
        for (index, item) in chapters.iter_mut().take(count).enumerate() {
            if let Some(toc_item) = sd_library.toc_item(index) {
                let is_subsection = sd_library.is_chapter_subsection(index);
                let has_subsections = sd_library.chapter_has_subsections(index);
                let level = if is_subsection {
                    if toc_item.level > 1 {
                        toc_item.level
                    } else {
                        2
                    }
                } else {
                    1
                };
                let expanded = if is_subsection {
                    if let Some(parent) = sd_library.find_parent_chapter(index) {
                        request.chapters_expanded == Some(parent as u16)
                    } else {
                        false
                    }
                } else {
                    request.chapters_expanded == Some(index as u16)
                };
                *item = UiTocItem {
                    title: toc_item.title,
                    level,
                    page: toc_item.page,
                    has_subsections,
                    expanded,
                    is_subsection,
                    chapter: index as u16,
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
        for (index, item) in chapters.iter_mut().take(count).enumerate() {
            *item = UiTocItem {
                title: "",
                level: 1,
                page: 0,
                has_subsections: false,
                expanded: false,
                is_subsection: false,
                chapter: index as u16,
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
                has_subsections: false,
                expanded: false,
                is_subsection: false,
                chapter: index as u16,
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

fn reading_buffer_ready(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    reading_layout_ready(request, sd_library) && sd_library.holds_global_page(request.page)
}

fn reading_layout_ready(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    let layout_current = sd_library.type_settings() == request.type_settings();
    layout_current
        && sd_library.portrait() == app_core::is_portrait(request.orientation)
        && sd_library.loaded_index == ReaderStore::selected_book_index(request.book_id)
        && !sd_library.text_holds_toc()
}

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

fn loading_plate_due(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    let status = sd_library.reader_status();
    !reading_layout_ready(request, sd_library)
        || matches!(status, BookLoadStatus::Empty | BookLoadStatus::Loading)
        || (!sd_library.holds_global_page(request.page) && status != BookLoadStatus::Error)
}

pub(crate) fn reader_shows_plate(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    request.view == AppView::Reading
        && ReaderSource::from_book_id(request.book_id).is_sd()
        && loading_plate_due(request, sd_library)
}

pub(crate) fn reader_section_loads_quickly(
    request: RenderRequest,
    sd_library: &ReaderStore,
) -> bool {
    let Some(index) = ReaderSource::from_book_id(request.book_id).sd_index() else {
        return false;
    };
    request.view == AppView::Reading
        && reading_layout_ready(request, sd_library)
        && sd_library.reader_status() == BookLoadStatus::Ready
        && sd_library.section_indexed(usize::from(index), request.page)
}

fn draw_sd_reader_page_with(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    body: impl FnOnce(&mut Framebuffer, proto::cache::PageRecord) -> bool,
) -> bool {
    if loading_plate_due(request, sd_library) {
        draw_sd_reader_loading(fb, request, sd_library);
        return true;
    }
    match sd_library.reader_status() {
        BookLoadStatus::Error => {
            draw_sd_reader_error(fb, request, sd_library);
        }
        _ => {
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

fn draw_reader_footer(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    page_count: u32,
) {
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
    let (message, next_step) = reader_fault_message(sd_library.reader_fault());
    draw_text_centered_truncated_local(fb, body_font, message, left, right, message_y);
    if let Some(next_step) = next_step {
        let next_y = message_y + i16::from(body_font.line_height);
        draw_text_centered_truncated_local(fb, body_font, next_step, left, right, next_y);
    }
}

fn reader_fault_message(fault: ReaderFault) -> (&'static str, Option<&'static str>) {
    match fault {
        ReaderFault::Unknown => ("Can't open this book.", None),
        ReaderFault::Damaged => (
            "This book's file is damaged.",
            Some("Copy it to the card again."),
        ),
        ReaderFault::Unsupported => ("This book's format isn't supported.", None),
        ReaderFault::TooLarge => ("This book is too large to open.", None),
        ReaderFault::NoText => ("This book has no text to show.", None),
        ReaderFault::Missing => ("Can't find this book on the card.", None),
        ReaderFault::Card => (
            "There was a problem with the card.",
            Some("Try opening the book again."),
        ),
    }
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

fn book_progress_permille(request: RenderRequest) -> u16 {
    if ReaderSource::from_book_id(request.book_id).is_sd() {
        return 0;
    }
    let chapters = catalog::chapter_count().max(1) as u32;
    ((request.chapter as u32 * 1000) / chapters.saturating_sub(1).max(1)) as u16
}
