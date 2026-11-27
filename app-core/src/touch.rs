//! Pure single-contact touch gesture recognition shared by firmware and emulators.
//!
//! Recognizes taps, stationary long-presses, directional swipes, and optional
//! vertical outer-edge slides using explicit `u32` monotonic millisecond
//! timestamps with wrap-safe elapsed arithmetic.

use display::Rect;

/// Logical display coordinates of a touch contact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TouchPoint {
    pub x: u16,
    pub y: u16,
}

impl TouchPoint {
    pub const fn new(x: u16, y: u16) -> Self {
        Self { x, y }
    }
}

/// Thresholds and capabilities controlling gesture classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GestureConfig {
    /// Maximum drift along either axis from touch-down to remain eligible as a
    /// tap or hold.
    pub tap_drift_max_px: u16,
    /// Minimum displacement along either axis to classify release as a swipe.
    pub swipe_min_px: u16,
    /// Width in logical pixels of the left/right outer strips eligible for
    /// vertical edge slides.
    pub edge_strip_px: u16,
    /// Minimum vertical displacement to claim an outer-edge slide.
    pub edge_slide_min_px: u16,
    /// Strict vertical dominance ratio required to claim an edge slide
    /// (`|dy| > edge_slide_vertical_ratio * |dx|`).
    pub edge_slide_vertical_ratio: u16,
    /// Duration (ms) a stationary contact inside `target_bounds` must stay
    /// held to emit [`GestureEvent::LongPress`]. Must be `<= MAX_TIMING_THRESHOLD_MS`.
    pub long_press_ms: u32,
    /// Maximum stationary duration (ms) without fresh reports before a contact
    /// times out and cancels. Must be `<= MAX_TIMING_THRESHOLD_MS`.
    pub max_stationary_hold_ms: u32,
    /// Whether outer-edge vertical slides are enabled on the current surface.
    pub edge_slide_enabled: bool,
}

/// Maximum valid timing threshold (`< 2^31` ms) reachable by [`elapsed_ms`].
pub const MAX_TIMING_THRESHOLD_MS: u32 = 0x7FFF_FFFF;

/// Validation error returned when installing an invalid [`GestureConfig`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GestureConfigError {
    /// `long_press_ms` exceeds [`MAX_TIMING_THRESHOLD_MS`].
    LongPressOutOfRange,
    /// `max_stationary_hold_ms` exceeds [`MAX_TIMING_THRESHOLD_MS`].
    StationaryHoldOutOfRange,
}

impl GestureConfig {
    pub const DEFAULT: Self = Self {
        tap_drift_max_px: 16,
        swipe_min_px: 60,
        edge_strip_px: 56,
        edge_slide_min_px: 24,
        edge_slide_vertical_ratio: 2,
        long_press_ms: 450,
        max_stationary_hold_ms: 8_000,
        edge_slide_enabled: false,
    };

    /// Validates that timing thresholds stay within `<= MAX_TIMING_THRESHOLD_MS`
    /// (`< 2^31` ms) so [`elapsed_ms`] can reach them across clock wraparound.
    pub const fn validate(&self) -> Result<(), GestureConfigError> {
        if self.long_press_ms > MAX_TIMING_THRESHOLD_MS {
            return Err(GestureConfigError::LongPressOutOfRange);
        }
        if self.max_stationary_hold_ms > MAX_TIMING_THRESHOLD_MS {
            return Err(GestureConfigError::StationaryHoldOutOfRange);
        }
        Ok(())
    }
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Cardinal direction of a recognized swipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwipeDirection {
    Left,
    Right,
    Up,
    Down,
}

/// Outer vertical edge strip claimed by an edge slide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeSide {
    Left,
    Right,
}

/// Why an active contact was canceled without completing a tap, hold, or swipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    Canceled,
    StationaryTimeout,
    TransportError,
    UnrecognizedMotion,
    ConflictingContact,
}

/// How an active edge slide concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeSlideEndReason {
    Released,
    Canceled(CancelReason),
}

/// Identity carried by a contact from `down` through gesture resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContactIdentity {
    pub id: u32,
    pub frame_epoch: u32,
}

/// Lifecycle phase of [`TouchRecognizer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecognizerPhase {
    Idle,
    Tracking,
    Sliding(EdgeSide),
    Consumed,
    Stale,
}

/// Outcome emitted by [`TouchRecognizer`] state transitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GestureEvent {
    Tap {
        contact: ContactIdentity,
        start: TouchPoint,
        end: TouchPoint,
    },
    LongPress {
        contact: ContactIdentity,
        start: TouchPoint,
        current: TouchPoint,
    },
    Swipe {
        contact: ContactIdentity,
        direction: SwipeDirection,
        start: TouchPoint,
        end: TouchPoint,
    },
    EdgeSlideStart {
        contact: ContactIdentity,
        side: EdgeSide,
        start: TouchPoint,
        current: TouchPoint,
        total_dy: i16,
    },
    EdgeSlideUpdate {
        contact: ContactIdentity,
        side: EdgeSide,
        start: TouchPoint,
        current: TouchPoint,
        delta_y: i16,
        total_dy: i16,
    },
    EdgeSlideEnd {
        contact: ContactIdentity,
        side: EdgeSide,
        start: TouchPoint,
        end: TouchPoint,
        total_dy: i16,
        reason: EdgeSlideEndReason,
    },
    Canceled {
        contact: ContactIdentity,
        reason: CancelReason,
    },
}

impl GestureEvent {
    pub const fn contact(&self) -> ContactIdentity {
        match *self {
            Self::Tap { contact, .. }
            | Self::LongPress { contact, .. }
            | Self::Swipe { contact, .. }
            | Self::EdgeSlideStart { contact, .. }
            | Self::EdgeSlideUpdate { contact, .. }
            | Self::EdgeSlideEnd { contact, .. }
            | Self::Canceled { contact, .. } => contact,
        }
    }
}

/// Wrap-safe elapsed milliseconds between `since_ms` and `now_ms`.
///
/// Differences `< 2^31` are treated as forward elapsed time; backward jitter
/// saturates to `0`.
pub const fn elapsed_ms(now_ms: u32, since_ms: u32) -> u32 {
    let delta = now_ms.wrapping_sub(since_ms);
    if delta < 0x8000_0000 {
        delta
    } else {
        0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveContact {
    contact: ContactIdentity,
    config: GestureConfig,
    start: TouchPoint,
    last: TouchPoint,
    down_ms: u32,
    last_report_ms: u32,
    hold_target: Option<Rect>,
    hold_armed: bool,
    exceeded_tap_drift: bool,
    edge_side: Option<EdgeSide>,
}

/// Single-contact gesture recognizer with stationary hold ticking, edge-slide
/// arbitration, and fail-closed stale contact suppression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TouchRecognizer {
    config: GestureConfig,
    phase: RecognizerPhase,
    contact: Option<ActiveContact>,
    next_contact_id: u32,
}

impl Default for TouchRecognizer {
    fn default() -> Self {
        Self::new()
    }
}

impl TouchRecognizer {
    pub const fn new() -> Self {
        Self {
            config: GestureConfig::DEFAULT,
            phase: RecognizerPhase::Idle,
            contact: None,
            next_contact_id: 0,
        }
    }

    pub const fn with_config(config: GestureConfig) -> Result<Self, GestureConfigError> {
        if let Err(err) = config.validate() {
            return Err(err);
        }
        Ok(Self {
            config,
            phase: RecognizerPhase::Idle,
            contact: None,
            next_contact_id: 0,
        })
    }

    pub const fn config(&self) -> GestureConfig {
        self.config
    }

    pub fn set_config(&mut self, config: GestureConfig) -> Result<(), GestureConfigError> {
        config.validate()?;
        self.config = config;
        Ok(())
    }

    /// Disarms any armed long-press on the active contact without canceling
    /// ongoing move/swipe/release tracking.
    pub fn disarm_hold(&mut self) {
        if let Some(c) = self.contact.as_mut() {
            c.hold_armed = false;
        }
    }

    pub const fn phase(&self) -> RecognizerPhase {
        self.phase
    }

    pub const fn is_idle(&self) -> bool {
        matches!(self.phase, RecognizerPhase::Idle)
    }

    pub const fn is_active(&self) -> bool {
        matches!(
            self.phase,
            RecognizerPhase::Tracking | RecognizerPhase::Sliding(_)
        )
    }

    pub const fn is_consumed(&self) -> bool {
        matches!(self.phase, RecognizerPhase::Consumed)
    }

    pub const fn is_stale(&self) -> bool {
        matches!(self.phase, RecognizerPhase::Stale)
    }

    pub const fn is_suppressed(&self) -> bool {
        matches!(
            self.phase,
            RecognizerPhase::Consumed | RecognizerPhase::Stale
        )
    }

    pub const fn active_contact(&self) -> Option<ContactIdentity> {
        match self.contact {
            Some(c) if self.is_active() => Some(c.contact),
            _ => None,
        }
    }

    pub const fn last_contact_id(&self) -> u32 {
        self.next_contact_id
    }

    /// Marks an idle recognizer as stale when a contact is first observed on an
    /// inactive or transitioning frame, suppressing actions until a clean lift.
    pub fn mark_stale(&mut self) {
        if self.phase == RecognizerPhase::Idle {
            self.phase = RecognizerPhase::Stale;
            self.contact = None;
        }
    }

    /// Begins tracking a new contact on `frame_epoch` at `now_ms`.
    ///
    /// Pass `target_bounds` when the touch-down lands on a hold-capable control;
    /// `None` (or bounds not containing `point`) keeps long-press disarmed.
    pub fn down(
        &mut self,
        point: TouchPoint,
        now_ms: u32,
        frame_epoch: u32,
        frame_width: u16,
        target_bounds: Option<Rect>,
    ) -> Option<GestureEvent> {
        match self.phase {
            RecognizerPhase::Consumed | RecognizerPhase::Stale => None,
            RecognizerPhase::Tracking | RecognizerPhase::Sliding(_) => {
                self.cancel_with(CancelReason::ConflictingContact)
            }
            RecognizerPhase::Idle => {
                let mut id = self.next_contact_id.wrapping_add(1);
                if id == 0 {
                    id = 1;
                }
                self.next_contact_id = id;
                let config = self.config;
                let hold_target = target_bounds.filter(|rect| rect.contains(point.x, point.y));
                let hold_armed = hold_target.is_some();
                let edge_side = if config.edge_slide_enabled && config.edge_strip_px > 0 {
                    if point.x < config.edge_strip_px {
                        Some(EdgeSide::Left)
                    } else if point.x >= frame_width.saturating_sub(config.edge_strip_px) {
                        Some(EdgeSide::Right)
                    } else {
                        None
                    }
                } else {
                    None
                };
                self.contact = Some(ActiveContact {
                    contact: ContactIdentity { id, frame_epoch },
                    config,
                    start: point,
                    last: point,
                    down_ms: now_ms,
                    last_report_ms: now_ms,
                    hold_target,
                    hold_armed,
                    exceeded_tap_drift: false,
                    edge_side,
                });
                self.phase = RecognizerPhase::Tracking;
                None
            }
        }
    }

    /// Processes a new contact sample while the finger remains down.
    pub fn move_to(&mut self, point: TouchPoint, now_ms: u32) -> Option<GestureEvent> {
        // Ignore delayed samples before changing either timing or position.
        // Equal timestamps are valid (multiple reports in one millisecond).
        if self
            .contact
            .is_some_and(|c| now_ms.wrapping_sub(c.last_report_ms) >= 0x8000_0000)
        {
            return None;
        }
        let side = match self.phase {
            RecognizerPhase::Tracking => None,
            RecognizerPhase::Sliding(side) => Some(side),
            RecognizerPhase::Consumed => {
                let Some(c) = self.contact.as_mut() else {
                    self.phase = RecognizerPhase::Stale;
                    return None;
                };
                if elapsed_ms(now_ms, c.last_report_ms) >= c.config.max_stationary_hold_ms {
                    self.phase = RecognizerPhase::Stale;
                    self.contact = None;
                } else {
                    c.last = point;
                    c.last_report_ms = now_ms;
                }
                return None;
            }
            RecognizerPhase::Idle | RecognizerPhase::Stale => {
                return None;
            }
        };
        let Some(c) = self.contact.as_mut() else {
            self.phase = RecognizerPhase::Stale;
            return None;
        };

        if elapsed_ms(now_ms, c.last_report_ms) >= c.config.max_stationary_hold_ms {
            return self.cancel_with(CancelReason::StationaryTimeout);
        }

        if let Some(side) = side {
            let delta_y = clamp_i16(i32::from(point.y) - i32::from(c.last.y));
            let total_dy = clamp_i16(i32::from(point.y) - i32::from(c.start.y));
            c.last = point;
            c.last_report_ms = now_ms;
            if delta_y != 0 {
                return Some(GestureEvent::EdgeSlideUpdate {
                    contact: c.contact,
                    side,
                    start: c.start,
                    current: point,
                    delta_y,
                    total_dy,
                });
            }
            return None;
        }

        c.last = point;
        c.last_report_ms = now_ms;

        let dx = i32::from(point.x) - i32::from(c.start.x);
        let dy = i32::from(point.y) - i32::from(c.start.y);
        let adx = dx.unsigned_abs();
        let ady = dy.unsigned_abs();

        let drift_max = u32::from(c.config.tap_drift_max_px);
        if adx > drift_max || ady > drift_max {
            c.exceeded_tap_drift = true;
        }
        if c.hold_armed
            && (c.exceeded_tap_drift
                || !c.hold_target.is_some_and(|r| r.contains(point.x, point.y)))
        {
            c.hold_armed = false;
        }

        if let Some(edge) = is_qualifying_edge_slide(c, adx, ady) {
            c.hold_armed = false;
            let contact = c.contact;
            let start = c.start;
            self.phase = RecognizerPhase::Sliding(edge);
            return Some(GestureEvent::EdgeSlideStart {
                contact,
                side: edge,
                start,
                current: point,
                total_dy: clamp_i16(dy),
            });
        }

        if c.hold_armed && elapsed_ms(now_ms, c.down_ms) >= c.config.long_press_ms {
            let contact = c.contact;
            let start = c.start;
            self.phase = RecognizerPhase::Consumed;
            return Some(GestureEvent::LongPress {
                contact,
                start,
                current: point,
            });
        }

        None
    }

    /// Advances stationary hold and timeout timers when no new sample arrived
    /// (for example GT911 `Ok(None)` or emulator `requestAnimationFrame` ticks).
    pub fn tick(&mut self, now_ms: u32) -> Option<GestureEvent> {
        match self.phase {
            RecognizerPhase::Idle | RecognizerPhase::Stale => None,
            RecognizerPhase::Consumed => {
                if let Some(c) = self.contact {
                    if elapsed_ms(now_ms, c.last_report_ms) >= c.config.max_stationary_hold_ms {
                        self.phase = RecognizerPhase::Stale;
                        self.contact = None;
                    }
                } else {
                    self.phase = RecognizerPhase::Stale;
                }
                None
            }
            RecognizerPhase::Sliding(_) => {
                let Some(c) = self.contact else {
                    self.phase = RecognizerPhase::Stale;
                    return None;
                };
                if elapsed_ms(now_ms, c.last_report_ms) >= c.config.max_stationary_hold_ms {
                    return self.cancel_with(CancelReason::StationaryTimeout);
                }
                None
            }
            RecognizerPhase::Tracking => {
                let Some(c) = self.contact else {
                    self.phase = RecognizerPhase::Stale;
                    return None;
                };
                if elapsed_ms(now_ms, c.last_report_ms) >= c.config.max_stationary_hold_ms {
                    return self.cancel_with(CancelReason::StationaryTimeout);
                }
                if c.hold_armed && elapsed_ms(now_ms, c.down_ms) >= c.config.long_press_ms {
                    self.phase = RecognizerPhase::Consumed;
                    return Some(GestureEvent::LongPress {
                        contact: c.contact,
                        start: c.start,
                        current: c.last,
                    });
                }
                None
            }
        }
    }

    /// Completes or clears the current contact on lift (`Ok(Some)` with no contact).
    pub fn up(&mut self, point: Option<TouchPoint>, now_ms: u32) -> Option<GestureEvent> {
        let phase = self.phase;
        let contact = self.contact.take();
        self.phase = RecognizerPhase::Idle;

        match phase {
            RecognizerPhase::Idle | RecognizerPhase::Consumed | RecognizerPhase::Stale => None,
            RecognizerPhase::Sliding(side) => {
                let c = contact?;
                let end = point.unwrap_or(c.last);
                if elapsed_ms(now_ms, c.last_report_ms) >= c.config.max_stationary_hold_ms {
                    return Some(GestureEvent::EdgeSlideEnd {
                        contact: c.contact,
                        side,
                        start: c.start,
                        end: c.last,
                        total_dy: clamp_i16(i32::from(c.last.y) - i32::from(c.start.y)),
                        reason: EdgeSlideEndReason::Canceled(CancelReason::StationaryTimeout),
                    });
                }
                Some(GestureEvent::EdgeSlideEnd {
                    contact: c.contact,
                    side,
                    start: c.start,
                    end,
                    total_dy: clamp_i16(i32::from(end.y) - i32::from(c.start.y)),
                    reason: EdgeSlideEndReason::Released,
                })
            }
            RecognizerPhase::Tracking => {
                let mut c = contact?;
                if elapsed_ms(now_ms, c.last_report_ms) >= c.config.max_stationary_hold_ms {
                    return Some(GestureEvent::Canceled {
                        contact: c.contact,
                        reason: CancelReason::StationaryTimeout,
                    });
                }
                if let Some(pt) = point {
                    let dx = i32::from(pt.x) - i32::from(c.start.x);
                    let dy = i32::from(pt.y) - i32::from(c.start.y);
                    let drift_max = u32::from(c.config.tap_drift_max_px);
                    if dx.unsigned_abs() > drift_max || dy.unsigned_abs() > drift_max {
                        c.exceeded_tap_drift = true;
                    }
                    if c.hold_armed
                        && (c.exceeded_tap_drift
                            || !c.hold_target.is_some_and(|r| r.contains(pt.x, pt.y)))
                    {
                        c.hold_armed = false;
                    }
                    c.last = pt;
                }
                let dx = i32::from(c.last.x) - i32::from(c.start.x);
                let dy = i32::from(c.last.y) - i32::from(c.start.y);
                let adx = dx.unsigned_abs();
                let ady = dy.unsigned_abs();
                let swipe_min = u32::from(c.config.swipe_min_px);
                let drift_max = u32::from(c.config.tap_drift_max_px);

                if let Some(side) = is_qualifying_edge_slide(&c, adx, ady) {
                    return Some(GestureEvent::EdgeSlideEnd {
                        contact: c.contact,
                        side,
                        start: c.start,
                        end: c.last,
                        total_dy: clamp_i16(dy),
                        reason: EdgeSlideEndReason::Released,
                    });
                }

                if c.hold_armed && elapsed_ms(now_ms, c.down_ms) >= c.config.long_press_ms {
                    return Some(GestureEvent::LongPress {
                        contact: c.contact,
                        start: c.start,
                        current: c.last,
                    });
                }

                if (adx > 0 || ady > 0) && (adx >= swipe_min || ady >= swipe_min) {
                    let direction = if adx >= ady {
                        if dx < 0 {
                            SwipeDirection::Left
                        } else {
                            SwipeDirection::Right
                        }
                    } else if dy < 0 {
                        SwipeDirection::Up
                    } else {
                        SwipeDirection::Down
                    };
                    Some(GestureEvent::Swipe {
                        contact: c.contact,
                        direction,
                        start: c.start,
                        end: c.last,
                    })
                } else if !c.exceeded_tap_drift && adx <= drift_max && ady <= drift_max {
                    Some(GestureEvent::Tap {
                        contact: c.contact,
                        start: c.start,
                        end: c.last,
                    })
                } else {
                    Some(GestureEvent::Canceled {
                        contact: c.contact,
                        reason: CancelReason::UnrecognizedMotion,
                    })
                }
            }
        }
    }

    /// Cancels an active contact (e.g. frame change, pointer loss, or conflicting
    /// physical button) and suppresses stale events until clean lift.
    pub fn cancel(&mut self) -> Option<GestureEvent> {
        self.cancel_with(CancelReason::Canceled)
    }

    /// Cancels immediately due to a controller/bus error and suppresses any later
    /// stale reports until a clean lift, even if the initial down was lost.
    pub fn on_error(&mut self) -> Option<GestureEvent> {
        if self.phase == RecognizerPhase::Idle {
            self.phase = RecognizerPhase::Stale;
        }
        self.cancel_with(CancelReason::TransportError)
    }

    fn cancel_with(&mut self, reason: CancelReason) -> Option<GestureEvent> {
        match self.phase {
            RecognizerPhase::Idle | RecognizerPhase::Stale => None,
            RecognizerPhase::Consumed => {
                self.phase = RecognizerPhase::Stale;
                self.contact = None;
                None
            }
            RecognizerPhase::Tracking => {
                let c = self.contact.take();
                self.phase = RecognizerPhase::Stale;
                c.map(|c| GestureEvent::Canceled {
                    contact: c.contact,
                    reason,
                })
            }
            RecognizerPhase::Sliding(side) => {
                let c = self.contact.take();
                self.phase = RecognizerPhase::Stale;
                c.map(|c| GestureEvent::EdgeSlideEnd {
                    contact: c.contact,
                    side,
                    start: c.start,
                    end: c.last,
                    total_dy: clamp_i16(i32::from(c.last.y) - i32::from(c.start.y)),
                    reason: EdgeSlideEndReason::Canceled(reason),
                })
            }
        }
    }
}

fn is_qualifying_edge_slide(c: &ActiveContact, adx: u32, ady: u32) -> Option<EdgeSide> {
    if !c.config.edge_slide_enabled {
        return None;
    }
    let edge = c.edge_side?;
    let ratio = u32::from(c.config.edge_slide_vertical_ratio);
    if ady >= u32::from(c.config.edge_slide_min_px)
        && ady > ratio.saturating_mul(adx)
        && adx < u32::from(c.config.swipe_min_px)
    {
        Some(edge)
    } else {
        None
    }
}

const fn clamp_i16(value: i32) -> i16 {
    if value < i16::MIN as i32 {
        i16::MIN
    } else if value > i16::MAX as i32 {
        i16::MAX
    } else {
        value as i16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: u16, y: u16) -> TouchPoint {
        TouchPoint::new(x, y)
    }

    #[test]
    fn default_config_matches_spec() {
        let cfg = GestureConfig::DEFAULT;
        assert_eq!(cfg.tap_drift_max_px, 16);
        assert_eq!(cfg.swipe_min_px, 60);
        assert_eq!(cfg.edge_strip_px, 56);
        assert_eq!(cfg.edge_slide_min_px, 24);
        assert_eq!(cfg.edge_slide_vertical_ratio, 2);
        assert_eq!(cfg.long_press_ms, 450);
        assert_eq!(cfg.max_stationary_hold_ms, 8_000);
        assert!(!cfg.edge_slide_enabled);
    }

    #[test]
    fn short_tap_and_drift_boundary() {
        let mut rec = TouchRecognizer::new();
        assert!(rec.is_idle());
        assert_eq!(rec.down(pt(200, 300), 100, 7, 480, None), None);
        assert!(rec.is_active());
        assert_eq!(
            rec.active_contact(),
            Some(ContactIdentity {
                id: 1,
                frame_epoch: 7
            })
        );

        // Exactly tap_drift_max_px (16 px) remains a valid tap.
        assert_eq!(rec.move_to(pt(216, 284), 120), None);
        assert_eq!(
            rec.up(Some(pt(216, 284)), 140),
            Some(GestureEvent::Tap {
                contact: ContactIdentity {
                    id: 1,
                    frame_epoch: 7
                },
                start: pt(200, 300),
                end: pt(216, 284),
            })
        );
        assert!(rec.is_idle());

        // Drift of 17 px (< swipe_min_px = 60) becomes UnrecognizedMotion even if
        // the finger returns to the start point before lift.
        assert_eq!(rec.down(pt(200, 300), 200, 8, 480, None), None);
        assert_eq!(rec.move_to(pt(217, 300), 220), None);
        assert_eq!(rec.move_to(pt(200, 300), 240), None);
        assert_eq!(
            rec.up(Some(pt(200, 300)), 260),
            Some(GestureEvent::Canceled {
                contact: ContactIdentity {
                    id: 2,
                    frame_epoch: 8
                },
                reason: CancelReason::UnrecognizedMotion,
            })
        );
        assert!(rec.is_idle());
    }

    #[test]
    fn stationary_no_report_hold_and_consumed_release() {
        let mut rec = TouchRecognizer::new();
        let target = Rect::new(40, 100, 400, 56);
        assert_eq!(rec.down(pt(120, 128), 1_000, 3, 480, Some(target)), None);

        // Stationary hold with no move_to reports: tick at 449 ms stays Tracking.
        assert_eq!(rec.tick(1_449), None);
        assert_eq!(rec.phase(), RecognizerPhase::Tracking);

        // At 450 ms emits LongPress once and enters Consumed.
        assert_eq!(
            rec.tick(1_450),
            Some(GestureEvent::LongPress {
                contact: ContactIdentity {
                    id: 1,
                    frame_epoch: 3
                },
                start: pt(120, 128),
                current: pt(120, 128),
            })
        );
        assert!(rec.is_consumed());
        assert!(rec.is_suppressed());
        assert!(!rec.is_active());

        // Further ticks and moves while held refresh keepalive without re-emitting;
        // release is consumed.
        assert_eq!(rec.move_to(pt(125, 130), 5_000), None);
        assert!(rec.is_consumed());
        assert_eq!(rec.tick(9_000), None);
        assert!(rec.is_consumed());
        // Stopping reports past max_stationary_hold_ms still transitions Consumed -> Stale.
        assert_eq!(rec.tick(13_000), None);
        assert!(rec.is_stale());
        assert_eq!(rec.up(Some(pt(125, 130)), 13_100), None);
        assert!(rec.is_idle());

        // Stationary lift just after long_press_ms (455 ms after 449 ms tick)
        // resolves as LongPress rather than falling back to Tap.
        assert_eq!(rec.down(pt(120, 128), 14_000, 3, 480, Some(target)), None);
        assert_eq!(rec.tick(14_449), None);
        assert_eq!(
            rec.up(Some(pt(120, 128)), 14_455),
            Some(GestureEvent::LongPress {
                contact: ContactIdentity {
                    id: 2,
                    frame_epoch: 3
                },
                start: pt(120, 128),
                current: pt(120, 128),
            })
        );
        assert!(rec.is_idle());

        // Lifting outside the target after 455 ms does not emit LongPress.
        let small_target = Rect::new(115, 120, 12, 12);
        assert_eq!(
            rec.down(pt(120, 128), 15_000, 3, 480, Some(small_target)),
            None
        );
        assert_eq!(rec.tick(15_449), None);
        assert_eq!(
            rec.up(Some(pt(130, 128)), 15_455),
            Some(GestureEvent::Tap {
                contact: ContactIdentity {
                    id: 3,
                    frame_epoch: 3
                },
                start: pt(120, 128),
                end: pt(130, 128),
            })
        );

        // Next touch starts a fresh contact.
        assert_eq!(rec.down(pt(120, 128), 16_100, 4, 480, Some(target)), None);
        assert_eq!(
            rec.up(Some(pt(120, 128)), 16_150),
            Some(GestureEvent::Tap {
                contact: ContactIdentity {
                    id: 4,
                    frame_epoch: 4
                },
                start: pt(120, 128),
                end: pt(120, 128),
            })
        );
    }

    #[test]
    fn hold_requires_target_and_is_disarmed_by_exit_or_drift() {
        let mut rec = TouchRecognizer::new();

        // None target bounds never arms hold.
        assert_eq!(rec.down(pt(100, 100), 0, 1, 480, None), None);
        assert_eq!(rec.tick(600), None);
        assert_eq!(rec.phase(), RecognizerPhase::Tracking);
        assert!(matches!(rec.up(None, 650), Some(GestureEvent::Tap { .. })));

        // Target exit and re-entry permanently disarms hold.
        let small_target = Rect::new(95, 95, 20, 20);
        assert_eq!(
            rec.down(pt(105, 105), 1_000, 1, 480, Some(small_target)),
            None
        );
        // Move outside target (within tap_drift_max_px = 16: dx=12) then back in.
        assert_eq!(rec.move_to(pt(117, 105), 1_100), None);
        assert_eq!(rec.move_to(pt(105, 105), 1_200), None);
        assert_eq!(rec.tick(1_700), None);
        assert_eq!(rec.phase(), RecognizerPhase::Tracking);
        // Release back inside still yields a tap because drift never exceeded 16 px.
        assert!(matches!(
            rec.up(Some(pt(105, 105)), 1_750),
            Some(GestureEvent::Tap { .. })
        ));

        // Exceeding tap_drift_max_px inside a large target also disarms hold permanently.
        let large_target = Rect::new(50, 50, 300, 200);
        assert_eq!(
            rec.down(pt(100, 100), 2_000, 1, 480, Some(large_target)),
            None
        );
        assert_eq!(rec.move_to(pt(120, 100), 2_100), None);
        assert_eq!(rec.move_to(pt(100, 100), 2_200), None);
        assert_eq!(rec.tick(3_000), None);
        assert_eq!(
            rec.up(Some(pt(100, 100)), 3_050),
            Some(GestureEvent::Canceled {
                contact: ContactIdentity {
                    id: 3,
                    frame_epoch: 1
                },
                reason: CancelReason::UnrecognizedMotion,
            })
        );
    }

    #[test]
    fn horizontal_and_vertical_swipes() {
        let mut rec = TouchRecognizer::new();
        let cases = [
            (pt(200, 200), pt(140, 210), SwipeDirection::Left),
            (pt(200, 200), pt(260, 190), SwipeDirection::Right),
            (pt(200, 200), pt(210, 140), SwipeDirection::Up),
            (pt(200, 200), pt(190, 260), SwipeDirection::Down),
            // Diagonal tie goes to horizontal.
            (pt(200, 200), pt(260, 260), SwipeDirection::Right),
        ];

        for (i, (start, end, dir)) in cases.into_iter().enumerate() {
            let t = (i as u32) * 100;
            assert_eq!(rec.down(start, t, 1, 480, None), None);
            assert_eq!(rec.move_to(end, t + 20), None);
            assert_eq!(
                rec.up(Some(end), t + 40),
                Some(GestureEvent::Swipe {
                    contact: ContactIdentity {
                        id: (i + 1) as u32,
                        frame_epoch: 1
                    },
                    direction: dir,
                    start,
                    end,
                })
            );
        }
    }

    #[test]
    fn edge_slide_arbitration_enabled_and_disabled() {
        let mut rec = TouchRecognizer::with_config(GestureConfig {
            edge_slide_enabled: true,
            ..GestureConfig::DEFAULT
        })
        .unwrap();

        // Left outer edge (x = 20 < 56): vertical motion 24 px with dx = 0 claims slide.
        assert_eq!(rec.down(pt(20, 400), 100, 5, 480, None), None);
        assert_eq!(
            rec.move_to(pt(20, 376), 120),
            Some(GestureEvent::EdgeSlideStart {
                contact: ContactIdentity {
                    id: 1,
                    frame_epoch: 5
                },
                side: EdgeSide::Left,
                start: pt(20, 400),
                current: pt(20, 376),
                total_dy: -24,
            })
        );
        assert_eq!(rec.phase(), RecognizerPhase::Sliding(EdgeSide::Left));
        // Further vertical drag emits incremental deltas; even > 60 px never swipes.
        assert_eq!(
            rec.move_to(pt(22, 320), 140),
            Some(GestureEvent::EdgeSlideUpdate {
                contact: ContactIdentity {
                    id: 1,
                    frame_epoch: 5
                },
                side: EdgeSide::Left,
                start: pt(20, 400),
                current: pt(22, 320),
                delta_y: -56,
                total_dy: -80,
            })
        );
        assert_eq!(
            rec.up(Some(pt(22, 320)), 160),
            Some(GestureEvent::EdgeSlideEnd {
                contact: ContactIdentity {
                    id: 1,
                    frame_epoch: 5
                },
                side: EdgeSide::Left,
                start: pt(20, 400),
                end: pt(22, 320),
                total_dy: -80,
                reason: EdgeSlideEndReason::Released,
            })
        );
        assert!(rec.is_idle());

        // Right outer edge (x = 424 >= 480 - 56): horizontal swipe from edge stays Swipe.
        assert_eq!(rec.down(pt(430, 300), 200, 5, 480, None), None);
        assert_eq!(rec.move_to(pt(370, 300), 220), None);
        assert!(matches!(
            rec.up(Some(pt(370, 300)), 240),
            Some(GestureEvent::Swipe {
                direction: SwipeDirection::Left,
                ..
            })
        ));

        // Exact 2:1 (|dy| == 2 * |dx|) is NOT strictly > 2 * |dx|, so does not claim.
        assert_eq!(rec.down(pt(20, 300), 300, 5, 480, None), None);
        assert_eq!(rec.move_to(pt(32, 276), 320), None);
        assert_eq!(rec.phase(), RecognizerPhase::Tracking);
        assert_eq!(
            rec.up(Some(pt(32, 276)), 340),
            Some(GestureEvent::Canceled {
                contact: ContactIdentity {
                    id: 3,
                    frame_epoch: 5
                },
                reason: CancelReason::UnrecognizedMotion,
            })
        );

        // Interior touch does not claim edge slide.
        assert_eq!(rec.down(pt(200, 300), 400, 5, 480, None), None);
        assert_eq!(rec.move_to(pt(200, 260), 420), None);
        assert_eq!(rec.phase(), RecognizerPhase::Tracking);
        assert!(matches!(
            rec.up(Some(pt(200, 240)), 440),
            Some(GestureEvent::Swipe {
                direction: SwipeDirection::Up,
                ..
            })
        ));

        // When edge_slide_enabled = false, edge-strip vertical drag falls through.
        rec.set_config(GestureConfig::DEFAULT).unwrap();
        assert_eq!(rec.down(pt(20, 400), 500, 5, 480, None), None);
        assert_eq!(rec.move_to(pt(20, 370), 520), None);
        assert_eq!(rec.phase(), RecognizerPhase::Tracking);
        assert_eq!(
            rec.up(Some(pt(20, 340)), 540),
            Some(GestureEvent::Swipe {
                contact: ContactIdentity {
                    id: 5,
                    frame_epoch: 5
                },
                direction: SwipeDirection::Up,
                start: pt(20, 400),
                end: pt(20, 340),
            })
        );
    }

    #[test]
    fn stationary_timeout_and_stale_suppression() {
        let mut rec = TouchRecognizer::new();
        assert_eq!(rec.down(pt(100, 100), 1_000, 2, 480, None), None);
        assert_eq!(rec.tick(8_999), None);
        assert!(rec.is_active());

        assert_eq!(
            rec.tick(9_000),
            Some(GestureEvent::Canceled {
                contact: ContactIdentity {
                    id: 1,
                    frame_epoch: 2
                },
                reason: CancelReason::StationaryTimeout,
            })
        );
        assert!(rec.is_stale());
        assert!(rec.is_suppressed());

        // Stale contact suppresses down/move/tick until clean lift.
        assert_eq!(rec.move_to(pt(100, 100), 9_100), None);
        assert_eq!(rec.tick(9_200), None);
        assert_eq!(rec.down(pt(100, 100), 9_300, 2, 480, None), None);
        assert_eq!(rec.up(Some(pt(100, 100)), 9_400), None);
        assert!(rec.is_idle());

        // Timeout during an active edge slide ends the slide cleanly.
        rec.set_config(GestureConfig {
            edge_slide_enabled: true,
            ..GestureConfig::DEFAULT
        })
        .unwrap();
        assert_eq!(rec.down(pt(10, 300), 10_000, 3, 480, None), None);
        assert!(matches!(
            rec.move_to(pt(10, 270), 10_020),
            Some(GestureEvent::EdgeSlideStart { .. })
        ));
        assert_eq!(
            rec.tick(18_020),
            Some(GestureEvent::EdgeSlideEnd {
                contact: ContactIdentity {
                    id: 2,
                    frame_epoch: 3
                },
                side: EdgeSide::Left,
                start: pt(10, 300),
                end: pt(10, 270),
                total_dy: -30,
                reason: EdgeSlideEndReason::Canceled(CancelReason::StationaryTimeout),
            })
        );
        assert!(rec.is_stale());
        assert_eq!(rec.up(None, 18_100), None);
        assert!(rec.is_idle());
    }

    #[test]
    fn on_error_and_cancel_suppress_later_lift() {
        let mut rec = TouchRecognizer::new();
        assert_eq!(rec.cancel(), None);
        assert!(rec.is_idle());
        assert_eq!(rec.on_error(), None);
        assert!(rec.is_stale());
        assert_eq!(rec.down(pt(120, 200), 10, 9, 480, None), None);
        assert_eq!(rec.move_to(pt(120, 200), 20), None);
        assert!(rec.is_stale());
        assert_eq!(rec.up(None, 30), None);
        assert!(rec.is_idle());

        assert_eq!(rec.down(pt(120, 200), 40, 9, 480, None), None);
        assert!(matches!(rec.up(None, 50), Some(GestureEvent::Tap { .. })));

        let mut rec = TouchRecognizer::new();
        assert_eq!(rec.down(pt(120, 200), 100, 9, 480, None), None);
        assert_eq!(
            rec.on_error(),
            Some(GestureEvent::Canceled {
                contact: ContactIdentity {
                    id: 1,
                    frame_epoch: 9
                },
                reason: CancelReason::TransportError,
            })
        );
        assert!(rec.is_stale());
        // Later lift cannot dispatch a stale tap.
        assert_eq!(rec.up(Some(pt(120, 200)), 150), None);
        assert!(rec.is_idle());

        // Active edge slide interrupted by error ends with TransportError.
        rec.set_config(GestureConfig {
            edge_slide_enabled: true,
            ..GestureConfig::DEFAULT
        })
        .unwrap();
        assert_eq!(rec.down(pt(12, 300), 200, 10, 480, None), None);
        assert!(matches!(
            rec.move_to(pt(12, 260), 220),
            Some(GestureEvent::EdgeSlideStart { .. })
        ));
        assert_eq!(
            rec.on_error(),
            Some(GestureEvent::EdgeSlideEnd {
                contact: ContactIdentity {
                    id: 2,
                    frame_epoch: 10
                },
                side: EdgeSide::Left,
                start: pt(12, 300),
                end: pt(12, 260),
                total_dy: -40,
                reason: EdgeSlideEndReason::Canceled(CancelReason::TransportError),
            })
        );
        assert!(rec.is_stale());
        assert_eq!(rec.up(None, 240), None);
        assert!(rec.is_idle());
    }

    #[test]
    fn u32_timestamp_wraparound() {
        let mut rec = TouchRecognizer::new();
        let target = Rect::new(0, 0, 100, 100);
        let start_ms = u32::MAX - 200;
        assert_eq!(rec.down(pt(50, 50), start_ms, 1, 480, Some(target)), None);

        // 449 ms across wrap -> no hold yet.
        assert_eq!(rec.tick(start_ms.wrapping_add(449)), None);
        // 450 ms across wrap -> LongPress fires.
        assert_eq!(
            rec.tick(start_ms.wrapping_add(450)),
            Some(GestureEvent::LongPress {
                contact: ContactIdentity {
                    id: 1,
                    frame_epoch: 1
                },
                start: pt(50, 50),
                current: pt(50, 50),
            })
        );
        assert_eq!(rec.up(None, start_ms.wrapping_add(500)), None);

        // Stationary timeout across u32 wraparound.
        let t0 = u32::MAX - 1000;
        assert_eq!(rec.down(pt(50, 50), t0, 2, 480, None), None);
        assert_eq!(rec.tick(t0.wrapping_add(7_999)), None);
        assert_eq!(
            rec.tick(t0.wrapping_add(8_000)),
            Some(GestureEvent::Canceled {
                contact: ContactIdentity {
                    id: 2,
                    frame_epoch: 2
                },
                reason: CancelReason::StationaryTimeout,
            })
        );
        assert_eq!(rec.up(None, t0.wrapping_add(8_100)), None);
    }

    #[test]
    fn delayed_move_does_not_replace_latest_position() {
        let mut rec = TouchRecognizer::new();
        rec.down(pt(200, 300), 1_000, 1, 480, None);
        rec.move_to(pt(260, 300), 1_020);
        assert_eq!(rec.move_to(pt(200, 300), 1_010), None);
        // A second sample with the same timestamp must still be accepted.
        rec.move_to(pt(280, 300), 1_020);
        assert_eq!(rec.move_to(pt(200, 300), 1_010), None);
        assert!(matches!(
            rec.up(None, 1_040),
            Some(GestureEvent::Swipe {
                direction: SwipeDirection::Right,
                end,
                ..
            }) if end == pt(280, 300)
        ));

        rec.set_config(GestureConfig {
            edge_slide_enabled: true,
            ..GestureConfig::DEFAULT
        })
        .unwrap();
        rec.down(pt(10, 300), u32::MAX - 10, 1, 480, None);
        assert!(matches!(
            rec.move_to(pt(10, 260), 5),
            Some(GestureEvent::EdgeSlideStart { .. })
        ));
        assert_eq!(rec.move_to(pt(10, 300), u32::MAX), None);
        assert!(matches!(
            rec.move_to(pt(10, 250), 5),
            Some(GestureEvent::EdgeSlideUpdate { delta_y: -10, .. })
        ));
        assert!(matches!(
            rec.up(None, 20),
            Some(GestureEvent::EdgeSlideEnd { total_dy: -50, .. })
        ));
    }

    #[test]
    fn backward_sample_timestamps_never_shorten_stationary_timeout() {
        let mut rec = TouchRecognizer::new();
        assert_eq!(rec.down(pt(200, 300), 1_000, 1, 480, None), None);
        // Out-of-order delayed sample stamped 990 ms must not pull last_report_ms backward.
        assert_eq!(rec.move_to(pt(260, 300), 990), None);
        // 8_000 ms after 1_000 is 9_000; at 8_999 the contact must still be active.
        assert_eq!(rec.tick(8_999), None);
        assert_eq!(rec.phase(), RecognizerPhase::Tracking);
        assert_eq!(
            rec.tick(9_000),
            Some(GestureEvent::Canceled {
                contact: ContactIdentity {
                    id: 1,
                    frame_epoch: 1
                },
                reason: CancelReason::StationaryTimeout,
            })
        );
    }

    #[test]
    fn edge_slide_priority_applies_when_lift_supplies_qualifying_movement() {
        let mut rec = TouchRecognizer::with_config(GestureConfig {
            edge_slide_enabled: true,
            ..GestureConfig::DEFAULT
        })
        .unwrap();

        // Vertical edge lift with no intermediate move sample emits EdgeSlideEnd
        // instead of falling through to Swipe::Up.
        assert_eq!(rec.down(pt(20, 400), 100, 1, 480, None), None);
        assert_eq!(
            rec.up(Some(pt(20, 320)), 140),
            Some(GestureEvent::EdgeSlideEnd {
                contact: ContactIdentity {
                    id: 1,
                    frame_epoch: 1,
                },
                side: EdgeSide::Left,
                start: pt(20, 400),
                end: pt(20, 320),
                total_dy: -80,
                reason: EdgeSlideEndReason::Released,
            })
        );

        // Horizontal lift from the edge strip does not qualify as an edge slide
        // and still emits Swipe::Right.
        assert_eq!(rec.down(pt(20, 200), 200, 2, 480, None), None);
        assert_eq!(
            rec.up(Some(pt(120, 200)), 240),
            Some(GestureEvent::Swipe {
                contact: ContactIdentity {
                    id: 2,
                    frame_epoch: 2,
                },
                direction: SwipeDirection::Right,
                start: pt(20, 200),
                end: pt(120, 200),
            })
        );
    }

    #[test]
    fn timing_thresholds_at_or_above_half_wrap_are_rejected() {
        let mut rec = TouchRecognizer::new();
        assert_eq!(
            TouchRecognizer::with_config(GestureConfig {
                long_press_ms: MAX_TIMING_THRESHOLD_MS + 1,
                ..GestureConfig::DEFAULT
            }),
            Err(GestureConfigError::LongPressOutOfRange)
        );
        assert_eq!(
            TouchRecognizer::with_config(GestureConfig {
                max_stationary_hold_ms: MAX_TIMING_THRESHOLD_MS + 1,
                ..GestureConfig::DEFAULT
            }),
            Err(GestureConfigError::StationaryHoldOutOfRange)
        );
        assert_eq!(
            rec.set_config(GestureConfig {
                long_press_ms: MAX_TIMING_THRESHOLD_MS + 1,
                ..GestureConfig::DEFAULT
            }),
            Err(GestureConfigError::LongPressOutOfRange)
        );
        assert_eq!(rec.config(), GestureConfig::DEFAULT);

        let boundary = GestureConfig {
            long_press_ms: MAX_TIMING_THRESHOLD_MS,
            max_stationary_hold_ms: MAX_TIMING_THRESHOLD_MS,
            ..GestureConfig::DEFAULT
        };
        assert!(TouchRecognizer::with_config(boundary).is_ok());
        assert_eq!(rec.set_config(boundary), Ok(()));
    }

    #[test]
    fn edge_slide_beats_hold_when_release_qualifies_at_long_press_time() {
        let mut rec = TouchRecognizer::with_config(GestureConfig {
            edge_slide_enabled: true,
            edge_slide_min_px: 8,
            ..GestureConfig::DEFAULT
        })
        .unwrap();
        let target = Rect::new(0, 350, 80, 100);

        // 10 px vertical lift inside edge hold target at long_press_ms emits EdgeSlideEnd,
        // matching move_to priority rather than firing LongPress.
        assert_eq!(rec.down(pt(20, 400), 100, 1, 480, Some(target)), None);
        assert_eq!(
            rec.up(Some(pt(20, 390)), 550),
            Some(GestureEvent::EdgeSlideEnd {
                contact: ContactIdentity {
                    id: 1,
                    frame_epoch: 1,
                },
                side: EdgeSide::Left,
                start: pt(20, 400),
                end: pt(20, 390),
                total_dy: -10,
                reason: EdgeSlideEndReason::Released,
            })
        );

        // Stationary lift inside the edge hold target at long_press_ms still emits LongPress.
        assert_eq!(rec.down(pt(20, 400), 600, 2, 480, Some(target)), None);
        assert_eq!(
            rec.up(Some(pt(20, 400)), 1_050),
            Some(GestureEvent::LongPress {
                contact: ContactIdentity {
                    id: 2,
                    frame_epoch: 2,
                },
                start: pt(20, 400),
                current: pt(20, 400),
            })
        );
    }

    #[test]
    fn zero_swipe_threshold_keeps_stationary_release_as_tap() {
        let mut rec = TouchRecognizer::with_config(GestureConfig {
            swipe_min_px: 0,
            ..GestureConfig::DEFAULT
        })
        .unwrap();

        assert_eq!(rec.down(pt(200, 300), 100, 1, 480, None), None);
        assert_eq!(
            rec.up(Some(pt(200, 300)), 140),
            Some(GestureEvent::Tap {
                contact: ContactIdentity {
                    id: 1,
                    frame_epoch: 1,
                },
                start: pt(200, 300),
                end: pt(200, 300),
            })
        );

        // Moving contact with swipe_min_px == 0 still classifies as a swipe.
        assert_eq!(rec.down(pt(200, 300), 200, 2, 480, None), None);
        assert_eq!(rec.move_to(pt(201, 300), 210), None);
        assert_eq!(
            rec.up(Some(pt(201, 300)), 220),
            Some(GestureEvent::Swipe {
                contact: ContactIdentity {
                    id: 2,
                    frame_epoch: 2,
                },
                direction: SwipeDirection::Right,
                start: pt(200, 300),
                end: pt(201, 300),
            })
        );
    }
}
