use display::fb::Framebuffer;
#[cfg(feature = "gui")]
use display::{HEIGHT, WIDTH};
#[cfg(feature = "gui")]
use egui::{Color32, ColorImage};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use ui::{
    app_render::{
        render_request_with_chapters as render_shared_request, render_sleep as render_shared_sleep,
        UiRenderModel,
    },
    UiBook, UiLibraryStatus, UiTocItem,
};

const DEMO_TITLE: &str = "Flowers for Algernon";
const DEMO_AUTHOR: &str = "Daniel Keyes";
const DEMO_CHAPTERS: [UiTocItem<'static>; 4] = [
    UiTocItem {
        title: "Bring Up",
        level: 1,
        page: 3,
        has_subsections: false,
        expanded: false,
        is_subsection: false,
        chapter: 0,
    },
    UiTocItem {
        title: "Architecture",
        level: 1,
        page: 9,
        has_subsections: false,
        expanded: false,
        is_subsection: false,
        chapter: 1,
    },
    UiTocItem {
        title: "Power",
        level: 1,
        page: 17,
        has_subsections: false,
        expanded: false,
        is_subsection: false,
        chapter: 2,
    },
    UiTocItem {
        title: "Next Phase",
        level: 1,
        page: 24,
        has_subsections: false,
        expanded: false,
        is_subsection: false,
        chapter: 3,
    },
];

pub fn render_request(
    fb: &mut Framebuffer,
    state: &app_core::ReaderState,
    request: app_core::RenderRequest,
    library_entries: &[(String, bool)],
    library_folder: &str,
) -> ui::render::DisplayedChapters {
    let rows = library_rows(library_entries);
    let sd_toc = (request.view == app_core::AppView::Chapters)
        .then(|| sd_chapter_storage(state, request))
        .flatten();
    let model = demo_model(state, request, &rows, library_folder, &sd_toc);
    render_shared_request(fb, request, &model)
}

/// The scenario's rows in the shape the shell draws them.
fn library_rows(entries: &[(String, bool)]) -> Vec<ui::UiLibraryRow<'_>> {
    entries
        .iter()
        .map(|(name, is_folder)| ui::UiLibraryRow {
            name: name.as_str(),
            is_folder: *is_folder,
            series: "",
            title_suffix: "",
            title_len: 0,
            volume: "",
            file_name: "",
        })
        .collect()
}

pub fn render_sleep(
    fb: &mut Framebuffer,
    state: &app_core::ReaderState,
    request: app_core::RenderRequest,
    library_entries: &[(String, bool)],
    library_folder: &str,
) {
    let rows = library_rows(library_entries);
    let model = demo_model(state, request, &rows, library_folder, &None);
    render_shared_sleep(fb, request, &model);
}

struct SdTocStorage<'a> {
    chapters: Vec<UiTocItem<'static>>,
    chapters_total: u16,
    chapter_subsections: &'a [u8],
}

fn sd_chapter_storage(
    state: &app_core::ReaderState,
    request: app_core::RenderRequest,
) -> Option<SdTocStorage<'_>> {
    if !app_core::ReaderSource::from_book_id(request.book_id).is_sd() {
        return None;
    }
    let count = usize::from(state.sd_chapter_count);
    let mut chapters = Vec::with_capacity(count);
    for c in 0..count {
        let ch = c as u16;
        let is_subsection = state.is_chapter_subsection(ch);
        let has_subsections = state.chapter_has_subsections(ch);
        let page = state
            .sd_chapter_pages
            .get(c)
            .copied()
            .map(|p| u32::from(p) + 1)
            .unwrap_or(0);
        chapters.push(UiTocItem {
            title: "",
            level: if is_subsection { 2 } else { 1 },
            page,
            has_subsections,
            expanded: has_subsections && state.chapters_expanded == Some(ch),
            is_subsection,
            chapter: ch,
        });
    }
    Some(SdTocStorage {
        chapters,
        chapters_total: state.sd_chapter_count,
        chapter_subsections: &state.sd_chapter_subsections,
    })
}

#[cfg(feature = "gui")]
pub fn framebuffer_to_color_image(fb: &Framebuffer) -> ColorImage {
    let mut pixels = Vec::with_capacity(WIDTH * HEIGHT);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            pixels.push(if fb.native_pixel(x, y) {
                Color32::from_rgb(238, 236, 226)
            } else {
                Color32::from_rgb(22, 22, 20)
            });
        }
    }
    ColorImage {
        size: [WIDTH, HEIGHT],
        pixels,
    }
}

pub fn write_png(path: &Path, fb: &Framebuffer) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    encode_png(&mut writer, fb)?;
    Ok(())
}

pub fn write_presented_png(
    path: &Path,
    fb: &Framebuffer,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    encode_presented_png(&mut writer, fb)?;
    Ok(())
}

pub fn encode_png<W: Write>(writer: W, fb: &Framebuffer) -> Result<(), png::EncodingError> {
    let mut encoder = png::Encoder::new(writer, display::WIDTH as u32, display::HEIGHT as u32);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&grayscale_pixels(fb))
}

/// The framebuffer as the 8-bit grayscale pixel rows encode_png writes;
/// golden checks compare these decoded pixels, not encoded PNG bytes.
pub fn grayscale_pixels(fb: &Framebuffer) -> Vec<u8> {
    let mut data = Vec::with_capacity(display::WIDTH * display::HEIGHT);
    for y in 0..display::HEIGHT {
        for x in 0..display::WIDTH {
            data.push(if fb.native_pixel(x, y) { 0xEE } else { 0x18 });
        }
    }
    data
}

pub fn encode_presented_png<W: Write>(
    writer: W,
    fb: &Framebuffer,
) -> Result<(), png::EncodingError> {
    let mut encoder = png::Encoder::new(writer, display::WIDTH as u32, display::HEIGHT as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let mut data = Vec::with_capacity(display::WIDTH * display::HEIGHT * 4);
    for y in 0..display::HEIGHT {
        for x in 0..display::WIDTH {
            if fb.native_pixel(x, y) {
                data.extend_from_slice(&[238, 236, 226, 255]);
            } else {
                data.extend_from_slice(&[22, 22, 20, 255]);
            }
        }
    }
    writer.write_image_data(&data)
}

fn demo_model<'a>(
    state: &app_core::ReaderState,
    request: app_core::RenderRequest,
    library_entries: &'a [ui::UiLibraryRow<'a>],
    library_folder: &'a str,
    sd_toc: &'a Option<SdTocStorage<'a>>,
) -> UiRenderModel<'a> {
    // SD books take their title from the scanned entry, mirroring the
    // firmware's catalog labels; the built-in demo book keeps its own.
    let sd_entry = app_core::ReaderSource::from_book_id(request.book_id)
        .sd_index()
        .and_then(|index| library_entries.get(index as usize))
        .map(|row| row.name);
    let (title, author) = match sd_entry {
        Some(entry) => (entry, ""),
        None => (DEMO_TITLE, DEMO_AUTHOR),
    };
    let (chapters, chapters_total, chapter_subsections) = if let Some(toc) = sd_toc.as_ref() {
        (
            toc.chapters.as_slice(),
            toc.chapters_total,
            toc.chapter_subsections,
        )
    } else if app_core::ReaderSource::from_book_id(request.book_id).is_sd() {
        (&[][..], state.sd_chapter_count, &[][..])
    } else {
        (
            DEMO_CHAPTERS.as_slice(),
            DEMO_CHAPTERS.len() as u16,
            &[][..],
        )
    };
    UiRenderModel {
        active_book: UiBook {
            title,
            author,
            progress_permille: progress_permille(request),
            cover: None,
        },
        library_status: if library_entries.is_empty() {
            UiLibraryStatus::Empty
        } else {
            UiLibraryStatus::Ready
        },
        library_entries,
        library_folder,
        // The emulator keeps the whole listing resident, so the window is the
        // full list starting at row 0.
        library_window_start: 0,
        chapters,
        chapters_window_start: 0,
        chapters_total,
        chapter_subsections,
        chapter_title: "",
        custom_font_name: "",
    }
}

fn progress_permille(request: app_core::RenderRequest) -> u16 {
    let total = if app_core::ReaderSource::from_book_id(request.book_id).is_sd() {
        24
    } else {
        4
    };
    (((request.page + request.chapter as u32) * 1000) / total).min(1000) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sd_contents_model_matches_loaded_chapter_count() {
        let mut state = app_core::ReaderState::boot();
        state = state.apply_library_event(
            app_core::ReducerContext::new(1, 4),
            app_core::LibraryEvent::Scanned {
                count: 1,
                catalog_epoch: 1,
            },
        );
        state = state.apply_library_event(
            app_core::ReducerContext::new(1, 4),
            app_core::LibraryEvent::Loaded {
                book_id: app_core::ReaderSource::sd(0).book_id(),
                pages: 10,
                complete: true,
                chapters: 1,
                current_chapter: 0,
                chapter_pages: [0; app_core::MAX_SD_CHAPTERS],
                chapter_subsections: [0; app_core::MAX_SD_CHAPTER_SUBSECTIONS / 8],
                position: Some(0),
                text_replaced: true,
            },
        );
        state.view = app_core::AppView::Chapters;

        let mut fb = Framebuffer::new();
        let chapters = render_request(
            &mut fb,
            &state,
            state.render_request(app_core::RenderKind::Page),
            &[(String::from("SD Book 1"), false)],
            "",
        );
        assert_eq!(chapters.rows_drawn, 1);

        // Synthetic chapters beyond 16 use empty titles so ui::render numbers them
        // dynamically (Chapter 17, Chapter 18, ...).
        state.sd_chapter_count = 18;
        let request = state.render_request(app_core::RenderKind::Page);
        let toc = sd_chapter_storage(&state, request).expect("sd chapters");
        assert_eq!(toc.chapters.len(), 18);
        assert!(toc.chapters[16].title.is_empty());
        assert!(toc.chapters[17].title.is_empty());

        // Offset zero means page one, but a chapter outside the offset
        // cache has no known page number.
        state.sd_chapter_count = (app_core::MAX_SD_CHAPTERS + 1) as u16;
        state.sd_chapter_pages[1] = 9;
        let request = state.render_request(app_core::RenderKind::Page);
        let toc = sd_chapter_storage(&state, request).expect("sd chapters");
        assert_eq!(toc.chapters[0].page, 1);
        assert_eq!(toc.chapters[1].page, 10);
        assert_eq!(toc.chapters[app_core::MAX_SD_CHAPTERS].page, 0);
    }
}
