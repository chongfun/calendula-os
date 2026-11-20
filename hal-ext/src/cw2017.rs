//! CellWise CW2017 battery fuel gauge — used on the Xteink X4 Pro.
//!
//! Unlike the TI BQ27220, the CW2017 reports 0% state of charge until a matching
//! 80-byte `BATINFO` cell profile is resident and its update flag is set.
//! Generic over any async `I2c` bus and `DelayNs` timer so it remains
//! MCU-neutral and host-testable.

use embedded_hal_async::delay::DelayNs;
use embedded_hal_async::i2c::I2c;

/// Default 7-bit I2C address of the CW2017.
pub const DEFAULT_ADDR: u8 = 0x63;

const REG_VERSION: u8 = 0x00;
const REG_VCELL_H: u8 = 0x02;
const REG_VCELL_L: u8 = 0x03;
const REG_SOC: u8 = 0x04;
const REG_MODE: u8 = 0x08;
const REG_SOC_ALERT: u8 = 0x0B;
const REG_BATINFO: u8 = 0x10;

const MODE_NORMAL: u8 = 0x00;
const MODE_RESTART: u8 = 0x30;
const MODE_DEFAULT: u8 = 0xF0;
const UPDATE_FLAG: u8 = 0x80;

const RESET_STEP_DELAY_MS: u32 = 20;
const VERSION_POLL_ATTEMPTS: usize = 50;
const VERSION_POLL_DELAY_MS: u32 = 20;
const SOC_POLL_ATTEMPTS: usize = 30;
const SOC_POLL_DELAY_MS: u32 = 100;

/// Exact 80-byte `BATINFO` cell profile uploaded by the Xteink X4 Pro OEM
/// firmware (from FreeInk `BatteryMonitor.cpp`).
pub const X4_PRO_BATINFO: [u8; 80] = [
    0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xaa, 0xbf, 0xb5, 0xb4, 0xa4, 0x9c, 0xeb, 0xe2,
    0xdf, 0xe5, 0xca, 0xa0, 0x8a, 0x62, 0x53, 0x48, 0x40, 0x3a, 0x32, 0xb1, 0xae, 0xda, 0xb5, 0xff,
    0xff, 0xff, 0xe8, 0xdb, 0xd9, 0xd6, 0xd4, 0xd2, 0xd0, 0xcb, 0xc3, 0xbc, 0x9e, 0x87, 0x7b, 0x71,
    0x72, 0x7c, 0x8c, 0xa3, 0xb7, 0xc8, 0xa5, 0x4f, 0x00, 0x00, 0xab, 0x02, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x23,
];

/// Returns `true` when `REG_VERSION` reports a running calculation engine
/// (`0x0D` or `0x0F`, as opposed to startup `0xA0`).
pub const fn version_is_running(version: u8) -> bool {
    (version & 0xFD) == 0x0D
}

/// Converts the 14-bit big-endian `VCELL` register pair (`0x02`, `0x03`) into
/// millivolts using the rounded `0.3125 mV/LSB` formula.
pub const fn decode_millivolts(hi: u8, lo: u8) -> u16 {
    let raw14 = ((hi as u32 & 0x3F) << 8) | lo as u32;
    ((raw14 * 5 + 8) >> 4) as u16
}

/// Async driver for the CW2017 battery fuel gauge.
pub struct Cw2017<I> {
    i2c: I,
    addr: u8,
}

impl<I> Cw2017<I> {
    /// Creates a driver targeting [`DEFAULT_ADDR`].
    pub const fn new(i2c: I) -> Self {
        Self::with_addr(i2c, DEFAULT_ADDR)
    }

    /// Creates a driver targeting a custom 7-bit I2C address.
    pub const fn with_addr(i2c: I, addr: u8) -> Self {
        Self { i2c, addr }
    }
}

impl<I: I2c> Cw2017<I> {
    async fn read_reg(&mut self, reg: u8) -> Result<u8, I::Error> {
        let mut buf = [0u8; 1];
        self.i2c.write_read(self.addr, &[reg], &mut buf).await?;
        Ok(buf[0])
    }

    async fn write_reg(&mut self, reg: u8, val: u8) -> Result<(), I::Error> {
        self.i2c.write(self.addr, &[reg, val]).await
    }

    async fn soft_reset<D: DelayNs>(&mut self, delay: &mut D) -> Result<(), I::Error> {
        self.write_reg(REG_MODE, MODE_DEFAULT).await?;
        delay.delay_ms(RESET_STEP_DELAY_MS).await;
        self.write_reg(REG_MODE, MODE_RESTART).await?;
        delay.delay_ms(RESET_STEP_DELAY_MS).await;
        self.write_reg(REG_MODE, MODE_NORMAL).await?;
        delay.delay_ms(RESET_STEP_DELAY_MS).await;
        Ok(())
    }

    async fn wait_until_ready<D: DelayNs>(&mut self, delay: &mut D) -> Result<bool, I::Error> {
        let mut version_ready = false;
        for _ in 0..VERSION_POLL_ATTEMPTS {
            let version = self.read_reg(REG_VERSION).await?;
            if version_is_running(version) {
                version_ready = true;
                break;
            }
            delay.delay_ms(VERSION_POLL_DELAY_MS).await;
        }
        if !version_ready {
            return Ok(false);
        }

        for _ in 0..SOC_POLL_ATTEMPTS {
            let soc = self.read_reg(REG_SOC).await?;
            if soc <= 100 {
                return Ok(true);
            }
            delay.delay_ms(SOC_POLL_DELAY_MS).await;
        }
        Ok(false)
    }

    /// Checks whether the profile-update flag is set and every resident
    /// `BATINFO` byte matches `profile`.
    pub async fn profile_matches(&mut self, profile: &[u8; 80]) -> Result<bool, I::Error> {
        let config = self.read_reg(REG_SOC_ALERT).await?;
        if (config & UPDATE_FLAG) == 0 {
            return Ok(false);
        }

        for (i, &expected) in profile.iter().enumerate() {
            let stored = self.read_reg(REG_BATINFO + i as u8).await?;
            if stored != expected {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Ensures the gauge is running in normal mode with `profile` loaded and a
    /// valid SoC ready, uploading the profile and/or performing a soft reset if
    /// needed. Returns `Ok(false)` if the chip stays unready past the bounded
    /// poll limits; propagates I2C failures so callers can retry cleanly.
    pub async fn ensure_profile<D: DelayNs>(
        &mut self,
        delay: &mut D,
        profile: &[u8; 80],
    ) -> Result<bool, I::Error> {
        let mode = self.read_reg(REG_MODE).await?;
        let version = self.read_reg(REG_VERSION).await?;
        let matches = self.profile_matches(profile).await?;

        let mut restart_required = mode != MODE_NORMAL || !version_is_running(version);
        if !matches {
            // Enter non-normal mode before uploading so a failed reset leaves
            // a durable restart requirement visible to subsequent retries.
            if mode == MODE_NORMAL {
                self.write_reg(REG_MODE, MODE_DEFAULT).await?;
            }
            for (i, &byte) in profile.iter().enumerate() {
                self.write_reg(REG_BATINFO + i as u8, byte).await?;
            }
            // Preserve the lower 7-bit SOC alert threshold when setting UPDATE_FLAG.
            let alert = self.read_reg(REG_SOC_ALERT).await?;
            self.write_reg(REG_SOC_ALERT, alert | UPDATE_FLAG).await?;
            delay.delay_ms(RESET_STEP_DELAY_MS).await;
            restart_required = true;
        }

        if restart_required {
            self.soft_reset(delay).await?;
            return self.wait_until_ready(delay).await;
        }

        let soc = self.read_reg(REG_SOC).await?;
        if soc <= 100 {
            return Ok(true);
        }

        self.soft_reset(delay).await?;
        self.wait_until_ready(delay).await
    }

    /// Reads the integer state of charge (`0..=100`), verifying `MODE == 0`,
    /// a running `VERSION`, and `SOC <= 100`. Returns `Ok(None)` if not ready.
    pub async fn read_soc(&mut self) -> Result<Option<u8>, I::Error> {
        let mode = self.read_reg(REG_MODE).await?;
        if mode != MODE_NORMAL {
            return Ok(None);
        }
        let version = self.read_reg(REG_VERSION).await?;
        if !version_is_running(version) {
            return Ok(None);
        }
        let soc = self.read_reg(REG_SOC).await?;
        if soc > 100 {
            return Ok(None);
        }
        Ok(Some(soc))
    }

    /// Reads the battery terminal voltage in millivolts from `VCELL` (`0x02..=0x03`).
    pub async fn read_millivolts(&mut self) -> Result<u16, I::Error> {
        let hi = self.read_reg(REG_VCELL_H).await?;
        let lo = self.read_reg(REG_VCELL_L).await?;
        Ok(decode_millivolts(hi, lo))
    }

    /// Reads both battery voltage (mV) and SoC (%) when the gauge is ready,
    /// returning `Ok(None)` when the gauge is not in a ready running state.
    pub async fn sample(&mut self) -> Result<Option<(u16, u8)>, I::Error> {
        let Some(soc) = self.read_soc().await? else {
            return Ok(None);
        };
        let mv = self.read_millivolts().await?;
        Ok(Some((mv, soc)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        block_on, I2cExpectation, MockDelay, MockI2cError, ScriptedI2c, Timeline, TimelineEvent,
    };
    use embedded_hal_async::i2c::{ErrorKind, NoAcknowledgeSource};
    use std::vec;
    use std::vec::Vec;

    fn read_exp(reg: u8, val: u8) -> I2cExpectation {
        I2cExpectation::WriteRead {
            addr: DEFAULT_ADDR,
            write: vec![reg],
            read: Ok(vec![val]),
        }
    }

    fn write_exp(reg: u8, val: u8) -> I2cExpectation {
        I2cExpectation::Write {
            addr: DEFAULT_ADDR,
            data: vec![reg, val],
            result: Ok(()),
        }
    }

    #[test]
    fn helpers_decode_version_and_millivolts() {
        assert!(version_is_running(0x0D));
        assert!(version_is_running(0x0F));
        assert!(!version_is_running(0xA0));
        assert!(!version_is_running(0x00));

        assert_eq!(decode_millivolts(0x00, 0x00), 0);
        // 12800 * 0.3125 mV = 4000 mV; upper two bits of hi are ignored.
        assert_eq!(decode_millivolts(0x32, 0x00), 4000);
        assert_eq!(decode_millivolts(0xF2, 0x00), 4000);
        // 13440 * 0.3125 mV = 4200 mV.
        assert_eq!(decode_millivolts(0x34, 0x80), 4200);
        // Max 14-bit value 0x3FFF = 16383 -> (16383 * 5 + 8) >> 4 = 5120 mV.
        assert_eq!(decode_millivolts(0xFF, 0xFF), 5120);
    }

    #[test]
    fn profile_matches_true_and_early_mismatches() {
        // Full match: UPDATE_FLAG set + all 80 BATINFO bytes match.
        let mut exps = vec![read_exp(REG_SOC_ALERT, UPDATE_FLAG)];
        for (i, &b) in X4_PRO_BATINFO.iter().enumerate() {
            exps.push(read_exp(REG_BATINFO + i as u8, b));
        }
        let i2c = ScriptedI2c::new(exps);
        let mut gauge = Cw2017::new(i2c);
        assert_eq!(block_on(gauge.profile_matches(&X4_PRO_BATINFO)), Ok(true));
        gauge.i2c.assert_exhausted();

        // Missing update flag stops immediately before reading BATINFO.
        let i2c = ScriptedI2c::new([read_exp(REG_SOC_ALERT, 0x00)]);
        let mut gauge = Cw2017::new(i2c);
        assert_eq!(block_on(gauge.profile_matches(&X4_PRO_BATINFO)), Ok(false));
        gauge.i2c.assert_exhausted();

        // Byte mismatch stops on first differing byte.
        let i2c = ScriptedI2c::new([
            read_exp(REG_SOC_ALERT, UPDATE_FLAG | 0x05),
            read_exp(REG_BATINFO, X4_PRO_BATINFO[0]),
            read_exp(REG_BATINFO + 1, X4_PRO_BATINFO[1] ^ 0xFF),
        ]);
        let mut gauge = Cw2017::new(i2c);
        assert_eq!(block_on(gauge.profile_matches(&X4_PRO_BATINFO)), Ok(false));
        gauge.i2c.assert_exhausted();
    }

    #[test]
    fn ensure_profile_skips_upload_and_reset_when_already_running() {
        let mut exps = vec![
            read_exp(REG_MODE, MODE_NORMAL),
            read_exp(REG_VERSION, 0x0D),
            read_exp(REG_SOC_ALERT, UPDATE_FLAG),
        ];
        for (i, &b) in X4_PRO_BATINFO.iter().enumerate() {
            exps.push(read_exp(REG_BATINFO + i as u8, b));
        }
        exps.push(read_exp(REG_SOC, 74));

        let i2c = ScriptedI2c::new(exps);
        let mut delay = MockDelay::new();
        let mut gauge = Cw2017::new(i2c);
        assert_eq!(
            block_on(gauge.ensure_profile(&mut delay, &X4_PRO_BATINFO)),
            Ok(true)
        );
        assert!(delay.delays_ms.is_empty());
        gauge.i2c.assert_exhausted();
    }

    #[test]
    fn ensure_profile_uploads_resets_and_polls_until_ready() {
        let timeline = Timeline::new();
        let mut exps = vec![
            read_exp(REG_MODE, MODE_NORMAL),
            read_exp(REG_VERSION, 0xA0),
            read_exp(REG_SOC_ALERT, 0x14),
            write_exp(REG_MODE, MODE_DEFAULT),
        ];
        for (i, &b) in X4_PRO_BATINFO.iter().enumerate() {
            exps.push(write_exp(REG_BATINFO + i as u8, b));
        }
        exps.push(read_exp(REG_SOC_ALERT, 0x14));
        exps.push(write_exp(REG_SOC_ALERT, 0x14 | UPDATE_FLAG));
        // Soft reset: 0xF0 -> 20ms -> 0x30 -> 20ms -> 0x00 -> 20ms.
        exps.push(write_exp(REG_MODE, MODE_DEFAULT));
        exps.push(write_exp(REG_MODE, MODE_RESTART));
        exps.push(write_exp(REG_MODE, MODE_NORMAL));
        // Version polls: 0xA0 twice, then 0x0F.
        exps.push(read_exp(REG_VERSION, 0xA0));
        exps.push(read_exp(REG_VERSION, 0xA0));
        exps.push(read_exp(REG_VERSION, 0x0F));
        // SoC polls: > 100 twice, then 83.
        exps.push(read_exp(REG_SOC, 255));
        exps.push(read_exp(REG_SOC, 101));
        exps.push(read_exp(REG_SOC, 83));

        let i2c = ScriptedI2c::with_timeline(exps, timeline.clone());
        let mut delay = MockDelay::with_timeline(timeline.clone());
        let mut gauge = Cw2017::new(i2c);

        assert_eq!(
            block_on(gauge.ensure_profile(&mut delay, &X4_PRO_BATINFO)),
            Ok(true)
        );
        gauge.i2c.assert_exhausted();

        // Check exact interleaved reset + poll sequence at the tail of the timeline.
        let events = timeline.snapshot();
        let tail_start = events.len() - 18;
        assert_eq!(
            events[tail_start..],
            [
                TimelineEvent::Write {
                    addr: DEFAULT_ADDR,
                    data: vec![REG_SOC_ALERT, 0x14 | UPDATE_FLAG],
                },
                TimelineEvent::DelayMs(20),
                TimelineEvent::Write {
                    addr: DEFAULT_ADDR,
                    data: vec![REG_MODE, MODE_DEFAULT],
                },
                TimelineEvent::DelayMs(20),
                TimelineEvent::Write {
                    addr: DEFAULT_ADDR,
                    data: vec![REG_MODE, MODE_RESTART],
                },
                TimelineEvent::DelayMs(20),
                TimelineEvent::Write {
                    addr: DEFAULT_ADDR,
                    data: vec![REG_MODE, MODE_NORMAL],
                },
                TimelineEvent::DelayMs(20),
                TimelineEvent::WriteRead {
                    addr: DEFAULT_ADDR,
                    write: vec![REG_VERSION],
                },
                TimelineEvent::DelayMs(20),
                TimelineEvent::WriteRead {
                    addr: DEFAULT_ADDR,
                    write: vec![REG_VERSION],
                },
                TimelineEvent::DelayMs(20),
                TimelineEvent::WriteRead {
                    addr: DEFAULT_ADDR,
                    write: vec![REG_VERSION],
                },
                TimelineEvent::WriteRead {
                    addr: DEFAULT_ADDR,
                    write: vec![REG_SOC],
                },
                TimelineEvent::DelayMs(100),
                TimelineEvent::WriteRead {
                    addr: DEFAULT_ADDR,
                    write: vec![REG_SOC],
                },
                TimelineEvent::DelayMs(100),
                TimelineEvent::WriteRead {
                    addr: DEFAULT_ADDR,
                    write: vec![REG_SOC],
                },
            ]
        );
    }

    #[test]
    fn ensure_profile_returns_false_when_version_or_soc_times_out() {
        // Version stuck at 0xA0 across all 50 polls after matching profile + non-normal mode.
        let mut exps = vec![
            read_exp(REG_MODE, 0xC0),
            read_exp(REG_VERSION, 0x0D),
            read_exp(REG_SOC_ALERT, UPDATE_FLAG),
        ];
        for (i, &b) in X4_PRO_BATINFO.iter().enumerate() {
            exps.push(read_exp(REG_BATINFO + i as u8, b));
        }
        exps.extend([
            write_exp(REG_MODE, MODE_DEFAULT),
            write_exp(REG_MODE, MODE_RESTART),
            write_exp(REG_MODE, MODE_NORMAL),
        ]);
        for _ in 0..VERSION_POLL_ATTEMPTS {
            exps.push(read_exp(REG_VERSION, 0xA0));
        }
        let mut delay = MockDelay::new();
        let mut gauge = Cw2017::new(ScriptedI2c::new(exps));
        assert_eq!(
            block_on(gauge.ensure_profile(&mut delay, &X4_PRO_BATINFO)),
            Ok(false)
        );
        let mut expected_delays = vec![20, 20, 20];
        expected_delays.resize(3 + VERSION_POLL_ATTEMPTS, VERSION_POLL_DELAY_MS);
        assert_eq!(delay.delays_ms, expected_delays);
        gauge.i2c.assert_exhausted();

        // Running gauge with SoC > 100 triggers controlled reset; all 30 SoC polls stay > 100.
        let mut exps = vec![
            read_exp(REG_MODE, MODE_NORMAL),
            read_exp(REG_VERSION, 0x0D),
            read_exp(REG_SOC_ALERT, UPDATE_FLAG),
        ];
        for (i, &b) in X4_PRO_BATINFO.iter().enumerate() {
            exps.push(read_exp(REG_BATINFO + i as u8, b));
        }
        exps.extend([
            read_exp(REG_SOC, 150),
            write_exp(REG_MODE, MODE_DEFAULT),
            write_exp(REG_MODE, MODE_RESTART),
            write_exp(REG_MODE, MODE_NORMAL),
            read_exp(REG_VERSION, 0x0D),
        ]);
        for _ in 0..SOC_POLL_ATTEMPTS {
            exps.push(read_exp(REG_SOC, 101));
        }
        let mut delay = MockDelay::new();
        let mut gauge = Cw2017::new(ScriptedI2c::new(exps));
        assert_eq!(
            block_on(gauge.ensure_profile(&mut delay, &X4_PRO_BATINFO)),
            Ok(false)
        );
        let mut expected_delays = vec![20, 20, 20];
        expected_delays.resize(3 + SOC_POLL_ATTEMPTS, SOC_POLL_DELAY_MS);
        assert_eq!(delay.delays_ms, expected_delays);
        gauge.i2c.assert_exhausted();
    }

    #[test]
    fn read_soc_millivolts_and_sample_handle_not_ready_and_ready() {
        // MODE != 0 -> None
        let mut gauge = Cw2017::new(ScriptedI2c::new([read_exp(REG_MODE, 0xC0)]));
        assert_eq!(block_on(gauge.read_soc()), Ok(None));
        gauge.i2c.assert_exhausted();

        // VERSION == 0xA0 -> None
        let mut gauge = Cw2017::new(ScriptedI2c::new([
            read_exp(REG_MODE, MODE_NORMAL),
            read_exp(REG_VERSION, 0xA0),
        ]));
        assert_eq!(block_on(gauge.read_soc()), Ok(None));
        gauge.i2c.assert_exhausted();

        // SOC > 100 -> None
        let mut gauge = Cw2017::new(ScriptedI2c::new([
            read_exp(REG_MODE, MODE_NORMAL),
            read_exp(REG_VERSION, 0x0D),
            read_exp(REG_SOC, 101),
        ]));
        assert_eq!(block_on(gauge.sample()), Ok(None));
        gauge.i2c.assert_exhausted();

        // Ready -> sample returns (mv, soc), using custom address.
        let custom_addr = 0x64;
        let custom_exps = [
            (REG_MODE, MODE_NORMAL),
            (REG_VERSION, 0x0F),
            (REG_SOC, 65),
            (REG_VCELL_H, 0x34),
            (REG_VCELL_L, 0x80),
        ]
        .into_iter()
        .map(|(reg, val)| I2cExpectation::WriteRead {
            addr: custom_addr,
            write: vec![reg],
            read: Ok(vec![val]),
        })
        .collect::<Vec<_>>();
        let mut gauge = Cw2017::with_addr(ScriptedI2c::new(custom_exps), custom_addr);
        assert_eq!(block_on(gauge.sample()), Ok(Some((4200, 65))));
        gauge.i2c.assert_exhausted();
    }

    #[test]
    fn propagates_i2c_errors() {
        let bus_err = MockI2cError(ErrorKind::Bus);
        let nack_err = MockI2cError(ErrorKind::NoAcknowledge(NoAcknowledgeSource::Address));

        let mut gauge = Cw2017::new(ScriptedI2c::new([I2cExpectation::WriteRead {
            addr: DEFAULT_ADDR,
            write: vec![REG_SOC_ALERT],
            read: Err(nack_err),
        }]));
        assert_eq!(
            block_on(gauge.profile_matches(&X4_PRO_BATINFO)),
            Err(nack_err)
        );
        gauge.i2c.assert_exhausted();

        let mut gauge = Cw2017::new(ScriptedI2c::new([I2cExpectation::WriteRead {
            addr: DEFAULT_ADDR,
            write: vec![REG_VCELL_H],
            read: Err(bus_err),
        }]));
        assert_eq!(block_on(gauge.read_millivolts()), Err(bus_err));
        gauge.i2c.assert_exhausted();

        let mut delay = MockDelay::new();
        let mut gauge = Cw2017::new(ScriptedI2c::new([
            read_exp(REG_MODE, MODE_NORMAL),
            read_exp(REG_VERSION, 0x0D),
            read_exp(REG_SOC_ALERT, 0x00),
            write_exp(REG_MODE, MODE_DEFAULT),
            I2cExpectation::Write {
                addr: DEFAULT_ADDR,
                data: vec![REG_BATINFO, X4_PRO_BATINFO[0]],
                result: Err(bus_err),
            },
        ]));
        assert_eq!(
            block_on(gauge.ensure_profile(&mut delay, &X4_PRO_BATINFO)),
            Err(bus_err)
        );
        gauge.i2c.assert_exhausted();
    }
}
