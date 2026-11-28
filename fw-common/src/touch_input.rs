//! Host-testable firmware touch polling controller pairing [`TouchRecognizer`]
//! with coherently snapshotted frame metadata and idle-sleep contact vetoes.

use crate::touch_frame::PublishedFrameMeta;
use app_core::touch::{
    elapsed_ms, CancelReason, EdgeSlideEndReason, GestureConfig, GestureEvent, HitMap, TouchPoint,
    TouchRecognizer, UiAction,
};
use app_core::{AppView, DisplayOrientation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TouchPollOutcome {
    pub action: Option<(UiAction, u32)>,
    pub record_physical_activity: bool,
    pub activity_view: Option<AppView>,
}

impl TouchPollOutcome {
    pub const NONE: Self = Self {
        action: None,
        record_physical_activity: false,
        activity_view: None,
    };
}

/// Stateful per-contact touch controller driven by S3 I2C polls and frame fences.
pub struct TouchPollController {
    recognizer: TouchRecognizer,
    active_map: HitMap,
    active_epoch: Option<u32>,
    stale_until_lift: bool,
    contact_veto: bool,
    last_contact_report_ms: Option<u32>,
}

impl TouchPollController {
    pub const fn new() -> Self {
        Self {
            recognizer: TouchRecognizer::new(),
            active_map: HitMap::EMPTY,
            active_epoch: None,
            stale_until_lift: false,
            contact_veto: false,
            last_contact_report_ms: None,
        }
    }

    pub const fn contact_veto(&self) -> bool {
        self.contact_veto
    }

    pub const fn is_stale(&self) -> bool {
        self.stale_until_lift
    }

    pub const fn active_epoch(&self) -> Option<u32> {
        self.active_epoch
    }

    pub const fn active_view(&self) -> AppView {
        self.active_map.view
    }

    fn enter_inactive_hold(&mut self) {
        let _ = self.recognizer.cancel();
        self.active_epoch = None;
        self.stale_until_lift = true;
    }

    fn expire_contact_veto(&mut self, now_ms: u32) {
        if let Some(since) = self.last_contact_report_ms {
            if elapsed_ms(now_ms, since) >= GestureConfig::DEFAULT.max_stationary_hold_ms {
                self.contact_veto = false;
                self.last_contact_report_ms = None;
            }
        }
    }

    /// Processes a GT911 sample (`Some(raw)` while touching, `None` on release).
    pub fn on_sample<S, M>(
        &mut self,
        raw_point: Option<TouchPoint>,
        now_ms: u32,
        pre_meta: PublishedFrameMeta,
        post_meta: PublishedFrameMeta,
        snapshot: S,
        map_raw: M,
    ) -> TouchPollOutcome
    where
        S: FnOnce(&mut HitMap) -> Option<PublishedFrameMeta>,
        M: FnOnce(DisplayOrientation, TouchPoint) -> TouchPoint,
    {
        let Some(raw) = raw_point else {
            self.contact_veto = false;
            self.last_contact_report_ms = None;
            if self.stale_until_lift {
                self.stale_until_lift = false;
                let _ = self.recognizer.up(None, now_ms);
                self.active_epoch = None;
                return TouchPollOutcome::NONE;
            }
            let Some(expected_epoch) = self.active_epoch.take() else {
                let _ = self.recognizer.up(None, now_ms);
                return TouchPollOutcome::NONE;
            };
            if !post_meta.active || post_meta.overflowed || post_meta.epoch != expected_epoch {
                let _ = self.recognizer.up(None, now_ms);
                return TouchPollOutcome::NONE;
            }
            let Some(ev) = self.recognizer.up(None, now_ms) else {
                return TouchPollOutcome::NONE;
            };
            if ev.contact().frame_epoch != post_meta.epoch {
                return TouchPollOutcome::NONE;
            }
            let action = self
                .active_map
                .route_gesture(ev)
                .map(|act| (act, post_meta.epoch));
            return TouchPollOutcome {
                action,
                record_physical_activity: false,
                activity_view: action.map(|_| self.active_map.view),
            };
        };

        // A contact can remain physically held after its gesture is consumed
        // or invalidated. Only actual reports extend its no-report deadline.
        // Once timed out or canceled by Power/error, do not re-arm until lift.
        self.expire_contact_veto(now_ms);
        if self.contact_veto {
            self.last_contact_report_ms = Some(now_ms);
        }
        if self.stale_until_lift {
            return TouchPollOutcome::NONE;
        }

        if let Some(expected_epoch) = self.active_epoch {
            if !post_meta.active || post_meta.overflowed || post_meta.epoch != expected_epoch {
                self.enter_inactive_hold();
                return TouchPollOutcome::NONE;
            }
            let view = self.active_map.view;
            let mapped = map_raw(self.active_map.orientation, raw);
            let ev = self.recognizer.move_to(mapped, now_ms);
            if !self.recognizer.is_active() {
                self.active_epoch = None;
                self.stale_until_lift = true;
                let stationary_timeout = matches!(
                    ev,
                    Some(GestureEvent::Canceled {
                        reason: CancelReason::StationaryTimeout,
                        ..
                    }) | Some(GestureEvent::EdgeSlideEnd {
                        reason: EdgeSlideEndReason::Canceled(CancelReason::StationaryTimeout),
                        ..
                    })
                );
                if stationary_timeout {
                    self.contact_veto = false;
                    self.last_contact_report_ms = None;
                }
            }
            let action = ev
                .filter(|e| e.contact().frame_epoch == expected_epoch)
                .and_then(|e| self.active_map.route_gesture(e))
                .map(|act| (act, expected_epoch));
            return TouchPollOutcome {
                action,
                record_physical_activity: false,
                activity_view: action.map(|_| view),
            };
        }

        // Fresh touch-down on glass.
        self.contact_veto = true;
        self.last_contact_report_ms = Some(now_ms);
        if !pre_meta.active
            || !post_meta.active
            || pre_meta.epoch != post_meta.epoch
            || post_meta.overflowed
        {
            self.enter_inactive_hold();
            return TouchPollOutcome {
                action: None,
                record_physical_activity: true,
                activity_view: None,
            };
        }

        let Some(snap_meta) = snapshot(&mut self.active_map) else {
            self.enter_inactive_hold();
            return TouchPollOutcome {
                action: None,
                record_physical_activity: true,
                activity_view: None,
            };
        };
        if !snap_meta.active || snap_meta.overflowed || snap_meta.epoch != post_meta.epoch {
            self.enter_inactive_hold();
            return TouchPollOutcome {
                action: None,
                record_physical_activity: true,
                activity_view: None,
            };
        }

        let mapped = map_raw(self.active_map.orientation, raw);
        let hold_bounds = self.active_map.hold_target_at(mapped);
        let ev = self.recognizer.down(
            mapped,
            now_ms,
            snap_meta.epoch,
            self.active_map.frame_width,
            hold_bounds,
        );
        if self.recognizer.is_active() {
            self.active_epoch = Some(snap_meta.epoch);
        } else {
            self.enter_inactive_hold();
        }
        let action = ev
            .filter(|e| e.contact().frame_epoch == snap_meta.epoch)
            .and_then(|e| self.active_map.route_gesture(e))
            .map(|act| (act, snap_meta.epoch));
        TouchPollOutcome {
            action,
            record_physical_activity: true,
            activity_view: self.recognizer.is_active().then_some(self.active_map.view),
        }
    }

    /// Processes a poll iteration where GT911 reported no new packet (`Ok(None)`).
    ///
    /// Never refreshes `last_report_ms`, preserving stationary hold and 8s timeout.
    pub fn on_no_report(
        &mut self,
        now_ms: u32,
        pre_meta: PublishedFrameMeta,
        post_meta: PublishedFrameMeta,
    ) -> Option<(UiAction, u32)> {
        self.expire_contact_veto(now_ms);
        if let Some(expected_epoch) = self.active_epoch {
            if !pre_meta.active
                || !post_meta.active
                || pre_meta.epoch != post_meta.epoch
                || post_meta.overflowed
                || post_meta.epoch != expected_epoch
            {
                self.enter_inactive_hold();
                return None;
            }
            if let Some(ev) = self.recognizer.tick(now_ms) {
                match ev {
                    GestureEvent::Canceled {
                        reason: CancelReason::StationaryTimeout,
                        ..
                    }
                    | GestureEvent::EdgeSlideEnd {
                        reason: EdgeSlideEndReason::Canceled(CancelReason::StationaryTimeout),
                        ..
                    } => {
                        self.contact_veto = false;
                        self.last_contact_report_ms = None;
                        self.active_epoch = None;
                        self.stale_until_lift = true;
                        return self
                            .active_map
                            .route_gesture(ev)
                            .map(|act| (act, expected_epoch));
                    }
                    GestureEvent::LongPress { .. } => {
                        self.active_epoch = None;
                        self.stale_until_lift = true;
                        return self
                            .active_map
                            .route_gesture(ev)
                            .map(|act| (act, expected_epoch));
                    }
                    _ => return None,
                }
            }
            return None;
        }

        None
    }

    pub fn on_transport_error(&mut self) {
        let had_contact = self.active_epoch.take().is_some()
            || self.stale_until_lift
            || self.recognizer.is_active()
            || self.recognizer.is_suppressed();
        if had_contact {
            let _ = self.recognizer.on_error();
            self.stale_until_lift = true;
        }
        self.contact_veto = false;
        self.last_contact_report_ms = None;
    }

    /// Cancels an active or pending gesture on physical button press.
    ///
    /// Pass `clear_veto = true` for `Button::Power` so explicit sleep is never
    /// vetoed while suppressing that same finger until lift.
    pub fn cancel_for_physical_input(&mut self, clear_veto: bool, now_ms: u32) {
        self.expire_contact_veto(now_ms);
        let was_active = self.active_epoch.take().is_some() || self.recognizer.is_active();
        let _ = self.recognizer.cancel();
        if clear_veto {
            if self.contact_veto || was_active {
                self.stale_until_lift = true;
            }
            self.contact_veto = false;
            self.last_contact_report_ms = None;
        } else if self.contact_veto || was_active {
            self.stale_until_lift = true;
        }
    }
}

impl Default for TouchPollController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::touch_frame::TouchFrameStore;
    use app_core::touch::{HitTarget, Rect, TouchModal};
    use app_core::{AppView, Button, DisplayOrientation};

    fn sample_map(orientation: DisplayOrientation, hold: bool) -> HitMap {
        let mut map = HitMap::EMPTY;
        map.reset(orientation, AppView::Reading, TouchModal::None);
        let target = if hold {
            HitTarget {
                bounds: Rect::new(0, 0, 200, 200),
                tap: Some(UiAction::LegacyButton(Button::Previous)),
                long_press: Some(UiAction::LegacyButton(Button::Back)),
            }
        } else {
            HitTarget::tap_only(
                Rect::new(0, 0, 200, 200),
                UiAction::LegacyButton(Button::Previous),
            )
        };
        map.push(target);
        map
    }

    fn identity_map(_ori: DisplayOrientation, raw: TouchPoint) -> TouchPoint {
        raw
    }

    #[test]
    fn active_down_no_report_long_press_and_consumed_lift() {
        let mut store = TouchFrameStore::new();
        store.publish(&sample_map(DisplayOrientation::PortraitButtonsLeft, true));
        let meta = store.meta();
        let mut ctrl = TouchPollController::new();

        let out = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            100,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(
            out,
            TouchPollOutcome {
                action: None,
                record_physical_activity: true,
                activity_view: Some(AppView::Reading),
            }
        );
        assert!(ctrl.contact_veto());

        // Stationary GT911 Ok(None) at 450ms triggers LongPress without extending clock.
        assert_eq!(ctrl.on_no_report(549, meta, meta), None);
        assert_eq!(
            ctrl.on_no_report(550, meta, meta),
            Some((UiAction::LegacyButton(Button::Back), meta.epoch))
        );
        assert!(ctrl.contact_veto());
        assert!(ctrl.is_stale());

        // Lift consumes release cleanly and clears veto.
        let up = ctrl.on_sample(
            None,
            600,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(up, TouchPollOutcome::NONE);
        assert!(!ctrl.contact_veto());
        assert!(!ctrl.is_stale());

        // Clean second tap works immediately.
        store.publish(&sample_map(DisplayOrientation::PortraitButtonsLeft, false));
        let meta2 = store.meta();
        let _ = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            700,
            meta2,
            meta2,
            |dst| store.snapshot(dst),
            identity_map,
        );
        let up2 = ctrl.on_sample(
            None,
            740,
            meta2,
            meta2,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(
            up2,
            TouchPollOutcome {
                action: Some((UiAction::LegacyButton(Button::Previous), meta2.epoch)),
                record_physical_activity: false,
                activity_view: Some(AppView::Reading),
            }
        );
    }

    #[test]
    fn orientation_is_locked_at_down_and_stationary_timeout_clears_veto() {
        let mut store = TouchFrameStore::new();
        store.publish(&sample_map(
            DisplayOrientation::LandscapeButtonsBottom,
            false,
        ));
        let meta = store.meta();
        let mut ctrl = TouchPollController::new();

        let _ = ctrl.on_sample(
            Some(TouchPoint::new(20, 20)),
            1_000,
            meta,
            meta,
            |dst| store.snapshot(dst),
            |ori, raw| {
                assert_eq!(ori, DisplayOrientation::LandscapeButtonsBottom);
                raw
            },
        );
        let _ = ctrl.on_sample(
            Some(TouchPoint::new(22, 20)),
            1_100,
            meta,
            meta,
            |dst| store.snapshot(dst),
            |ori, raw| {
                assert_eq!(ori, DisplayOrientation::LandscapeButtonsBottom);
                raw
            },
        );

        // 8s without fresh GT911 report clears contact_veto while suppressing release until lift.
        assert_eq!(ctrl.on_no_report(9_099, meta, meta), None);
        assert!(ctrl.contact_veto());
        assert_eq!(ctrl.on_no_report(9_100, meta, meta), None);
        assert!(!ctrl.contact_veto());
        assert!(ctrl.is_stale());

        let up = ctrl.on_sample(
            None,
            9_500,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(up, TouchPollOutcome::NONE);
        assert!(!ctrl.is_stale());
    }

    #[test]
    fn inactive_frame_and_mid_contact_frame_change_veto_and_suppress() {
        let mut store = TouchFrameStore::new();
        let mut ctrl = TouchPollController::new();
        let inactive = store.meta();

        // Down on inactive frame sets veto and physical activity without dispatching touch.
        let down = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            100,
            inactive,
            inactive,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(
            down,
            TouchPollOutcome {
                action: None,
                record_physical_activity: true,
                activity_view: None,
            }
        );
        assert!(ctrl.contact_veto());
        assert!(ctrl.is_stale());

        // Still held while frame becomes active: remains suppressed until lift.
        store.publish(&sample_map(DisplayOrientation::PortraitButtonsLeft, false));
        let active = store.meta();
        let mv = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            120,
            active,
            active,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(mv, TouchPollOutcome::NONE);
        assert!(ctrl.contact_veto());
        let up = ctrl.on_sample(
            None,
            140,
            active,
            active,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(up, TouchPollOutcome::NONE);
        assert!(!ctrl.contact_veto());
        assert!(!ctrl.is_stale());

        // Frame invalidates mid-contact: cancels gesture, keeps veto until 8s or lift, never dispatches.
        let _ = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            200,
            active,
            active,
            |dst| store.snapshot(dst),
            identity_map,
        );
        store.invalidate();
        let inv = store.meta();
        assert_eq!(ctrl.on_no_report(220, inv, inv), None);
        assert!(ctrl.contact_veto());
        assert!(ctrl.is_stale());
        assert_eq!(
            ctrl.on_sample(None, 240, inv, inv, |dst| store.snapshot(dst), identity_map),
            TouchPollOutcome::NONE
        );
        assert!(!ctrl.contact_veto());
    }

    #[test]
    fn pr416_touch_down_rejects_frames_changed_during_poll_or_snapshot() {
        let mut store = TouchFrameStore::new();
        store.publish(&sample_map(DisplayOrientation::PortraitButtonsLeft, false));
        let old = store.meta();
        store.publish(&sample_map(DisplayOrientation::LandscapeButtonsTop, false));
        let polled = store.meta();
        store.publish(&sample_map(DisplayOrientation::PortraitButtonsRight, false));
        let settled = store.meta();
        let point = TouchPoint::new(50, 50);

        for during_poll in [true, false] {
            let mut ctrl = TouchPollController::new();
            let pre = if during_poll { old } else { polled };
            let down = ctrl.on_sample(
                Some(point),
                100,
                pre,
                polled,
                |dst| {
                    assert!(!during_poll, "a changed poll epoch must skip the snapshot");
                    store.snapshot(dst)
                },
                |_, _| panic!("a mismatched frame must not interpret the coordinates"),
            );
            assert_eq!(
                down,
                TouchPollOutcome {
                    action: None,
                    record_physical_activity: true,
                    activity_view: None,
                }
            );
            assert!(ctrl.contact_veto());
            assert_eq!(ctrl.active_epoch(), None);

            let mut sample = |point, time| {
                ctrl.on_sample(
                    point,
                    time,
                    settled,
                    settled,
                    |dst| store.snapshot(dst),
                    identity_map,
                )
            };
            assert_eq!(sample(Some(point), 120), TouchPollOutcome::NONE);
            assert_eq!(sample(None, 140), TouchPollOutcome::NONE);
            assert!(sample(Some(point), 200).record_physical_activity);
            assert_eq!(
                sample(None, 240).action,
                Some((UiAction::LegacyButton(Button::Previous), settled.epoch))
            );
            assert!(!ctrl.contact_veto());
        }
    }

    #[test]
    fn pr416_idle_veto_timeout_wraps_without_rearming_the_same_finger() {
        let store = TouchFrameStore::new();
        let inactive = store.meta();
        let mut ctrl = TouchPollController::new();
        let point = Some(TouchPoint::new(50, 50));
        let sample = |ctrl: &mut TouchPollController, point, time| {
            ctrl.on_sample(
                point,
                time,
                inactive,
                inactive,
                |dst| store.snapshot(dst),
                identity_map,
            )
        };

        assert!(sample(&mut ctrl, point, u32::MAX - 4_000).record_physical_activity);
        assert_eq!(
            sample(&mut ctrl, point, u32::MAX - 1_000),
            TouchPollOutcome::NONE
        );
        assert_eq!(ctrl.on_no_report(6_998, inactive, inactive), None);
        assert!(ctrl.contact_veto());
        assert_eq!(ctrl.on_no_report(6_999, inactive, inactive), None);
        assert!(!ctrl.contact_veto());

        assert_eq!(sample(&mut ctrl, point, 7_020), TouchPollOutcome::NONE);
        assert!(!ctrl.contact_veto());
        assert_eq!(sample(&mut ctrl, None, 7_040), TouchPollOutcome::NONE);
        assert!(sample(&mut ctrl, point, 7_060).record_physical_activity);
        assert!(ctrl.contact_veto());
    }

    #[test]
    fn power_and_nav_buttons_and_error_cancel_cleanly() {
        let mut store = TouchFrameStore::new();
        store.publish(&sample_map(DisplayOrientation::PortraitButtonsLeft, false));
        let meta = store.meta();
        let mut ctrl = TouchPollController::new();

        let _ = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            100,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        // Physical nav button cancels touch gesture, keeps veto up to 8s.
        ctrl.cancel_for_physical_input(false, 120);
        assert!(ctrl.contact_veto());
        assert!(ctrl.is_stale());
        assert_eq!(
            ctrl.on_sample(
                None,
                140,
                meta,
                meta,
                |dst| store.snapshot(dst),
                identity_map
            ),
            TouchPollOutcome::NONE
        );
        assert!(!ctrl.contact_veto());

        // Physical Power clears veto immediately and suppresses same finger until lift.
        let _ = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            200,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        ctrl.cancel_for_physical_input(true, 220);
        assert!(!ctrl.contact_veto());
        assert!(ctrl.is_stale());
        assert_eq!(
            ctrl.on_sample(
                None,
                240,
                meta,
                meta,
                |dst| store.snapshot(dst),
                identity_map
            ),
            TouchPollOutcome::NONE
        );

        // Transport error clears veto and suppresses until lift.
        let _ = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            300,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        ctrl.on_transport_error();
        assert!(!ctrl.contact_veto());
        assert!(ctrl.is_stale());
    }

    #[test]
    fn move_to_deactivation_clears_stationary_veto_and_expires_consumed_veto() {
        let mut store = TouchFrameStore::new();
        store.publish(&sample_map(DisplayOrientation::PortraitButtonsLeft, true));
        let meta = store.meta();
        let mut ctrl = TouchPollController::new();

        // StationaryTimeout triggered on a move_to sample clears veto immediately.
        let _ = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            100,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        let timed_out = ctrl.on_sample(
            Some(TouchPoint::new(51, 50)),
            8_100,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(timed_out, TouchPollOutcome::NONE);
        assert!(!ctrl.contact_veto());
        assert!(ctrl.is_stale());

        let _ = ctrl.on_sample(
            None,
            8_120,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert!(!ctrl.is_stale());

        // LongPress resolved via move_to enters stale_until_lift with an 8s expiry deadline.
        let _ = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            10_000,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        let held = ctrl.on_sample(
            Some(TouchPoint::new(51, 50)),
            10_450,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(
            held,
            TouchPollOutcome {
                action: Some((UiAction::LegacyButton(Button::Back), meta.epoch)),
                record_physical_activity: false,
                activity_view: Some(AppView::Reading),
            }
        );
        assert!(ctrl.contact_veto());
        assert!(ctrl.is_stale());

        // Reports keep the physical veto alive even after a consumed action
        // changes the frame. Only eight seconds without reports expires it.
        store.invalidate();
        let invalid = store.meta();
        for now in [15_000, 19_000, 23_000] {
            assert_eq!(
                ctrl.on_sample(
                    Some(TouchPoint::new(51, 50)),
                    now,
                    invalid,
                    invalid,
                    |dst| store.snapshot(dst),
                    identity_map,
                ),
                TouchPollOutcome::NONE
            );
            assert!(ctrl.contact_veto());
        }
        assert_eq!(ctrl.on_no_report(30_999, invalid, invalid), None);
        assert!(ctrl.contact_veto());
        assert_eq!(ctrl.on_no_report(31_000, invalid, invalid), None);
        assert!(!ctrl.contact_veto());
    }

    #[test]
    fn activity_view_tracks_accepted_frame_and_defaults_none_when_inactive() {
        let mut store = TouchFrameStore::new();
        let mut map = sample_map(DisplayOrientation::PortraitButtonsLeft, false);
        map.view = AppView::Library;
        store.publish(&map);
        let meta = store.meta();
        let mut ctrl = TouchPollController::new();

        let down = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            100,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(down.activity_view, Some(AppView::Library));

        let up = ctrl.on_sample(
            None,
            130,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(up.activity_view, Some(AppView::Library));
        assert_eq!(ctrl.active_view(), AppView::Library);

        // Pre/post epoch skew rejects touch-down with activity_view = None so caller uses post-poll view.
        let mut map2 = sample_map(DisplayOrientation::PortraitButtonsLeft, false);
        map2.view = AppView::Settings;
        store.publish(&map2);
        let post_meta = store.meta();
        let skewed = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            200,
            meta,
            post_meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(
            skewed,
            TouchPollOutcome {
                action: None,
                record_physical_activity: true,
                activity_view: None,
            }
        );
    }

    #[test]
    fn epoch_change_on_release_discards_tap_and_rearms_for_next_tap() {
        let mut store = TouchFrameStore::new();
        store.publish(&sample_map(DisplayOrientation::PortraitButtonsLeft, false));
        let meta1 = store.meta();
        let mut ctrl = TouchPollController::new();

        let _ = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            100,
            meta1,
            meta1,
            |dst| store.snapshot(dst),
            identity_map,
        );

        // Publish a new frame epoch before the release poll runs.
        store.publish(&sample_map(DisplayOrientation::PortraitButtonsLeft, false));
        let meta2 = store.meta();
        let up = ctrl.on_sample(
            None,
            130,
            meta2,
            meta2,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(up, TouchPollOutcome::NONE);
        assert!(!ctrl.is_stale());
        assert!(!ctrl.contact_veto());

        // Very next tap against the settled frame must succeed immediately.
        let _ = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            200,
            meta2,
            meta2,
            |dst| store.snapshot(dst),
            identity_map,
        );
        let up2 = ctrl.on_sample(
            None,
            230,
            meta2,
            meta2,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(
            up2,
            TouchPollOutcome {
                action: Some((UiAction::LegacyButton(Button::Previous), meta2.epoch)),
                record_physical_activity: false,
                activity_view: Some(AppView::Reading),
            }
        );
    }

    #[test]
    fn transport_error_while_idle_allows_retry_while_active_suppresses_until_lift() {
        let mut store = TouchFrameStore::new();
        store.publish(&sample_map(DisplayOrientation::PortraitButtonsLeft, false));
        let meta = store.meta();
        let mut ctrl = TouchPollController::new();

        // Idle GT911 error must not poison a pending retry packet for a fresh touch-down.
        ctrl.on_transport_error();
        assert!(!ctrl.is_stale());
        assert!(!ctrl.contact_veto());

        let down = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            100,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(down.activity_view, Some(AppView::Reading));
        let up = ctrl.on_sample(
            None,
            130,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        assert_eq!(
            up.action,
            Some((UiAction::LegacyButton(Button::Previous), meta.epoch))
        );

        // Active contact I2C error must suppress the rest of the contact until lift.
        let _ = ctrl.on_sample(
            Some(TouchPoint::new(50, 50)),
            200,
            meta,
            meta,
            |dst| store.snapshot(dst),
            identity_map,
        );
        ctrl.on_transport_error();
        assert!(!ctrl.contact_veto());
        assert!(ctrl.is_stale());
        assert_eq!(
            ctrl.on_sample(
                Some(TouchPoint::new(50, 50)),
                220,
                meta,
                meta,
                |dst| store.snapshot(dst),
                identity_map,
            ),
            TouchPollOutcome::NONE
        );
        assert_eq!(
            ctrl.on_sample(
                None,
                250,
                meta,
                meta,
                |dst| store.snapshot(dst),
                identity_map,
            ),
            TouchPollOutcome::NONE
        );
        assert!(!ctrl.is_stale());
    }
}
