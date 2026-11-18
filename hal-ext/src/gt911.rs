//! Goodix GT911 capacitive touch controller — used on the Xteink X4 Pro.
//!
//! Uses 16-bit big-endian register addresses (`0x814E` status, `0x8150` first
//! contact record), supports both standard (`coords_at_byte0 = false`) and
//! X4 Pro coordinate-first (`coords_at_byte0 = true`) 8-byte point layouts,
//! and decodes the capacitive Home key bit (`0x814E & 0x10`). Generic over any
//! async `I2c` bus so it remains MCU-neutral and host-testable.

use embedded_hal_async::i2c::{Error, ErrorKind, I2c};

/// Primary 7-bit I2C address when INT is held LOW during reset.
pub const PRIMARY_ADDR: u8 = 0x5D;
/// Alternate 7-bit I2C address when INT is held HIGH during reset.
pub const ALT_ADDR: u8 = 0x14;

/// Touch status register (`bit 7`: buffer ready, `bit 4`: capacitive key,
/// `bits 3..0`: contact count).
pub const REG_STATUS: u16 = 0x814E;
/// First contact 8-byte point record register.
pub const REG_FIRST_POINT: u16 = 0x8150;

/// Status bit indicating a fresh controller frame is ready to read.
pub const STATUS_BUFFER_READY: u8 = 0x80;
/// Status bit indicating the capacitive Home key is pressed.
pub const STATUS_HAVE_KEY: u8 = 0x10;

/// Minimum pointer displacement (logical pixels) to classify a gesture as a
/// swipe instead of a tap (matches FreeInk `TOUCH_SWIPE_MIN_PX = 60`).
pub const SWIPE_MIN_PX: u16 = 60;

/// Logical display coordinates of a decoded touch contact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TouchPoint {
    pub x: u16,
    pub y: u16,
}

/// One completed frame polled from the GT911 status and point registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TouchSample {
    pub count: u8,
    pub home_key: bool,
    pub point: Option<TouchPoint>,
    pub raw_point: Option<TouchPoint>,
}

/// Digitizer-to-display mounting transform and point-record layout selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TouchTransform {
    pub width: u16,
    pub height: u16,
    pub swap_xy: bool,
    pub flip_x: bool,
    pub flip_y: bool,
    pub coords_at_byte0: bool,
}

impl TouchTransform {
    /// Confirmed Xteink X4 Pro landscape configuration from FreeInk
    /// `BoardConfig.h`: portrait sensor (`480x800`) mounted over an `800x480`
    /// landscape panel with `swap_xy`, `flip_y`, and `coords_at_byte0`.
    /// Matches Calendula's `LandscapeButtonsTop` (`FbFrame::LandscapeFlipped`).
    pub const X4_PRO_LANDSCAPE: Self = Self {
        width: 800,
        height: 480,
        swap_xy: true,
        flip_x: false,
        flip_y: true,
        coords_at_byte0: true,
    };

    /// Xteink X4 Pro upright portrait transform (`480x800`), matching
    /// `FbFrame::Portrait` where `px = raw_x` and `py = raw_y`.
    pub const X4_PRO_PORTRAIT: Self = Self {
        width: 480,
        height: 800,
        swap_xy: false,
        flip_x: false,
        flip_y: false,
        coords_at_byte0: true,
    };

    /// Xteink X4 Pro `LandscapeButtonsBottom` (`FbFrame::Landscape`, `800x480`),
    /// rotated 90° clockwise from `X4_PRO_PORTRAIT` (`lx = 799 - raw_y`,
    /// `ly = raw_x`).
    pub const X4_PRO_LANDSCAPE_BOTTOM: Self = Self {
        width: 800,
        height: 480,
        swap_xy: true,
        flip_x: true,
        flip_y: false,
        coords_at_byte0: true,
    };

    /// Extracts raw sensor `(x, y)` coordinates from an 8-byte contact record.
    pub const fn raw_point(&self, record: &[u8; 8]) -> TouchPoint {
        let offset = if self.coords_at_byte0 { 0 } else { 1 };
        let x = u16::from_le_bytes([record[offset], record[offset + 1]]);
        let y = u16::from_le_bytes([record[offset + 2], record[offset + 3]]);
        TouchPoint { x, y }
    }

    /// Maps raw sensor `(raw_x, raw_y)` coordinates into logical display
    /// coordinates clamped to `0..width` and `0..height`.
    pub const fn map_raw(&self, raw: TouchPoint) -> TouchPoint {
        let sx = if self.swap_xy { raw.y } else { raw.x };
        let sy = if self.swap_xy { raw.x } else { raw.y };

        let max_x = self.width.saturating_sub(1);
        let max_y = self.height.saturating_sub(1);
        let mut x = if sx > max_x { max_x } else { sx };
        let mut y = if sy > max_y { max_y } else { sy };

        if self.flip_x {
            x = max_x - x;
        }
        if self.flip_y {
            y = max_y - y;
        }

        TouchPoint { x, y }
    }

    /// Decodes an 8-byte contact record from [`REG_FIRST_POINT`] into logical
    /// display coordinates clamped to `0..width` and `0..height`.
    pub const fn decode_point(&self, record: &[u8; 8]) -> TouchPoint {
        self.map_raw(self.raw_point(record))
    }
}

/// High-level navigation action classified from a touch gesture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchAction {
    Home,
    Prev,
    Next,
    Confirm,
    Back,
}

/// Active display geometry and dash-rail placement for screen-aware touch
/// classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchLayout {
    /// Upright portrait (`480x800`) shell screen with four bottom dash icons
    /// centered at `x = 120, 200, 280, 360`, `y >= 700`.
    PortraitShell,
    /// Upright portrait (`480x800`) reading page. When `reading_sheet` is true,
    /// the bottom dash bar (`y >= 700`) is active over the footer.
    PortraitReading { reading_sheet: bool },
    /// Landscape (`800x480`) shell screen with the dash rail on the left
    /// (`x < 180`, rows centered at `y = 120, 200, 280, 360`).
    LandscapeLeftRail,
    /// Landscape (`800x480`) shell screen with the dash rail on the right
    /// (`x >= 620`, rows centered at `y = 120, 200, 280, 360`).
    LandscapeRightRail,
    /// Landscape (`800x480`) reading page.
    LandscapeReading,
}

/// Outcome of classifying a touch gesture against a [`TouchLayout`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenTouch {
    /// Tap on one of the four physical dash-key slots (`0..=3` in visual
    /// left-to-right or top-to-bottom order).
    DashSlot(u8),
    /// Gesture or content-region tap mapped to a visual navigation action.
    Action(TouchAction),
}

const fn classify_swipe(dx: i32, dy: i32) -> TouchAction {
    let adx = dx.unsigned_abs();
    let ady = dy.unsigned_abs();
    if adx >= ady {
        if dx < 0 {
            TouchAction::Next
        } else {
            TouchAction::Prev
        }
    } else if dy < 0 {
        TouchAction::Home
    } else {
        TouchAction::Back
    }
}

const fn classify_thirds(x: u16, min_x: u16, max_x: u16) -> TouchAction {
    let span = max_x.saturating_sub(min_x) as u32;
    let rel = x.saturating_sub(min_x) as u32;
    if rel < span / 3 {
        TouchAction::Prev
    } else if rel < (span * 2) / 3 {
        TouchAction::Confirm
    } else {
        TouchAction::Next
    }
}

const fn classify_rail_slot(coord: u16) -> u8 {
    // Dash centers sit at 120, 200, 280, 360 with 80 px spacing.
    if coord < 160 {
        0
    } else if coord < 240 {
        1
    } else if coord < 320 {
        2
    } else {
        3
    }
}

/// Returns whether the pointer motion from `start` to `end` meets [`SWIPE_MIN_PX`]
/// along either axis.
pub const fn is_swipe(start: TouchPoint, end: TouchPoint) -> bool {
    let dx = end.x as i32 - start.x as i32;
    let dy = end.y as i32 - start.y as i32;
    let min = SWIPE_MIN_PX as u32;
    dx.unsigned_abs() >= min || dy.unsigned_abs() >= min
}

/// Classifies a single-contact gesture from `start` to `end` (in logical
/// display coordinates for `layout`) into either a dash-rail slot (`0..=3`)
/// or a navigation [`TouchAction`].
pub const fn classify_screen_touch(
    start: TouchPoint,
    end: TouchPoint,
    layout: TouchLayout,
) -> ScreenTouch {
    if is_swipe(start, end) {
        let dx = end.x as i32 - start.x as i32;
        let dy = end.y as i32 - start.y as i32;
        return ScreenTouch::Action(classify_swipe(dx, dy));
    }

    match layout {
        TouchLayout::PortraitShell
        | TouchLayout::PortraitReading {
            reading_sheet: true,
        } => {
            if start.y >= 700 {
                ScreenTouch::DashSlot(classify_rail_slot(start.x))
            } else {
                ScreenTouch::Action(classify_thirds(start.x, 0, 480))
            }
        }
        TouchLayout::PortraitReading {
            reading_sheet: false,
        } => ScreenTouch::Action(classify_thirds(start.x, 0, 480)),
        TouchLayout::LandscapeLeftRail => {
            if start.x < 180 {
                ScreenTouch::DashSlot(classify_rail_slot(start.y))
            } else {
                ScreenTouch::Action(classify_thirds(start.x, 180, 800))
            }
        }
        TouchLayout::LandscapeRightRail => {
            if start.x >= 620 {
                ScreenTouch::DashSlot(classify_rail_slot(start.y))
            } else {
                ScreenTouch::Action(classify_thirds(start.x, 0, 620))
            }
        }
        TouchLayout::LandscapeReading => ScreenTouch::Action(classify_thirds(start.x, 0, 800)),
    }
}

/// Classifies a single-contact gesture from `start` to `end` on a screen of
/// `width` logical pixels into a [`TouchAction`].
///
/// Motions of at least [`SWIPE_MIN_PX`] along the dominant axis classify as a
/// directional swipe (ties go to horizontal: left -> `Next`, right -> `Prev`,
/// up -> `Home`, down -> `Back`). Shorter motions classify as a tap across the
/// three horizontal zones (left third -> `Prev`, middle third -> `Confirm`,
/// right third -> `Next`).
pub const fn classify_tap_or_swipe(start: TouchPoint, end: TouchPoint, width: u16) -> TouchAction {
    let dx = end.x as i32 - start.x as i32;
    let dy = end.y as i32 - start.y as i32;
    let adx = dx.unsigned_abs();
    let ady = dy.unsigned_abs();
    let min = SWIPE_MIN_PX as u32;

    if adx >= min || ady >= min {
        if adx >= ady {
            if dx < 0 {
                TouchAction::Next
            } else {
                TouchAction::Prev
            }
        } else if dy < 0 {
            TouchAction::Home
        } else {
            TouchAction::Back
        }
    } else {
        let left_end = (width as u32) / 3;
        let mid_end = (width as u32 * 2) / 3;
        let x = start.x as u32;
        if x < left_end {
            TouchAction::Prev
        } else if x < mid_end {
            TouchAction::Confirm
        } else {
            TouchAction::Next
        }
    }
}

/// Async driver for the GT911 capacitive touch controller.
pub struct Gt911<I> {
    i2c: I,
    addr: u8,
    transform: TouchTransform,
}

impl<I> Gt911<I> {
    /// Creates a driver targeting [`PRIMARY_ADDR`] with `transform`.
    pub const fn new(i2c: I, transform: TouchTransform) -> Self {
        Self::with_addr(i2c, PRIMARY_ADDR, transform)
    }

    /// Creates a driver targeting a specific 7-bit I2C address and `transform`.
    pub const fn with_addr(i2c: I, addr: u8, transform: TouchTransform) -> Self {
        Self {
            i2c,
            addr,
            transform,
        }
    }

    /// Returns the configured 7-bit I2C address.
    pub const fn addr(&self) -> u8 {
        self.addr
    }
}

impl<I: I2c> Gt911<I> {
    /// Probes whether the controller acknowledges a read of [`REG_STATUS`] at
    /// `self.addr`. Returns `Ok(false)` on address/data NACK and propagates
    /// other bus errors.
    pub async fn probe(&mut self) -> Result<bool, I::Error> {
        let mut status = [0u8; 1];
        match self
            .i2c
            .write_read(self.addr, &REG_STATUS.to_be_bytes(), &mut status)
            .await
        {
            Ok(()) => Ok(true),
            Err(err) if matches!(err.kind(), ErrorKind::NoAcknowledge(_)) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Polls [`REG_STATUS`]. When [`STATUS_BUFFER_READY`] is set, reads the
    /// primary point from [`REG_FIRST_POINT`] if `count > 0`, clears
    /// [`REG_STATUS`] to `0`, and returns the decoded [`TouchSample`].
    ///
    /// If reading [`REG_FIRST_POINT`] fails, returns the I2C error without
    /// clearing [`REG_STATUS`] so the pending frame is not lost.
    pub async fn poll(&mut self) -> Result<Option<TouchSample>, I::Error> {
        let status_reg = REG_STATUS.to_be_bytes();
        let mut status_buf = [0u8; 1];
        self.i2c
            .write_read(self.addr, &status_reg, &mut status_buf)
            .await?;
        let status = status_buf[0];

        if (status & STATUS_BUFFER_READY) == 0 {
            return Ok(None);
        }

        let home_key = (status & STATUS_HAVE_KEY) != 0;
        let count = status & 0x0F;
        let (point, raw_point) = if count > 0 {
            let mut record = [0u8; 8];
            self.i2c
                .write_read(self.addr, &REG_FIRST_POINT.to_be_bytes(), &mut record)
                .await?;
            let raw = self.transform.raw_point(&record);
            (Some(self.transform.map_raw(raw)), Some(raw))
        } else {
            (None, None)
        };

        self.i2c
            .write(self.addr, &[status_reg[0], status_reg[1], 0x00])
            .await?;

        Ok(Some(TouchSample {
            count,
            home_key,
            point,
            raw_point,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{block_on, I2cExpectation, MockI2cError, ScriptedI2c};
    use embedded_hal_async::i2c::{ErrorKind, NoAcknowledgeSource};
    use std::vec;

    fn make_record(coords_at_byte0: bool, raw_x: u16, raw_y: u16) -> [u8; 8] {
        let [xl, xh] = raw_x.to_le_bytes();
        let [yl, yh] = raw_y.to_le_bytes();
        if coords_at_byte0 {
            [xl, xh, yl, yh, 0x20, 0x00, 0x00, 0x00]
        } else {
            [0x03, xl, xh, yl, yh, 0x20, 0x00, 0x00]
        }
    }

    #[test]
    fn decodes_points_with_coords_at_byte0_true_and_false() {
        let byte0_rec = make_record(true, 120, 350);
        let t_byte0 = TouchTransform {
            width: 800,
            height: 480,
            swap_xy: false,
            flip_x: false,
            flip_y: false,
            coords_at_byte0: true,
        };
        assert_eq!(
            t_byte0.decode_point(&byte0_rec),
            TouchPoint { x: 120, y: 350 }
        );

        let std_rec = make_record(false, 640, 200);
        let t_std = TouchTransform {
            coords_at_byte0: false,
            flip_x: true,
            ..t_byte0
        };
        assert_eq!(
            t_std.decode_point(&std_rec),
            TouchPoint {
                x: 799 - 640,
                y: 200
            }
        );
    }

    #[test]
    fn maps_all_four_corners_of_x4_pro_landscape() {
        let t = TouchTransform::X4_PRO_LANDSCAPE;
        // Portrait sensor (0..480 x 0..800) mapped with swap_xy + flip_y to 800x480:
        // (0, 0) -> bottom-left (0, 479)
        assert_eq!(
            t.decode_point(&make_record(true, 0, 0)),
            TouchPoint { x: 0, y: 479 }
        );
        // (479, 0) -> top-left (0, 0)
        assert_eq!(
            t.decode_point(&make_record(true, 479, 0)),
            TouchPoint { x: 0, y: 0 }
        );
        // (0, 799) -> bottom-right (799, 479)
        assert_eq!(
            t.decode_point(&make_record(true, 0, 799)),
            TouchPoint { x: 799, y: 479 }
        );
        // (479, 799) -> top-right (799, 0)
        assert_eq!(
            t.decode_point(&make_record(true, 479, 799)),
            TouchPoint { x: 799, y: 0 }
        );
        // Out-of-bounds raw coordinates clamp cleanly before flipping.
        assert_eq!(
            t.decode_point(&make_record(true, 480, 800)),
            TouchPoint { x: 799, y: 0 }
        );
    }

    #[test]
    fn classifies_tap_zones_and_swipes() {
        let w = 800;
        // Tap zones (movement < 60 px): left / middle / right thirds.
        assert_eq!(
            classify_tap_or_swipe(
                TouchPoint { x: 100, y: 200 },
                TouchPoint { x: 110, y: 205 },
                w
            ),
            TouchAction::Prev
        );
        assert_eq!(
            classify_tap_or_swipe(
                TouchPoint { x: 265, y: 200 },
                TouchPoint { x: 265, y: 200 },
                w
            ),
            TouchAction::Prev
        );
        assert_eq!(
            classify_tap_or_swipe(
                TouchPoint { x: 266, y: 200 },
                TouchPoint { x: 266, y: 200 },
                w
            ),
            TouchAction::Confirm
        );
        assert_eq!(
            classify_tap_or_swipe(
                TouchPoint { x: 400, y: 240 },
                TouchPoint { x: 420, y: 250 },
                w
            ),
            TouchAction::Confirm
        );
        assert_eq!(
            classify_tap_or_swipe(
                TouchPoint { x: 533, y: 200 },
                TouchPoint { x: 533, y: 200 },
                w
            ),
            TouchAction::Next
        );
        assert_eq!(
            classify_tap_or_swipe(
                TouchPoint { x: 700, y: 300 },
                TouchPoint { x: 690, y: 295 },
                w
            ),
            TouchAction::Next
        );

        // Directional swipes (>= 60 px).
        assert_eq!(
            classify_tap_or_swipe(
                TouchPoint { x: 500, y: 240 },
                TouchPoint { x: 400, y: 250 },
                w
            ),
            TouchAction::Next
        );
        assert_eq!(
            classify_tap_or_swipe(
                TouchPoint { x: 300, y: 240 },
                TouchPoint { x: 400, y: 230 },
                w
            ),
            TouchAction::Prev
        );
        assert_eq!(
            classify_tap_or_swipe(
                TouchPoint { x: 400, y: 350 },
                TouchPoint { x: 410, y: 250 },
                w
            ),
            TouchAction::Home
        );
        assert_eq!(
            classify_tap_or_swipe(
                TouchPoint { x: 400, y: 150 },
                TouchPoint { x: 410, y: 250 },
                w
            ),
            TouchAction::Back
        );
        // 59 px motion stays a tap; 60 px motion classifies as a swipe.
        assert_eq!(
            classify_tap_or_swipe(
                TouchPoint { x: 400, y: 200 },
                TouchPoint { x: 459, y: 200 },
                w
            ),
            TouchAction::Confirm
        );
        assert_eq!(
            classify_tap_or_swipe(
                TouchPoint { x: 400, y: 200 },
                TouchPoint { x: 460, y: 260 },
                w
            ),
            TouchAction::Prev
        );
    }

    #[test]
    fn probe_reports_ack_nack_and_bus_errors() {
        let nack = MockI2cError(ErrorKind::NoAcknowledge(NoAcknowledgeSource::Address));
        let bus = MockI2cError(ErrorKind::Bus);

        let mut gt = Gt911::new(
            ScriptedI2c::new([
                I2cExpectation::WriteRead {
                    addr: PRIMARY_ADDR,
                    write: vec![0x81, 0x4E],
                    read: Ok(vec![0x00]),
                },
                I2cExpectation::WriteRead {
                    addr: PRIMARY_ADDR,
                    write: vec![0x81, 0x4E],
                    read: Err(nack),
                },
                I2cExpectation::WriteRead {
                    addr: PRIMARY_ADDR,
                    write: vec![0x81, 0x4E],
                    read: Err(bus),
                },
            ]),
            TouchTransform::X4_PRO_LANDSCAPE,
        );
        assert_eq!(gt.addr(), PRIMARY_ADDR);
        assert_eq!(block_on(gt.probe()), Ok(true));
        assert_eq!(block_on(gt.probe()), Ok(false));
        assert_eq!(block_on(gt.probe()), Err(bus));
        gt.i2c.assert_exhausted();

        let alt = Gt911::with_addr(
            ScriptedI2c::new([I2cExpectation::WriteRead {
                addr: ALT_ADDR,
                write: vec![0x81, 0x4E],
                read: Ok(vec![0x00]),
            }]),
            ALT_ADDR,
            TouchTransform::X4_PRO_LANDSCAPE,
        );
        assert_eq!(alt.addr(), ALT_ADDR);
        let mut alt = alt;
        assert_eq!(block_on(alt.probe()), Ok(true));
        alt.i2c.assert_exhausted();
    }

    #[test]
    fn poll_handles_not_ready_home_key_touch_and_status_clearing() {
        let point_record = make_record(true, 479, 799); // maps to (799, 0)

        let mut gt = Gt911::new(
            ScriptedI2c::new([
                // 1. Buffer not ready (even if bit 4 set): returns None, no clear write.
                I2cExpectation::WriteRead {
                    addr: PRIMARY_ADDR,
                    write: vec![0x81, 0x4E],
                    read: Ok(vec![STATUS_HAVE_KEY]),
                },
                // 2. Buffer ready + Home key only (count == 0): no point read, clears status.
                I2cExpectation::WriteRead {
                    addr: PRIMARY_ADDR,
                    write: vec![0x81, 0x4E],
                    read: Ok(vec![STATUS_BUFFER_READY | STATUS_HAVE_KEY]),
                },
                I2cExpectation::Write {
                    addr: PRIMARY_ADDR,
                    data: vec![0x81, 0x4E, 0x00],
                    result: Ok(()),
                },
                // 3. Buffer ready + release (count == 0, no key): clears status.
                I2cExpectation::WriteRead {
                    addr: PRIMARY_ADDR,
                    write: vec![0x81, 0x4E],
                    read: Ok(vec![STATUS_BUFFER_READY]),
                },
                I2cExpectation::Write {
                    addr: PRIMARY_ADDR,
                    data: vec![0x81, 0x4E, 0x00],
                    result: Ok(()),
                },
                // 4. Buffer ready + 1 contact + Home key: reads point, decodes, clears status.
                I2cExpectation::WriteRead {
                    addr: PRIMARY_ADDR,
                    write: vec![0x81, 0x4E],
                    read: Ok(vec![STATUS_BUFFER_READY | STATUS_HAVE_KEY | 0x01]),
                },
                I2cExpectation::WriteRead {
                    addr: PRIMARY_ADDR,
                    write: vec![0x81, 0x50],
                    read: Ok(point_record.to_vec()),
                },
                I2cExpectation::Write {
                    addr: PRIMARY_ADDR,
                    data: vec![0x81, 0x4E, 0x00],
                    result: Ok(()),
                },
            ]),
            TouchTransform::X4_PRO_LANDSCAPE,
        );

        assert_eq!(block_on(gt.poll()), Ok(None));
        assert_eq!(
            block_on(gt.poll()),
            Ok(Some(TouchSample {
                count: 0,
                home_key: true,
                point: None,
                raw_point: None,
            }))
        );
        assert_eq!(
            block_on(gt.poll()),
            Ok(Some(TouchSample {
                count: 0,
                home_key: false,
                point: None,
                raw_point: None,
            }))
        );
        assert_eq!(
            block_on(gt.poll()),
            Ok(Some(TouchSample {
                count: 1,
                home_key: true,
                point: Some(TouchPoint { x: 799, y: 0 }),
                raw_point: Some(TouchPoint { x: 479, y: 799 }),
            }))
        );
        gt.i2c.assert_exhausted();
    }

    #[test]
    fn poll_leaves_status_intact_if_point_read_fails() {
        let bus_err = MockI2cError(ErrorKind::Bus);
        let mut gt = Gt911::new(
            ScriptedI2c::new([
                I2cExpectation::WriteRead {
                    addr: PRIMARY_ADDR,
                    write: vec![0x81, 0x4E],
                    read: Ok(vec![STATUS_BUFFER_READY | 0x01]),
                },
                I2cExpectation::WriteRead {
                    addr: PRIMARY_ADDR,
                    write: vec![0x81, 0x50],
                    read: Err(bus_err),
                },
            ]),
            TouchTransform::X4_PRO_LANDSCAPE,
        );
        assert_eq!(block_on(gt.poll()), Err(bus_err));
        gt.i2c.assert_exhausted();
    }

    #[test]
    fn screen_touch_maps_portrait_and_landscape_dash_bars_and_content() {
        // Portrait shell bottom dash icons sit at x=120, 200, 280, 360, y=776.
        assert_eq!(
            classify_screen_touch(
                TouchPoint { x: 120, y: 776 },
                TouchPoint { x: 122, y: 778 },
                TouchLayout::PortraitShell,
            ),
            ScreenTouch::DashSlot(0)
        );
        assert_eq!(
            classify_screen_touch(
                TouchPoint { x: 200, y: 776 },
                TouchPoint { x: 200, y: 776 },
                TouchLayout::PortraitShell,
            ),
            ScreenTouch::DashSlot(1)
        );
        assert_eq!(
            classify_screen_touch(
                TouchPoint { x: 280, y: 776 },
                TouchPoint { x: 280, y: 776 },
                TouchLayout::PortraitShell,
            ),
            ScreenTouch::DashSlot(2)
        );
        assert_eq!(
            classify_screen_touch(
                TouchPoint { x: 360, y: 776 },
                TouchPoint { x: 360, y: 776 },
                TouchLayout::PortraitShell,
            ),
            ScreenTouch::DashSlot(3)
        );

        // Portrait reading: bottom bar acts as thirds unless reading_sheet is open.
        assert_eq!(
            classify_screen_touch(
                TouchPoint { x: 400, y: 776 },
                TouchPoint { x: 400, y: 776 },
                TouchLayout::PortraitReading {
                    reading_sheet: false
                },
            ),
            ScreenTouch::Action(TouchAction::Next)
        );
        assert_eq!(
            classify_screen_touch(
                TouchPoint { x: 120, y: 776 },
                TouchPoint { x: 120, y: 776 },
                TouchLayout::PortraitReading {
                    reading_sheet: true
                },
            ),
            ScreenTouch::DashSlot(0)
        );

        // Landscape left and right rails hit slots 0..3 vertically.
        assert_eq!(
            classify_screen_touch(
                TouchPoint { x: 60, y: 200 },
                TouchPoint { x: 60, y: 200 },
                TouchLayout::LandscapeLeftRail,
            ),
            ScreenTouch::DashSlot(1)
        );
        assert_eq!(
            classify_screen_touch(
                TouchPoint { x: 740, y: 360 },
                TouchPoint { x: 740, y: 360 },
                TouchLayout::LandscapeRightRail,
            ),
            ScreenTouch::DashSlot(3)
        );

        // Swipes override tap zones.
        assert_eq!(
            classify_screen_touch(
                TouchPoint { x: 300, y: 776 },
                TouchPoint { x: 200, y: 776 },
                TouchLayout::PortraitShell,
            ),
            ScreenTouch::Action(TouchAction::Next)
        );
    }
}
