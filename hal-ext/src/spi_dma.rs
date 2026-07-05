use embassy_time::{Duration, Instant, Timer};
use embedded_hal::digital::{InputPin, OutputPin};
use embedded_hal_async::digital::Wait;
use embedded_hal_async::spi::SpiBus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusyWaitStep {
    Reached,
    TimedOut,
    PinError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "BUSY assertion/release failures must be observed or explicitly discarded"]
pub struct BusyWaitOutcome {
    pub initial_active: Option<bool>,
    pub assertion: BusyWaitStep,
    pub assertion_ms: u64,
    pub release: BusyWaitStep,
    pub release_ms: u64,
    pub final_active: Option<bool>,
}

pub struct EpdBus<SPI, CS, DC, BUSY, RST> {
    spi: SPI,
    cs: CS,
    dc: DC,
    busy: BUSY,
    rst: RST,
}

impl<SPI, CS, DC, BUSY, RST> EpdBus<SPI, CS, DC, BUSY, RST>
where
    SPI: SpiBus,
    CS: OutputPin,
    DC: OutputPin,
    BUSY: InputPin + Wait,
    RST: OutputPin,
{
    pub fn new(spi: SPI, cs: CS, dc: DC, busy: BUSY, rst: RST) -> Self {
        Self {
            spi,
            cs,
            dc,
            busy,
            rst,
        }
    }

    pub async fn reset(&mut self) {
        let _ = self.rst.set_high();
        Timer::after_millis(20).await;
        let _ = self.rst.set_low();
        Timer::after_millis(2).await;
        let _ = self.rst.set_high();
        Timer::after_millis(20).await;
    }

    pub async fn command(&mut self, cmd: u8, data: &[u8]) -> Result<(), SPI::Error> {
        self.select_command();
        let command_result = self.spi.write(&[cmd]).await;
        if command_result.is_err() {
            self.deselect();
            return command_result;
        }

        if !data.is_empty() {
            let _ = self.dc.set_high();
            let data_result = self.spi.write(data).await;
            self.deselect();
            data_result
        } else {
            self.deselect();
            Ok(())
        }
    }

    pub async fn begin_ram_write(&mut self, cmd: u8) -> Result<(), SPI::Error> {
        self.select_command();
        let result = self.spi.write(&[cmd]).await;
        if result.is_ok() {
            let _ = self.dc.set_high();
        } else {
            self.deselect();
        }
        result
    }

    pub async fn ram_chunk(&mut self, data: &[u8]) -> Result<(), SPI::Error> {
        self.spi.write(data).await
    }

    pub fn end_ram_write(&mut self) {
        self.deselect();
    }

    pub async fn wait_ready(&mut self, busy_active_high: bool) -> BusyWaitOutcome {
        let initial_active = self
            .busy
            .is_high()
            .ok()
            .map(|high| high == busy_active_high);

        let assertion_start = Instant::now();
        let (assertion, assertion_ms, release, release_ms) = if busy_active_high {
            // X4 (SSD1677): BUSY is active high. Give BUSY time to assert
            // after a command before the level wait; a too-early check would
            // sail straight through a refresh.
            Timer::after_millis(1).await;
            let assertion = match self.busy.is_high() {
                Ok(true) => BusyWaitStep::Reached,
                Ok(false) => BusyWaitStep::TimedOut,
                Err(_) => BusyWaitStep::PinError,
            };
            let assertion_ms = assertion_start.elapsed().as_millis();
            // The interrupt-driven level wait returns immediately if the pin
            // is already low, replacing the 20 ms poll loop's wake-ups and
            // exit jitter; the ceiling matches the poll loop's previous ~15 s
            // give-up.
            let release_start = Instant::now();
            let release =
                match embassy_time::with_timeout(Duration::from_secs(15), self.busy.wait_for_low())
                    .await
                {
                    Ok(Ok(())) => BusyWaitStep::Reached,
                    Ok(Err(_)) => BusyWaitStep::PinError,
                    Err(_) => BusyWaitStep::TimedOut,
                };
            (
                assertion,
                assertion_ms,
                release,
                release_start.elapsed().as_millis(),
            )
        } else {
            // X3 (UC8253): BUSY is active low. Observe assertion (low) before
            // release (high) so a delayed BUSY edge cannot let the caller sail
            // through a refresh.
            let assertion = match embassy_time::with_timeout(
                Duration::from_millis(100),
                self.busy.wait_for_low(),
            )
            .await
            {
                Ok(Ok(())) => BusyWaitStep::Reached,
                Ok(Err(_)) => BusyWaitStep::PinError,
                Err(_) => BusyWaitStep::TimedOut,
            };
            let assertion_ms = assertion_start.elapsed().as_millis();
            let release_start = Instant::now();
            let release = match embassy_time::with_timeout(
                Duration::from_secs(30),
                self.busy.wait_for_high(),
            )
            .await
            {
                Ok(Ok(())) => BusyWaitStep::Reached,
                Ok(Err(_)) => BusyWaitStep::PinError,
                Err(_) => BusyWaitStep::TimedOut,
            };
            (
                assertion,
                assertion_ms,
                release,
                release_start.elapsed().as_millis(),
            )
        };
        let final_active = self
            .busy
            .is_high()
            .ok()
            .map(|high| high == busy_active_high);

        BusyWaitOutcome {
            initial_active,
            assertion,
            assertion_ms,
            release,
            release_ms,
            final_active,
        }
    }

    pub fn deselect_display(&mut self) {
        self.deselect();
    }

    pub fn spi_mut(&mut self) -> &mut SPI {
        &mut self.spi
    }

    fn select_command(&mut self) {
        let _ = self.dc.set_low();
        let _ = self.cs.set_low();
    }

    fn deselect(&mut self) {
        let _ = self.cs.set_high();
    }
}
