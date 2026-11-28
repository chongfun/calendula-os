//! Coherent frame-fenced touch [`HitMap`] publication and idle-sleep contact veto.

use app_core::touch::HitMap;
use app_core::SleepTrigger;
use core::cell::RefCell;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;

/// Lightweight published frame status safe to inspect without copying [`HitMap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublishedFrameMeta {
    pub epoch: u32,
    pub active: bool,
    pub overflowed: bool,
}

impl PublishedFrameMeta {
    pub const INACTIVE: Self = Self {
        epoch: 0,
        active: false,
        overflowed: false,
    };
}

/// Single critical-section store pairing frame generation, active visibility, and hit metadata.
pub struct TouchFrameStore {
    epoch: u32,
    active: bool,
    map: HitMap,
}

impl TouchFrameStore {
    pub const fn new() -> Self {
        Self {
            epoch: 0,
            active: false,
            map: HitMap::EMPTY,
        }
    }

    pub fn invalidate(&mut self) {
        self.active = false;
        self.epoch = self.epoch.wrapping_add(1);
        self.map = HitMap::EMPTY;
    }

    pub fn publish(&mut self, map: &HitMap) {
        self.epoch = self.epoch.wrapping_add(1);
        self.map = *map;
        self.active = !map.overflowed();
    }

    pub const fn meta(&self) -> PublishedFrameMeta {
        PublishedFrameMeta {
            epoch: self.epoch,
            active: self.active,
            overflowed: self.map.overflowed(),
        }
    }

    pub fn snapshot(&self, out: &mut HitMap) -> Option<PublishedFrameMeta> {
        if self.active && !self.map.overflowed() {
            *out = self.map;
            Some(self.meta())
        } else {
            None
        }
    }
}

impl Default for TouchFrameStore {
    fn default() -> Self {
        Self::new()
    }
}

static PUBLISHED_TOUCH_FRAME: Mutex<CriticalSectionRawMutex, RefCell<TouchFrameStore>> =
    Mutex::new(RefCell::new(TouchFrameStore::new()));

static TOUCH_CONTACT_VETO: portable_atomic::AtomicBool = portable_atomic::AtomicBool::new(false);

pub fn invalidate_touch_frame() {
    PUBLISHED_TOUCH_FRAME.lock(|cell| {
        cell.borrow_mut().invalidate();
    });
}

pub fn publish_touch_frame(map: &HitMap) {
    PUBLISHED_TOUCH_FRAME.lock(|cell| {
        cell.borrow_mut().publish(map);
    });
}

pub fn published_touch_frame_meta() -> PublishedFrameMeta {
    PUBLISHED_TOUCH_FRAME.lock(|cell| cell.borrow().meta())
}

pub fn snapshot_touch_frame(out: &mut HitMap) -> Option<PublishedFrameMeta> {
    PUBLISHED_TOUCH_FRAME.lock(|cell| cell.borrow().snapshot(out))
}

pub fn set_touch_contact_veto(active: bool) {
    TOUCH_CONTACT_VETO.store(active, portable_atomic::Ordering::Release);
}

pub fn touch_contact_veto_active() -> bool {
    TOUCH_CONTACT_VETO.load(portable_atomic::Ordering::Acquire)
}

pub fn sleep_vetoed_by_touch(trigger: SleepTrigger) -> bool {
    trigger == SleepTrigger::Idle && touch_contact_veto_active()
}

#[cfg(test)]
mod tests {
    use super::*;
    use app_core::touch::{HitTarget, Rect, SwipePolicy, TouchModal, TouchPoint, UiAction};
    use app_core::{AppView, Button, DisplayOrientation};

    #[test]
    fn store_publish_invalidate_and_overflow_are_coherent() {
        let mut store = TouchFrameStore::new();
        assert_eq!(store.meta(), PublishedFrameMeta::INACTIVE);
        let mut out = HitMap::EMPTY;
        assert_eq!(store.snapshot(&mut out), None);

        let mut map = HitMap::EMPTY;
        map.reset(
            DisplayOrientation::PortraitButtonsLeft,
            AppView::Home,
            TouchModal::None,
        );
        map.push(HitTarget::tap_only(
            Rect::new(0, 750, 120, 48),
            UiAction::LegacyButton(Button::Back),
        ));
        store.publish(&map);
        let meta = store.meta();
        assert!(meta.active);
        assert!(!meta.overflowed);
        assert_eq!(meta.epoch, 1);
        assert_eq!(store.snapshot(&mut out), Some(meta));
        assert!(matches!(
            out.hit_test(TouchPoint::new(50, 770)),
            app_core::touch::HitResult::Target(t) if t.tap == Some(UiAction::LegacyButton(Button::Back))
        ));
        assert!(matches!(
            store.snapshot(&mut out),
            Some(m) if m.epoch == 1
        ));

        store.invalidate();
        assert_eq!(
            store.meta(),
            PublishedFrameMeta {
                epoch: 2,
                active: false,
                overflowed: false,
            }
        );
        assert_eq!(store.snapshot(&mut out), None);

        // Overflowed HitMap must stay inactive even if published.
        let mut overflow_map = HitMap::EMPTY;
        overflow_map.reset(
            DisplayOrientation::LandscapeButtonsBottom,
            AppView::Chapters,
            TouchModal::None,
        );
        overflow_map.set_swipe(SwipePolicy::LEGACY_NAV);
        for idx in 0..=app_core::touch::MAX_HIT_TARGETS {
            overflow_map.push(HitTarget::tap_only(
                Rect::new(idx as u16, 0, 1, 10),
                UiAction::DismissLibraryModal,
            ));
        }
        assert!(overflow_map.overflowed());
        store.publish(&overflow_map);
        assert_eq!(
            store.meta(),
            PublishedFrameMeta {
                epoch: 3,
                active: false,
                overflowed: true,
            }
        );
        assert_eq!(store.snapshot(&mut out), None);
    }

    #[test]
    fn contact_veto_gates_idle_sleep_only() {
        set_touch_contact_veto(true);
        assert!(sleep_vetoed_by_touch(SleepTrigger::Idle));
        assert!(!sleep_vetoed_by_touch(SleepTrigger::Explicit));
        set_touch_contact_veto(false);
        assert!(!sleep_vetoed_by_touch(SleepTrigger::Idle));
    }
}
