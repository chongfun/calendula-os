//! TI BQ27220 battery fuel gauge — the Xteink X3's charge source.
//!
//! The X4 reads battery voltage off an ADC divider; the X3 has no such
//! divider and instead carries this I2C gauge, which reports state-of-charge
//! directly (no voltage-to-percent curve to guess at) plus voltage and a
//! signed current whose sign tells charging from discharging.
//!
//! Generic over any async I2C bus so it stays host-testable and free of
//! esp-hal types. Standard-command reads only (write the 1-byte register,
//! read the little-endian u16); the gauge answers these without unsealing.

use embedded_hal_async::i2c::I2c;

/// 7-bit I2C address of the gauge.
pub const ADDRESS: u8 = 0x55;

// Standard commands (little-endian u16 each).
const REG_VOLTAGE: u8 = 0x08;
const REG_CURRENT: u8 = 0x14;
const REG_STATE_OF_CHARGE: u8 = 0x2C;

pub struct Bq27220<I2C> {
    i2c: I2C,
}

impl<I2C: I2c> Bq27220<I2C> {
    pub fn new(i2c: I2C) -> Self {
        Self { i2c }
    }

    async fn read_u16(&mut self, reg: u8) -> Result<u16, I2C::Error> {
        let mut buf = [0u8; 2];
        self.i2c.write_read(ADDRESS, &[reg], &mut buf).await?;
        Ok(u16::from_le_bytes(buf))
    }

    /// Battery terminal voltage in millivolts.
    pub async fn voltage_mv(&mut self) -> Result<u16, I2C::Error> {
        self.read_u16(REG_VOLTAGE).await
    }

    /// State of charge in percent, clamped to 0..=100.
    pub async fn state_of_charge(&mut self) -> Result<u8, I2C::Error> {
        Ok(self.read_u16(REG_STATE_OF_CHARGE).await?.min(100) as u8)
    }

    /// Whether the pack is charging: the average-current register is signed,
    /// positive into the battery.
    pub async fn charging(&mut self) -> Result<bool, I2C::Error> {
        Ok((self.read_u16(REG_CURRENT).await? as i16) > 0)
    }

    /// One round trip that returns the pair the input task needs: voltage
    /// (mV) and state of charge (percent).
    pub async fn read(&mut self) -> Result<(u16, u8), I2C::Error> {
        let mv = self.voltage_mv().await?;
        let soc = self.state_of_charge().await?;
        Ok((mv, soc))
    }
}
