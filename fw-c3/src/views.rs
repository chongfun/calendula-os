use crate::RenderRequest;
use display::fb::Framebuffer;
use display::render::{draw_ascii, fill_rect, stroke_rect};
use display::Rect;
use reader_cache::store::ReaderStore;

#[cfg(feature = "bench-selftest")]
pub(crate) use storage::views::reader_reads_card;
pub(crate) use storage::views::{
    reader_page_has_images, reader_page_has_pictures, reader_section_loads_quickly,
    reader_shows_plate, render_sleep, render_sleep_blank,
};

const SHOW_INPUT_DEBUG: bool = false;

pub(crate) fn render(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    storage::views::render(fb, request, sd_library);
    if SHOW_INPUT_DEBUG {
        draw_input_sample(fb, request);
    }
}

pub(crate) fn render_sd_reading_while(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    go_on: impl FnMut() -> bool,
) -> bool {
    let finished = storage::views::render_sd_reading_while(fb, request, sd_library, go_on);
    if SHOW_INPUT_DEBUG {
        draw_input_sample(fb, request);
    }
    finished
}

pub(crate) fn page_can_be_staged(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    !SHOW_INPUT_DEBUG && storage::views::page_can_be_staged(request, sd_library)
}

pub(crate) fn render_reader_from_root(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    root: &crate::sd_session::SdRoot<'_>,
    band: &mut [u8],
    custom: bool,
) {
    storage::views::render_reader_from_root(
        fb,
        request,
        sd_library,
        font_metrics,
        root,
        band,
        custom,
        clock_us,
    );
}

fn clock_us() -> u64 {
    embassy_time::Instant::now().as_micros()
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
