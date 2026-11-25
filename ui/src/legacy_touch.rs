//! Temporary Milestone 1 touch routing adapter for the existing 7-button shell.
//!
//! Keeps `tools/emulator` and `tools/web-emulator` able to exercise touch input
//! against the current layout without depending on `hal-ext`. Replaced by
//! renderer-published hit maps in #402.

use crate::app_render::{library_sheet_tap, list_row_tap, ui_orientation};
use crate::render::{hit_test_chapter_chevron, MIRRORED_RAIL_X};
use app_core::touch::{GestureEvent, SwipeDirection, TouchPoint};
use app_core::{
    is_portrait, physical_key_for, AppView, Button, DisplayOrientation, FrontButtons, LibraryMenu,
    RenderRequest,
};
use display::fb::FbFrame;
use display::{HEIGHT, WIDTH};

/// Snapshot of the displayed frame's legacy layout parameters captured at
/// touch-down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegacyTouchContext {
    pub view: AppView,
    pub orientation: DisplayOrientation,
    pub front_buttons: FrontButtons,
    pub reading_sheet: bool,
    pub frontlight_available: bool,
    pub library_modal: bool,
    pub chapter_levels: [u8; 16],
}

impl LegacyTouchContext {
    pub const fn from_request(request: RenderRequest, chapter_levels: [u8; 16]) -> Self {
        Self {
            view: request.view,
            orientation: request.orientation,
            front_buttons: request.front_buttons,
            reading_sheet: request.reading_sheet,
            frontlight_available: request.frontlight_available,
            library_modal: !matches!(request.library_menu, LibraryMenu::None),
            chapter_levels,
        }
    }
}

/// Legacy reducer input produced by routing a [`GestureEvent`] against the
/// current 7-button shell layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegacyTouchOutcome {
    Button(Button),
    ListRow { row: u8, toggle_expand: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LegacyThirdAction {
    Prev,
    Next,
    Confirm,
}

const RAIL_WIDTH: u16 = 180;
const PORTRAIT_RAIL_HEIGHT: u16 = 76;

const fn rail_midpoint(x4_y: i16) -> u16 {
    ((x4_y as i32 * HEIGHT as i32) / 480) as u16
}

const fn classify_rail_slot(coord: u16) -> u8 {
    if coord < rail_midpoint(160) {
        0
    } else if coord < rail_midpoint(240) {
        1
    } else if coord < rail_midpoint(320) {
        2
    } else {
        3
    }
}

const fn classify_thirds(x: u16, min_x: u16, max_x: u16) -> LegacyThirdAction {
    let span = max_x.saturating_sub(min_x) as u32;
    let rel = x.saturating_sub(min_x) as u32;
    if rel < span / 3 {
        LegacyThirdAction::Prev
    } else if rel < (span * 2) / 3 {
        LegacyThirdAction::Confirm
    } else {
        LegacyThirdAction::Next
    }
}

fn dash_slot_button(pos: u8, ctx: LegacyTouchContext) -> Option<Button> {
    let pages_left = ctx.view != AppView::Home && ctx.front_buttons == FrontButtons::PagesLeft;
    let slot = if pages_left { (pos + 2) % 4 } else { pos };
    let logical_button = match slot {
        0 => Button::Back,
        1 => Button::Confirm,
        2 => Button::Previous,
        _ => Button::Next,
    };
    physical_key_for(ctx.view, ctx.orientation, ctx.front_buttons, logical_button)
}

fn dash_slot_at(point: TouchPoint, ctx: LegacyTouchContext) -> Option<u8> {
    let portrait = is_portrait(ctx.orientation);
    let reading = ctx.view == AppView::Reading;
    if portrait {
        if !reading || ctx.reading_sheet {
            let rail_height = if reading {
                crate::reading::PORTRAIT_READING_SHEET_HEIGHT as u16
            } else {
                PORTRAIT_RAIL_HEIGHT
            };
            let rail_top = (FbFrame::Portrait.height() as u16).saturating_sub(rail_height);
            if point.y >= rail_top {
                return Some(classify_rail_slot(point.x));
            }
        }
        None
    } else if reading {
        None
    } else {
        match ctx.orientation {
            DisplayOrientation::LandscapeButtonsBottom if point.x < RAIL_WIDTH => {
                Some(classify_rail_slot(point.y))
            }
            DisplayOrientation::LandscapeButtonsTop if point.x >= MIRRORED_RAIL_X => {
                Some(classify_rail_slot(point.y))
            }
            _ => None,
        }
    }
}

/// Routes a recognized [`GestureEvent`] against `ctx` using the legacy
/// 7-button shell geometry. Returns `None` for non-legacy gestures (`LongPress`,
/// `EdgeSlide*`, `Canceled`) and taps on inactive whitespace.
pub fn route_gesture(event: GestureEvent, ctx: LegacyTouchContext) -> Option<LegacyTouchOutcome> {
    match event {
        GestureEvent::Swipe { direction, .. } => {
            let logical = match direction {
                SwipeDirection::Left => Button::Next,
                SwipeDirection::Right => Button::Previous,
                SwipeDirection::Up | SwipeDirection::Down => Button::Back,
            };
            physical_key_for(ctx.view, ctx.orientation, ctx.front_buttons, logical)
                .map(LegacyTouchOutcome::Button)
        }
        GestureEvent::Tap { start, .. } => {
            if let Some(slot) = dash_slot_at(start, ctx) {
                return dash_slot_button(slot, ctx).map(LegacyTouchOutcome::Button);
            }

            if ctx.view == AppView::Library && ctx.library_modal {
                return Some(match library_sheet_tap(ctx.orientation, start.x, start.y) {
                    Some(action_row) => LegacyTouchOutcome::ListRow {
                        row: action_row,
                        toggle_expand: true,
                    },
                    None => LegacyTouchOutcome::ListRow {
                        row: 0,
                        toggle_expand: false,
                    },
                });
            }

            let is_list_content = matches!(ctx.view, AppView::Chapters | AppView::Settings)
                || (ctx.view == AppView::Library && !ctx.library_modal);
            if is_list_content {
                return list_row_tap(
                    ctx.view,
                    ctx.orientation,
                    ctx.frontlight_available,
                    ctx.library_modal,
                    start.x,
                    start.y,
                )
                .map(|target| LegacyTouchOutcome::ListRow {
                    row: target.row,
                    toggle_expand: if ctx.view == AppView::Chapters {
                        let level = ctx
                            .chapter_levels
                            .get(usize::from(target.row))
                            .copied()
                            .unwrap_or(0);
                        hit_test_chapter_chevron(ui_orientation(ctx.orientation), level, start.x)
                    } else {
                        target.toggle_expand
                    },
                });
            }

            if !matches!(ctx.view, AppView::Home | AppView::Reading) {
                return None;
            }

            let (min_x, max_x) = if is_portrait(ctx.orientation) {
                (0, FbFrame::Portrait.width() as u16)
            } else if ctx.view == AppView::Reading {
                (0, WIDTH as u16)
            } else {
                match ctx.orientation {
                    DisplayOrientation::LandscapeButtonsBottom => (RAIL_WIDTH, WIDTH as u16),
                    DisplayOrientation::LandscapeButtonsTop => (0, MIRRORED_RAIL_X),
                    DisplayOrientation::PortraitButtonsLeft
                    | DisplayOrientation::PortraitButtonsRight => {
                        (0, FbFrame::Portrait.width() as u16)
                    }
                }
            };

            let logical = match classify_thirds(start.x, min_x, max_x) {
                LegacyThirdAction::Prev => Button::Previous,
                LegacyThirdAction::Confirm => Button::Confirm,
                LegacyThirdAction::Next => Button::Next,
            };
            physical_key_for(ctx.view, ctx.orientation, ctx.front_buttons, logical)
                .map(LegacyTouchOutcome::Button)
        }
        GestureEvent::LongPress { .. }
        | GestureEvent::EdgeSlideStart { .. }
        | GestureEvent::EdgeSlideUpdate { .. }
        | GestureEvent::EdgeSlideEnd { .. }
        | GestureEvent::Canceled { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_core::touch::{CancelReason, ContactIdentity, EdgeSide, EdgeSlideEndReason};

    fn ctx(view: AppView, orientation: DisplayOrientation) -> LegacyTouchContext {
        LegacyTouchContext {
            view,
            orientation,
            front_buttons: FrontButtons::PagesRight,
            reading_sheet: false,
            frontlight_available: false,
            library_modal: false,
            chapter_levels: [0; 16],
        }
    }

    const CONTACT: ContactIdentity = ContactIdentity {
        id: 1,
        frame_epoch: 1,
    };

    #[test]
    fn portrait_dash_slots_and_swipes_route_through_physical_keys() {
        let home = ctx(AppView::Home, DisplayOrientation::PortraitButtonsLeft);
        // Portrait slot 0 -> Back -> Home maps Back to Library.
        let slot0_y = (FbFrame::Portrait.height() as u16).saturating_sub(24);
        assert_eq!(
            route_gesture(
                GestureEvent::Tap {
                    contact: CONTACT,
                    start: TouchPoint::new(100, slot0_y),
                    end: TouchPoint::new(100, slot0_y),
                },
                home
            ),
            Some(LegacyTouchOutcome::Button(Button::Back))
        );

        // Under PagesLeft in Library, slot 0 draws "previous", so tapping it
        // produces the physical key (`Back`) that `apply_input` maps to
        // logical `Previous`.
        let mut lib_left = ctx(AppView::Library, DisplayOrientation::PortraitButtonsLeft);
        lib_left.front_buttons = FrontButtons::PagesLeft;
        assert_eq!(
            route_gesture(
                GestureEvent::Tap {
                    contact: CONTACT,
                    start: TouchPoint::new(100, slot0_y),
                    end: TouchPoint::new(100, slot0_y),
                },
                lib_left
            ),
            Some(LegacyTouchOutcome::Button(Button::Back))
        );

        // Swipe left -> logical Next -> under LandscapeButtonsTop physical Back
        // (which `orient_button` maps to logical Next).
        let reading_top = ctx(AppView::Reading, DisplayOrientation::LandscapeButtonsTop);
        assert_eq!(
            route_gesture(
                GestureEvent::Swipe {
                    contact: CONTACT,
                    direction: SwipeDirection::Left,
                    start: TouchPoint::new(500, 200),
                    end: TouchPoint::new(400, 200),
                },
                reading_top
            ),
            Some(LegacyTouchOutcome::Button(Button::Back))
        );
    }

    #[test]
    fn list_views_and_reading_thirds_respect_layout() {
        // Library row tap hits row 0, misses header whitespace.
        let lib = ctx(AppView::Library, DisplayOrientation::PortraitButtonsLeft);
        assert_eq!(
            route_gesture(
                GestureEvent::Tap {
                    contact: CONTACT,
                    start: TouchPoint::new(200, 118),
                    end: TouchPoint::new(200, 118),
                },
                lib
            ),
            Some(LegacyTouchOutcome::ListRow {
                row: 0,
                toggle_expand: false
            })
        );
        assert_eq!(
            route_gesture(
                GestureEvent::Tap {
                    contact: CONTACT,
                    start: TouchPoint::new(200, 30),
                    end: TouchPoint::new(200, 30),
                },
                lib
            ),
            None
        );

        // Library modal sheet tap vs outside dismiss.
        let mut lib_modal = lib;
        lib_modal.library_modal = true;
        assert_eq!(
            route_gesture(
                GestureEvent::Tap {
                    contact: CONTACT,
                    start: TouchPoint::new(150, 600),
                    end: TouchPoint::new(150, 600),
                },
                lib_modal
            ),
            Some(LegacyTouchOutcome::ListRow {
                row: 0,
                toggle_expand: true
            })
        );
        assert_eq!(
            route_gesture(
                GestureEvent::Tap {
                    contact: CONTACT,
                    start: TouchPoint::new(150, 150),
                    end: TouchPoint::new(150, 150),
                },
                lib_modal
            ),
            Some(LegacyTouchOutcome::ListRow {
                row: 0,
                toggle_expand: false
            })
        );

        // Chapters disclosure chevron uses captured chapter_levels.
        let mut chapters = ctx(AppView::Chapters, DisplayOrientation::PortraitButtonsLeft);
        chapters.chapter_levels[0] = 1;
        assert_eq!(
            route_gesture(
                GestureEvent::Tap {
                    contact: CONTACT,
                    start: TouchPoint::new(44, 118),
                    end: TouchPoint::new(44, 118),
                },
                chapters
            ),
            Some(LegacyTouchOutcome::ListRow {
                row: 0,
                toggle_expand: true
            })
        );

        // Non-routing gestures return None.
        assert_eq!(
            route_gesture(
                GestureEvent::LongPress {
                    contact: CONTACT,
                    start: TouchPoint::new(200, 118),
                    current: TouchPoint::new(200, 118),
                },
                lib
            ),
            None
        );
        assert_eq!(
            route_gesture(
                GestureEvent::EdgeSlideEnd {
                    contact: CONTACT,
                    side: EdgeSide::Left,
                    start: TouchPoint::new(10, 200),
                    end: TouchPoint::new(10, 150),
                    total_dy: -50,
                    reason: EdgeSlideEndReason::Released,
                },
                lib
            ),
            None
        );
        assert_eq!(
            route_gesture(
                GestureEvent::Canceled {
                    contact: CONTACT,
                    reason: CancelReason::Canceled,
                },
                lib
            ),
            None
        );

        // Portrait reading sheet uses PORTRAIT_READING_SHEET_HEIGHT (48 px):
        // y=730 is exposed page content (Prev), while y=776 is inside the sheet (Back).
        let mut reading_sheet = ctx(AppView::Reading, DisplayOrientation::PortraitButtonsLeft);
        reading_sheet.reading_sheet = true;
        let exposed_y = (FbFrame::Portrait.height() as u16).saturating_sub(70);
        let sheet_y = (FbFrame::Portrait.height() as u16).saturating_sub(24);
        assert_eq!(
            route_gesture(
                GestureEvent::Tap {
                    contact: CONTACT,
                    start: TouchPoint::new(100, exposed_y),
                    end: TouchPoint::new(100, exposed_y),
                },
                reading_sheet
            ),
            Some(LegacyTouchOutcome::Button(Button::Previous))
        );
        assert_eq!(
            route_gesture(
                GestureEvent::Tap {
                    contact: CONTACT,
                    start: TouchPoint::new(100, sheet_y),
                    end: TouchPoint::new(100, sheet_y),
                },
                reading_sheet
            ),
            Some(LegacyTouchOutcome::Button(Button::Back))
        );

        // Wireless does not fall back to thirds outside the dash rail, so a
        // stray tap over the prompt/QR cannot confirm forgetting a network.
        let wireless = ctx(AppView::Wireless, DisplayOrientation::PortraitButtonsLeft);
        assert_eq!(
            route_gesture(
                GestureEvent::Tap {
                    contact: CONTACT,
                    start: TouchPoint::new(240, 450),
                    end: TouchPoint::new(240, 450),
                },
                wireless
            ),
            None
        );
        // Labeled Confirm dash slot on Wireless still routes.
        assert_eq!(
            route_gesture(
                GestureEvent::Tap {
                    contact: CONTACT,
                    start: TouchPoint::new(200, sheet_y),
                    end: TouchPoint::new(200, sheet_y),
                },
                wireless
            ),
            Some(LegacyTouchOutcome::Button(Button::Confirm))
        );

        // Mirrored landscape (especially X3 where WIDTH=792 and 792-180=612)
        // must keep the row 0 Library action gutter (x=615) in row hit-testing
        // while keeping the Back rail label in rail hit-testing.
        let mut mirrored_lib = ctx(AppView::Library, DisplayOrientation::LandscapeButtonsTop);
        mirrored_lib.front_buttons = FrontButtons::PagesRight;
        assert_eq!(
            route_gesture(
                GestureEvent::Tap {
                    contact: CONTACT,
                    start: TouchPoint::new(615, 118),
                    end: TouchPoint::new(615, 118),
                },
                mirrored_lib
            ),
            Some(LegacyTouchOutcome::ListRow {
                row: 0,
                toggle_expand: true,
            })
        );
        let back_y = ((120i32 * HEIGHT as i32) / 480) as u16;
        let expected_back = physical_key_for(
            AppView::Library,
            DisplayOrientation::LandscapeButtonsTop,
            FrontButtons::PagesRight,
            Button::Back,
        )
        .unwrap();
        assert_eq!(
            route_gesture(
                GestureEvent::Tap {
                    contact: CONTACT,
                    start: TouchPoint::new(WIDTH as u16 - 60, back_y),
                    end: TouchPoint::new(WIDTH as u16 - 60, back_y),
                },
                mirrored_lib
            ),
            Some(LegacyTouchOutcome::Button(expected_back))
        );
    }
}
