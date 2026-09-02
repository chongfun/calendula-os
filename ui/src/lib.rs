#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub use app_core::LibraryMenu;
use app_core::{PortalPsk, PortalSsid};

pub mod app_render;
pub mod custom_font;
pub mod icons;
pub mod join_qr;
pub mod layout;
pub mod qr_generated;
pub mod reading;
pub mod render;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiView {
    Home,
    Library,
    Chapters,
    Wireless,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiOrientation {
    LandscapeButtonsBottom,
    LandscapeButtonsTop,
    PortraitButtonsLeft,
    PortraitButtonsRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiRefreshPolicy {
    FastOnly,
    FullOnWake,
    FullEveryTen,
}

/// Wireless screen lifecycle, mirrored from app-core so the renderer stays
/// decoupled from reducer types the way UiView mirrors AppView.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiSyncStatus {
    NotConfigured,
    Idle,
    ForgetPending,
    Starting,
    Connecting,
    Connected([u8; 4]),
    /// The onboarding hotspot is up; carries the session's WPA2 PSK
    /// for the join QR and manual-join text. Carried as [`PortalPsk`]
    /// rather than raw bytes so its redacted `Debug` keeps the live
    /// password out of any formatted UI state.
    PortalUp(PortalPsk, PortalSsid),
    Serving([u8; 4]),
    CredentialsSaved,
    Error(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiLibraryStatus {
    NotScanned,
    Scanning,
    Ready,
    Empty,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiCover<'a> {
    pub width: u16,
    pub height: u16,
    pub stride: u16,
    pub bits: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiBook<'a> {
    pub title: &'a str,
    pub author: &'a str,
    pub progress_permille: u16,
    pub cover: Option<UiCover<'a>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiTocItem<'a> {
    pub title: &'a str,
    pub level: u8,
    /// 1-based book page the chapter starts on; 0 when unknown.
    pub page: u32,
}

/// One row of the Library list: a book to open, or a folder to go into.
///
/// The kind is drawn as a mark on the name rather than as any difference in
/// weight or shade, because the panel is one bit deep and a reader with low
/// vision has only shape to go on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiLibraryRow<'a> {
    pub name: &'a str,
    pub is_folder: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiShell<'a> {
    pub view: UiView,
    pub orientation: UiOrientation,
    /// The front page-turn pair sits left of back/confirm instead of right
    /// of it; the key rail's labels follow the buttons.
    pub front_pages_left: bool,
    pub refresh_policy: UiRefreshPolicy,
    pub font_size: display::font::FontSize,
    pub line_spacing: display::font::LineSpacing,
    pub font_weight: display::font::FontWeight,
    pub font_family: display::font::FontFamily,
    pub custom_font_name: &'a str,
    pub selection: u16,
    pub chapter: u16,
    /// The current chapter's title resolved over the whole book (past the
    /// resident `chapters` cap). When non-empty the colophon prefers it over
    /// `chapters[chapter]`; empty falls back to the list or a numeral.
    pub chapter_title: &'a str,
    pub page: u32,
    pub page_count: u32,
    pub battery_percent: u8,
    pub active_book: UiBook<'a>,
    pub library_status: UiLibraryStatus,
    /// The resident slice of the current folder's listing:
    /// `library_entries[i]` is row `library_window_start + i`, and rows are
    /// the folder's books followed by its folders. The folder is read from
    /// the card a page at a time, so this holds only the rows around the
    /// selection.
    pub library_entries: &'a [UiLibraryRow<'a>],
    pub library_window_start: u16,
    /// The folder being shown, or empty at the library root. Names the
    /// screen and decides whether Back goes up a level or leaves for Home,
    /// so the rail never promises the wrong one.
    pub library_folder: &'a str,
    /// Total book count across the whole catalog, independent of the resident
    /// window — drives the "x of N" footer and the scroll math.
    pub library_total: u16,
    /// The resident slice of the on-disk TOC the Contents page draws from:
    /// `chapters[i]` is the chapter at absolute index
    /// `chapters_window_start + i`. Long TOCs are windowed like the catalog.
    pub chapters: &'a [UiTocItem<'a>],
    pub chapters_window_start: u16,
    /// Full chapter count on disk, independent of the resident window.
    pub chapters_total: u16,
    pub sync_status: UiSyncStatus,
    /// The saved Wi-Fi network's name; empty when none is saved. Names
    /// the network on the Wireless screen's idle and forget states.
    pub wifi_ssid: &'a str,
    /// The Library per-book actions sheet's progress on the selected row:
    /// the sheet overlays the list and takes the key rail; a picked action
    /// executes on that press, and the waiting and settled states show in
    /// the footer where the position line normally sits.
    pub library_menu: app_core::LibraryMenu,
    /// Whether a move through the folder tree is outstanding. The list is
    /// held still while it is, so the rail has to stop offering the presses
    /// that wait swallows.
    pub library_move_pending: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_psk_stays_redacted_in_ui_debug_output() {
        let psk = PortalPsk::EMULATOR_DEMO;
        let status = UiSyncStatus::PortalUp(psk, PortalSsid::EMULATOR_DEMO);
        let shell = UiShell {
            view: UiView::Wireless,
            orientation: UiOrientation::PortraitButtonsRight,
            front_pages_left: false,
            refresh_policy: UiRefreshPolicy::FullEveryTen,
            font_size: Default::default(),
            line_spacing: Default::default(),
            font_weight: Default::default(),
            font_family: Default::default(),
            custom_font_name: "",
            selection: 0,
            chapter: 0,
            chapter_title: "",
            page: 1,
            page_count: 1,
            battery_percent: 100,
            active_book: UiBook {
                title: "",
                author: "",
                progress_permille: 0,
                cover: None,
            },
            library_status: UiLibraryStatus::NotScanned,
            library_entries: &[],
            library_window_start: 0,
            library_folder: "",
            library_total: 0,
            chapters: &[],
            chapters_window_start: 0,
            chapters_total: 0,
            sync_status: status,
            wifi_ssid: "",
            library_menu: app_core::LibraryMenu::None,
            library_move_pending: false,
        };
        for rendered in [format!("{status:?}"), format!("{shell:?}")] {
            assert!(
                !rendered.contains(psk.as_str()),
                "debug output leaks the live PSK: {rendered}"
            );
        }
    }
}
