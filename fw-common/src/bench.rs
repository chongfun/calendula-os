//! Optional board-owned bench observer; absent from normal production builds.

use app_core::{AppView, DisplayOrientation};
use core::cell::Cell;
use critical_section::Mutex;

pub struct Hooks {
    pub note_settled: fn(),
    pub publish_view: fn(AppView, DisplayOrientation, bool, u8, u32, u16),
}

static HOOKS: Mutex<Cell<Option<&'static Hooks>>> = Mutex::new(Cell::new(None));

pub fn install(hooks: &'static Hooks) {
    critical_section::with(|cs| HOOKS.borrow(cs).set(Some(hooks)));
}

pub fn note_settled() {
    let hooks = critical_section::with(|cs| HOOKS.borrow(cs).get());
    if let Some(hooks) = hooks {
        (hooks.note_settled)();
    }
}

pub fn publish_view(
    view: AppView,
    orientation: DisplayOrientation,
    pages_left: bool,
    depth: u8,
    page: u32,
    selection: u16,
) {
    let hooks = critical_section::with(|cs| HOOKS.borrow(cs).get());
    if let Some(hooks) = hooks {
        (hooks.publish_view)(view, orientation, pages_left, depth, page, selection);
    }
}
