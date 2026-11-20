//! BM8563 (PCF8563 register-compatible) real-time clock — used on the Xteink X4 Pro.
//!
//! Generic over any async `I2c` bus so it remains MCU-neutral and host-testable.

use embedded_hal_async::i2c::I2c;

/// Default 7-bit I2C address of the BM8563 / PCF8563.
pub const DEFAULT_ADDR: u8 = 0x51;

const REG_CONTROL_STATUS1: u8 = 0x00;
const REG_TIME: u8 = 0x02;
const REG_CLKOUT: u8 = 0x0D;

const CLKOUT_DISABLED: u8 = 0x00;
const VL_FLAG: u8 = 0x80;
const CENTURY_FLAG: u8 = 0x80;

/// Calendar date and time represented by the BM8563 timekeeping registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcDateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub weekday: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Converts a packed two-nibble BCD byte into decimal.
pub const fn bcd_to_dec(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 0x0F)
}

/// Converts a decimal value (`0..=99`) into packed two-nibble BCD.
pub const fn dec_to_bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

const fn decode_bcd_checked(v: u8) -> Option<u8> {
    let hi = v >> 4;
    let lo = v & 0x0F;
    if hi > 9 || lo > 9 {
        None
    } else {
        Some(hi * 10 + lo)
    }
}

const fn is_leap_year(year: u16) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

const fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Decodes the 7 BM8563 time registers (`0x02..=0x08`: seconds, minutes, hours,
/// days, weekdays, months/century, years).
///
/// Returns `None` when `VL_FLAG` (`0x80` in seconds) is set, any BCD nibble is
/// above `9`, or any decoded field falls outside valid clock bounds. Handles the
/// century bit (`0x80` in month: `1` = 1900s, `0` = 2000s).
pub fn decode_time_regs(raw: &[u8; 7]) -> Option<RtcDateTime> {
    if (raw[0] & VL_FLAG) != 0 {
        return None;
    }

    let second = decode_bcd_checked(raw[0] & 0x7F)?;
    let minute = decode_bcd_checked(raw[1] & 0x7F)?;
    let hour = decode_bcd_checked(raw[2] & 0x3F)?;
    let day = decode_bcd_checked(raw[3] & 0x3F)?;
    let weekday = decode_bcd_checked(raw[4] & 0x07)?;
    let month = decode_bcd_checked(raw[5] & 0x1F)?;
    let yy = decode_bcd_checked(raw[6])?;

    if second > 59 || minute > 59 || hour > 23 || day == 0 || day > 31 || weekday > 6 {
        return None;
    }
    if month == 0 || month > 12 {
        return None;
    }

    let base_year = if (raw[5] & CENTURY_FLAG) != 0 {
        1900u16
    } else {
        2000u16
    };
    let year = base_year + u16::from(yy);
    if day > days_in_month(year, month) {
        return None;
    }
    Some(RtcDateTime {
        year,
        month,
        day,
        weekday,
        hour,
        minute,
        second,
    })
}

/// Encodes `dt` into the 7 BM8563 time registers (`0x02..=0x08`), returning
/// `None` if any field is out of range (`year` must be in `1900..=2099`).
pub fn encode_time_regs(dt: &RtcDateTime) -> Option<[u8; 7]> {
    if !(1900..=2099).contains(&dt.year)
        || dt.month == 0
        || dt.month > 12
        || dt.day == 0
        || dt.day > days_in_month(dt.year, dt.month)
        || dt.weekday > 6
        || dt.hour > 23
        || dt.minute > 59
        || dt.second > 59
    {
        return None;
    }

    let century_bit = if dt.year < 2000 { CENTURY_FLAG } else { 0x00 };
    let yy = (dt.year % 100) as u8;
    Some([
        dec_to_bcd(dt.second),
        dec_to_bcd(dt.minute),
        dec_to_bcd(dt.hour),
        dec_to_bcd(dt.day),
        dec_to_bcd(dt.weekday),
        dec_to_bcd(dt.month) | century_bit,
        dec_to_bcd(yy),
    ])
}

/// Async driver for the BM8563 / PCF8563 RTC.
pub struct Bm8563<I> {
    i2c: I,
    addr: u8,
}

impl<I> Bm8563<I> {
    /// Creates a driver targeting [`DEFAULT_ADDR`].
    pub const fn new(i2c: I) -> Self {
        Self::with_addr(i2c, DEFAULT_ADDR)
    }

    /// Creates a driver targeting a custom 7-bit I2C address.
    pub const fn with_addr(i2c: I, addr: u8) -> Self {
        Self { i2c, addr }
    }
}

impl<I: I2c> Bm8563<I> {
    /// Reads `CONTROL_STATUS1` (`0x00`) and disables `CLKOUT` (`0x0D = 0x00`),
    /// returning the status byte read before disabling `CLKOUT`.
    pub async fn init(&mut self) -> Result<u8, I::Error> {
        let mut status = [0u8; 1];
        self.i2c
            .write_read(self.addr, &[REG_CONTROL_STATUS1], &mut status)
            .await?;
        self.i2c
            .write(self.addr, &[REG_CLKOUT, CLKOUT_DISABLED])
            .await?;
        Ok(status[0])
    }

    /// Reads the 7 timekeeping registers and decodes them, returning `Ok(None)`
    /// if the oscillator voltage-low flag (`VL_FLAG`) is set or fields are invalid.
    pub async fn read_time(&mut self) -> Result<Option<RtcDateTime>, I::Error> {
        let mut raw = [0u8; 7];
        self.i2c
            .write_read(self.addr, &[REG_TIME], &mut raw)
            .await?;
        Ok(decode_time_regs(&raw))
    }

    /// Encodes and writes `dt` starting at `0x02` (clearing `VL_FLAG`).
    /// Returns `Ok(false)` without touching the bus when `dt` has invalid fields.
    pub async fn write_time(&mut self, dt: &RtcDateTime) -> Result<bool, I::Error> {
        let Some(regs) = encode_time_regs(dt) else {
            return Ok(false);
        };
        let payload = [
            REG_TIME, regs[0], regs[1], regs[2], regs[3], regs[4], regs[5], regs[6],
        ];
        self.i2c.write(self.addr, &payload).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{block_on, I2cExpectation, MockI2cError, ScriptedI2c};
    use embedded_hal_async::i2c::{ErrorKind, NoAcknowledgeSource};
    use std::vec;

    #[test]
    fn bcd_conversion_round_trips() {
        for v in 0..=99 {
            let bcd = dec_to_bcd(v);
            assert_eq!(bcd_to_dec(bcd), v);
        }
        assert_eq!(dec_to_bcd(59), 0x59);
        assert_eq!(bcd_to_dec(0x26), 26);
    }

    #[test]
    fn decode_and_encode_round_trip_both_centuries() {
        let dt_2000s = RtcDateTime {
            year: 2026,
            month: 11,
            day: 17,
            weekday: 2,
            hour: 5,
            minute: 42,
            second: 9,
        };
        let regs_2000s = encode_time_regs(&dt_2000s).expect("valid 2026 datetime");
        assert_eq!(regs_2000s, [0x09, 0x42, 0x05, 0x17, 0x02, 0x11, 0x26]);
        assert_eq!(decode_time_regs(&regs_2000s), Some(dt_2000s));

        let dt_1900s = RtcDateTime {
            year: 1999,
            month: 12,
            day: 31,
            weekday: 5,
            hour: 23,
            minute: 59,
            second: 59,
        };
        let regs_1900s = encode_time_regs(&dt_1900s).expect("valid 1999 datetime");
        assert_eq!(
            regs_1900s,
            [0x59, 0x59, 0x23, 0x31, 0x05, 0x12 | CENTURY_FLAG, 0x99]
        );
        assert_eq!(decode_time_regs(&regs_1900s), Some(dt_1900s));
    }

    #[test]
    fn rejects_vl_flag_invalid_bcd_and_out_of_range_fields() {
        let valid = [0x09, 0x42, 0x05, 0x17, 0x02, 0x11, 0x26];
        assert!(decode_time_regs(&valid).is_some());

        // VL_FLAG set in seconds register.
        let mut vl = valid;
        vl[0] |= VL_FLAG;
        assert_eq!(decode_time_regs(&vl), None);

        // Unused upper bits in minutes/hours/days/weekdays/months are masked cleanly.
        let masked_unused = [
            0x09,
            0x80 | 0x42,
            0xC0 | 0x05,
            0xC0 | 0x17,
            0xF8 | 0x02,
            0x60 | 0x11,
            0x26,
        ];
        assert_eq!(decode_time_regs(&masked_unused), decode_time_regs(&valid));

        // Invalid BCD nibbles (A..=F).
        let invalid_bcd_cases = [
            [0x1A, 0x00, 0x00, 0x01, 0x00, 0x01, 0x26], // second low nibble A
            [0x00, 0x1A, 0x00, 0x01, 0x00, 0x01, 0x26], // minute low nibble A
            [0x00, 0x00, 0x1F, 0x01, 0x00, 0x01, 0x26], // hour low nibble F
            [0x00, 0x00, 0x00, 0x1C, 0x00, 0x01, 0x26], // day low nibble C
            [0x00, 0x00, 0x00, 0x01, 0x00, 0x0A, 0x26], // month low nibble A
            [0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0xA0], // year high nibble A
            [0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0xAF], // year invalid BCD
        ];
        for raw in invalid_bcd_cases {
            assert_eq!(decode_time_regs(&raw), None, "expected None for {raw:02X?}");
        }

        // Out-of-range valid-BCD fields.
        let out_of_range_raw = [
            [0x60, 0x00, 0x00, 0x01, 0x00, 0x01, 0x26], // sec = 60
            [0x00, 0x60, 0x00, 0x01, 0x00, 0x01, 0x26], // min = 60
            [0x00, 0x00, 0x24, 0x01, 0x00, 0x01, 0x26], // hour = 24
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x26], // day = 0
            [0x00, 0x00, 0x00, 0x32, 0x00, 0x01, 0x26], // day = 32
            [0x00, 0x00, 0x00, 0x01, 0x07, 0x01, 0x26], // weekday = 7
            [0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x26], // month = 0
            [0x00, 0x00, 0x00, 0x01, 0x00, 0x13, 0x26], // month = 13
        ];
        for raw in out_of_range_raw {
            assert_eq!(decode_time_regs(&raw), None, "expected None for {raw:02X?}");
        }

        // Encode rejects out-of-range fields including unsupported years.
        let base = RtcDateTime {
            year: 2026,
            month: 1,
            day: 1,
            weekday: 4,
            hour: 12,
            minute: 0,
            second: 0,
        };
        let invalid_dts = [
            RtcDateTime { year: 1899, ..base },
            RtcDateTime { year: 2100, ..base },
            RtcDateTime { month: 0, ..base },
            RtcDateTime { month: 13, ..base },
            RtcDateTime { day: 0, ..base },
            RtcDateTime { day: 32, ..base },
            RtcDateTime { weekday: 7, ..base },
            RtcDateTime { hour: 24, ..base },
            RtcDateTime { minute: 60, ..base },
            RtcDateTime { second: 60, ..base },
        ];
        for dt in invalid_dts {
            assert_eq!(encode_time_regs(&dt), None, "expected None for {dt:?}");
        }
    }

    #[test]
    fn init_read_time_and_write_time_over_i2c() {
        let dt = RtcDateTime {
            year: 2026,
            month: 11,
            day: 17,
            weekday: 2,
            hour: 14,
            minute: 35,
            second: 8,
        };
        let expected_regs = encode_time_regs(&dt).unwrap();
        let mut write_payload = vec![REG_TIME];
        write_payload.extend_from_slice(&expected_regs);

        let i2c = ScriptedI2c::new([
            I2cExpectation::WriteRead {
                addr: DEFAULT_ADDR,
                write: vec![REG_CONTROL_STATUS1],
                read: Ok(vec![0x08]),
            },
            I2cExpectation::Write {
                addr: DEFAULT_ADDR,
                data: vec![REG_CLKOUT, CLKOUT_DISABLED],
                result: Ok(()),
            },
            I2cExpectation::Write {
                addr: DEFAULT_ADDR,
                data: write_payload,
                result: Ok(()),
            },
            I2cExpectation::WriteRead {
                addr: DEFAULT_ADDR,
                write: vec![REG_TIME],
                read: Ok(expected_regs.to_vec()),
            },
            I2cExpectation::WriteRead {
                addr: DEFAULT_ADDR,
                write: vec![REG_TIME],
                read: Ok(vec![VL_FLAG, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00]),
            },
        ]);
        let mut rtc = Bm8563::new(i2c);
        assert_eq!(block_on(rtc.init()), Ok(0x08));
        assert_eq!(block_on(rtc.write_time(&dt)), Ok(true));
        assert_eq!(block_on(rtc.read_time()), Ok(Some(dt)));
        assert_eq!(block_on(rtc.read_time()), Ok(None));

        // Invalid datetime returns Ok(false) without issuing any I2C write.
        let invalid = RtcDateTime { year: 2100, ..dt };
        assert_eq!(block_on(rtc.write_time(&invalid)), Ok(false));
        rtc.i2c.assert_exhausted();
    }

    #[test]
    fn propagates_i2c_errors() {
        let bus_err = MockI2cError(ErrorKind::Bus);
        let nack_err = MockI2cError(ErrorKind::NoAcknowledge(NoAcknowledgeSource::Address));

        let mut rtc = Bm8563::with_addr(
            ScriptedI2c::new([I2cExpectation::WriteRead {
                addr: 0x52,
                write: vec![REG_CONTROL_STATUS1],
                read: Err(nack_err),
            }]),
            0x52,
        );
        assert_eq!(block_on(rtc.init()), Err(nack_err));
        rtc.i2c.assert_exhausted();

        let mut rtc = Bm8563::new(ScriptedI2c::new([
            I2cExpectation::WriteRead {
                addr: DEFAULT_ADDR,
                write: vec![REG_CONTROL_STATUS1],
                read: Ok(vec![0x00]),
            },
            I2cExpectation::Write {
                addr: DEFAULT_ADDR,
                data: vec![REG_CLKOUT, CLKOUT_DISABLED],
                result: Err(bus_err),
            },
        ]));
        assert_eq!(block_on(rtc.init()), Err(bus_err));
        rtc.i2c.assert_exhausted();

        let mut rtc = Bm8563::new(ScriptedI2c::new([I2cExpectation::WriteRead {
            addr: DEFAULT_ADDR,
            write: vec![REG_TIME],
            read: Err(bus_err),
        }]));
        assert_eq!(block_on(rtc.read_time()), Err(bus_err));
        rtc.i2c.assert_exhausted();

        let dt = RtcDateTime {
            year: 2026,
            month: 1,
            day: 2,
            weekday: 5,
            hour: 3,
            minute: 4,
            second: 5,
        };
        let mut payload = vec![REG_TIME];
        payload.extend_from_slice(&encode_time_regs(&dt).unwrap());
        let mut rtc = Bm8563::new(ScriptedI2c::new([I2cExpectation::Write {
            addr: DEFAULT_ADDR,
            data: payload,
            result: Err(bus_err),
        }]));
        assert_eq!(block_on(rtc.write_time(&dt)), Err(bus_err));
        rtc.i2c.assert_exhausted();
    }

    #[test]
    fn calendar_day_bounds_reject_impossible_dates_and_accept_leap_day() {
        let leap_ok = RtcDateTime {
            year: 2024,
            month: 2,
            day: 29,
            weekday: 4,
            hour: 12,
            minute: 0,
            second: 0,
        };
        let regs = encode_time_regs(&leap_ok).expect("Feb 29, 2024 is valid");
        assert_eq!(decode_time_regs(&regs), Some(leap_ok));

        let non_leap = RtcDateTime {
            year: 2026,
            ..leap_ok
        };
        assert_eq!(encode_time_regs(&non_leap), None);
        assert_eq!(
            decode_time_regs(&[0x00, 0x00, 0x12, 0x29, 0x00, 0x02, 0x26]),
            None
        );
        assert_eq!(
            decode_time_regs(&[0x00, 0x00, 0x12, 0x31, 0x00, 0x02, 0x26]),
            None
        );
        assert_eq!(
            decode_time_regs(&[0x00, 0x00, 0x12, 0x31, 0x00, 0x04, 0x26]),
            None
        );
    }
}
