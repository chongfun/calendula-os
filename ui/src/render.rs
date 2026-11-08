//! The Imprint shell: the UI is typeset like a fine book. Three text
//! voices — upright body for content, italic for the book's voice,
//! letterspaced small caps for the device's voice (headings and the
//! margin keys). The four left-bezel buttons get em-dash margin notes
//! aligned beside them (KEY_YS); key order is semantic: slot 1 is the
//! screen's primary action (bold caps), slot 2 the way elsewhere/out,
//! slots 3-4 the paired browse keys. Apparatus shows battery percent
//! only — the device does not tell time.

use crate::{
    join_qr, UiLibraryStatus, UiOrientation, UiRefreshPolicy, UiShell, UiSyncStatus, UiTocItem,
    UiView,
};
use display::fb::{FbFrame, Framebuffer};
use display::font::{
    draw_text, literata, literata_display, literata_small, measure_text, BitmapFont, FontStyle,
};
use display::render::fill_rect;
use display::{Rect, HEIGHT, WIDTH};

/// Vertical centers of the four left-bezel buttons on screen,
/// top to bottom: Back, Confirm, Previous, Next. Positioned as a fraction
/// of panel height (the X4's 480 gives the historical 120/200/280/360), so
/// the same button arrangement stays aligned on the X3's differently sized
/// panel rather than bunching toward the top.
const KEY_YS: [i16; 4] = [key_y(120), key_y(200), key_y(280), key_y(360)];

const fn key_y(x4_y: i16) -> i16 {
    (x4_y as i32 * HEIGHT as i32 / 480) as i16
}
const KEY_DASH_X: i16 = 10;
const KEY_LABEL_X: i16 = 40;
const CONTENT_X: i16 = 210;
const CONTENT_RIGHT: i16 = 740;
/// The chapter colophon is a single line with nothing to its right on that row,
/// so it may run past the wrapped-title content column out to the panel margin
/// (matching the centered sleep colophon's edge) before a long name truncates.
const COLOPHON_RIGHT: i16 = 760;
const HEADING_CX: i16 = 480;
/// Right edge for the footer battery readout. It sits in the panel corner
/// with nothing beside it, so it tucks to a 24px inset from the panel edge
/// rather than the content column's value margin — the corner then reads the
/// same in a menu as when reading. Panel-relative (the X4's historical 776).
const FOOTER_RIGHT: i16 = WIDTH as i16 - 24;
const ROW_STEP: i16 = 56;
const FIRST_ROW_Y: i16 = 118;
/// Room kept at the end of a folder row for its trailing separator, so the
/// mark that makes it a folder is never the part that gets truncated away.
const FOLDER_MARK_WIDTH: usize = 16;
/// Rows the Library list shows at once. Public so the firmware slides the
/// resident catalog window over the visible range it must stream in. The
/// portrait page runs the long axis upright, so it seats more rows above
/// its bottom key rail than landscape does above its footer line.
pub const fn library_visible_rows(portrait: bool) -> usize {
    if portrait {
        10
    } else {
        6
    }
}
/// Footer baseline: 24px up from the panel's bottom edge (the X4's
/// historical 456). Panel-relative so the taller X3 keeps its apparatus in
/// the corner rather than floating it mid-page.
const FOOTER_Y: i16 = HEIGHT as i16 - 24;
/// Baseline-to-baseline leading for the wrapped 46px display title,
/// tighter than the face's default 62px line height as title blocks
/// conventionally are.
const TITLE_LEADING: i16 = 54;

#[derive(Clone, Copy)]
struct ShellLayout {
    mirrored: bool,
    portrait: bool,
    /// Front buttons run pages-first: the key rail's slot labels must sit
    /// beside the buttons that now carry them, so slot lookups rotate the
    /// two pairs.
    pages_left: bool,
    /// Drawing-frame height: the panel's long axis stands upright in
    /// portrait, so vertical furniture (footer, key rail) hangs off this
    /// rather than the panel HEIGHT constant.
    frame_height: i16,
    content_x: i16,
    content_right: i16,
    colophon_right: i16,
    heading_cx: i16,
}

impl ShellLayout {
    const fn for_orientation(orientation: UiOrientation) -> Self {
        match orientation {
            UiOrientation::LandscapeButtonsTop => Self {
                mirrored: true,
                portrait: false,
                pages_left: false,
                frame_height: HEIGHT as i16,
                content_x: WIDTH as i16 - CONTENT_RIGHT,
                content_right: WIDTH as i16 - CONTENT_X,
                colophon_right: WIDTH as i16 - (COLOPHON_RIGHT - CONTENT_RIGHT),
                heading_cx: WIDTH as i16 - HEADING_CX,
            },
            UiOrientation::PortraitButtonsLeft | UiOrientation::PortraitButtonsRight => {
                // The margin rail moves to the bottom edge beside the front
                // buttons, so content spans the frame's full (short) width.
                let width = FbFrame::Portrait.width() as i16;
                Self {
                    mirrored: false,
                    portrait: true,
                    pages_left: false,
                    frame_height: FbFrame::Portrait.height() as i16,
                    content_x: 44,
                    content_right: width - 36,
                    colophon_right: width - 24,
                    heading_cx: width / 2,
                }
            }
            _ => Self {
                mirrored: false,
                portrait: false,
                pages_left: false,
                frame_height: HEIGHT as i16,
                content_x: CONTENT_X,
                content_right: CONTENT_RIGHT,
                colophon_right: COLOPHON_RIGHT,
                heading_cx: HEADING_CX,
            },
        }
    }

    /// Physical key position for a semantic slot. With pages-left front
    /// buttons the two pairs trade places whole, so semantic slots 0-1
    /// (back, primary) draw beside physical positions 2-3 and vice versa.
    const fn key_pos(self, slot: usize) -> i16 {
        if self.pages_left {
            KEY_YS[(slot + 2) % 4]
        } else {
            KEY_YS[slot]
        }
    }

    const fn content_width(self) -> i16 {
        self.content_right - self.content_x
    }

    const fn selection_x(self) -> i16 {
        if self.mirrored {
            self.content_right + 22
        } else {
            self.content_x - 32
        }
    }

    /// Footer baseline: the panel-bottom corner line in landscape; lifted
    /// above the bottom key rail in portrait.
    const fn footer_y(self) -> i16 {
        if self.portrait {
            self.frame_height - 100
        } else {
            FOOTER_Y
        }
    }
}

fn shell_layout(shell: &UiShell<'_>) -> ShellLayout {
    let mut layout = ShellLayout::for_orientation(shell.orientation);
    layout.pages_left = shell.front_pages_left;
    layout
}

pub fn render_shell(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    match shell.view {
        UiView::Home => render_home(fb, shell),
        UiView::Library => render_library(fb, shell),
        UiView::Chapters => render_chapters(fb, shell),
        UiView::Wireless => render_wireless(fb, shell),
        UiView::Settings => render_settings(fb, shell),
    }
}

pub fn render_shell_overlay(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    render_shell(fb, shell);
}

/// Home is a title page: big title, the author in letterspaced caps,
/// the progress rule, and a colophon in chapter-and-pages terms.
fn render_home(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let mut layout = shell_layout(shell);
    // Home direct-maps the physical key column and the reducer ignores the
    // front-pair swap there (see apply_input), so its labels stay put too.
    layout.pages_left = false;
    dash_key(fb, layout, 0, "library", false);
    dash_key(fb, layout, 1, "continue", true);
    dash_key(fb, layout, 2, "wireless", false);
    dash_key(fb, layout, 3, "settings", false);

    // Long titles wrap to further lines that grow upward, keeping the
    // author/rule/colophon furniture (and one-line titles) fixed. The
    // portrait title page drops its block deeper into the taller leaf
    // (the same three-eighths position), keeping the block's own leading —
    // and its narrower measure gets a third line, since titles that fill
    // two landscape lines need three upright ones.
    let title_y = if layout.portrait {
        layout.frame_height * 3 / 8
    } else {
        180
    };
    let max_lines = if layout.portrait { 3 } else { 2 };
    let title_font = literata_display();
    let (lines, count, overflow) = wrap_title_lines(
        title_font,
        shell.active_book.title,
        layout.content_width() as u16,
        max_lines,
    );
    for (index, line) in lines[..count].iter().enumerate() {
        let y = title_y - TITLE_LEADING * (count - 1 - index) as i16;
        let end = draw_text(fb, title_font, line, layout.content_x, y, false);
        if overflow && index == count - 1 {
            draw_text(fb, title_font, "...", end, y, false);
        }
    }
    if !shell.active_book.author.is_empty() {
        ls_caps(
            fb,
            literata_small(FontStyle::Regular),
            shell.active_book.author,
            layout.content_x,
            title_y + 42,
            3,
        );
    }

    let permille = book_progress_permille(
        shell.page,
        shell.page_count,
        shell.active_book.progress_permille,
    );
    progress_rule(fb, layout.content_x, title_y + 100, 240, permille);

    // Colophon: the chapter name alone, in the book's italic voice —
    // the progress rule already answers "how far". Roman numeral
    // fallback when the book has no usable chapter title.
    draw_chapter_colophon(
        fb,
        shell.chapters,
        shell.chapter,
        shell.chapter_title,
        layout.content_x,
        title_y + 132,
        layout.colophon_right - layout.content_x,
    );

    draw_battery_percent(fb, layout, shell.battery_percent);
}

pub(crate) fn draw_chapter_colophon(
    fb: &mut Framebuffer,
    chapters: &[UiTocItem<'_>],
    chapter: u16,
    title_override: &str,
    x: i16,
    baseline: i16,
    max_w: i16,
) -> i16 {
    let chapter_name = colophon_chapter_name(chapters, chapter, title_override);
    if chapter_name.is_empty() {
        let mut numeral = [0u8; 16];
        let mut cursor = 0;
        push_roman(&mut numeral, &mut cursor, chapter as usize + 1);
        let numeral = core::str::from_utf8(&numeral[..cursor]).unwrap_or("");
        draw_text(
            fb,
            literata_small(FontStyle::Regular),
            numeral,
            x,
            baseline,
            false,
        )
    } else {
        let italic = literata_small(FontStyle::Italic);
        let shown = fit_text(italic, chapter_name, max_w.max(60) as u16);
        draw_text(fb, italic, shown, x, baseline, false)
    }
}

/// The chapter name the colophon shows: the firmware-resolved title when
/// present (covers the whole book), else the resident list entry.
fn colophon_chapter_name<'a>(
    chapters: &'a [UiTocItem<'a>],
    chapter: u16,
    title_override: &'a str,
) -> &'a str {
    if !title_override.is_empty() {
        return title_override;
    }
    chapters
        .get(chapter as usize)
        .map(|item| item.title)
        .unwrap_or("")
}

/// Width the colophon will occupy, for centered layouts.
pub(crate) fn chapter_colophon_width(
    chapters: &[UiTocItem<'_>],
    chapter: u16,
    title_override: &str,
    max_w: i16,
) -> i16 {
    let chapter_name = colophon_chapter_name(chapters, chapter, title_override);
    if chapter_name.is_empty() {
        let mut numeral = [0u8; 16];
        let mut cursor = 0;
        push_roman(&mut numeral, &mut cursor, chapter as usize + 1);
        let numeral = core::str::from_utf8(&numeral[..cursor]).unwrap_or("");
        measure_text(literata_small(FontStyle::Regular), numeral) as i16
    } else {
        let italic = literata_small(FontStyle::Italic);
        let shown = fit_text(italic, chapter_name, max_w.max(60) as u16);
        measure_text(italic, shown) as i16
    }
}

/// How far through the book `page` is, for the Home and sleep rules. A
/// `page_count` of 0 means the book has not been measured yet, and the
/// caller's `fallback` stands in; any other count is the book's own, one page
/// included, which is read to the end once it is open.
pub fn book_progress_permille(page: u32, page_count: u32, fallback: u16) -> u16 {
    if page_count == 0 {
        return fallback;
    }
    ((page.saturating_add(1).min(page_count) as u64 * 1000) / page_count as u64) as u16
}

/// The 1px rule with a 3px head filled to the reading position.
pub(crate) fn progress_rule(fb: &mut Framebuffer, x: i16, y: i16, w: i16, permille: u16) {
    hline(fb, x, y, w);
    let fill = ((w as i32 * permille.min(1000) as i32) / 1000) as i16;
    fill_rect(
        fb,
        Rect::new(x as u16, (y - 1) as u16, fill.max(2) as u16, 3),
        false,
    );
}

fn push_roman(buf: &mut [u8], cursor: &mut usize, value: usize) {
    const PAIRS: [(usize, &str); 9] = [
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut remaining = value.min(399);
    for (weight, glyphs) in PAIRS {
        while remaining >= weight {
            push_str(buf, cursor, glyphs);
            remaining -= weight;
        }
    }
}

/// Which line the Library footer carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LibraryFooterLine {
    /// A picked action's wait or its result has something of its own to say.
    Note,
    /// A pick is waiting on a rescan of the card, which takes seconds on a
    /// large library. Said in place of the position, so the wait does not
    /// read as a hang.
    Rescanning,
    /// Just the position. The teaching line names the sheet key, and that
    /// press does something else here, or nothing.
    Position,
    /// The resting line, which teaches the sheet key.
    Hint,
}

/// What the footer may say, given what the sheet key would do.
///
/// The teaching line is the only thing here that promises a press, so it
/// only appears when that press opens the sheet. A move through the tree
/// swallows it exactly as the sheet being up already redirects it. An action
/// note takes priority, followed by a rescan notice; other waits show position.
fn library_footer_line(
    menu: app_core::LibraryMenu,
    move_pending: bool,
    rescanning: bool,
) -> LibraryFooterLine {
    match menu {
        app_core::LibraryMenu::Busy { .. } | app_core::LibraryMenu::Done { .. } => {
            LibraryFooterLine::Note
        }
        _ if rescanning => LibraryFooterLine::Rescanning,
        app_core::LibraryMenu::Sheet { .. } | app_core::LibraryMenu::Details => {
            LibraryFooterLine::Position
        }
        app_core::LibraryMenu::None if move_pending => LibraryFooterLine::Position,
        app_core::LibraryMenu::None => LibraryFooterLine::Hint,
    }
}

/// Which Library controls the reducer will honour on the next press.
///
/// The rail draws what this says and nothing else, because a label is a
/// promise: every key it names has to do what it says when pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LibraryRail {
    /// The actions sheet is up and owns the keys.
    Sheet,
    /// A selected book's static details replace the listing until closed.
    Details,
    /// Something is outstanding, either a picked action or a move through the
    /// tree. The reducer swallows every press but Back, and Back leaves
    /// Library rather than going up a level, so one live key and one word.
    Held,
    /// Nothing outstanding. Back is a level inside a folder and the way out
    /// at the root, and the other three act.
    Live { back: &'static str },
}

/// What the rail may offer, given what a press would actually do.
///
/// A wait outranks the sheet: while a move or an action is outstanding the
/// reducer swallows the sheet's keys too, so a rail drawn from the sheet
/// alone would name three presses that go nowhere.
fn library_rail(in_folder: bool, menu: app_core::LibraryMenu, move_pending: bool) -> LibraryRail {
    if move_pending || matches!(menu, app_core::LibraryMenu::Busy { .. }) {
        LibraryRail::Held
    } else if matches!(menu, app_core::LibraryMenu::Sheet { .. }) {
        LibraryRail::Sheet
    } else if menu == app_core::LibraryMenu::Details {
        LibraryRail::Details
    } else {
        LibraryRail::Live {
            back: if in_folder { "up" } else { "home" },
        }
    }
}

/// Clear and draw the Library's resident rows, selection, controls, and footer.
/// Unavailable or empty listings show a status note; rows outside the resident
/// window stay blank. Action and rescan notices replace the position footer.
fn render_library(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let layout = shell_layout(shell);
    // A relist can fence or clear the resident rows before the reducer sees
    // the replacement event. Never let Details hide the current card status.
    if shell.library_menu == app_core::LibraryMenu::Details
        && shell.library_status == UiLibraryStatus::Ready
    {
        render_library_details(fb, shell, layout);
        return;
    }
    let in_folder = !shell.library_folder.is_empty();
    // No label may promise an action the press will not take.
    match library_rail(in_folder, shell.library_menu, shell.library_move_pending) {
        LibraryRail::Sheet => {
            dash_key(fb, layout, 0, "cancel", false);
            dash_key(fb, layout, 1, "select", true);
            dash_key(fb, layout, 2, "previous", false);
            dash_key(fb, layout, 3, "next", false);
        }
        // Only the key that works is drawn; the rest of the rail stays bare
        // for the moment the wait takes.
        LibraryRail::Held => {
            dash_key(fb, layout, 0, "home", false);
        }
        LibraryRail::Details => {
            dash_key(fb, layout, 0, "close", false);
        }
        // A settled note is dismissed by any press, and that press still
        // acts, so the ordinary rail is honest again.
        LibraryRail::Live { back } => {
            dash_key(fb, layout, 0, back, false);
            dash_key(fb, layout, 1, "open", true);
            dash_key(fb, layout, 2, "previous", false);
            dash_key(fb, layout, 3, "next", false);
        }
    }
    // Inside a folder the screen is that folder, and Back is a level rather
    // than the way out; the rail says which so the label matches the press.
    heading(
        fb,
        layout,
        if in_folder {
            fitted_heading(shell.library_folder)
        } else {
            "Library"
        },
    );

    match shell.library_status {
        UiLibraryStatus::NotScanned | UiLibraryStatus::Scanning => {
            centered_note(fb, layout, "reading the card\u{2026}");
            finish_working_screen(fb, shell, layout);
            return;
        }
        UiLibraryStatus::Error => {
            centered_note(fb, layout, "the library is unavailable");
            finish_working_screen(fb, shell, layout);
            return;
        }
        UiLibraryStatus::Empty => {
            centered_note(fb, layout, "no books \u{2014} add EPUB files to /books");
            finish_working_screen(fb, shell, layout);
            return;
        }
        UiLibraryStatus::Ready => {}
    }
    let total = shell.library_total as usize;
    if total == 0 || shell.library_entries.is_empty() {
        centered_note(fb, layout, "no books \u{2014} add EPUB files to /books");
        finish_working_screen(fb, shell, layout);
        return;
    }

    // `selection` and `start` are absolute catalog indices; rows are read out
    // of the resident window, which the firmware guarantees covers the visible
    // range. A miss (stale window mid-refill) leaves that row blank rather than
    // drawing the wrong book.
    let selected_index = (shell.selection as usize).min(total.saturating_sub(1));
    let start = library_scroll_start(selected_index, total, layout.portrait);
    let window_start = shell.library_window_start as usize;
    let body = literata(FontStyle::Regular);
    let mut y = FIRST_ROW_Y;
    for row in 0..library_visible_rows(layout.portrait) {
        let abs = start + row;
        if abs >= total {
            break;
        }
        let Some(entry) = abs
            .checked_sub(window_start)
            .and_then(|offset| shell.library_entries.get(offset))
        else {
            y += ROW_STEP;
            continue;
        };
        if abs == selected_index {
            selection_arrow(fb, layout, y);
        }
        // A folder wears a trailing separator, the way it does on a computer.
        // Shape rather than weight or shade: the panel is one bit deep, and a
        // reader who cannot pick out a subtle difference still reads a slash.
        if entry.is_folder {
            let drawn = draw_end_truncated(
                fb,
                body,
                entry.name,
                layout.content_x,
                y,
                (layout.content_width().max(0) as usize).saturating_sub(FOLDER_MARK_WIDTH),
            );
            draw_text_truncated(
                fb,
                body,
                "/",
                layout.content_x + drawn as i16,
                y,
                FOLDER_MARK_WIDTH,
                false,
            );
        } else {
            draw_library_book(fb, body, layout, entry, shell.library_folder, y);
        }
        y += ROW_STEP;
    }

    // The actions sheet overlays the list; the footer carries a running
    // action's progress and its settled notes. The selection arrow above
    // still names the book being acted on.
    if let app_core::LibraryMenu::Sheet { row } = shell.library_menu {
        let selected_entry = (shell.selection as usize)
            .checked_sub(window_start)
            .and_then(|offset| shell.library_entries.get(offset))
            .map(|entry| entry.name)
            .unwrap_or("this book");
        render_library_sheet(fb, layout, selected_entry, row);
    }
    match library_footer_line(
        shell.library_menu,
        shell.library_move_pending,
        shell.library_rescanning,
    ) {
        LibraryFooterLine::Note => match shell.library_menu {
            app_core::LibraryMenu::Busy {
                action: app_core::LibraryAction::ClearCache,
                ..
            } => footer_note(fb, layout, "clearing\u{2026}"),
            app_core::LibraryMenu::Done {
                action: app_core::LibraryAction::ClearCache,
                ok,
            } => footer_note(
                fb,
                layout,
                if ok {
                    "cache cleared"
                } else {
                    "cache not cleared"
                },
            ),
            _ => position_footer(fb, layout, selected_index + 1, total),
        },
        LibraryFooterLine::Rescanning => rescan_footer(fb, layout, shell.library_rescan_percent),
        LibraryFooterLine::Position => position_footer(fb, layout, selected_index + 1, total),
        LibraryFooterLine::Hint => library_footer(fb, layout, selected_index + 1, total),
    }
    finish_working_screen(fb, shell, layout);
}

const RESCAN_NOTE: &str = "updating the library\u{2026}";
const RESCAN_RULE_WIDTH: i16 = 200;
/// Room either side of the rescan note and rule, for italic overhang.
const RESCAN_NOTE_PAD: i16 = 8;

/// The note's baseline and the rule's line, straddling the footer line. The
/// redraw clears between them, so both read them from here.
const fn rescan_lines(layout: ShellLayout) -> (i16, i16) {
    (layout.footer_y() - 10, layout.footer_y() + 6)
}

// In the apparatus size, with the rule below carrying how far the scan has got.
fn rescan_footer(fb: &mut Framebuffer, layout: ShellLayout, percent: Option<u8>) {
    let font = literata_small(FontStyle::Italic);
    let (note_y, rule_y) = rescan_lines(layout);
    draw_text_centered(fb, font, RESCAN_NOTE, layout.heading_cx, note_y);
    let permille = percent.map_or(0, |p| u16::from(p) * 10);
    progress_rule(
        fb,
        layout.heading_cx - RESCAN_RULE_WIDTH / 2,
        rule_y,
        RESCAN_RULE_WIDTH,
        permille,
    );
}

/// Clear the rescan note and progress rule, as wide as the wider of the two
/// and no wider, and draw them again at `percent`. The battery shares the
/// line in landscape, so the clear stops short of the corner.
pub(crate) fn redraw_rescan_footer(
    fb: &mut Framebuffer,
    orientation: UiOrientation,
    percent: Option<u8>,
) {
    let layout = ShellLayout::for_orientation(orientation);
    let font = literata_small(FontStyle::Italic);
    let text_w = measure_text(font, RESCAN_NOTE) as i16;
    let widest = RESCAN_RULE_WIDTH.max(text_w);
    let x = layout.heading_cx - widest / 2 - RESCAN_NOTE_PAD;
    let (note_y, rule_y) = rescan_lines(layout);
    // From above the note's ascent to below the rule's 3px head.
    let y = note_y - font.baseline as i16 - 2;
    let h = rule_y + 5 - y;
    fill_rect(
        fb,
        Rect::new(
            x as u16,
            y as u16,
            (widest + 2 * RESCAN_NOTE_PAD) as u16,
            h as u16,
        ),
        true,
    );
    rescan_footer(fb, layout, percent);
}

/// Library's resting footer teaches the sheet key — the shell's only
/// side-key binding a label cannot sit beside — in the corner the battery
/// leaves free, naming the key by its hold-independent reading function.
/// Portrait's centered position would collide with a corner line, so the
/// line splits: hint left, position right.
fn library_footer(fb: &mut Framebuffer, layout: ShellLayout, current: usize, total: usize) {
    let hint = "page-back opens book actions";
    let italic = literata_small(FontStyle::Italic);
    if layout.portrait {
        draw_text(fb, italic, hint, layout.content_x, layout.footer_y(), false);
        let mut buf = [0u8; 32];
        let mut cursor = 0;
        push_str(&mut buf, &mut cursor, "\u{2013} ");
        push_usize(&mut buf, &mut cursor, current);
        push_str(&mut buf, &mut cursor, " of ");
        push_usize(&mut buf, &mut cursor, total);
        push_str(&mut buf, &mut cursor, " \u{2013}");
        let label = core::str::from_utf8(&buf[..cursor]).unwrap_or("");
        let small = literata_small(FontStyle::Regular);
        let width = measure_text(small, label) as i16;
        draw_text(
            fb,
            small,
            label,
            layout.content_right - width,
            layout.footer_y(),
            false,
        );
    } else if layout.mirrored {
        let width = measure_text(italic, hint) as i16;
        draw_text(fb, italic, hint, WIDTH as i16 - 24 - width, FOOTER_Y, false);
        position_footer(fb, layout, current, total);
    } else {
        draw_text(fb, italic, hint, 24, FOOTER_Y, false);
        position_footer(fb, layout, current, total);
    }
}

/// The sheet row label for an action.
const fn library_action_row(action: app_core::LibraryAction) -> &'static str {
    match action {
        app_core::LibraryAction::ClearCache => "Clear cache",
        app_core::LibraryAction::Details => "Book details",
    }
}

/// The per-book actions sheet: a bordered card over the lower list, headed
/// by the book it acts on, one row per action with the browse cursor.
fn render_library_sheet(fb: &mut Framebuffer, layout: ShellLayout, title: &str, row: u8) {
    let header_h: i16 = 46;
    let row_h: i16 = 48;
    let pad: i16 = 12;
    let actions = app_core::LIBRARY_ACTIONS;
    // List every action. A sheet showing only the cursor's row gives no
    // sign that Previous/Next reach another one.
    let height = header_h + row_h * actions.len() as i16 + pad;
    let bottom = layout.footer_y() - 16;
    let top = bottom - height;
    let x0 = layout.content_x - 8;
    let x1 = layout.content_right + 8;
    fill_rect(
        fb,
        Rect::new(x0 as u16, top as u16, (x1 - x0) as u16, height as u16),
        true,
    );
    // A 2px frame keeps the card legible over whatever rows it covers.
    for (bx, by, bw, bh) in [
        (x0, top, x1 - x0, 2),
        (x0, bottom - 2, x1 - x0, 2),
        (x0, top, 2, height),
        (x1 - 2, top, 2, height),
    ] {
        fill_rect(
            fb,
            Rect::new(bx as u16, by as u16, bw as u16, bh as u16),
            false,
        );
    }
    let text_x = x0 + 24;
    draw_text_truncated(
        fb,
        literata_small(FontStyle::Italic),
        title,
        text_x,
        top + 32,
        (x1 - x0 - 48) as usize,
        false,
    );
    hline(fb, x0 + 12, top + header_h - 4, x1 - x0 - 24);
    let cursor = row as usize % actions.len().max(1);
    for (index, action) in actions.iter().enumerate() {
        let y = top + header_h + 32 + row_h * index as i16;
        if index == cursor {
            draw_text(
                fb,
                literata(FontStyle::Regular),
                "\u{2192}",
                text_x,
                y,
                false,
            );
        }
        draw_text(
            fb,
            literata(FontStyle::Regular),
            library_action_row(*action),
            text_x + 36,
            y,
            false,
        );
    }
}

/// A cut moves back to a word break only when the word fragment it drops is
/// at most this fraction of the row's width (a twentieth, about two letters).
/// That avoids a one-letter stub without leaving a gap before the mark.
const WORD_SLACK_DIVISOR: u16 = 20;

/// Marks text a row or a catalog record left out: the single ellipsis
/// glyph, which Literata draws narrower than three periods.
const OMISSION: &str = "\u{2026}";

/// Room for a joined catalog title: the stored prefix, an omission mark,
/// and the stored ending.
const JOINED_TITLE_BYTES: usize = proto::catalog::CATALOG_TITLE_BYTES
    + OMISSION.len()
    + proto::catalog::CATALOG_TITLE_SUFFIX_BYTES;

/// Punctuation that dangles next to an omission mark or at the edge of a
/// stripped series name, apostrophes included ("Hitchhiker'").
fn is_join_punctuation(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            ',' | ':'
                | ';'
                | '-'
                | '/'
                | '|'
                | '_'
                | '~'
                | '\''
                | '\u{2019}'
                | '\u{2013}'
                | '\u{2014}'
        )
}

/// Deduplicate only overlap proven by the normalized source-title length.
/// Coincidentally repeated text in a long title cannot prove adjacency, and
/// a row without a recorded length keeps an explicit omission mark.
fn title_suffix_continuation<'a>(
    beginning: &str,
    saved_end: &'a str,
    title_len: u8,
) -> (&'a str, bool) {
    if saved_end.is_empty() {
        return ("", false);
    }
    if title_len == 0 {
        return (saved_end, true);
    }
    let stored = beginning.len() + saved_end.len();
    let overlap = stored.saturating_sub(usize::from(title_len));
    if overlap == 0 {
        return (saved_end, usize::from(title_len) > stored);
    }
    // An ending that lies wholly inside the prefix adds nothing to it.
    if overlap <= saved_end.len()
        && saved_end.is_char_boundary(overlap)
        && beginning.ends_with(&saved_end[..overlap])
    {
        return (&saved_end[overlap..], false);
    }
    // Keep the omission mark between corrupt or ambiguous fragments.
    (saved_end, true)
}

/// Return the book's title as one string. A kept ending follows the prefix
/// past their proven overlap, or after an omission mark; the returned flag
/// says the mark is present. One string wraps a straddling word whole.
fn joined_title<'a>(
    book: &crate::UiLibraryRow<'a>,
    buf: &'a mut [u8; JOINED_TITLE_BYTES],
) -> (&'a str, bool) {
    let (continuation, gap) =
        title_suffix_continuation(book.name, book.title_suffix, book.title_len);
    if continuation.is_empty() {
        return (book.name, gap);
    }
    let mut len = 0;
    for part in [book.name, if gap { OMISSION } else { "" }, continuation] {
        let Some(slot) = buf.get_mut(len..len + part.len()) else {
            // The parts exceed what a catalog row holds, so show the name.
            return (book.name, gap);
        };
        slot.copy_from_slice(part.as_bytes());
        len += part.len();
    }
    let buf: &'a [u8; JOINED_TITLE_BYTES] = buf;
    // This function copies whole UTF-8 strings end to end, so the buffer
    // always decodes.
    (core::str::from_utf8(&buf[..len]).unwrap_or(book.name), gap)
}

/// Pull a cut that fell inside a word back to the last word break, when the
/// part of the word it drops measures at most `slack` pixels. A longer
/// fragment keeps the original cut, so a title fills its row.
/// Unspaced scripts keep the cut.
fn head_at_word(font: &BitmapFont, text: &str, cut: usize, slack: u16) -> usize {
    let inside_word = cut < text.len()
        && !text[cut..].starts_with(char::is_whitespace)
        && !text[..cut].ends_with(char::is_whitespace);
    match text[..cut].rfind(char::is_whitespace) {
        Some(at) if inside_word && measure_text(font, &text[at..cut]) <= slack => at,
        _ => cut,
    }
}

/// Move a tail that starts inside a word to a word start: forward to the
/// next word when the tail has one, otherwise back to its own word's start.
/// Unspaced scripts keep the cut.
fn tail_at_word(text: &str, start: usize) -> usize {
    let inside_word = start > 0
        && start < text.len()
        && !text[..start].ends_with(char::is_whitespace)
        && !text[start..].starts_with(char::is_whitespace);
    if !inside_word {
        return start;
    }
    if let Some(at) = text[start..].find(char::is_whitespace) {
        return start + at;
    }
    text[..start]
        .rfind(char::is_whitespace)
        .map_or(start, |at| at + 1)
}

/// A title's visible beginning and end, including a mark when the middle
/// has been omitted. All returned slices remain on UTF-8 boundaries. The
/// list draws through [`middle_parts`]; this form is kept for the tests.
#[cfg(test)]
fn middle_title_parts<'a>(
    font: &BitmapFont,
    beginning: &'a str,
    saved_end: &'a str,
    title_len: u8,
    width: u16,
) -> (&'a str, &'a str, bool) {
    let (continuation, missing_middle) = title_suffix_continuation(beginning, saved_end, title_len);
    middle_parts(font, beginning, continuation, missing_middle, width)
}

/// A title's visible beginning and end once its ending is known:
/// `continuation` follows `beginning` directly unless `missing_middle`. Cuts land between words
/// where the text has spaces, keeping the head the larger part.
fn middle_parts<'a>(
    font: &BitmapFont,
    beginning: &'a str,
    continuation: &'a str,
    missing_middle: bool,
    width: u16,
) -> (&'a str, &'a str, bool) {
    if !missing_middle
        && measure_text(font, beginning).saturating_add(measure_text(font, continuation)) <= width
    {
        return (beginning, continuation, false);
    }
    let room = width.saturating_sub(measure_text(font, OMISSION));
    let tail_source = if continuation.is_empty() {
        beginning
    } else {
        continuation
    };
    // Preserve more of the beginning so titles remain recognizable, but
    // give the final volume/qualifier a guaranteed part of the measure.
    let tail_budget = room * 2 / 5;
    let mut start = tail_source.len();
    for (at, _) in tail_source.char_indices().rev() {
        if measure_text(font, &tail_source[at..]) > tail_budget {
            break;
        }
        start = at;
    }
    // A word start that would take more than three fifths of the room
    // keeps the measured cut instead.
    let measured = start;
    let mut start = tail_at_word(tail_source, start);
    if measure_text(font, &tail_source[start..]) > room * 3 / 5 {
        start = measured;
    }
    let mut tail = tail_source[start..].trim_start_matches(is_join_punctuation);
    if tail.is_empty() {
        tail = &tail_source[start..];
    }
    let fitted = fit_text(
        font,
        beginning,
        room.saturating_sub(measure_text(font, tail)),
    );
    let cut = head_at_word(font, beginning, fitted.len(), room / WORD_SLACK_DIVISOR);
    let mut head = trim_cut_end(&beginning[..cut]);
    if head.is_empty() {
        head = fitted;
    }
    if continuation.is_empty() {
        // These are two slices of the same string: they cannot overlap.
        while head.len() + tail.len() > beginning.len() {
            let advance = tail.chars().next().map_or(0, char::len_utf8);
            if advance == 0 {
                break;
            }
            tail = &tail[advance..];
        }
    }
    (head, tail, true)
}

/// The end of a cut before its omission mark, without dangling punctuation
/// or a period ("story.…").
fn trim_cut_end(text: &str) -> &str {
    text.trim_end_matches(|c| is_join_punctuation(c) || c == '.')
}

/// Cut `text` to `max_w`, keeping room for an omission mark, and return
/// whether it was cut. The cut lands between words where it can.
fn end_truncated<'a>(font: &BitmapFont, text: &'a str, max_w: u16) -> (&'a str, bool) {
    if measure_text(font, text) <= max_w {
        return (text, false);
    }
    let fitted = fit_text(
        font,
        text,
        max_w.saturating_sub(measure_text(font, OMISSION)),
    );
    let cut = head_at_word(font, text, fitted.len(), max_w / WORD_SLACK_DIVISOR);
    let head = trim_cut_end(&text[..cut]);
    (if head.is_empty() { fitted } else { head }, true)
}

/// Draw `text` cut at its end to `max_w`, with an omission mark when cut,
/// and return the width drawn.
fn draw_end_truncated(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    text: &str,
    x: i16,
    y: i16,
    max_w: usize,
) -> u16 {
    let (shown, cut) = end_truncated(font, text, max_w.min(u16::MAX as usize) as u16);
    let at = draw_text(fb, font, shown, x, y, false);
    if cut {
        draw_text(fb, font, OMISSION, at, y, false);
        measure_text(font, shown) + measure_text(font, OMISSION)
    } else {
        measure_text(font, shown)
    }
}

/// A volume as the Library shows it: leading zeros and a zero fraction
/// dropped, so a Calibre "1.0" or a padded "01" reads "1". The result is a
/// slice of `volume`.
fn display_volume(volume: &str) -> &str {
    let mut text = volume;
    if let Some((whole, fraction)) = text.split_once('.') {
        let fraction = fraction.trim_end_matches('0');
        text = if fraction.is_empty() {
            whole
        } else {
            &text[..whole.len() + 1 + fraction.len()]
        };
    }
    let zeros = text.bytes().take_while(|&b| b == b'0').count();
    let keep = if text[zeros..].starts_with(|c: char| c.is_ascii_digit()) {
        zeros
    } else {
        // "0", "0.5": the last zero of the whole part stays.
        zeros.saturating_sub(1)
    };
    &text[keep..]
}

/// The volume number at the end of `text` and where it starts: digits with
/// at most one fraction. A dot with no digits before it ("Vol.3") is not
/// part of the number.
fn trailing_number(text: &str) -> Option<(usize, &str)> {
    let digits = |s: &str| s.len() - s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let last = digits(text);
    if last == 0 {
        return None;
    }
    let mut start = text.len() - last;
    if let Some(before) = text[..start].strip_suffix('.') {
        let whole = digits(before);
        if whole > 0 {
            start = before.len() - whole;
        }
    }
    Some((start, &text[start..]))
}

/// Strip "<marker> <number>" from the end of `text` when the number is the
/// book's volume: "Vol. 3", "vol 3", "Volume 03", "Book 3", "#3". Returns
/// the title before it, without the separator that joined them.
fn strip_volume_marker<'a>(text: &'a str, volume: &str, bracket: Option<char>) -> Option<&'a str> {
    let (start, number) = trailing_number(text)?;
    if display_volume(number) != volume {
        return None;
    }
    let before = text[..start].trim_end();
    let mut head = None;
    for marker in ["volume", "vol.", "vol", "book", "#", "no."] {
        let Some(at) = before.len().checked_sub(marker.len()) else {
            continue;
        };
        if before.is_char_boundary(at)
            && before[at..].eq_ignore_ascii_case(marker)
            && !before[..at].ends_with(char::is_alphanumeric)
        {
            head = Some(before[..at].trim_end());
            break;
        }
    }
    // Inside brackets the number may stand alone: "Title (3)".
    let head = match (head, bracket) {
        (Some(head), _) => head,
        (None, Some(_)) => before,
        (None, None) => return None,
    };
    let head = match bracket {
        Some(open) => head.strip_suffix(open)?.trim_end(),
        None => head,
    };
    let head = head.trim_end_matches(is_join_punctuation);
    head.contains(char::is_alphanumeric).then_some(head)
}

/// Remove a terminal volume marker only if its number agrees with the
/// persisted volume metadata; a bare trailing number stays. `title` must
/// run to the title's real end, as the string from [`joined_title`] does
/// even across an omission mark.
fn title_without_volume_suffix<'a>(title: &'a str, volume: &str) -> &'a str {
    let volume = display_volume(volume);
    for (close, open) in [(')', '('), (']', '[')] {
        if let Some(inner) = title.strip_suffix(close) {
            if let Some(head) = strip_volume_marker(inner.trim_end(), volume, Some(open)) {
                return head;
            }
        }
    }
    strip_volume_marker(title, volume, None).unwrap_or(title)
}

/// `text` without the bracketed notes it ends with, nested ones included:
/// "Vol. 9 (light novel) (Series (novel))" ends at "Vol. 9". An unbalanced
/// bracket stops the stripping.
fn without_trailing_notes(text: &str) -> &str {
    let mut text = text.trim_end();
    while let Some(close) = text.chars().last().filter(|c| matches!(c, ')' | ']')) {
        let open = if close == ')' { '(' } else { '[' };
        let mut depth = 0usize;
        let mut start = None;
        for (at, ch) in text.char_indices().rev() {
            if ch == close {
                depth += 1;
            } else if ch == open {
                depth -= 1;
                if depth == 0 {
                    start = Some(at);
                    break;
                }
            }
        }
        match start {
            Some(at) if at > 0 => text = text[..at].trim_end(),
            _ => break,
        }
    }
    text
}

/// Return a volume row's title without the number its column displays: a
/// matching marker at the end, before bracketed notes ("Series, Vol. 3
/// (Illustrated Edition)" reads "Series (Illustrated Edition)"), or before a
/// subtitle ("Series: Mist"). Drop notes that repeat the title too.
fn title_without_volume<'a>(
    title: &'a str,
    volume: &str,
    buf: &'a mut [u8; JOINED_TITLE_BYTES],
) -> &'a str {
    let shown = title_without_volume_suffix(title, volume);
    if shown.len() < title.len() {
        return shown;
    }
    let bare = without_trailing_notes(title);
    let head = title_without_volume_suffix(bare, volume);
    let marker_in_head = head.len() < bare.len();
    let notes = &title[bare.len()..];
    if marker_in_head || notes_hold_volume(notes, volume) {
        let mut out = TitleWriter::new(buf);
        out.push(head);
        let mut dropped_marker = marker_in_head;
        let mut rest = notes;
        while let Some((note, after)) = next_note(rest) {
            rest = after;
            // Skip the volume note, a note the omission cut off partway, and
            // a note that repeats the title; keep every other note.
            if !dropped_marker && is_volume_note(note, volume) {
                dropped_marker = true;
            } else if !note.contains(OMISSION) && !note_repeats_title(note, head) {
                out.push(" ");
                out.push(note);
            }
        }
        return out.finish().unwrap_or(title);
    }
    let volume = display_volume(volume);
    for (at, _) in title.match_indices([':', '-', '\u{2013}', '\u{2014}']) {
        let separator = title[at..].chars().next().map_or(1, char::len_utf8);
        // A dash separates a subtitle only with spaces around it.
        if !title[at..].starts_with(':')
            && !(title[..at].ends_with(' ') && title[at + separator..].starts_with(' '))
        {
            continue;
        }
        let Some(head) = strip_volume_marker(title[..at].trim_end(), volume, None) else {
            continue;
        };
        let subtitle = title[at + separator..].trim_start();
        if !subtitle.contains(char::is_alphanumeric) {
            continue;
        }
        let mut out = TitleWriter::new(buf);
        out.push(head);
        out.push(": ");
        out.push(subtitle);
        return out.finish().unwrap_or(title);
    }
    title
}

/// Writes whole strings end to end into a title buffer.
struct TitleWriter<'a> {
    buf: &'a mut [u8; JOINED_TITLE_BYTES],
    len: usize,
    fits: bool,
}

impl<'a> TitleWriter<'a> {
    fn new(buf: &'a mut [u8; JOINED_TITLE_BYTES]) -> Self {
        Self {
            buf,
            len: 0,
            fits: true,
        }
    }

    fn push(&mut self, text: &str) {
        match self.buf.get_mut(self.len..self.len + text.len()) {
            Some(slot) => {
                slot.copy_from_slice(text.as_bytes());
                self.len += text.len();
            }
            None => self.fits = false,
        }
    }

    /// Return the text written, or `None` when a part did not fit.
    fn finish(self) -> Option<&'a str> {
        let buf: &'a [u8; JOINED_TITLE_BYTES] = self.buf;
        // The writer copies in only whole UTF-8 strings, so the buffer stays
        // valid UTF-8.
        self.fits
            .then(|| core::str::from_utf8(&buf[..self.len]).ok())
            .flatten()
    }
}

/// Return the first bracketed group of `notes` and what follows it. `notes` is the
/// tail [`without_trailing_notes`] cut off: balanced groups and spaces.
fn next_note(notes: &str) -> Option<(&str, &str)> {
    let notes = notes.trim_start();
    let open = notes.chars().next()?;
    let close = match open {
        '(' => ')',
        '[' => ']',
        _ => return None,
    };
    let mut depth = 0usize;
    for (at, ch) in notes.char_indices() {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some((&notes[..=at], &notes[at + 1..]));
            }
        }
    }
    None
}

/// Whether a bracketed group is the volume itself: "(Vol. 3)", "[3]".
fn is_volume_note(note: &str, volume: &str) -> bool {
    let inner = note[1..note.len() - 1].trim();
    let Some((start, number)) = trailing_number(inner) else {
        return false;
    };
    let before = inner[..start].trim_end();
    display_volume(number) == display_volume(volume)
        && (before.is_empty()
            || ["volume", "vol.", "vol", "book", "#", "no."]
                .iter()
                .any(|marker| before.eq_ignore_ascii_case(marker)))
}

fn notes_hold_volume(notes: &str, volume: &str) -> bool {
    let mut rest = notes;
    while let Some((note, after)) = next_note(rest) {
        if is_volume_note(note, volume) {
            return true;
        }
        rest = after;
    }
    false
}

/// Whether a note's letters and digits equal the title's, as in "Some
/// Series, Vol. 5 (Some Series)". A note with any other words, such as
/// "(Some Series Complete Edition)", names something the row should keep.
fn note_repeats_title(note: &str, title: &str) -> bool {
    let inner = &note[1..note.len() - 1];
    name_letters(inner).next().is_some() && name_letters(inner).eq(name_letters(title))
}

/// Letters and digits of `text`, ASCII case folded, for loose name matching.
fn name_letters(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
}

/// Whether a folder is named for the book's series: the two names have the
/// same letters and digits, ignoring case and punctuation ("Re-Zero" for
/// "Re:Zero"). A folder named for only the start of a series ("Star" for
/// "Star Wars") does not count.
fn folder_names_series(folder: &str, series: &str) -> bool {
    !series.is_empty() && name_letters(folder).eq(name_letters(series))
}

/// Return the title without the series name it starts with, when the book's
/// series metadata names the folder it sits in: "Ascendance of a Bookworm:
/// Part 3" reads "Part 3" under that folder's heading. A folder name alone
/// is not evidence of a series, so a book without matching series metadata
/// keeps its whole title, as does one where nothing would remain.
fn title_without_series_prefix<'a>(
    title: &'a str,
    book: &crate::UiLibraryRow<'_>,
    folder: &str,
) -> &'a str {
    if !folder_names_series(folder, book.series) {
        return title;
    }
    // The two names agree, so either spelling may be the one the title uses.
    let mut best = title;
    for name in [book.series, folder] {
        let Some(head) = title.get(..name.len()) else {
            continue;
        };
        if !head.eq_ignore_ascii_case(name)
            || title[name.len()..].starts_with(char::is_alphanumeric)
        {
            continue;
        }
        let rest = title[name.len()..].trim_start_matches(is_join_punctuation);
        if rest.contains(char::is_alphanumeric) && rest.len() < best.len() {
            best = rest;
        }
    }
    best
}

/// Keep this much space between a title and its volume so the two cannot
/// touch.
const LIBRARY_VOLUME_GAP: usize = 12;

/// Return the volume's own width, so a single digit takes little room from
/// the title. A cut "100…" for 1000.5 would read as another volume, so a
/// wide one stays whole.
fn library_volume_width(font: &BitmapFont, volume: &str) -> usize {
    usize::from(measure_text(font, volume))
}

/// Right-align the volume at the row's right edge and return its width.
fn draw_library_volume(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    layout: ShellLayout,
    volume: &str,
    y: i16,
) -> usize {
    let width = library_volume_width(font, volume);
    draw_text(
        fb,
        font,
        volume,
        layout.content_right - width as i16,
        y,
        false,
    );
    width
}

/// Draw one book row of the list. With a volume, the title starts after the
/// arrow without the repeated volume marker or series name, is cut at its
/// end, and the volume sits at the right edge. With none, the title takes
/// the whole row and a middle cut keeps the ending, where a "Book 3" may sit.
fn draw_library_book(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    layout: ShellLayout,
    book: &crate::UiLibraryRow<'_>,
    folder: &str,
    y: i16,
) {
    let width = layout.content_width().max(0) as usize;
    let volume = display_volume(book.volume);
    let mut joined = [0; JOINED_TITLE_BYTES];
    if volume.is_empty() {
        // A title joined across a proven overlap is cut as one string, so
        // its tail can reach back past the stored ending.
        let (title, gap) = joined_title(book, &mut joined);
        let (beginning, continuation) = if gap {
            let (continuation, _) =
                title_suffix_continuation(book.name, book.title_suffix, book.title_len);
            (book.name, continuation)
        } else {
            (title, "")
        };
        let (head, tail, omitted) = middle_parts(
            font,
            title_without_series_prefix(beginning, book, folder),
            continuation,
            gap,
            width.min(usize::from(u16::MAX)) as u16,
        );
        let mut at = draw_text(fb, font, head, layout.content_x, y, false);
        if omitted {
            at = draw_text(fb, font, OMISSION, at, y, false);
        }
        draw_text(fb, font, tail, at, y, false);
        return;
    }
    let column = draw_library_volume(fb, font, layout, volume, y) + LIBRARY_VOLUME_GAP;
    let (title, _) = joined_title(book, &mut joined);
    let mut shown = [0; JOINED_TITLE_BYTES];
    let title = title_without_volume(title, volume, &mut shown);
    draw_end_truncated(
        fb,
        font,
        title_without_series_prefix(title, book, folder),
        layout.content_x,
        y,
        width.saturating_sub(column),
    );
}

/// Break a file name over at most two lines. Names rarely have spaces, so
/// a line may also end after `_` or `-`, or before a `.` so an extension
/// moves down whole. With no such break in the line's second half, the line
/// ends where the measure ends.
fn wrap_file_name<'a>(font: &BitmapFont, name: &'a str, max_w: u16) -> ([&'a str; 2], usize, bool) {
    if measure_text(font, name) <= max_w {
        return ([name, ""], 1, false);
    }
    let fitted = fit_text(font, name, max_w);
    let cut = match fitted.rfind([' ', '_', '-', '.']) {
        Some(at) if fitted[at..].starts_with('.') && at * 2 >= fitted.len() => at,
        Some(at) if (at + 1) * 2 >= fitted.len() => at + 1,
        _ => fitted.len(),
    };
    let (second, overflow) = end_truncated(font, name[cut..].trim_start(), max_w);
    ([name[..cut].trim_end(), second], 2, overflow)
}

/// The label column beside Book details values in landscape.
const DETAILS_LABEL_WIDTH: i16 = 96;

/// Draw the read-only Book details sheet: the catalog's prefix and ending
/// joined and wrapped as one title, then the series, volume and file name
/// when the book has them. An omission mark stands in for a middle the
/// catalog could not keep.
fn render_library_details(fb: &mut Framebuffer, shell: &UiShell<'_>, layout: ShellLayout) {
    // Back, Confirm and the sheet key all close it; one label says so.
    dash_key(fb, layout, 0, "close", false);
    heading(fb, layout, "Book details");

    let selected = (shell.selection as usize)
        .checked_sub(shell.library_window_start as usize)
        .and_then(|offset| shell.library_entries.get(offset))
        .filter(|entry| !entry.is_folder);
    let Some(book) = selected else {
        centered_note(fb, layout, "book details unavailable");
        finish_working_screen(fb, shell, layout);
        return;
    };
    let label_font = literata_small(FontStyle::Italic);
    let body_font = literata(FontStyle::Regular);
    let x = layout.content_x;
    let width = layout.content_width().max(0);
    // Portrait stacks each label over its value. Landscape is too short for
    // that with every field present, so labels sit beside their values.
    let (value_x, label_step, line_step, field_gap) = if layout.portrait {
        (x, 32, 38, 18)
    } else {
        (x + DETAILS_LABEL_WIDTH, 0, 36, 14)
    };
    let value_width = (width - (value_x - x)).max(0) as u16;
    let mut joined = [0; JOINED_TITLE_BYTES];
    let (title, gap) = joined_title(book, &mut joined);
    let mut y = 112;
    for (label, value, lines) in [
        ("TITLE", title, 3),
        ("SERIES", book.series, 1),
        ("VOLUME", display_volume(book.volume), 1),
        ("FILE", book.file_name, 0),
    ] {
        if value.is_empty() {
            continue;
        }
        draw_text(fb, label_font, label, x, y, false);
        y += label_step;
        let (rows, count, overflow) = if lines == 0 {
            let ([first, second], count, overflow) = wrap_file_name(body_font, value, value_width);
            ([first, second, ""], count, overflow)
        } else {
            wrap_title_lines(body_font, value, value_width, lines)
        };
        for (index, row) in rows[..count].iter().enumerate() {
            let at = draw_text(fb, body_font, row, value_x, y, false);
            if overflow && index + 1 == count {
                draw_text(fb, body_font, OMISSION, at, y, false);
            }
            y += line_step;
        }
        y += field_gap;
    }
    if gap {
        footer_note(fb, layout, "long title: beginning and end shown");
    }
    finish_working_screen(fb, shell, layout);
}

/// An italic footer-line note in the position footer's place.
fn footer_note(fb: &mut Framebuffer, layout: ShellLayout, text: &str) {
    draw_text_centered(
        fb,
        literata_small(FontStyle::Italic),
        text,
        layout.heading_cx,
        layout.footer_y(),
    );
}

/// Absolute catalog index of the first visible Library row that keeps
/// `selection` on screen. Shared by the renderer and the firmware's window
/// loader so both agree on which slice of the catalog is resident.
pub fn library_scroll_start(selection: usize, total: usize, portrait: bool) -> usize {
    let rows = library_visible_rows(portrait);
    let start = if selection >= rows {
        selection + 1 - rows
    } else {
        0
    };
    start.min(total.saturating_sub(rows))
}

// The contents page uses tight index rows — a real table of contents,
// not a menu: title, dot leaders, the chapter's book page right-aligned.
const TOC_ROW_STEP: i16 = 36;

pub const fn toc_visible_rows(portrait: bool) -> usize {
    if portrait {
        15
    } else {
        9
    }
}

/// Absolute TOC index of the first visible Contents row that keeps
/// `selection` on screen. Shared by the renderer and the firmware's TOC
/// window loader so both agree on which slice must be resident.
pub fn toc_scroll_start(selection: usize, total: usize, portrait: bool) -> usize {
    let rows = toc_visible_rows(portrait);
    let start = if selection >= rows {
        selection + 1 - rows
    } else {
        0
    };
    start.min(total.saturating_sub(rows))
}

fn render_chapters(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let layout = shell_layout(shell);
    dash_key(fb, layout, 0, "close", false);
    dash_key(fb, layout, 1, "open", true);
    dash_key(fb, layout, 2, "previous", false);
    dash_key(fb, layout, 3, "next", false);
    heading(fb, layout, "Contents");

    let total = (shell.chapters_total as usize).max(shell.chapters.len());
    if total == 0 || shell.chapters.is_empty() {
        centered_note(fb, layout, "no chapters found");
        finish_working_screen(fb, shell, layout);
        return;
    }

    // The full chapter list streams off the card (~a second); until it lands
    // the resident list can be shorter than the reading position. A cursor
    // past its end must not snap back to the last resident row -- that paints
    // a wrong chapter that "jumps" forward on the first key. Hold a note until
    // the real list arrives and the selection is in range.
    if shell.selection as usize >= total {
        centered_note(fb, layout, "loading contents\u{2026}");
        finish_working_screen(fb, shell, layout);
        return;
    }

    let window_start = shell.chapters_window_start as usize;

    let is_chapter_visible = |c: usize, current_parent: &mut Option<u16>| -> bool {
        if !shell.chapter_subsections.is_empty() {
            let is_sub = if c < shell.chapter_subsections.len() * 8 {
                (shell.chapter_subsections[c / 8] & (1 << (c % 8))) != 0
            } else {
                false
            };
            if !is_sub {
                *current_parent = Some(c as u16);
            }
            !is_sub
                || (shell.chapters_expanded.is_some() && shell.chapters_expanded == *current_parent)
        } else {
            c.checked_sub(window_start)
                .and_then(|offset| shell.chapters.get(offset))
                .map(|item| item.is_visible())
                .unwrap_or(true)
        }
    };

    // First pass: count visible items across the complete logical TOC
    // and locate the selection ordinal among visible items.
    let mut visible_total = 0usize;
    let mut selected_visible = 0usize;
    let mut current_parent = None;

    for c in 0..total {
        if is_chapter_visible(c, &mut current_parent) {
            if c <= shell.selection as usize {
                selected_visible = visible_total;
            }
            visible_total += 1;
        }
    }

    if visible_total == 0 {
        centered_note(fb, layout, "no chapters found");
        finish_working_screen(fb, shell, layout);
        return;
    }

    let selected = selected_visible.min(visible_total - 1);
    let start_visible = toc_scroll_start(selected, visible_total, layout.portrait);
    let max_rows = toc_visible_rows(layout.portrait);

    // Second pass: draw rows within the visible window.
    let mut current_visible = 0usize;
    let mut current_parent = None;
    let mut y = FIRST_ROW_Y;
    let mut rows_drawn = 0;

    for c in 0..total {
        if !is_chapter_visible(c, &mut current_parent) {
            continue;
        }
        if current_visible >= start_visible && rows_drawn < max_rows {
            if let Some(item) = c
                .checked_sub(window_start)
                .and_then(|offset| shell.chapters.get(offset))
            {
                if c == shell.selection as usize {
                    selection_arrow(fb, layout, y);
                }
                draw_toc_row(fb, layout, item, c, y);
                y += TOC_ROW_STEP;
                rows_drawn += 1;
            }
        }
        current_visible += 1;
        if rows_drawn >= max_rows {
            break;
        }
    }

    position_footer(fb, layout, selected + 1, visible_total);
    finish_working_screen(fb, shell, layout);
}

fn render_settings(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let layout = shell_layout(shell);
    dash_key(fb, layout, 0, "home", false);
    dash_key(fb, layout, 1, "change", true);
    dash_key(fb, layout, 2, "previous", false);
    dash_key(fb, layout, 3, "next", false);
    heading(fb, layout, "Settings");

    // Eight rows must clear the landscape footer line, so the settings
    // index runs tighter than the Library's ROW_STEP.
    const SETTINGS_ROW_STEP: i16 = 45;
    let rows: [(&str, &str); 8] = [
        (
            "Typeface",
            font_family_label(shell.font_family, shell.custom_font_name),
        ),
        ("Type size", font_size_label(shell.font_size)),
        ("Type weight", font_weight_label(shell.font_weight)),
        ("Line spacing", line_spacing_label(shell.line_spacing)),
        ("Alignment", alignment_label(shell.alignment)),
        ("Refresh", refresh_policy_label(shell.refresh_policy)),
        ("Orientation", orientation_label(shell.orientation)),
        ("Front buttons", front_buttons_label(shell.front_pages_left)),
    ];
    for (index, (name, value)) in rows.into_iter().enumerate() {
        index_row(
            fb,
            layout,
            name,
            value,
            FIRST_ROW_Y + index as i16 * SETTINGS_ROW_STEP,
            shell.selection == index as u16,
        );
    }

    finish_working_screen(fb, shell, layout);
}

fn render_wireless(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let layout = shell_layout(shell);
    match shell.sync_status {
        UiSyncStatus::ForgetPending => dash_key(fb, layout, 0, "cancel", false),
        _ => dash_key(fb, layout, 0, "home", false),
    }
    match shell.sync_status {
        UiSyncStatus::Idle => dash_key(fb, layout, 1, "connect", true),
        UiSyncStatus::NotConfigured => dash_key(fb, layout, 1, "set up", true),
        UiSyncStatus::ForgetPending => dash_key(fb, layout, 1, "forget", true),
        UiSyncStatus::Error(_) => dash_key(fb, layout, 1, "again", true),
        UiSyncStatus::CredentialsSaved | UiSyncStatus::Serving(_) => {
            dash_key(fb, layout, 1, "done", true)
        }
        _ => dash_unused(fb, layout, 1),
    }
    match shell.sync_status {
        UiSyncStatus::Idle => dash_key(fb, layout, 2, "forget", false),
        _ => dash_unused(fb, layout, 2),
    }
    dash_unused(fb, layout, 3);
    heading(fb, layout, "Wireless");

    let hint_y = if layout.portrait { 330 } else { 280 };
    match shell.sync_status {
        UiSyncStatus::NotConfigured => {
            centered_note(fb, layout, "no wi-fi network saved yet");
            draw_text_centered(
                fb,
                literata_small(FontStyle::Italic),
                "set up opens a hotspot your phone can configure",
                layout.heading_cx,
                hint_y,
            );
        }
        UiSyncStatus::Idle => {
            let mut buf = [0u8; 48];
            let mut cursor = 0;
            push_str(&mut buf, &mut cursor, "network \u{201C}");
            push_str(&mut buf, &mut cursor, shell.wifi_ssid);
            push_str(&mut buf, &mut cursor, "\u{201D}");
            centered_note(fb, layout, text_in(&buf, cursor));
            draw_text_centered(
                fb,
                literata_small(FontStyle::Italic),
                "connect to add and manage books from your browser",
                layout.heading_cx,
                hint_y,
            );
        }
        UiSyncStatus::ForgetPending => {
            let mut buf = [0u8; 48];
            let mut cursor = 0;
            push_str(&mut buf, &mut cursor, "forget \u{201C}");
            push_str(&mut buf, &mut cursor, shell.wifi_ssid);
            push_str(&mut buf, &mut cursor, "\u{201D}?");
            centered_note(fb, layout, text_in(&buf, cursor));
            draw_text_centered(
                fb,
                literata_small(FontStyle::Italic),
                "removes the saved wi-fi network \u{00b7} set up runs again next time",
                layout.heading_cx,
                hint_y,
            );
        }
        UiSyncStatus::Starting | UiSyncStatus::Connecting => {
            centered_note(fb, layout, "joining wi-fi \u{2026}");
        }
        UiSyncStatus::Connected(ip) => {
            let mut buf = [0u8; 40];
            let mut cursor = 0;
            push_str(&mut buf, &mut cursor, "connected at ");
            push_ipv4(&mut buf, &mut cursor, ip);
            centered_note(fb, layout, text_in(&buf, cursor));
        }
        UiSyncStatus::PortalUp(psk, ssid) => {
            // The QR joins the WPA2 hotspot, whose PSK this session
            // minted; the captive DNS then raises the phone's sign-in
            // sheet with the credential form. The QR is encoded here at
            // render time — the PSK exists nowhere at build time — and
            // the password is printed under it for phones that cannot
            // scan.
            let psk_text = psk.as_str();
            let mut temp = [0u8; join_qr::BUFFER_LEN];
            let mut out = [0u8; join_qr::BUFFER_LEN];
            // 140 + 33 modules * 5 px + the 20 px quiet zone ends at
            // y 325; the first caption baseline at 352 keeps its
            // ascenders out of the cleared band.
            let mut ssid_buf = [0u8; app_core::PortalSsid::LEN];
            let ssid_text = ssid.write_into(&mut ssid_buf);
            if let Some(qr) = join_qr::encode(ssid_text, psk_text, &mut temp, &mut out) {
                draw_qr(fb, &qr, layout.heading_cx, 140, 5);
            }
            let mut buf = [0u8; 48];
            let mut cursor = 0;
            push_str(&mut buf, &mut cursor, "scan to join \u{201c}");
            push_str(&mut buf, &mut cursor, ssid_text);
            push_str(&mut buf, &mut cursor, "\u{201d}");
            draw_text_centered(
                fb,
                literata_small(FontStyle::Regular),
                text_in(&buf, cursor),
                layout.heading_cx,
                352,
            );
            let mut buf = [0u8; 32];
            let mut cursor = 0;
            push_str(&mut buf, &mut cursor, "password ");
            push_str(&mut buf, &mut cursor, psk_text);
            draw_text_centered(
                fb,
                literata_small(FontStyle::Regular),
                text_in(&buf, cursor),
                layout.heading_cx,
                384,
            );
            draw_text_centered(
                fb,
                literata_small(FontStyle::Italic),
                "then enter your wi-fi in the page that opens \u{00b7} http://192.168.4.1",
                layout.heading_cx,
                416,
            );
        }
        UiSyncStatus::Serving(ip) => {
            let mut buf = [0u8; 56];
            let mut cursor = 0;
            push_str(&mut buf, &mut cursor, "visit ");
            push_ipv4(&mut buf, &mut cursor, ip);
            push_str(&mut buf, &mut cursor, " to add and remove books");
            centered_note(fb, layout, text_in(&buf, cursor));
        }
        UiSyncStatus::CredentialsSaved => {
            centered_note(fb, layout, "wi-fi saved");
            draw_text_centered(
                fb,
                literata_small(FontStyle::Italic),
                "press done to restart, then connect from this screen",
                layout.heading_cx,
                hint_y,
            );
        }
        UiSyncStatus::Error(reason) => {
            let mut buf = [0u8; 64];
            let mut cursor = 0;
            push_str(&mut buf, &mut cursor, "could not connect \u{00B7} ");
            push_str(&mut buf, &mut cursor, reason);
            centered_note(fb, layout, text_in(&buf, cursor));
        }
    }

    finish_working_screen(fb, shell, layout);
}

/// Blits a QR symbol centered on `cx`, `scale` pixels per module, with
/// the spec's four-module quiet zone cleared around it — regular QR
/// needs the full four on every side (two is a Micro QR allowance), and
/// marginal phone cameras punish anything less. Keep neighbors out of
/// the cleared band.
fn draw_qr(
    fb: &mut Framebuffer,
    qr: &qrcodegen_no_heap::QrCode<'_>,
    cx: i16,
    top: i16,
    scale: i16,
) {
    let size = qr.size() as usize;
    let edge = size as i16 * scale;
    let left = (cx - edge / 2).max(0) as u16;
    let top = top.max(0) as u16;
    let quiet = (scale * 4) as u16;
    fill_rect(
        fb,
        Rect {
            x: left.saturating_sub(quiet),
            y: top.saturating_sub(quiet),
            w: edge as u16 + quiet * 2,
            h: edge as u16 + quiet * 2,
        },
        true,
    );
    let scale = scale as u16;
    for row in 0..size {
        for col in 0..size {
            if qr.get_module(col as i32, row as i32) {
                fill_rect(
                    fb,
                    Rect {
                        x: left + col as u16 * scale,
                        y: top + row as u16 * scale,
                        w: scale,
                        h: scale,
                    },
                    false,
                );
            }
        }
    }
}

fn push_ipv4(buf: &mut [u8], cursor: &mut usize, ip: [u8; 4]) {
    for (index, octet) in ip.iter().enumerate() {
        if index > 0 {
            push_str(buf, cursor, ".");
        }
        push_usize(buf, cursor, *octet as usize);
    }
}

fn text_in(buf: &[u8], len: usize) -> &str {
    core::str::from_utf8(&buf[..len]).unwrap_or("?")
}

// ------------------------------------------------------------------
// Imprint furniture
// ------------------------------------------------------------------

/// An em-dash faces the physical button; the label is letterspaced
/// small caps, bold for the screen's one primary action.
fn dash_key(fb: &mut Framebuffer, layout: ShellLayout, slot: usize, label: &str, primary: bool) {
    if layout.portrait {
        // Portrait keys are named by a 1bpp icon centered over the button,
        // not by a letterspaced label; `primary` carries no weight here.
        let glyph = crate::icons::icon_for_label(label);
        crate::icons::draw_icon(
            fb,
            glyph,
            layout.key_pos(slot) - crate::icons::ICON_SIZE / 2,
            layout.frame_height - 24 - crate::icons::ICON_SIZE / 2,
        );
        return;
    }
    let style = if primary {
        FontStyle::Bold
    } else {
        FontStyle::Regular
    };
    let label_font = literata_small(style);
    let y = layout.key_pos(slot);
    let dash_font = literata(FontStyle::Regular);
    let dash = "\u{2014}";
    let dash_x = if layout.mirrored {
        WIDTH as i16 - KEY_DASH_X - measure_text(dash_font, dash) as i16
    } else {
        KEY_DASH_X
    };
    draw_text(fb, dash_font, dash, dash_x, y + 8, false);
    if layout.mirrored {
        let width = ls_width(label_font, label, 2);
        ls_caps(fb, label_font, label, dash_x - 24 - width, y + 6, 2);
    } else {
        ls_caps(fb, label_font, label, KEY_LABEL_X, y + 6, 2);
    }
}

/// An unused key keeps its bare dash: the mark stays, the word goes.
/// Portrait names keys with icons only, so an unused key shows nothing.
fn dash_unused(fb: &mut Framebuffer, layout: ShellLayout, slot: usize) {
    if layout.portrait {
        return;
    }
    let dash_font = literata(FontStyle::Regular);
    let dash = "\u{2014}";
    let dash_x = if layout.mirrored {
        WIDTH as i16 - KEY_DASH_X - measure_text(dash_font, dash) as i16
    } else {
        KEY_DASH_X
    };
    draw_text(fb, dash_font, dash, dash_x, layout.key_pos(slot) + 8, false);
}

/// The summoned reading key sheet: portrait reading stays chrome-free, and
/// a named-key press raises this white band over the page's bottom edge —
/// footer and all, the page reserves nothing for it. A hairline closes its
/// top edge; inside, four 1bpp icons sit on one baseline, each centered
/// over the physical button it names (following the front-pair swap), so
/// the band stays shallow. Paging dismisses it (the reducer owns that).
pub fn render_reading_sheet(fb: &mut Framebuffer, orientation: UiOrientation, pages_left: bool) {
    let mut layout = ShellLayout::for_orientation(orientation);
    layout.pages_left = pages_left;
    // The reducer only holds the sheet up in portrait; landscape callers
    // never reach here.
    debug_assert!(layout.portrait);
    let top = layout.frame_height - crate::reading::PORTRAIT_READING_SHEET_HEIGHT;
    let width = fb.frame_width() as u16;
    fill_rect(
        fb,
        Rect::new(
            0,
            top as u16,
            width,
            crate::reading::PORTRAIT_READING_SHEET_HEIGHT as u16,
        ),
        true,
    );
    hline(fb, 0, top, width as i16);
    dash_key(fb, layout, 0, "home", false);
    dash_key(fb, layout, 1, "contents", true);
    dash_key(fb, layout, 2, "previous", false);
    dash_key(fb, layout, 3, "next", false);
}

/// Width of the rule under a heading, and so the width a heading has.
const HEADING_RULE_W: i16 = 320;

fn heading(fb: &mut Framebuffer, layout: ShellLayout, text: &str) {
    let small = literata_small(FontStyle::Regular);
    let width = ls_width(small, text, 5);
    ls_caps(fb, small, text, layout.heading_cx - width / 2, 42, 5);
    hline(
        fb,
        layout.heading_cx - HEADING_RULE_W / 2,
        56,
        HEADING_RULE_W,
    );
}

/// The longest prefix of `text` that fits under the rule.
///
/// Headings are a fixed word until a folder name becomes one, and a name is
/// whatever a reader called a directory. Measured with the letterspacing the
/// draw uses, or the fit would be computed for a narrower string than the one
/// that lands.
fn fitted_heading(text: &str) -> &str {
    let small = literata_small(FontStyle::Regular);
    let mut end = text.len();
    while end > 0 {
        if text.is_char_boundary(end) && ls_width(small, &text[..end], 5) <= HEADING_RULE_W {
            return &text[..end];
        }
        end -= 1;
    }
    ""
}

/// Letterspaced all-caps, the small-caps stand-in for this bitmap set.
pub(crate) fn ls_caps(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    text: &str,
    x: i16,
    baseline: i16,
    extra: i16,
) {
    let mut cursor = x;
    for ch in text.chars() {
        let upper = ch.to_ascii_uppercase();
        let mut buf = [0u8; 4];
        let glyph = upper.encode_utf8(&mut buf);
        cursor = draw_text(fb, font, glyph, cursor, baseline, false) + extra;
    }
}

pub(crate) fn ls_width(font: &BitmapFont, text: &str, extra: i16) -> i16 {
    let mut width = 0i16;
    let mut count = 0i16;
    for ch in text.chars() {
        let upper = ch.to_ascii_uppercase();
        let mut buf = [0u8; 4];
        let glyph = upper.encode_utf8(&mut buf);
        width += measure_text(font, glyph) as i16;
        count += 1;
    }
    width + (count - 1).max(0) * extra
}

/// Name, dot leaders, italic value right-aligned: the index pattern
/// shared by every list screen.
fn index_row(
    fb: &mut Framebuffer,
    layout: ShellLayout,
    name: &str,
    value: &str,
    y: i16,
    selected: bool,
) {
    if selected {
        selection_arrow(fb, layout, y);
    }
    let body = literata(FontStyle::Regular);
    let end_x = draw_text(fb, body, name, layout.content_x, y, false);
    let value_font = literata(FontStyle::Italic);
    let value_w = measure_text(value_font, value) as i16;
    let mut dx = end_x + 16;
    while dx < layout.content_right - value_w - 14 {
        fill_rect(fb, Rect::new(dx as u16, (y - 2) as u16, 1, 1), false);
        dx += 8;
    }
    draw_text(
        fb,
        value_font,
        value,
        layout.content_right - value_w,
        y,
        false,
    );
}

fn draw_toc_row(
    fb: &mut Framebuffer,
    layout: ShellLayout,
    item: &UiTocItem<'_>,
    index: usize,
    y: i16,
) {
    let body = literata(FontStyle::Regular);
    let indent = layout.content_x + (item.level.saturating_sub(1) as i16 * 18);
    let mut title_x = indent;
    if item.has_subsections {
        let chevron = if item.expanded {
            "\u{25bc} "
        } else {
            "\u{25b6} "
        };
        title_x = draw_text(fb, body, chevron, indent, y, false);
    }
    let mut numbered = [0u8; 32];
    let title = if item.title.is_empty() {
        fmt_numbered_chapter(index + 1, &mut numbered)
    } else {
        item.title
    };

    if item.page == 0 {
        draw_text_truncated(
            fb,
            body,
            title,
            title_x,
            y,
            (layout.content_right - title_x).max(0) as usize,
            false,
        );
        return;
    }

    let mut page_buf = [0u8; 16];
    let mut cursor = 0;
    push_usize(&mut page_buf, &mut cursor, item.page as usize);
    let page = core::str::from_utf8(&page_buf[..cursor]).unwrap_or("");
    let page_w = measure_text(body, page) as i16;

    let title_max = (layout.content_right - title_x - page_w - 40).max(40) as usize;
    let shown = fit_text(body, title, title_max as u16);
    let end_x = draw_text(fb, body, shown, title_x, y, false);
    let mut dx = end_x + 16;
    while dx < layout.content_right - page_w - 14 {
        fill_rect(fb, Rect::new(dx as u16, (y - 2) as u16, 1, 1), false);
        dx += 8;
    }
    draw_text(fb, body, page, layout.content_right - page_w, y, false);
}

fn selection_arrow(fb: &mut Framebuffer, layout: ShellLayout, y: i16) {
    let arrow = if layout.mirrored {
        "\u{2190}"
    } else {
        "\u{2192}"
    };
    draw_text(
        fb,
        literata(FontStyle::Regular),
        arrow,
        layout.selection_x(),
        y,
        false,
    );
}

fn centered_note(fb: &mut Framebuffer, layout: ShellLayout, text: &str) {
    // The taller portrait page carries its note a little further down to
    // keep it in the same optical position under the heading rule.
    let y = if layout.portrait { 280 } else { 230 };
    draw_text_centered(fb, literata(FontStyle::Italic), text, layout.heading_cx, y);
}

/// "– n of m –" centered on the content column.
fn position_footer(fb: &mut Framebuffer, layout: ShellLayout, current: usize, total: usize) {
    let mut buf = [0u8; 32];
    let mut cursor = 0;
    push_str(&mut buf, &mut cursor, "\u{2013} ");
    push_usize(&mut buf, &mut cursor, current);
    push_str(&mut buf, &mut cursor, " of ");
    push_usize(&mut buf, &mut cursor, total);
    push_str(&mut buf, &mut cursor, " \u{2013}");
    let label = core::str::from_utf8(&buf[..cursor]).unwrap_or("");
    draw_text_centered(
        fb,
        literata_small(FontStyle::Regular),
        label,
        layout.heading_cx,
        layout.footer_y(),
    );
}

fn draw_battery_percent(fb: &mut Framebuffer, layout: ShellLayout, percent: u8) {
    let mut buf = [0u8; 8];
    let mut cursor = 0;
    push_usize(&mut buf, &mut cursor, percent.min(100) as usize);
    push_str(&mut buf, &mut cursor, "%");
    let label = core::str::from_utf8(&buf[..cursor]).unwrap_or("");
    let small = literata_small(FontStyle::Regular);
    if layout.portrait {
        // The battery percentage sits tucked in the top-right corner,
        // above the heading area to avoid overlapping.
        let width = measure_text(small, label) as i16;
        let frame_width = fb.frame_width() as i16;
        draw_text(fb, small, label, frame_width - 16 - width, 24, false);
    } else if layout.mirrored {
        draw_text(
            fb,
            small,
            label,
            WIDTH as i16 - FOOTER_RIGHT,
            FOOTER_Y,
            false,
        );
    } else {
        let width = measure_text(small, label) as i16;
        draw_text(fb, small, label, FOOTER_RIGHT - width, FOOTER_Y, false);
    }
}

fn finish_working_screen(fb: &mut Framebuffer, shell: &UiShell<'_>, layout: ShellLayout) {
    draw_battery_percent(fb, layout, shell.battery_percent);
}

fn hline(fb: &mut Framebuffer, x: i16, y: i16, w: i16) {
    fill_rect(fb, Rect::new(x as u16, y as u16, w as u16, 1), false);
}

fn draw_text_centered(fb: &mut Framebuffer, font: &BitmapFont, text: &str, cx: i16, y: i16) {
    let x = cx - measure_text(font, text) as i16 / 2;
    draw_text(fb, font, text, x, y, false);
}

fn fmt_numbered_chapter(number: usize, buf: &mut [u8; 32]) -> &str {
    let mut cursor = 0;
    push_str(buf, &mut cursor, "Chapter ");
    push_usize(buf, &mut cursor, number);
    core::str::from_utf8(&buf[..cursor]).unwrap_or("Chapter")
}

/// Draws `text` clipped to `max_w`, and reports the width it actually took,
/// so a caller can put something immediately after it.
fn draw_text_truncated(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    text: &str,
    x: i16,
    y: i16,
    max_w: usize,
    white: bool,
) -> u16 {
    let text = fit_text(font, text, max_w.min(u16::MAX as usize) as u16);
    draw_text(fb, font, text, x, y, white);
    measure_text(font, text)
}

/// Greedy two-line word wrap for the display-face title. Returns the
/// title's two lines; the second is empty when one line fits. The
/// second line is glyph-truncated if the remainder still overflows,
/// and a single unbreakable overlong word truncates on line one. The
/// screens draw through [`wrap_title_lines`], whose overflow flag says when
/// to add `...`; this two-line view is kept for the tests.
#[cfg(test)]
pub(crate) fn wrap_title<'a>(font: &BitmapFont, text: &'a str, max_w: u16) -> (&'a str, &'a str) {
    let (lines, _, _) = wrap_title_lines(font, text, max_w, 2);
    (lines[0], lines[1])
}

/// Break a title into up to `max_lines` lines at word boundaries, each
/// within the measure. The final line is cut to the measure when the title
/// still overflows, with room reserved for the caller's `...`; the returned
/// flag says whether that happened.
pub(crate) fn wrap_title_lines<'a>(
    font: &BitmapFont,
    text: &'a str,
    max_w: u16,
    max_lines: usize,
) -> ([&'a str; 3], usize, bool) {
    let max_lines = max_lines.clamp(1, 3);
    let mut lines = [""; 3];
    let mut count = 0;
    let mut rest = text;
    while count < max_lines - 1 && measure_text(font, rest) > max_w {
        let (line, tail) = split_title_line(font, rest, max_w);
        if tail.is_empty() {
            // An unbreakable over-wide word: the cut line is final, and the
            // caller's `...` goes on it, so it is cut to leave that room.
            let dots = measure_text(font, "...");
            lines[count] = fit_text(font, line, max_w.saturating_sub(dots));
            return (lines, count + 1, true);
        }
        lines[count] = line;
        count += 1;
        rest = tail;
    }
    if measure_text(font, rest) <= max_w {
        lines[count] = rest;
        return (lines, count + 1, false);
    }
    let dots = measure_text(font, "...");
    lines[count] = fit_text(font, rest, max_w.saturating_sub(dots));
    (lines, count + 1, true)
}

/// One title line at the widest word boundary within the measure, plus the
/// untrimmed remainder. An over-wide first word cannot break: it comes back
/// cut to the measure with an empty remainder.
fn split_title_line<'a>(font: &BitmapFont, text: &'a str, max_w: u16) -> (&'a str, &'a str) {
    let mut split = 0usize;
    for (index, _) in text.match_indices(' ') {
        if measure_text(font, &text[..index]) <= max_w {
            split = index;
        } else {
            break;
        }
    }
    if split == 0 {
        return (fit_text(font, text, max_w), "");
    }
    (&text[..split], text[split + 1..].trim_start())
}

pub(crate) fn fit_text<'a>(font: &BitmapFont, text: &'a str, max_w: u16) -> &'a str {
    if measure_text(font, text) <= max_w {
        return text;
    }
    let mut end = 0usize;
    for (index, _) in text.char_indices() {
        let candidate = &text[..index];
        if !candidate.is_empty() && measure_text(font, candidate) > max_w {
            break;
        }
        end = index;
    }
    text[..end].trim_end()
}

fn push_str(buf: &mut [u8], cursor: &mut usize, value: &str) {
    for byte in value.bytes() {
        if *cursor >= buf.len() {
            return;
        }
        buf[*cursor] = byte;
        *cursor += 1;
    }
}

fn push_usize(buf: &mut [u8], cursor: &mut usize, value: usize) {
    let mut digits = [0u8; 20];
    let mut len = 0;
    let mut value = value;
    if value == 0 {
        digits[0] = b'0';
        len = 1;
    }
    while value > 0 && len < digits.len() {
        digits[len] = b'0' + (value % 10) as u8;
        value /= 10;
        len += 1;
    }
    for index in (0..len).rev() {
        if *cursor >= buf.len() {
            return;
        }
        buf[*cursor] = digits[index];
        *cursor += 1;
    }
}

fn refresh_policy_label(policy: UiRefreshPolicy) -> &'static str {
    match policy {
        UiRefreshPolicy::FastOnly => "fast only",
        UiRefreshPolicy::FullOnWake => "full on wake",
        UiRefreshPolicy::FullEveryTen => "full every ten",
    }
}

fn orientation_label(orientation: UiOrientation) -> &'static str {
    match orientation {
        UiOrientation::LandscapeButtonsBottom => "buttons down",
        UiOrientation::LandscapeButtonsTop => "buttons up",
        UiOrientation::PortraitButtonsLeft | UiOrientation::PortraitButtonsRight => "portrait",
    }
}

fn front_buttons_label(pages_left: bool) -> &'static str {
    if pages_left {
        "pages left"
    } else {
        "pages right"
    }
}

fn font_size_label(size: display::font::FontSize) -> &'static str {
    match size {
        display::font::FontSize::Small => "small",
        display::font::FontSize::Medium => "medium",
        display::font::FontSize::Large => "large",
    }
}

fn line_spacing_label(spacing: display::font::LineSpacing) -> &'static str {
    match spacing {
        display::font::LineSpacing::Compact => "compact",
        display::font::LineSpacing::Normal => "normal",
        display::font::LineSpacing::Relaxed => "relaxed",
    }
}

fn font_weight_label(weight: display::font::FontWeight) -> &'static str {
    match weight {
        display::font::FontWeight::Normal => "regular",
        display::font::FontWeight::Heavy => "heavier",
    }
}

fn alignment_label(alignment: display::font::Alignment) -> &'static str {
    match alignment {
        display::font::Alignment::Justified => "justified",
        display::font::Alignment::Left => "left",
    }
}

fn font_family_label(family: display::font::FontFamily, custom_name: &str) -> &str {
    match family {
        display::font::FontFamily::Literata => "literata",
        display::font::FontFamily::Merriweather => "merriweather",
        display::font::FontFamily::Custom => {
            if custom_name.is_empty() {
                "custom"
            } else {
                custom_name
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUSY: app_core::LibraryMenu = app_core::LibraryMenu::Busy {
        action: app_core::LibraryAction::ClearCache,
        index: 0,
        request_id: 1,
    };

    /// The Home and sleep rule takes a book's own measure only once there is
    /// one. A card book restored with no saved count has none, and the
    /// caller's fallback stands; one measured at a single page is read to the
    /// end, which a count of 1 standing for "unknown" used to hide.
    ///
    /// Provisional pagination (e.g. first spine item completed while background
    /// build continues) provides navigation bounds, but leaves whole-book
    /// progress unknown until indexing finishes.
    #[test]
    fn the_home_rule_tells_an_unmeasured_book_from_a_one_page_one() {
        const FALLBACK: u16 = 777;
        let ctx = app_core::ReducerContext::new(1, 3);
        let book_id = app_core::ReaderSource::sd(0).book_id();
        let rule = |state: app_core::ReaderState| {
            let request = state.render_request(app_core::RenderKind::Page);
            book_progress_permille(request.page, request.page_count, FALLBACK)
        };

        let restored = app_core::ReaderState::boot().apply_library_event(
            ctx,
            app_core::LibraryEvent::Restored {
                book_id,
                chapter: 3,
                page: 0,
                page_count: 0,
                reading_orientation: 0,
                refresh_policy: 0,
                font_size: 0,
                line_spacing: 0,
                font_weight: 0,
                font_family: 0,
                front_buttons: 0,
                alignment: 0,
            },
        );
        assert_eq!(rule(restored), FALLBACK, "not measured yet");

        let provisional = restored.apply_library_event(
            ctx,
            app_core::LibraryEvent::Loaded {
                book_id,
                pages: 1,
                complete: false,
                chapters: 1,
                current_chapter: 0,
                chapter_pages: [0; app_core::MAX_SD_CHAPTERS],
                chapter_subsections: [0; app_core::MAX_SD_CHAPTER_SUBSECTIONS / 8],
                position: None,
                text_replaced: true,
            },
        );
        assert_eq!(
            rule(provisional),
            FALLBACK,
            "provisional index while pagination is unfinished leaves progress unknown"
        );

        let finished = provisional.apply_library_event(
            ctx,
            app_core::LibraryEvent::Loaded {
                book_id,
                pages: 40,
                complete: true,
                chapters: 2,
                current_chapter: 0,
                chapter_pages: [0; app_core::MAX_SD_CHAPTERS],
                chapter_subsections: [0; app_core::MAX_SD_CHAPTER_SUBSECTIONS / 8],
                position: None,
                text_replaced: false,
            },
        );
        assert_eq!(
            rule(finished),
            25,
            "once pagination completes, whole-book progress is calculated (page 1 of 40 = 25‰)"
        );

        let one_page_finished = restored.apply_library_event(
            ctx,
            app_core::LibraryEvent::Loaded {
                book_id,
                pages: 1,
                complete: true,
                chapters: 1,
                current_chapter: 0,
                chapter_pages: [0; app_core::MAX_SD_CHAPTERS],
                chapter_subsections: [0; app_core::MAX_SD_CHAPTER_SUBSECTIONS / 8],
                position: None,
                text_replaced: true,
            },
        );
        assert_eq!(
            rule(one_page_finished),
            1000,
            "a genuinely measured one-page book that is complete reads 1000‰"
        );

        let book_b = app_core::ReaderSource::sd(1).book_id();
        let restored_b = finished.apply_library_event(
            ctx,
            app_core::LibraryEvent::Restored {
                book_id: book_b,
                chapter: 1,
                page: 10,
                page_count: 0,
                reading_orientation: 0,
                refresh_policy: 0,
                font_size: 0,
                line_spacing: 0,
                font_weight: 0,
                font_family: 0,
                front_buttons: 0,
                alignment: 0,
            },
        );
        assert_eq!(
            rule(restored_b),
            FALLBACK,
            "restoring book B with an unknown count leaves progress unknown"
        );

        assert_eq!(book_progress_permille(0, 2, FALLBACK), 500);
    }

    /// Every label is a promise. Both kinds of wait swallow every press but
    /// Back, and Back then leaves Library rather than going up a level, so
    /// the rail offers one key and calls it home.
    #[test]
    fn a_wait_leaves_one_live_key_whatever_the_depth() {
        for in_folder in [true, false] {
            assert_eq!(
                library_rail(in_folder, BUSY, false),
                LibraryRail::Held,
                "a picked action, in_folder={in_folder}"
            );
            assert_eq!(
                library_rail(in_folder, app_core::LibraryMenu::None, true),
                LibraryRail::Held,
                "a move through the tree, in_folder={in_folder}"
            );
        }
    }

    /// The teaching line names the sheet key, so it may only show when that
    /// press opens the sheet. A move through the tree swallows it, and that
    /// wait has no note of its own, so the plain position stands in exactly
    /// as it does while the sheet is up.
    #[test]
    fn a_pending_move_drops_the_line_that_teaches_a_swallowed_key() {
        assert_eq!(
            library_footer_line(app_core::LibraryMenu::None, true, false),
            LibraryFooterLine::Position
        );
        assert_eq!(
            library_footer_line(app_core::LibraryMenu::None, false, false),
            LibraryFooterLine::Hint,
            "at rest the key it names works"
        );
        assert_eq!(
            library_footer_line(app_core::LibraryMenu::Sheet { row: 0 }, false, false),
            LibraryFooterLine::Position
        );
        assert_eq!(
            library_footer_line(BUSY, false, false),
            LibraryFooterLine::Note,
            "a picked action says what it is doing"
        );
    }

    /// A pick waiting on a rescan says so where the position sits, since the
    /// wait runs to seconds and a bare footer reads as a hang.
    #[test]
    fn a_pick_waiting_on_a_rescan_says_so() {
        assert_eq!(
            library_footer_line(app_core::LibraryMenu::None, true, true),
            LibraryFooterLine::Rescanning
        );
        assert_eq!(
            library_rail(false, app_core::LibraryMenu::None, true),
            LibraryRail::Held,
            "and the rail still offers only home"
        );
    }

    /// A move outstanding swallows the sheet's keys too, so a rail drawn
    /// from the sheet alone would name three presses that go nowhere.
    #[test]
    fn a_move_outranks_the_sheet() {
        let sheet = app_core::LibraryMenu::Sheet { row: 0 };
        assert_eq!(library_rail(true, sheet, false), LibraryRail::Sheet);
        assert_eq!(library_rail(true, sheet, true), LibraryRail::Held);
    }

    /// With nothing outstanding the whole rail acts, and Back is a level
    /// inside a folder and the way out at the root.
    #[test]
    fn an_idle_library_offers_the_rail_it_can_keep() {
        assert_eq!(
            library_rail(true, app_core::LibraryMenu::None, false),
            LibraryRail::Live { back: "up" }
        );
        assert_eq!(
            library_rail(false, app_core::LibraryMenu::None, false),
            LibraryRail::Live { back: "home" }
        );
    }

    #[test]
    fn middle_truncation_keeps_start_and_numeric_tail_on_utf8_boundaries() {
        let font = literata(FontStyle::Regular);
        let start = "異世界の物語 and an extraordinarily long title";
        let suffix = " Volume 12.5";
        let (head, tail, omitted) = middle_title_parts(font, start, suffix, 0, 190);
        assert!(omitted);
        assert!(start.starts_with(head));
        assert!(suffix.ends_with(tail));
        assert!(tail.ends_with("12.5"), "{tail:?}");
        assert!(
            measure_text(font, head) + measure_text(font, OMISSION) + measure_text(font, tail)
                <= 190
        );

        let (head, tail, omitted) = middle_title_parts(font, "Dune", "", 0, 190);
        assert_eq!((head, tail, omitted), ("Dune", "", false));
        let (head, tail, omitted) = middle_title_parts(font, start, "", 0, 190);
        assert!(omitted);
        assert!(start.starts_with(head));
        assert!(start.ends_with(tail));
        assert!(head.len() + tail.len() <= start.len());
    }

    #[test]
    fn overlapping_catalog_title_parts_render_as_one_continuous_title() {
        // A v13 title between 65 and 87 bytes retains some of the same
        // characters in its 64-byte prefix and 24-byte suffix.
        let title = "An Adventurer's Guide to the Distant Lands and Other Remarkable Vol2";
        assert!(title.len() > 64 && title.len() < 88);
        let beginning = &title[..64];
        let saved_end = &title[title.len() - 24..];
        let (continuation, missing_middle) =
            title_suffix_continuation(beginning, saved_end, title.len() as u8);
        assert!(!missing_middle);
        assert_eq!(format!("{beginning}{continuation}"), title);
        assert_eq!(continuation, "Vol2");

        let font = literata(FontStyle::Regular);
        let (head, tail, omitted) =
            middle_title_parts(font, beginning, saved_end, title.len() as u8, 2000);
        assert!(!omitted);
        assert_eq!(format!("{head}{tail}"), title);
        let (head, tail, omitted) =
            middle_title_parts(font, beginning, saved_end, title.len() as u8, 220);
        assert!(omitted);
        assert!(title.starts_with(head));
        assert!(title.ends_with(tail));
        assert!(head.len() + tail.len() <= title.len());

        let (tail, gap) = title_suffix_continuation("A", "Unrelated last chapter", 100);
        assert!(gap);
        assert_eq!(tail, "Unrelated last chapter");
        // Keep UTF-8 boundaries when a multibyte character straddles the
        // stored prefix/tail overlap.
        let utf8 = format!("{}物語巻末続編", "A".repeat(59));
        let prefix = &utf8[..62];
        let suffix = &utf8[utf8.len() - 24..];
        let (end, gap) = title_suffix_continuation(prefix, suffix, utf8.len() as u8);
        assert!(!gap);
        assert_eq!(format!("{prefix}{end}"), utf8);
    }

    #[test]
    fn repeated_title_fragments_do_not_hide_an_unstored_middle() {
        // The first and last 24 bytes match, but the title has 40 unseen
        // middle bytes. A textual overlap is not evidence of contiguity.
        let full = format!("{}{}{}", "A".repeat(64), "B".repeat(40), "A".repeat(24));
        let prefix = &full[..64];
        let suffix = &full[full.len() - 24..];
        let (tail, missing_middle) = title_suffix_continuation(prefix, suffix, full.len() as u8);
        assert!(missing_middle);
        assert_eq!(tail, suffix);

        // v13 rows without a saved title length must also be conservative.
        let (tail, unknown_gap) = title_suffix_continuation(prefix, suffix, 0);
        assert!(unknown_gap);
        assert_eq!(tail, suffix);

        let font = literata(FontStyle::Regular);
        let (head, tail, omitted) =
            middle_title_parts(font, prefix, suffix, full.len() as u8, 2000);
        assert!(omitted);
        assert_eq!(head, prefix);
        assert_eq!(tail, suffix);
    }

    #[test]
    fn volumes_take_only_their_own_width_and_stay_whole() {
        let font = literata(FontStyle::Regular);
        for number in ["1", "10", "12.5", "100", "1000.5", "1234567890123456"] {
            assert_eq!(
                library_volume_width(font, number),
                usize::from(measure_text(font, number)),
                "{number}"
            );
        }
        assert!(library_volume_width(font, "9") < library_volume_width(font, "10"));
    }

    /// A cut title leaves at most a twentieth of its row, plus one glyph, empty
    /// before the mark. The cut moves back to a word break only within that.
    #[test]
    fn cut_titles_fill_their_row() {
        let font = literata(FontStyle::Regular);
        for title in [
            "Sentenced to Be a Hero: The Prison Records of Penal Hero Unit 9004",
            "86\u{2014}EIGHTY-SIX: Gun Smoke on the Water",
            "Mushoku Tensei: Jobless Reincarnation",
            "Zilbagias the Demon Prince: How the Seventh Prince Brought Down the Kingdom",
        ] {
            for width in [180u16, 240, 300, 360, 420] {
                let (shown, cut) = end_truncated(font, title, width);
                if !cut {
                    continue;
                }
                let used = measure_text(font, shown) + measure_text(font, OMISSION);
                assert!(used <= width, "{title} at {width}");
                assert!(
                    width - used <= width / WORD_SLACK_DIVISOR + 24,
                    "{title} at {width}: {shown:?} leaves {} px",
                    width - used
                );
            }
        }
    }

    #[test]
    fn cut_titles_carry_an_omission_mark() {
        let font = literata(FontStyle::Regular);
        let title = "Mushoku Tensei: Jobless Reincarnation";
        assert_eq!(end_truncated(font, title, 2000), (title, false));
        let (shown, cut) = end_truncated(font, title, 200);
        assert!(cut);
        assert!(title.starts_with(shown) && shown.len() < title.len());
        assert!(measure_text(font, shown) + measure_text(font, OMISSION) <= 200);
    }

    #[test]
    fn an_ending_inside_the_prefix_adds_nothing_and_marks_no_gap() {
        // A 64-byte title followed by a newline in the OPF overflows the
        // prefix by the whitespace alone. A row may hold the title's last
        // 23 bytes as an ending; recorded length 64 shows the prefix is the
        // whole title.
        let title = "A".repeat(64);
        let ending = &title[41..];
        assert_eq!(title_suffix_continuation(&title, ending, 64), ("", false));
        let font = literata(FontStyle::Regular);
        let (head, tail, omitted) = middle_title_parts(font, &title, ending, 64, u16::MAX);
        assert_eq!((head, tail, omitted), (title.as_str(), "", false));
        let book = crate::UiLibraryRow {
            name: &title,
            is_folder: false,
            series: "",
            title_suffix: ending,
            title_len: 64,
            volume: "",
            file_name: "",
        };
        let mut buf = [0; JOINED_TITLE_BYTES];
        assert_eq!(joined_title(&book, &mut buf), (title.as_str(), false));
    }

    #[test]
    fn volumes_display_without_padding_or_a_zero_fraction() {
        for (stored, shown) in [
            ("1", "1"),
            ("01", "1"),
            ("1.0", "1"),
            ("1.50", "1.5"),
            ("2.05", "2.05"),
            ("100.0", "100"),
            ("10", "10"),
            ("0", "0"),
            ("00", "0"),
            ("0.5", "0.5"),
            ("00.5", "0.5"),
            ("0.0", "0"),
        ] {
            assert_eq!(display_volume(stored), shown, "{stored}");
        }
    }

    #[test]
    fn the_omission_mark_is_one_narrower_glyph() {
        let font = literata(FontStyle::Regular);
        assert!(font.glyph(0x2026).is_some());
        assert!(measure_text(font, OMISSION) < measure_text(font, "..."));
    }

    #[test]
    fn cuts_land_between_words_when_the_dropped_fragment_is_short() {
        let font = literata(FontStyle::Regular);
        // A cut inside a word keeps the fragment only when the fragment is
        // wider than the slack; otherwise the cut moves back to a word break.
        let check = |text: &str, head: &str, room: u16| {
            if text[head.len()..].starts_with(|c: char| !c.is_alphanumeric()) {
                return;
            }
            let fragment = head.rfind(' ').map_or(head, |at| &head[at..]);
            assert!(
                measure_text(font, fragment) > room / WORD_SLACK_DIVISOR,
                "{room}: {head:?} kept a short fragment"
            );
        };
        let title = "Mushoku Tensei: Jobless Reincarnation";
        for width in [150, 200, 250, 300] {
            let (shown, cut) = end_truncated(font, title, width);
            assert!(cut, "{width}");
            check(title, shown, width);
            assert!(!shown.ends_with(':'), "{width}: {shown:?}");
        }
        let long = "The Hitchhiker's Guide to the Galaxy: The Original Radio Scripts";
        for width in [260, 300, 340, 380, 420] {
            let (head, tail, omitted) = middle_title_parts(font, long, "", 0, width);
            assert!(omitted);
            check(
                long,
                head,
                width.saturating_sub(measure_text(font, OMISSION)),
            );
            assert!(!head.ends_with('\''), "{width}: {head:?}");
            let start = long.len() - tail.len();
            assert!(long[..start].ends_with(' '), "{width}: {tail:?}");
        }
        let adventurer = "An Adventurer's Guide to the Distant Lands and Other Remarkable Places";
        for width in [300, 360, 400] {
            let (_, tail, _) = middle_title_parts(font, adventurer, "", 0, width);
            let start = adventurer.len() - tail.len();
            assert!(adventurer[..start].ends_with(' '), "{width}: {tail:?}");
        }
        // Without spaces the cut stays where the measure put it.
        let unspaced = "異世界の物語とその続きの長い長い題名です";
        let (shown, cut) = end_truncated(font, unspaced, 120);
        assert!(cut && !shown.is_empty() && unspaced.starts_with(shown));
    }

    #[test]
    fn file_names_wrap_at_underscores_and_dots() {
        let font = literata(FontStyle::Regular);
        let name = "Spellblades_v03_collectors_edition.epub";
        let ([first, second], count, overflow) = wrap_file_name(font, name, 300);
        assert_eq!(count, 2);
        assert!(!overflow, "{first:?} {second:?}");
        assert_eq!(
            (first, second),
            ("Spellblades_v03_collectors_", "edition.epub")
        );
        // With the dot in reach the extension moves down whole.
        let ([first, second], _, _) = wrap_file_name(font, name, 400);
        assert_eq!(
            (first, second),
            ("Spellblades_v03_collectors_edition", ".epub")
        );
        // A name too long for two lines has its second line cut, with no
        // period left against the mark.
        let longer = "Spellblades_v03_collectors_edition_with_bonus_story.epub";
        let ([_, second], _, overflow) = wrap_file_name(font, longer, 300);
        assert!(overflow && !second.ends_with('.'), "{second:?}");
        assert_eq!(
            wrap_file_name(font, "Dune.epub", 300),
            (["Dune.epub", ""], 1, false)
        );
    }

    /// A no-volume title joined across a proven overlap is cut as one
    /// string: its tail reaches back past the 3-byte stored ending.
    #[test]
    fn a_joined_title_is_cut_in_the_middle_as_one_string() {
        let full = "The Complete Works of William Shakespeare in One Volume, with Notes";
        assert!(full.len() > 64 && full.len() < 88);
        let stored = crate::UiLibraryRow {
            name: &full[..64],
            is_folder: false,
            series: "",
            title_suffix: &full[full.len() - 24..],
            title_len: full.len() as u8,
            volume: "",
            file_name: "",
        };
        let whole = crate::UiLibraryRow {
            name: full,
            title_suffix: "",
            ..stored
        };
        for orientation in [
            UiOrientation::LandscapeButtonsBottom,
            UiOrientation::PortraitButtonsRight,
        ] {
            let draw = |row: crate::UiLibraryRow<'_>| {
                let rows = [row];
                let mut fb = Framebuffer::new();
                fb.set_frame(frame_for(orientation));
                render_shell(
                    &mut fb,
                    &UiShell {
                        selection: 0,
                        library_entries: &rows,
                        library_total: 1,
                        library_move_pending: false,
                        library_rescanning: false,
                        library_menu: app_core::LibraryMenu::None,
                        ..rescan_shell(orientation, None)
                    },
                );
                fb
            };
            assert_eq!(draw(stored).bytes(), draw(whole).bytes(), "{orientation:?}");
        }
    }

    #[test]
    fn series_folders_drop_the_series_name_their_titles_repeat() {
        let book = |series, volume| crate::UiLibraryRow {
            name: "",
            is_folder: false,
            series,
            title_suffix: "",
            title_len: 0,
            volume,
            file_name: "",
        };
        let strip = |title, row: &crate::UiLibraryRow<'_>, folder| {
            title_without_series_prefix(title, row, folder)
        };
        let bookworm = book("Ascendance of a Bookworm", "2.5");
        let title = "Ascendance of a Bookworm: Part 3";
        assert_eq!(
            strip(title, &bookworm, "Ascendance of a Bookworm"),
            "Part 3"
        );
        // Case and punctuation may differ between folder and series.
        assert_eq!(
            strip(title, &bookworm, "ascendance of a bookworm"),
            "Part 3"
        );
        let rezero = book("Re:Zero", "4");
        assert_eq!(
            strip("Re:Zero - Truth of Zero", &rezero, "Re-Zero"),
            "Truth of Zero"
        );
        // The title stays whole when nothing would remain, at the root, or in
        // a folder of other books.
        let mushoku = book("Mushoku Tensei: Jobless Reincarnation", "1");
        let whole = "Mushoku Tensei: Jobless Reincarnation";
        assert_eq!(
            strip(whole, &mushoku, "Mushoku Tensei: Jobless Reincarnation"),
            whole
        );
        assert_eq!(strip(title, &bookworm, ""), title);
        assert_eq!(strip(title, &bookworm, "Light Novels"), title);
        // A folder name alone is not evidence of a series. A book whose title
        // starts with the folder's name keeps it unless its series metadata
        // names that folder: no series, an unrelated series, or a series the
        // folder names only the start of.
        let new_hope = "Star Wars: A New Hope";
        assert_eq!(strip(new_hope, &book("", "4"), "Star"), new_hope);
        assert_eq!(
            strip(new_hope, &book("Original Trilogy", "1"), "Star"),
            new_hope
        );
        assert_eq!(strip(new_hope, &book("Star Wars", "4"), "Star"), new_hope);
        assert_eq!(
            strip(title, &book("", "3"), "Ascendance of a Bookworm"),
            title
        );
        assert_eq!(strip(whole, &mushoku, "Mushoku Tensei"), whole);
        // The series name must end at a word break in the title.
        assert_eq!(
            strip("Dunes of Arrakis", &book("Dune", "1"), "Dune"),
            "Dunes of Arrakis"
        );
    }

    /// These titles come from a real card and from common edition and
    /// completion notes.
    #[test]
    fn volume_rows_drop_only_the_volume_marker() {
        for (title, volume, expected) in [
            // The full trailing note repeats the title and adds "novel", so
            // it stays like any other note with more words.
            (
                "Reign of the Seven Spellblades, Vol. 9 (light novel) (Reign of the Seven \
                 Spellblades (novel))",
                "9",
                "Reign of the Seven Spellblades (light novel) (Reign of the Seven \
                 Spellblades (novel))",
            ),
            (
                "Stuck in a Time Loop Volume 2 [Complete]",
                "2",
                "Stuck in a Time Loop [Complete]",
            ),
            (
                "Some Series, Vol. 3 (Illustrated Edition)",
                "3",
                "Some Series (Illustrated Edition)",
            ),
            (
                "Some Series, Vol. 4 (Omnibus Edition) [Complete]",
                "4",
                "Some Series (Omnibus Edition) [Complete]",
            ),
            ("Story (Vol. 3) [Complete]", "3", "Story [Complete]"),
            ("Story [Complete] (Vol. 2)", "2", "Story [Complete]"),
            ("Story [3] (Deluxe)", "3", "Story (Deluxe)"),
            ("Some Series, Vol. 5 (Some Series)", "5", "Some Series"),
            ("Some Series, Vol. 5 (some series)", "5", "Some Series"),
            (
                "Some Series, Vol. 5 (Some Series (Manga))",
                "5",
                "Some Series (Some Series (Manga))",
            ),
            // A note that repeats the title with more words names an edition.
            (
                "Some Series, Vol. 3 (Some Series Illustrated Edition)",
                "3",
                "Some Series (Some Series Illustrated Edition)",
            ),
            (
                "Some Series, Vol. 3 (Some Series Complete Edition)",
                "3",
                "Some Series (Some Series Complete Edition)",
            ),
            (
                "Some Series, Vol. 3 (Some Series (Collector's Edition))",
                "3",
                "Some Series (Some Series (Collector's Edition))",
            ),
            // The card keeps this title's first 64 and last 24 bytes, so the
            // joined title has an omission inside its last note.
            (
                "Reign of the Seven Spellblades, Vol. 9 (light novel) (Reign of t\u{2026}\
                 Seven Spellblades (novel))",
                "9",
                "Reign of the Seven Spellblades (light novel)",
            ),
            (
                "86\u{2014}EIGHTY-SIX, Vol. 7: Mist",
                "07",
                "86\u{2014}EIGHTY-SIX: Mist",
            ),
            (
                "Series, Vol. 7: Mist (Special Edition)",
                "7",
                "Series: Mist (Special Edition)",
            ),
            (
                "Sentenced to Be a Hero, Vol. 1: The Prison Records",
                "1",
                "Sentenced to Be a Hero: The Prison Records",
            ),
            ("Series Vol. 4 - The Return", "4", "Series: The Return"),
            (
                "Series Vol. 4 \u{2014} The Return",
                "4",
                "Series: The Return",
            ),
            // Unchanged: another volume, a dash inside a word, no marker,
            // a note that is not the volume, or nothing before the marker.
            (
                "86\u{2014}EIGHTY-SIX, Vol. 7: Mist",
                "8",
                "86\u{2014}EIGHTY-SIX, Vol. 7: Mist",
            ),
            ("Re-Zero Vol. 4-Truth", "4", "Re-Zero Vol. 4-Truth"),
            ("A Tale (light novel)", "1", "A Tale (light novel)"),
            ("A Tale (Part 3)", "3", "A Tale (Part 3)"),
            ("Volume 3: Origins", "3", "Volume 3: Origins"),
            ("Story (Vol. 3)", "3", "Story"),
        ] {
            let mut buf = [0; JOINED_TITLE_BYTES];
            assert_eq!(
                title_without_volume(title, volume, &mut buf),
                expected,
                "{title}"
            );
        }
    }

    #[test]
    fn known_volume_removes_only_matching_terminal_title_marker() {
        for (title, volume, expected) in [
            (
                "Reign of the Seven Spellblades, Vol. 1",
                "1",
                "Reign of the Seven Spellblades",
            ),
            (
                "Reign of the Seven Spellblades, Vol. 10",
                "10",
                "Reign of the Seven Spellblades",
            ),
            ("Another Story Volume 12.5", "12.5", "Another Story"),
            ("Another Story (Volume 2)", "2", "Another Story"),
            ("Another Story Vol.3", "3", "Another Story"),
            ("Volume 2", "2", "Volume 2"),
            ("My Story Volume 2", "3", "My Story Volume 2"),
            ("Story 2", "2", "Story 2"),
            ("Story: Book 2", "2", "Story"),
            ("Story vol 3", "3", "Story"),
            ("Story VOLUME 3", "3", "Story"),
            ("Story, Vol. 03", "3", "Story"),
            ("Story Vol. 3", "3.0", "Story"),
            ("Story #3", "3", "Story"),
            ("Story (3)", "3", "Story"),
            ("Story [Vol. 3]", "3", "Story"),
            ("Story - Volume 2.5", "2.50", "Story"),
            ("Story Vol. 31", "3", "Story Vol. 31"),
            ("Story Revol 3", "3", "Story Revol 3"),
            ("Story Vol.", "3", "Story Vol."),
        ] {
            assert_eq!(
                title_without_volume_suffix(title, volume),
                expected,
                "{title}"
            );
        }
        assert_eq!(
            title_without_volume_suffix("異世界物語 Vol. 12", "12"),
            "異世界物語"
        );
    }

    #[test]
    fn library_titles_keep_their_width_when_a_hidden_volume_enters_the_window() {
        // A loaded 11-row slice spans both viewports, but its last row is
        // offscreen in either orientation. Its metadata must not change the
        // width or truncation of the visible book titles.
        let book = crate::UiLibraryRow {
            name: concat!(
                "A Very Long Book Title with an Unusually Detailed Subtitle That Goes On ",
                "for Several More Sentences and Ends with Volume Eleven"
            ),
            is_folder: false,
            series: "",
            title_suffix: "",
            title_len: 0,
            volume: "",
            file_name: "",
        };
        let rows_without_volume = [book; 11];
        let mut rows_with_hidden_volume = rows_without_volume;
        rows_with_hidden_volume[10].volume = "11";

        for orientation in [
            UiOrientation::LandscapeButtonsBottom,
            UiOrientation::LandscapeButtonsTop,
            UiOrientation::PortraitButtonsLeft,
            UiOrientation::PortraitButtonsRight,
        ] {
            let base = UiShell {
                selection: 0,
                library_entries: &rows_without_volume,
                library_total: 11,
                library_move_pending: false,
                library_rescanning: false,
                library_menu: app_core::LibraryMenu::None,
                ..rescan_shell(orientation, None)
            };
            let mut first = Framebuffer::new();
            first.set_frame(frame_for(orientation));
            render_shell(&mut first, &base);
            let changed = UiShell {
                library_entries: &rows_with_hidden_volume,
                ..base
            };
            let mut second = Framebuffer::new();
            second.set_frame(frame_for(orientation));
            render_shell(&mut second, &changed);
            assert_eq!(
                first.bytes(),
                second.bytes(),
                "{orientation:?}: an offscreen volume must not reflow visible titles"
            );
        }
    }

    /// The catalog keeps a long title as a 64-byte prefix and a 24-byte
    /// ending, which overlap for a title under 88 bytes. Joined and wrapped
    /// as one string, it must draw exactly as the whole title would, in the
    /// list and in Book details. A word across the join stays in one piece.
    #[test]
    fn stored_title_fragments_draw_as_the_whole_title() {
        let full = "Reign of the Seven Spellblades: The Complete and Unabridged \
                    Collector's Edition, Vol. 3";
        assert!(full.len() > 64 && full.len() < 88);
        let stored = crate::UiLibraryRow {
            name: &full[..64],
            is_folder: false,
            series: "Reign of the Seven Spellblades",
            title_suffix: &full[full.len() - 24..],
            title_len: full.len() as u8,
            volume: "3",
            file_name: "",
        };
        let whole = crate::UiLibraryRow {
            name: full,
            title_suffix: "",
            ..stored
        };
        for orientation in [
            UiOrientation::LandscapeButtonsBottom,
            UiOrientation::LandscapeButtonsTop,
            UiOrientation::PortraitButtonsLeft,
            UiOrientation::PortraitButtonsRight,
        ] {
            for menu in [app_core::LibraryMenu::None, app_core::LibraryMenu::Details] {
                let draw = |row: crate::UiLibraryRow<'_>| {
                    let rows = [row];
                    let mut fb = Framebuffer::new();
                    fb.set_frame(frame_for(orientation));
                    render_shell(
                        &mut fb,
                        &UiShell {
                            selection: 0,
                            library_entries: &rows,
                            library_total: 1,
                            library_move_pending: false,
                            library_rescanning: false,
                            library_menu: menu,
                            ..rescan_shell(orientation, None)
                        },
                    );
                    fb
                };
                assert_eq!(
                    draw(stored).bytes(),
                    draw(whole).bytes(),
                    "{orientation:?} {menu:?}"
                );
            }
        }
    }

    #[test]
    fn details_cannot_mask_unreadable_or_relisting_library_status() {
        for orientation in [
            UiOrientation::LandscapeButtonsBottom,
            UiOrientation::LandscapeButtonsTop,
            UiOrientation::PortraitButtonsLeft,
            UiOrientation::PortraitButtonsRight,
        ] {
            let details = UiShell {
                library_menu: app_core::LibraryMenu::Details,
                library_entries: &[],
                library_total: 0,
                ..rescan_shell(orientation, None)
            };
            let scanning = UiShell {
                library_status: UiLibraryStatus::Scanning,
                ..details
            };
            let error = UiShell {
                library_status: UiLibraryStatus::Error,
                ..details
            };
            let mut loading_frame = Framebuffer::new();
            loading_frame.set_frame(frame_for(orientation));
            render_shell(&mut loading_frame, &scanning);
            let mut error_frame = Framebuffer::new();
            error_frame.set_frame(frame_for(orientation));
            render_shell(&mut error_frame, &error);
            assert_ne!(
                loading_frame.bytes(),
                error_frame.bytes(),
                "{orientation:?}: a stale Details view must not mask the status note"
            );
        }
    }

    #[test]
    fn details_screen_is_static_and_does_not_draw_the_underlying_list() {
        for orientation in [
            UiOrientation::LandscapeButtonsBottom,
            UiOrientation::LandscapeButtonsTop,
            UiOrientation::PortraitButtonsLeft,
            UiOrientation::PortraitButtonsRight,
        ] {
            let rows = [crate::UiLibraryRow {
                name: "An Adventurer's Guide to the Distant Lands and Other Remarkable Places",
                is_folder: false,
                series: "The Wanderer's Library",
                title_suffix: "Volume 12.5",
                title_len: 0,
                volume: "12.5",
                file_name: "",
            }];
            let normal = UiShell {
                selection: 0,
                library_entries: &rows,
                library_total: 1,
                library_move_pending: false,
                library_rescanning: false,
                library_menu: app_core::LibraryMenu::None,
                ..rescan_shell(orientation, None)
            };
            let mut list = Framebuffer::new();
            list.set_frame(frame_for(orientation));
            render_shell(&mut list, &normal);
            let details = UiShell {
                library_menu: app_core::LibraryMenu::Details,
                ..normal
            };
            let mut first = Framebuffer::new();
            first.set_frame(frame_for(orientation));
            render_shell(&mut first, &details);
            let mut again = Framebuffer::new();
            again.set_frame(frame_for(orientation));
            render_shell(&mut again, &details);
            assert_eq!(first.bytes(), again.bytes(), "{orientation:?}");
            assert_ne!(first.bytes(), list.bytes(), "{orientation:?}");

            // Changing any persisted metadata field must change the Details
            // frame, in both portrait and landscape. This also ensures the
            // trailing title and volume are not accidentally omitted.
            let variants = [
                (
                    "title",
                    crate::UiLibraryRow {
                        name: "A Different Book",
                        ..rows[0]
                    },
                ),
                (
                    "series",
                    crate::UiLibraryRow {
                        series: "Another Series",
                        ..rows[0]
                    },
                ),
                (
                    "title suffix",
                    crate::UiLibraryRow {
                        title_suffix: "Volume 13.5",
                        ..rows[0]
                    },
                ),
                (
                    "volume",
                    crate::UiLibraryRow {
                        volume: "13.5",
                        ..rows[0]
                    },
                ),
                (
                    "file name",
                    crate::UiLibraryRow {
                        file_name: "Adventurer's Guide 12.5.epub",
                        ..rows[0]
                    },
                ),
            ];
            for (field, row) in variants {
                let changed_rows = [row];
                let changed = UiShell {
                    library_entries: &changed_rows,
                    ..details
                };
                let mut frame = Framebuffer::new();
                frame.set_frame(frame_for(orientation));
                render_shell(&mut frame, &changed);
                assert_ne!(
                    first.bytes(),
                    frame.bytes(),
                    "{orientation:?}: changing {field} should affect Details"
                );
            }
        }
    }

    #[test]
    fn wrap_title_lines_fits_alice_on_three_portrait_lines() {
        // The portrait content measure (444 - 44). Two lines cut this title
        // mid-word; the third line lets it render whole.
        let font = literata_display();
        let title = "Alice's Adventures in Wonderland";
        let (lines, count, overflow) = wrap_title_lines(font, title, 400, 3);
        assert_eq!(&lines[..count], &["Alice's", "Adventures in", "Wonderland"]);
        assert!(!overflow);
        for line in &lines[..count] {
            assert!(measure_text(font, line) <= 400);
        }
    }

    #[test]
    fn wrap_title_lines_flags_overflow_and_reserves_ellipsis_room() {
        let font = literata_display();
        let title = "The Collected Correspondence of an Unhurried Victorian Naturalist";
        let (lines, count, overflow) = wrap_title_lines(font, title, 400, 3);
        assert_eq!(count, 3);
        assert!(overflow);
        let dots = measure_text(font, "...");
        assert!(measure_text(font, lines[2]) + dots <= 400);
    }

    #[test]
    fn wrap_title_keeps_short_titles_on_one_line() {
        let font = literata_display();
        let (first, second) = wrap_title(font, "Dune", 530);
        assert_eq!(first, "Dune");
        assert!(second.is_empty());
    }

    #[test]
    fn wrap_title_breaks_long_titles_at_a_word_boundary() {
        let font = literata_display();
        let title = "Harry Potter and the Methods of Rationality";
        let (first, second) = wrap_title(font, title, 530);
        assert!(!second.is_empty());
        assert!(measure_text(font, first) <= 530);
        assert!(measure_text(font, second) <= 530);
        // The break consumes the separating space and loses no words up
        // to the second line's own truncation point.
        assert!(title.starts_with(first));
        assert!(title[first.len() + 1..].starts_with(second));
    }

    #[test]
    fn wrap_title_truncates_an_unbreakable_word() {
        let font = literata_display();
        let title = "Donaudampfschifffahrtsgesellschaftskapitaen";
        let (first, second) = wrap_title(font, title, 200);
        assert!(measure_text(font, first) <= 200);
        assert!(second.is_empty());
        // The cut is final and the caller draws `...` after it, so the two
        // together stay within the measure, as on the last line of a
        // breakable title.
        let (lines, count, overflow) = wrap_title_lines(font, title, 200, 2);
        assert!(overflow);
        assert_eq!(count, 1);
        assert!(
            measure_text(font, lines[0]) + measure_text(font, "...") <= 200,
            "{:?} plus the dots overruns",
            lines[0]
        );
        assert!(!lines[0].is_empty());
    }

    /// The rule carries the percent: it reads empty until the scan says, it
    /// grows with the percent, and it stops at full.
    #[test]
    fn the_rescan_rule_carries_its_percent() {
        let footer = |percent: Option<u8>| {
            let mut fb = Framebuffer::new();
            fb.set_frame(FbFrame::Landscape);
            fb.clear(true);
            rescan_footer(
                &mut fb,
                ShellLayout::for_orientation(UiOrientation::LandscapeButtonsBottom),
                percent,
            );
            fb.bytes().to_vec()
        };
        assert_eq!(footer(None), footer(Some(0)));
        assert_ne!(footer(Some(0)), footer(Some(40)));
        assert_ne!(footer(Some(40)), footer(Some(100)));
        assert_eq!(footer(Some(250)), footer(Some(100)));
    }

    fn rescan_shell(orientation: UiOrientation, percent: Option<u8>) -> UiShell<'static> {
        const ROWS: [crate::UiLibraryRow<'static>; 3] = [
            crate::UiLibraryRow {
                name: "Shelf",
                is_folder: true,
                series: "",
                title_suffix: "",
                title_len: 0,
                volume: "",
                file_name: "",
            },
            crate::UiLibraryRow {
                name: "A Very Long Book Title That Runs Past The Column",
                is_folder: false,
                series: "",
                title_suffix: "",
                title_len: 0,
                volume: "",
                file_name: "",
            },
            crate::UiLibraryRow {
                name: "Added",
                is_folder: false,
                series: "",
                title_suffix: "",
                title_len: 0,
                volume: "",
                file_name: "",
            },
        ];
        UiShell {
            view: UiView::Library,
            orientation,
            front_pages_left: false,
            refresh_policy: UiRefreshPolicy::FullOnWake,
            font_size: Default::default(),
            line_spacing: Default::default(),
            font_weight: Default::default(),
            font_family: Default::default(),
            alignment: Default::default(),
            custom_font_name: "",
            selection: 2,
            chapter: 0,
            chapter_title: "",
            page: 0,
            page_count: 0,
            // Three digits: the widest corner the clear has to stop short of.
            battery_percent: 100,
            active_book: crate::UiBook {
                title: "",
                author: "",
                progress_permille: 0,
                cover: None,
            },
            library_status: UiLibraryStatus::Ready,
            library_entries: &ROWS,
            library_window_start: 0,
            library_folder: "",
            library_total: 3,
            chapters: &[],
            chapters_window_start: 0,
            chapters_total: 0,
            chapters_expanded: None,
            chapter_subsections: &[],
            sync_status: UiSyncStatus::NotConfigured,
            wifi_ssid: "",
            library_menu: app_core::LibraryMenu::None,
            library_move_pending: true,
            library_rescanning: true,
            library_rescan_percent: percent,
        }
    }

    fn frame_for(orientation: UiOrientation) -> FbFrame {
        match orientation {
            UiOrientation::LandscapeButtonsBottom => FbFrame::Landscape,
            UiOrientation::LandscapeButtonsTop => FbFrame::LandscapeFlipped,
            UiOrientation::PortraitButtonsLeft | UiOrientation::PortraitButtonsRight => {
                FbFrame::Portrait
            }
        }
    }

    #[test]
    fn pending_relist_draws_reading_the_card_instead_of_an_empty_library() {
        let orientation = UiOrientation::LandscapeButtonsBottom;
        let draw = |status: UiLibraryStatus| {
            let mut fb = Framebuffer::new();
            fb.set_frame(frame_for(orientation));
            render_shell(
                &mut fb,
                &UiShell {
                    library_status: status,
                    library_entries: &[],
                    library_total: 2,
                    ..rescan_shell(orientation, None)
                },
            );
            fb
        };
        let pending = draw(UiLibraryStatus::Ready.while_relisting(true));
        let reading_card = draw(UiLibraryStatus::Scanning);
        let no_books = draw(UiLibraryStatus::Empty);
        assert_eq!(pending.bytes(), reading_card.bytes());
        assert_ne!(pending.bytes(), no_books.bytes());
        assert_eq!(
            UiLibraryStatus::Error.while_relisting(true),
            UiLibraryStatus::Error
        );
        assert_eq!(
            UiLibraryStatus::Empty.while_relisting(false),
            UiLibraryStatus::Empty
        );
    }

    /// Home draws the progress rule from a restored page before any open
    /// has clamped it to the count. A page at the top of the range draws the
    /// full rule, as the last page does.
    #[test]
    fn home_progress_from_a_restored_page_at_the_top_is_full() {
        let orientation = UiOrientation::LandscapeButtonsBottom;
        let home = |page| {
            let mut fb = Framebuffer::new();
            fb.set_frame(frame_for(orientation));
            render_shell(
                &mut fb,
                &UiShell {
                    view: UiView::Home,
                    page,
                    page_count: 500,
                    ..rescan_shell(orientation, None)
                },
            );
            fb
        };
        assert!(home(u32::MAX).bytes() == home(499).bytes());
        assert!(home(499).bytes() != home(0).bytes());
    }

    /// The firmware's progress repaint redraws only the note, over a frame
    /// whose rows it cannot read mid-scan. What it leaves must be exactly the
    /// frame a full render draws, in every orientation, and the battery in
    /// the corner of the same line must survive it.
    #[test]
    fn a_progress_redraw_is_the_full_frame_at_that_percent() {
        for orientation in [
            UiOrientation::LandscapeButtonsBottom,
            UiOrientation::LandscapeButtonsTop,
            UiOrientation::PortraitButtonsLeft,
            UiOrientation::PortraitButtonsRight,
        ] {
            for (from, to) in [
                (None, Some(40)),
                (Some(40), Some(87)),
                (Some(9), Some(100)),
                (Some(100), Some(1)),
            ] {
                let mut redrawn = Framebuffer::new();
                redrawn.set_frame(frame_for(orientation));
                render_shell(&mut redrawn, &rescan_shell(orientation, from));
                redraw_rescan_footer(&mut redrawn, orientation, to);

                let mut full = Framebuffer::new();
                full.set_frame(frame_for(orientation));
                render_shell(&mut full, &rescan_shell(orientation, to));
                assert!(
                    redrawn.bytes() == full.bytes(),
                    "{orientation:?}: {from:?} redrawn as {to:?}"
                );
            }
        }
    }

    #[test]
    fn contents_322_entry_windowed_list_reports_correct_footer_position() {
        let orientation = UiOrientation::PortraitButtonsLeft;
        let layout = ShellLayout::for_orientation(orientation);

        // A 322-entry book whose resident window covers entries 173..322.
        // Entry 301 is selected.
        let mut chapters = [UiTocItem {
            title: "Chapter",
            level: 1,
            page: 1,
            has_subsections: false,
            expanded: false,
            is_subsection: false,
        }; 149];
        for (i, item) in chapters.iter_mut().enumerate() {
            item.page = (174 + i) as u32;
        }

        let chapter_subsections = [0u8; app_core::MAX_SD_CHAPTER_SUBSECTIONS / 8];
        let shell = UiShell {
            view: UiView::Chapters,
            selection: 301,
            chapters: &chapters,
            chapters_window_start: 173,
            chapters_total: 322,
            chapters_expanded: None,
            chapter_subsections: &chapter_subsections,
            ..rescan_shell(orientation, None)
        };

        let mut fb = Framebuffer::new();
        fb.set_frame(frame_for(orientation));
        render_shell(&mut fb, &shell);

        let mut fb_expected_footer = Framebuffer::new();
        fb_expected_footer.set_frame(frame_for(orientation));
        fb_expected_footer.clear(true);
        position_footer(&mut fb_expected_footer, layout, 302, 322);

        let mut fb_buggy_footer = Framebuffer::new();
        fb_buggy_footer.set_frame(frame_for(orientation));
        fb_buggy_footer.clear(true);
        position_footer(&mut fb_buggy_footer, layout, 129, 149);

        let footer_y = layout.footer_y() as usize;
        let y_min = footer_y.saturating_sub(15);
        let y_max = footer_y + 10;
        let width = if layout.portrait {
            display::WIDTH
        } else {
            display::HEIGHT
        };

        let mut expected_ink = 0usize;
        let mut matching_ink = 0usize;
        for y in y_min..=y_max {
            for x in 0..width {
                if !fb_expected_footer.pixel(x, y) {
                    expected_ink += 1;
                    if !fb.pixel(x, y) {
                        matching_ink += 1;
                    }
                }
            }
        }
        assert!(expected_ink > 0, "expected footer must have drawn ink");
        assert_eq!(
            matching_ink, expected_ink,
            "all ink pixels of '302 of 322' must be present on screen"
        );

        let mut buggy_ink_mismatch = 0usize;
        for y in y_min..=y_max {
            for x in 0..width {
                if !fb_buggy_footer.pixel(x, y) && fb.pixel(x, y) {
                    buggy_ink_mismatch += 1;
                }
            }
        }
        assert!(
            buggy_ink_mismatch > 0,
            "rendered screen must not match '129 of 149'"
        );
    }

    #[test]
    fn contents_orphaned_level2_entries_remain_visible_roots() {
        let orientation = UiOrientation::PortraitButtonsLeft;
        let chapters = [
            UiTocItem {
                title: "Chapter 1",
                level: 2,
                page: 1,
                has_subsections: false,
                expanded: false,
                is_subsection: false,
            },
            UiTocItem {
                title: "Chapter 2",
                level: 2,
                page: 10,
                has_subsections: false,
                expanded: false,
                is_subsection: false,
            },
        ];

        let chapter_subsections = [0u8; app_core::MAX_SD_CHAPTER_SUBSECTIONS / 8];
        let shell = UiShell {
            view: UiView::Chapters,
            selection: 0,
            chapters: &chapters,
            chapters_window_start: 0,
            chapters_total: 2,
            chapters_expanded: None,
            chapter_subsections: &chapter_subsections,
            ..rescan_shell(orientation, None)
        };

        let mut fb = Framebuffer::new();
        fb.set_frame(frame_for(orientation));
        render_shell(&mut fb, &shell);

        let mut fb_empty = Framebuffer::new();
        fb_empty.set_frame(frame_for(orientation));
        render_shell(
            &mut fb_empty,
            &UiShell {
                view: UiView::Chapters,
                selection: 0,
                chapters: &[],
                chapters_window_start: 0,
                chapters_total: 0,
                chapters_expanded: None,
                chapter_subsections: &[],
                ..rescan_shell(orientation, None)
            },
        );

        assert_ne!(
            fb.bytes(),
            fb_empty.bytes(),
            "orphaned entries must remain visible and not display 'no chapters found'"
        );
    }
}
