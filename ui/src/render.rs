use crate::{UiLibraryStatus, UiOrientation, UiRefreshPolicy, UiShell, UiTocItem, UiView};
use display::fb::Framebuffer;
use display::render::{draw_ascii, fill_rect, glyph_5x7};
use display::{Rect, HEIGHT, WIDTH};

const HOME_ITEMS: [&str; 4] = ["READ", "FILES", "SYNC", "SETTINGS"];
const SETTINGS_ITEMS: [&str; 3] = ["ORIENTATION", "REFRESH", "BACK TO HOME"];
const SHELL_ORIENTATION: UiOrientation = UiOrientation::PortraitButtonsLeft;

pub fn render_shell(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    match shell.view {
        UiView::Home => render_home(fb, shell),
        UiView::Library => render_library(fb, shell),
        UiView::Chapters => render_chapters_landscape(fb, shell),
        UiView::Sync => render_sync(fb),
        UiView::Settings => render_settings(fb, shell),
    }
}

pub fn render_shell_overlay(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    match shell.view {
        UiView::Home => render_home(fb, shell),
        UiView::Library => render_library(fb, shell),
        UiView::Chapters => render_chapters_landscape(fb, shell),
        UiView::Sync => render_sync(fb),
        UiView::Settings => render_settings(fb, shell),
    }
}

fn render_home(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    let mut ui = Ui::new(fb, SHELL_ORIENTATION);
    draw_home_status(&mut ui, shell);
    draw_cover_placeholder(&mut ui, 68, 76, 344, 500);
    draw_progress_bar(
        &mut ui,
        142,
        600,
        196,
        6,
        shell.active_book.progress_permille,
    );
    ui.draw_ascii(
        shell.active_book.title,
        centered_x_for(480, shell.active_book.title),
        636,
        false,
    );
    ui.draw_ascii(
        shell.active_book.author,
        centered_x_for(480, shell.active_book.author),
        666,
        false,
    );
    draw_home_soft_keys(&mut ui);
}

fn draw_home_status(ui: &mut Ui<'_>, shell: &UiShell<'_>) {
    ui.draw_ascii("XTEINK", 36, 48, false);
    let mut buf = [0u8; 10];
    ui.draw_ascii(fmt_percent(shell.battery_percent, &mut buf), 366, 50, false);
    draw_battery_icon(ui, 404, 48, battery_bars(shell.battery_percent));
}

fn draw_home_soft_keys(ui: &mut Ui<'_>) {
    let tab_y = 742;
    let tab_h = 58;
    let tab_w = 120;
    let mut x = 0;
    for item in HOME_ITEMS {
        ui.stroke_rect(x, tab_y, tab_w, tab_h, false);
        ui.draw_ascii(
            item,
            x as usize + centered_x_for(tab_w as usize, item),
            tab_y as usize + 24,
            false,
        );
        x += tab_w;
    }
}

fn render_library(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    let mut ui = Ui::new(fb, SHELL_ORIENTATION);
    ui.draw_ascii("FILES", 64, 72, false);
    ui.fill_rect(64, 110, 352, 2, false);
    ui.draw_ascii("/books then /", 64, 132, false);

    match shell.library_status {
        UiLibraryStatus::NotScanned | UiLibraryStatus::Scanning => {
            ui.draw_ascii("SCANNING MICROSD", 64, 216, false);
            return;
        }
        UiLibraryStatus::Error => {
            ui.draw_ascii("MICROSD NOT READY", 64, 216, false);
            ui.draw_ascii("USE FAT16/FAT32", 64, 248, false);
            return;
        }
        UiLibraryStatus::Empty => {
            ui.draw_ascii("NO EPUB FILES FOUND", 64, 216, false);
            ui.draw_ascii("PUT BOOKS IN /books", 64, 248, false);
            return;
        }
        UiLibraryStatus::Ready => {}
    }

    if shell.library_entries.is_empty() {
        ui.draw_ascii("NO EPUB FILES FOUND", 64, 216, false);
        return;
    }

    let mut y = 198;
    for (index, entry) in shell.library_entries.iter().take(9).enumerate() {
        let selected = index == shell.selection as usize;
        if selected {
            ui.fill_rect(56, y - 12, 368, 32, false);
        }
        ui.draw_ascii(if selected { ">" } else { " " }, 76, y as usize, selected);
        ui.draw_ascii(entry, 112, y as usize, selected);
        y += 48;
    }
}

fn render_settings(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    let mut ui = Ui::new(fb, SHELL_ORIENTATION);
    draw_menu(&mut ui, "SETTINGS", &SETTINGS_ITEMS, shell.selection);
    ui.draw_ascii("READING ORIENTATION", 64, 380, false);
    ui.draw_ascii(orientation_label(shell.orientation), 64, 408, false);
    ui.draw_ascii("REFRESH", 64, 464, false);
    ui.draw_ascii(refresh_policy_label(shell.refresh_policy), 64, 492, false);
}

fn render_sync(fb: &mut Framebuffer) {
    let mut ui = Ui::new(fb, SHELL_ORIENTATION);
    ui.draw_ascii("SYNC", centered_x_for(480, "SYNC"), 300, false);
    ui.draw_ascii(
        "NOT CONFIGURED",
        centered_x_for(480, "NOT CONFIGURED"),
        344,
        false,
    );
    ui.draw_ascii("BACK", centered_x_for(480, "BACK"), 620, false);
}

fn render_chapters_landscape(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    draw_ascii(fb, "CHAPTERS", 96, 112, false);
    if shell.chapters.is_empty() {
        draw_ascii(fb, "NO CHAPTERS", 96, 168, false);
        return;
    }
    let selected = (shell.selection as usize).min(shell.chapters.len().saturating_sub(1));
    let first = selected.saturating_sub(4);
    let mut item_y = 168usize;
    for (index, item) in shell.chapters.iter().enumerate().skip(first).take(8) {
        draw_toc_item(fb, item, index == selected, item_y);
        item_y += 36;
    }
    draw_ascii(fb, "OK JUMPS TO CHAPTER", 96, 408, false);
}

fn draw_toc_item(fb: &mut Framebuffer, item: &UiTocItem<'_>, selected: bool, y: usize) {
    if selected {
        fill_rect(fb, Rect::new(88, y as u16 - 10, 624, 28), false);
    }
    draw_ascii(fb, if selected { ">" } else { " " }, 104, y, selected);
    let indent = 136 + (item.level.saturating_sub(1) as usize * 18);
    draw_ascii_truncated(
        fb,
        item.title,
        indent,
        y,
        66usize.saturating_sub(item.level as usize * 2),
        selected,
    );
}

fn draw_menu(ui: &mut Ui<'_>, title: &str, items: &[&str], selection: u8) {
    ui.draw_ascii(title, 64, 72, false);
    ui.fill_rect(64, 110, 352, 2, false);
    let mut y = 172;
    for (index, item) in items.iter().enumerate() {
        let selected = index == selection as usize;
        if selected {
            ui.fill_rect(56, y - 12, 368, 32, false);
        }
        ui.draw_ascii(if selected { ">" } else { " " }, 76, y as usize, selected);
        ui.draw_ascii(item, 112, y as usize, selected);
        y += 48;
    }
}

fn draw_cover_placeholder(ui: &mut Ui<'_>, x: u16, y: u16, w: u16, h: u16) {
    ui.stroke_rect(x, y, w, h, false);
    ui.stroke_rect(x + 8, y + 8, w - 16, h - 16, false);
    ui.fill_rect(x + 24, y + 38, 2, h - 76, false);
    ui.draw_ascii("BRING", x as usize + 90, y as usize + 126, false);
    ui.draw_ascii("UP", x as usize + 106, y as usize + 158, false);
    ui.draw_ascii("NOTES", x as usize + 94, y as usize + 190, false);
}

fn draw_progress_bar(ui: &mut Ui<'_>, x: u16, y: u16, w: u16, h: u16, permille: u16) {
    ui.stroke_rect(x, y, w, h, false);
    let inner_w = w.saturating_sub(4);
    let fill_w = ((inner_w as u32 * permille.min(1000) as u32) / 1000) as u16;
    if fill_w > 0 {
        let fill_h = h.saturating_sub(4).max(1);
        let fill_y = if h > 4 { y + 2 } else { y + 1 };
        ui.fill_rect(x + 2, fill_y, fill_w, fill_h, false);
    }
}

fn draw_battery_icon(ui: &mut Ui<'_>, x: u16, y: u16, bars: u8) {
    ui.stroke_rect(x, y, 36, 16, false);
    ui.fill_rect(x + 36, y + 5, 4, 6, false);
    for bar in 0..bars.min(4) {
        ui.fill_rect(x + 4 + bar as u16 * 8, y + 4, 5, 8, false);
    }
}

fn draw_ascii_truncated(
    fb: &mut Framebuffer,
    text: &str,
    x: usize,
    y: usize,
    max_chars: usize,
    inverted: bool,
) {
    let mut cursor = x;
    for byte in text.bytes().take(max_chars) {
        let glyph = glyph_5x7(byte);
        for (col, bits) in glyph.iter().enumerate() {
            for row in 0..7 {
                if bits & (1 << row) != 0 {
                    fb.set_pixel(cursor + col, y + row, inverted);
                }
            }
        }
        cursor += 8;
    }
}

fn battery_bars(percent: u8) -> u8 {
    match percent {
        0..=10 => 0,
        11..=35 => 1,
        36..=60 => 2,
        61..=85 => 3,
        _ => 4,
    }
}

fn centered_x_for(width: usize, text: &str) -> usize {
    width.saturating_sub(text.len() * 8) / 2
}

fn orientation_label(orientation: UiOrientation) -> &'static str {
    match orientation {
        UiOrientation::LandscapeButtonsBottom => "LANDSCAPE BOTTOM",
        UiOrientation::LandscapeButtonsTop => "LANDSCAPE TOP",
        UiOrientation::PortraitButtonsLeft => "PORTRAIT LEFT",
        UiOrientation::PortraitButtonsRight => "PORTRAIT RIGHT",
    }
}

fn refresh_policy_label(policy: UiRefreshPolicy) -> &'static str {
    match policy {
        UiRefreshPolicy::FastOnly => "FAST ONLY",
        UiRefreshPolicy::FullOnWake => "FULL ON WAKE",
        UiRefreshPolicy::FullEveryTen => "FULL EVERY 10",
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

fn fmt_percent(n: u8, buf: &mut [u8; 10]) -> &str {
    let mut tmp = [0u8; 10];
    let number = fmt_u32(n as u32, &mut tmp).as_bytes();
    if number.len() + 1 > buf.len() {
        return "?";
    }
    buf[..number.len()].copy_from_slice(number);
    buf[number.len()] = b'%';
    core::str::from_utf8(&buf[..number.len() + 1]).unwrap_or("?")
}

struct Ui<'a> {
    fb: &'a mut Framebuffer,
    orientation: UiOrientation,
}

impl<'a> Ui<'a> {
    fn new(fb: &'a mut Framebuffer, orientation: UiOrientation) -> Self {
        Self { fb, orientation }
    }

    fn fill_rect(&mut self, x: u16, y: u16, w: u16, h: u16, white: bool) {
        let y = self.logical_y_for_height(y, h);
        for yy in y..y.saturating_add(h) {
            for xx in x..x.saturating_add(w) {
                self.set_pixel(xx as usize, yy as usize, white);
            }
        }
    }

    fn stroke_rect(&mut self, x: u16, y: u16, w: u16, h: u16, white: bool) {
        if w == 0 || h == 0 {
            return;
        }
        let y = self.logical_y_for_height(y, h);
        let x1 = x + w - 1;
        let y1 = y + h - 1;
        for xx in x..=x1 {
            self.set_pixel(xx as usize, y as usize, white);
            self.set_pixel(xx as usize, y1 as usize, white);
        }
        for yy in y..=y1 {
            self.set_pixel(x as usize, yy as usize, white);
            self.set_pixel(x1 as usize, yy as usize, white);
        }
    }

    fn draw_ascii(&mut self, text: &str, x: usize, y: usize, white: bool) {
        let y = self.logical_y_for_height(y as u16, 7) as usize;
        let mut cursor = x;
        for byte in text.bytes() {
            self.draw_glyph(byte, cursor, y, white);
            cursor += 8;
        }
    }

    fn draw_glyph(&mut self, byte: u8, x: usize, y: usize, white: bool) {
        let glyph = glyph_5x7(byte);
        for (col, bits) in glyph.iter().enumerate() {
            for row in 0..7 {
                if bits & (1 << row) != 0 {
                    self.set_pixel(x + col, y + row, white);
                }
            }
        }
    }

    fn set_pixel(&mut self, x: usize, y: usize, white: bool) {
        let Some((fx, fy)) = map_ui_pixel(self.orientation, x, y) else {
            return;
        };
        self.fb.set_pixel(fx, fy, white);
    }

    fn logical_y_for_height(&self, y: u16, h: u16) -> u16 {
        match self.orientation {
            UiOrientation::PortraitButtonsLeft | UiOrientation::PortraitButtonsRight => {
                (WIDTH as u16).saturating_sub(y.saturating_add(h))
            }
            UiOrientation::LandscapeButtonsBottom | UiOrientation::LandscapeButtonsTop => y,
        }
    }
}

fn map_ui_pixel(orientation: UiOrientation, x: usize, y: usize) -> Option<(usize, usize)> {
    match orientation {
        UiOrientation::LandscapeButtonsBottom => {
            if x < WIDTH && y < HEIGHT {
                Some((x, y))
            } else {
                None
            }
        }
        UiOrientation::LandscapeButtonsTop => {
            if x < WIDTH && y < HEIGHT {
                Some((WIDTH - 1 - x, HEIGHT - 1 - y))
            } else {
                None
            }
        }
        UiOrientation::PortraitButtonsRight => {
            if x < HEIGHT && y < WIDTH {
                Some((WIDTH - 1 - y, x))
            } else {
                None
            }
        }
        UiOrientation::PortraitButtonsLeft => {
            if x < HEIGHT && y < WIDTH {
                Some((y, HEIGHT - 1 - x))
            } else {
                None
            }
        }
    }
}
