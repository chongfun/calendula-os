use display::fb::Framebuffer;
use fw_common::{AppView, ReaderSource, RenderRequest};
use reader_cache::store::ReaderStore;

pub(crate) use storage::views::{
    reader_page_has_images, reader_page_has_pictures, reader_section_loads_quickly,
    reader_shows_plate, render_sleep, render_sleep_blank,
};

pub(crate) fn render(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    let levels = storage::views::render(fb, request, sd_library);
    if !(request.view == AppView::Reading && ReaderSource::from_book_id(request.book_id).is_sd()) {
        crate::input::set_chapter_disclosure_levels(levels);
    }
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
