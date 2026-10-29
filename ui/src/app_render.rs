use crate::{
    render::render_shell, UiBook, UiLibraryStatus, UiOrientation, UiRefreshPolicy, UiShell,
    UiSyncStatus, UiTocItem, UiView,
};
use app_core::{
    AppView, Button, DisplayOrientation, FrontButtons, RefreshPolicy, RenderRequest, SyncError,
    SyncStatus,
};
use display::fb::{FbFrame, Framebuffer};
use display::font::{draw_text, literata_display, literata_small, measure_text, FontStyle};
use display::render::draw_ascii;

#[derive(Clone, Copy, Debug)]
pub struct UiRenderModel<'a> {
    pub active_book: UiBook<'a>,
    pub library_status: UiLibraryStatus,
    pub library_entries: &'a [crate::UiLibraryRow<'a>],
    /// The folder being shown, or empty at the library root.
    pub library_folder: &'a str,
    /// Absolute row number of `library_entries[0]`; the resident window
    /// the firmware streamed in around the current selection.
    pub library_window_start: u16,
    pub chapters: &'a [UiTocItem<'a>],
    /// Absolute TOC index of `chapters[0]` and the full on-disk chapter
    /// count; long TOCs stream a window around the visible rows.
    pub chapters_window_start: u16,
    pub chapters_total: u16,
    /// Current chapter title resolved over the whole book; empty for built-in
    /// books or before a book is open. See `UiShell::chapter_title`.
    pub chapter_title: &'a str,
    pub custom_font_name: &'a str,
}

/// The drawing frame for an orientation: where `set_pixel` coordinates put
/// ink on the panel as held. Every renderer sets this before drawing; the
/// framebuffer is a long-lived static that remembers the previous frame.
pub fn fb_frame(orientation: DisplayOrientation) -> FbFrame {
    match orientation {
        DisplayOrientation::LandscapeButtonsBottom => FbFrame::Landscape,
        DisplayOrientation::LandscapeButtonsTop => FbFrame::LandscapeFlipped,
        DisplayOrientation::PortraitButtonsLeft | DisplayOrientation::PortraitButtonsRight => {
            FbFrame::Portrait
        }
    }
}

/// Draw the summoned portrait key sheet over a finished reading page.
/// Shared by every reading compositor (built-in books here, the firmware's
/// SD reader paths, the web emulator), so the band and its labels cannot
/// drift between them. A no-op unless the request holds the sheet up.
pub fn render_reading_sheet_overlay(fb: &mut Framebuffer, request: RenderRequest) {
    if !request.reading_sheet {
        return;
    }
    crate::render::render_reading_sheet(
        fb,
        ui_orientation(request.orientation),
        request.front_buttons == FrontButtons::PagesLeft,
    );
}

/// Set the framebuffer's drawing orientation and render the requested view
/// using the supplied model. Reading uses the built-in book renderer and any
/// requested key sheet; other views use the shell, including Library wait notices.
pub fn render_request(fb: &mut Framebuffer, request: RenderRequest, model: &UiRenderModel<'_>) {
    fb.set_frame(fb_frame(request.orientation));
    if request.view == AppView::Reading {
        render_builtin_reading(fb, request, model);
        render_reading_sheet_overlay(fb, request);
        return;
    }

    let shell = UiShell {
        view: ui_view(request.view),
        orientation: ui_orientation(request.orientation),
        front_pages_left: request.front_buttons == FrontButtons::PagesLeft,
        refresh_policy: ui_refresh_policy(request.refresh_policy),
        font_size: request.font_size,
        line_spacing: request.line_spacing,
        font_weight: request.font_weight,
        font_family: request.font_family,
        custom_font_name: model.custom_font_name,
        selection: request.selection,
        chapter: request.chapter,
        chapter_title: model.chapter_title,
        page: request.page,
        page_count: request.page_count,
        battery_percent: request.battery_percent,
        active_book: model.active_book,
        library_status: model.library_status,
        library_entries: model.library_entries,
        library_folder: model.library_folder,
        library_window_start: model.library_window_start,
        library_total: request.library_count,
        chapters: model.chapters,
        chapters_window_start: model.chapters_window_start,
        chapters_total: model.chapters_total,
        sync_status: ui_sync_status(request.sync_status),
        wifi_ssid: core::str::from_utf8(
            &request.wifi_ssid[..request.wifi_ssid_len.min(32) as usize],
        )
        .unwrap_or(""),
        library_menu: request.library_menu,
        library_move_pending: request.library_move_pending,
        library_rescanning: request.library_rescanning,
        library_rescan_percent: request.footer_percent,
    };
    render_shell(fb, &shell);
}

/// Redraw only the rescan note of the Library frame in `fb`, giving the
/// frame [`render_request`] draws for `request`. For progress repaints
/// mid-scan, when the scan holds the rows' catalog and arena.
pub fn render_library_rescan_progress(fb: &mut Framebuffer, request: RenderRequest) {
    fb.set_frame(fb_frame(request.orientation));
    crate::render::redraw_rescan_footer(
        fb,
        ui_orientation(request.orientation),
        request.footer_percent,
    );
}

fn ui_sync_status(status: SyncStatus) -> UiSyncStatus {
    match status {
        SyncStatus::NotConfigured => UiSyncStatus::NotConfigured,
        SyncStatus::Idle => UiSyncStatus::Idle,
        SyncStatus::ForgetPending => UiSyncStatus::ForgetPending,
        SyncStatus::Starting => UiSyncStatus::Starting,
        SyncStatus::Connecting => UiSyncStatus::Connecting,
        SyncStatus::Connected(ip) => UiSyncStatus::Connected(ip),
        SyncStatus::PortalUp(psk, ssid) => UiSyncStatus::PortalUp(psk, ssid),
        SyncStatus::Serving(ip) => UiSyncStatus::Serving(ip),
        SyncStatus::CredentialsSaved => UiSyncStatus::CredentialsSaved,
        SyncStatus::Error(error) => UiSyncStatus::Error(sync_error_label(error)),
    }
}

fn sync_error_label(error: SyncError) -> &'static str {
    match error {
        SyncError::RadioInit => "radio failed",
        SyncError::Join => "wi-fi join failed",
        SyncError::Dhcp => "no network address",
        SyncError::Storage => "card write failed",
    }
}

/// The sleep bookplate: no key is listening, so there is no margin
/// rail — the one ceremonial centered screen. Same furniture as home
/// (caps author, progress rule, italic chapter name), centered. No
/// battery; a days-old panel image must not show stale numbers.
pub fn render_sleep(fb: &mut Framebuffer, request: RenderRequest, model: &UiRenderModel<'_>) {
    fb.set_frame(fb_frame(request.orientation));
    fb.clear(true);

    // Centered ceremonial furniture, sized to the frame: the landscape
    // plate keeps its historical 800-wide geometry (including the X3's
    // 400-centered block); the portrait plate narrows to the upright leaf
    // and drops the block deeper into it.
    let portrait = fb.frame_width() < fb.frame_height();
    let cx = if portrait {
        fb.frame_width() as i16 / 2
    } else {
        400
    };
    let max_w = if portrait {
        fb.frame_width() as i16 - 60
    } else {
        720
    };
    let title_y = if portrait {
        fb.frame_height() as i16 * 3 / 7
    } else {
        204
    };

    draw_sleep_title(fb, model.active_book.title, cx, title_y, max_w as u16);
    if !model.active_book.author.is_empty() {
        let caps = literata_small(FontStyle::Regular);
        let width = crate::render::ls_width(caps, model.active_book.author, 3);
        crate::render::ls_caps(
            fb,
            caps,
            model.active_book.author,
            cx - width / 2,
            title_y + 42,
            3,
        );
    }

    let permille = crate::render::book_progress_permille(
        request.page,
        request.page_count,
        model.active_book.progress_permille,
    );
    crate::render::progress_rule(fb, cx - 120, title_y + 98, 240, permille);

    // The sleep colophon is centered on the full frame width, so it can run
    // wider than Home's left-column colophon before a long chapter name
    // needs truncating.
    let colophon_max_w = max_w;
    let colophon_w = crate::render::chapter_colophon_width(
        model.chapters,
        request.chapter,
        model.chapter_title,
        colophon_max_w,
    );
    crate::render::draw_chapter_colophon(
        fb,
        model.chapters,
        request.chapter,
        model.chapter_title,
        cx - colophon_w / 2,
        title_y + 136,
        colophon_max_w,
    );
}

/// The sleep plate's display-face title, centered on `cx` within `max_w`.
/// Two-line titles grow upward so the author/rule furniture below keeps
/// its place, mirroring the home title page, and a title cut short ends
/// in `...` as it does there.
fn draw_sleep_title(fb: &mut Framebuffer, title: &str, cx: i16, title_y: i16, max_w: u16) {
    let font = literata_display();
    let (lines, count, overflow) = crate::render::wrap_title_lines(font, title, max_w, 2);
    for (index, line) in lines[..count].iter().enumerate() {
        let y = title_y - 54 * (count - 1 - index) as i16;
        let ellipsis = if overflow && index == count - 1 {
            "..."
        } else {
            ""
        };
        draw_font_centered_fit(fb, font, line, ellipsis, cx, y, max_w);
    }
}

/// Draws `text` then `suffix` centered on `cx`, trimming `text` until the
/// two together fit `max_w`.
fn draw_font_centered_fit(
    fb: &mut Framebuffer,
    font: &display::font::BitmapFont,
    text: &str,
    suffix: &str,
    cx: i16,
    y: i16,
    max_w: u16,
) {
    let room = max_w.saturating_sub(measure_text(font, suffix));
    let mut shown = text;
    while measure_text(font, shown) > room && !shown.is_empty() {
        let mut end = shown.len() - 1;
        while end > 0 && !shown.is_char_boundary(end) {
            end -= 1;
        }
        shown = shown[..end].trim_end();
    }
    let width = measure_text(font, shown) + measure_text(font, suffix);
    let x = cx - width as i16 / 2;
    let end = draw_text(fb, font, shown, x, y, false);
    draw_text(fb, font, suffix, end, y, false);
}

fn render_builtin_reading(fb: &mut Framebuffer, request: RenderRequest, model: &UiRenderModel<'_>) {
    fb.clear(true);
    draw_ascii(fb, "READ MODE", 64, 96, false);
    draw_ascii(fb, model.active_book.title, 64, 136, false);
    draw_ascii(fb, "BACK RETURNS HOME", 64, 176, false);
    let mut chapter_buf = [0u8; 10];
    draw_ascii(fb, "CHAPTER", 64, 232, false);
    draw_ascii(
        fb,
        fmt_u32(request.chapter as u32 + 1, &mut chapter_buf),
        160,
        232,
        false,
    );
    if let Some(button) = request.last_button {
        draw_ascii(fb, button_label(button), 64, 280, false);
    }
}

fn ui_view(view: AppView) -> UiView {
    match view {
        AppView::Home => UiView::Home,
        AppView::Library => UiView::Library,
        AppView::Reading => UiView::Home,
        AppView::Chapters => UiView::Chapters,
        AppView::Wireless => UiView::Wireless,
        AppView::Settings => UiView::Settings,
    }
}

fn ui_orientation(orientation: DisplayOrientation) -> UiOrientation {
    match orientation {
        DisplayOrientation::LandscapeButtonsBottom => UiOrientation::LandscapeButtonsBottom,
        DisplayOrientation::LandscapeButtonsTop => UiOrientation::LandscapeButtonsTop,
        DisplayOrientation::PortraitButtonsLeft => UiOrientation::PortraitButtonsLeft,
        DisplayOrientation::PortraitButtonsRight => UiOrientation::PortraitButtonsRight,
    }
}

fn ui_refresh_policy(policy: RefreshPolicy) -> UiRefreshPolicy {
    match policy {
        RefreshPolicy::FastOnly => UiRefreshPolicy::FastOnly,
        RefreshPolicy::FullOnWake => UiRefreshPolicy::FullOnWake,
        RefreshPolicy::FullEveryTen => UiRefreshPolicy::FullEveryTen,
    }
}

fn button_label(button: Button) -> &'static str {
    match button {
        Button::Power => "POWER",
        Button::Back => "BACK",
        Button::Confirm => "OK",
        Button::Previous => "PREV",
        Button::Next => "NEXT",
        Button::PagePrevious => "PAGE-",
        Button::PageNext => "PAGE+",
    }
}

fn fmt_u32(n: u32, buf: &mut [u8; 10]) -> &str {
    let mut i = buf.len();
    let mut v = n;
    if v == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    core::str::from_utf8(&buf[i..]).unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sleep plate draws the progress rule from a restored page, which no
    /// open may have clamped to the count yet. A page at the top of the range
    /// draws the full rule, as the last page does.
    #[test]
    fn sleep_progress_from_a_restored_page_at_the_top_is_full() {
        let model = UiRenderModel {
            active_book: UiBook {
                title: "",
                author: "",
                progress_permille: 0,
                cover: None,
            },
            library_status: UiLibraryStatus::Ready,
            library_entries: &[],
            library_folder: "",
            library_window_start: 0,
            chapters: &[],
            chapters_window_start: 0,
            chapters_total: 0,
            chapter_title: "",
            custom_font_name: "",
        };
        let plate = |page| {
            let mut state = app_core::ReaderState::boot();
            state.page = page;
            state.sd_page_count = 500;
            state.sd_total_pages = 500;
            let mut fb = Framebuffer::new();
            render_sleep(
                &mut fb,
                state.render_request(app_core::RenderKind::Page),
                &model,
            );
            fb
        };
        assert!(plate(u32::MAX).bytes() == plate(499).bytes());
        assert!(plate(499).bytes() != plate(0).bytes());
    }

    /// The landscape sleep plate's title, alone on a cleared frame, and the
    /// leftmost and rightmost inked columns on the frame.
    fn sleep_title(title: &str) -> (Framebuffer, Option<(usize, usize)>) {
        let mut fb = Framebuffer::new();
        fb.set_frame(FbFrame::Landscape);
        fb.clear(true);
        draw_sleep_title(&mut fb, title, 400, 204, 720);
        let mut ink: Option<(usize, usize)> = None;
        for y in 0..fb.frame_height() {
            for x in 0..fb.frame_width() {
                if !fb.pixel(x, y) {
                    ink = Some(ink.map_or((x, x), |(l, r)| (l.min(x), r.max(x))));
                }
            }
        }
        (fb, ink)
    }

    /// A title too long for the plate ends in `...`, as on Home, rather
    /// than being cut silently: it must not draw the same as a title that
    /// is only the part it shows, the dots stay within the measure, and the
    /// trailing ink consists of exactly three dot glyphs.
    #[test]
    fn sleep_title_cut_short_ends_in_an_ellipsis() {
        let font = literata_display();
        let mut fb_dot = Framebuffer::new();
        fb_dot.set_frame(FbFrame::Landscape);
        fb_dot.clear(true);
        draw_text(&mut fb_dot, font, ".", 0, 204, false);
        let mut dot_min_x = usize::MAX;
        let mut dot_max_x = 0;
        let mut dot_min_y = usize::MAX;
        let mut dot_max_y = 0;
        for y in 0..fb_dot.frame_height() {
            for x in 0..fb_dot.frame_width() {
                if !fb_dot.pixel(x, y) {
                    dot_min_x = dot_min_x.min(x);
                    dot_max_x = dot_max_x.max(x);
                    dot_min_y = dot_min_y.min(y);
                    dot_max_y = dot_max_y.max(y);
                }
            }
        }
        let dot_width = dot_max_x - dot_min_x + 1;
        let dot_advance = measure_text(font, ".") as usize;

        let matches_dot = |fb: &Framebuffer, end_x: usize| -> bool {
            if end_x + 1 < dot_width {
                return false;
            }
            let start_x = end_x + 1 - dot_width;
            for dy in 0..=(dot_max_y - dot_min_y) {
                for dx in 0..dot_width {
                    if fb.pixel(start_x + dx, dot_min_y + dy)
                        != fb_dot.pixel(dot_min_x + dx, dot_min_y + dy)
                    {
                        return false;
                    }
                }
            }
            for dy in 1..=4 {
                for dx in 0..dot_width {
                    if !fb.pixel(start_x + dx, dot_min_y - dy)
                        || !fb.pixel(start_x + dx, dot_max_y + dy)
                    {
                        return false;
                    }
                }
            }
            true
        };

        for title in [
            "The Collected Correspondence of an Unhurried Victorian Naturalist, \
             with Notes on the Hedgerows of Three English Counties",
            "Donaudampfschifffahrtsgesellschaftskapitaenswitwenrentenversicherung",
        ] {
            let (lines, count, overflow) = crate::render::wrap_title_lines(font, title, 720, 2);
            assert!(overflow, "{title:?} should overflow the plate");
            // What the plate shows of the title, as a title of its own: it
            // fits, so it draws with no dots.
            let mut shown = [0u8; 256];
            let mut len = 0;
            for (index, line) in lines[..count].iter().enumerate() {
                if index > 0 {
                    shown[len] = b' ';
                    len += 1;
                }
                shown[len..len + line.len()].copy_from_slice(line.as_bytes());
                len += line.len();
            }
            let shown = core::str::from_utf8(&shown[..len]).expect("joined lines are UTF-8");

            let (cut, ink) = sleep_title(title);
            let (whole, _) = sleep_title(shown);
            assert_ne!(
                cut.bytes(),
                whole.bytes(),
                "{title:?} is cut with no ellipsis"
            );
            let (left, right) = ink.expect("the title draws");
            assert!(left >= 400 - 360 && right < 400 + 360, "{left}..{right}");

            let mut line_right: Option<usize> = None;
            for y in dot_min_y..=dot_max_y {
                for x in 0..cut.frame_width() {
                    if !cut.pixel(x, y) {
                        line_right = Some(line_right.map_or(x, |r| r.max(x)));
                    }
                }
            }
            let line_right = line_right.expect("last line has ink at the dot baseline");

            let mut dots = 0;
            let mut end_x = line_right;
            while matches_dot(&cut, end_x) {
                dots += 1;
                if end_x < dot_advance {
                    break;
                }
                end_x -= dot_advance;
            }
            assert_eq!(dots, 3, "{title:?} rendered with {dots} dots, expected 3");
        }
    }
}
