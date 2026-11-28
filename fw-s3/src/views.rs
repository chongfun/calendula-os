use display::fb::Framebuffer;
use fw_common::RenderRequest;
use reader_cache::store::ReaderStore;

pub(crate) use storage::views::{
    reader_page_has_images, reader_page_has_pictures, reader_section_loads_quickly,
    reader_shows_plate, render_sleep, render_sleep_blank,
};

pub(crate) fn render_with_hit_map(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    hit_map: &mut ui::hit_map::HitMap,
) {
    storage::views::render_with_hit_map(fb, request, sd_library, hit_map);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_reader_from_root_with_hit_map(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    font_metrics: &mut storage::custom_font::MetricCache,
    root: &crate::sd_session::SdRoot<'_>,
    band: &mut [u8],
    custom: bool,
    hit_map: &mut ui::hit_map::HitMap,
) {
    storage::views::render_reader_from_root_with_hit_map(
        fb,
        request,
        sd_library,
        font_metrics,
        root,
        band,
        custom,
        clock_us,
        hit_map,
    );
}

fn clock_us() -> u64 {
    embassy_time::Instant::now().as_micros()
}
