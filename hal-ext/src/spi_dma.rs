use embassy_time::{Duration, Instant, Timer};
use embedded_hal::digital::{InputPin, OutputPin};
use embedded_hal_async::digital::Wait;
use embedded_hal_async::spi::SpiBus;

/// A BUSY handshake that did not complete: the panel either never asserted
/// busy after a command that must run one (the command was ignored), or
/// never released it inside the wait ceiling (the controller is wedged).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusyError {
    NeverAsserted,
    TimedOut,
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

    pub async fn wait_ready(&mut self) -> Result<(), BusyError> {
        // Give BUSY time to assert after a command before the level wait;
        // a too-early check would sail straight through a refresh. A bounded
        // edge wait returns as soon as BUSY asserts instead of always burning
        // the old fixed 1 ms sleep, and its 2 ms ceiling covers a slower
        // assert the fixed delay would have missed. Every caller sits right
        // after a command whose BUSY pulse lasts multiple milliseconds, so a
        // ceiling hit means the command was ignored (BUSY never rose and the
        // low wait below would fall through over a panel that never ran it) —
        // surface that as `NeverAsserted` rather than reporting the
        // transition settled.
        embassy_time::with_timeout(Duration::from_millis(2), self.busy.wait_for_high())
            .await
            .map_err(|_| BusyError::NeverAsserted)?
            .map_err(|_| BusyError::NeverAsserted)?;
        // BUSY is active high. The interrupt-driven level wait returns
        // immediately if the pin is already low, replacing the 20 ms poll
        // loop's wake-ups and exit jitter; the ceiling matches the poll
        // loop's previous ~15 s give-up — but a wait that hits it now
        // surfaces as an error instead of silently proceeding over a panel
        // that is still busy (or a BUSY line that stopped reading).
        embassy_time::with_timeout(Duration::from_secs(15), self.busy.wait_for_low())
            .await
            .map_err(|_| BusyError::TimedOut)?
            .map_err(|_| BusyError::TimedOut)
    }

    /// Two-phase BUSY wait for the UC8253 (Xteink X3): BUSY drops LOW while
    /// the controller works and returns HIGH when done, the opposite sense
    /// and shape of the SSD1677's active-high line. Wait for the falling
    /// edge first (bounded to 1 s — a refresh so quick it never shows LOW
    /// has already finished, so proceed), then for the return to HIGH.
    /// Mirrors CrossPoint's `BusyPolarity::X3TwoPhase` poll.
    ///
    /// Returns the elapsed milliseconds of a completed handshake. A command
    /// that never drops BUSY was ignored (`NeverAsserted` — the difference
    /// between "refresh ran invisibly" and "refresh never happened"); one
    /// that never returns HIGH inside the 30 s ceiling is `TimedOut`.
    pub async fn wait_two_phase(&mut self) -> Result<u64, BusyError> {
        let start = Instant::now();
        let saw_low = embassy_time::with_timeout(Duration::from_secs(1), self.busy.wait_for_low())
            .await
            .is_ok();
        if !saw_low {
            return Err(BusyError::NeverAsserted);
        }
        embassy_time::with_timeout(Duration::from_secs(30), self.busy.wait_for_high())
            .await
            .map_err(|_| BusyError::TimedOut)?
            .map_err(|_| BusyError::TimedOut)?;
        Ok(start.elapsed().as_millis())
    }

    /// Two-phase BUSY wait for the UC8279 X4 Pro (`BusyPolarity::UcIdleHigh`):
    /// BUSY is HIGH when idle, drops LOW while the controller works, and
    /// returns HIGH when done. Wait up to 50 ms for the initial falling edge
    /// (continuing even if a fast transition already completed), pause 1 ms,
    /// then wait up to 30 s for the return to HIGH.
    pub async fn wait_uc_idle_high(&mut self) -> Result<u64, BusyError> {
        let start = clock::now();
        let _ = clock::with_timeout(Duration::from_millis(50), self.busy.wait_for_low()).await;
        clock::after_millis(1).await;
        clock::with_timeout(Duration::from_secs(30), self.busy.wait_for_high())
            .await
            .map_err(|_| BusyError::TimedOut)?
            .map_err(|_| BusyError::TimedOut)?;
        Ok(clock::elapsed_ms(start))
    }

    /// Strict UC8279 two-phase wait for display refresh commands (`DRF`):
    /// returns [`BusyError::NeverAsserted`] if BUSY never drops LOW within
    /// 50 ms so a missed refresh command cannot be recorded as a settled frame.
    pub async fn wait_uc_idle_high_asserted(&mut self) -> Result<u64, BusyError> {
        let start = clock::now();
        match clock::with_timeout(Duration::from_millis(50), self.busy.wait_for_low()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(BusyError::NeverAsserted),
            Err(_) if self.busy.is_high().unwrap_or(true) => return Err(BusyError::NeverAsserted),
            Err(_) => {}
        }
        clock::after_millis(1).await;
        clock::with_timeout(Duration::from_secs(30), self.busy.wait_for_high())
            .await
            .map_err(|_| BusyError::TimedOut)?
            .map_err(|_| BusyError::TimedOut)?;
        Ok(clock::elapsed_ms(start))
    }

    /// Raw BUSY level sample, for bring-up probes that need the idle level
    /// rather than an edge wait (`None` if the pin read errors).
    pub fn busy_is_high(&mut self) -> Option<bool> {
        self.busy.is_high().ok()
    }

    pub fn deselect_display(&mut self) {
        self.deselect();
    }

    pub fn spi_mut(&mut self) -> &mut SPI {
        &mut self.spi
    }

    /// Mutable access to the panel `RST` pin for configuring pad holds without
    /// `unsafe`.
    pub fn rst_mut(&mut self) -> &mut RST {
        &mut self.rst
    }

    fn select_command(&mut self) {
        let _ = self.dc.set_low();
        let _ = self.cs.set_low();
    }

    fn deselect(&mut self) {
        let _ = self.cs.set_high();
    }
}

#[cfg(not(test))]
mod clock {
    use core::future::Future;
    use embassy_time::{Duration, Instant, TimeoutError, Timer};

    #[inline]
    pub(super) fn now() -> Instant {
        Instant::now()
    }

    #[inline]
    pub(super) fn elapsed_ms(start: Instant) -> u64 {
        start.elapsed().as_millis()
    }

    #[inline]
    pub(super) async fn after_millis(ms: u64) {
        Timer::after_millis(ms).await;
    }

    #[inline]
    pub(super) async fn with_timeout<F: Future>(
        duration: Duration,
        fut: F,
    ) -> Result<F::Output, TimeoutError> {
        embassy_time::with_timeout(duration, fut).await
    }
}

#[cfg(test)]
mod clock {
    use core::cell::RefCell;
    use core::future::{poll_fn, Future};
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use embassy_time::Duration;
    use std::vec::Vec;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct MockInstant(pub(super) u64);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum ClockEvent {
        WithTimeout { at_ms: u64, timeout_ms: u64 },
        AfterMillis { at_ms: u64, ms: u64 },
    }

    struct State {
        now_ms: u64,
        next_wake_ms: Option<u64>,
        events: Vec<ClockEvent>,
    }

    impl State {
        const fn new() -> Self {
            Self {
                now_ms: 0,
                next_wake_ms: None,
                events: Vec::new(),
            }
        }

        fn reset(&mut self) {
            self.now_ms = 0;
            self.next_wake_ms = None;
            self.events.clear();
        }
    }

    std::thread_local! {
        static MOCK_CLOCK: RefCell<State> = const { RefCell::new(State::new()) };
    }

    struct ResetGuard;

    impl ResetGuard {
        fn new() -> Self {
            MOCK_CLOCK.with(|cell| cell.borrow_mut().reset());
            Self
        }
    }

    impl Drop for ResetGuard {
        fn drop(&mut self) {
            MOCK_CLOCK.with(|cell| cell.borrow_mut().reset());
        }
    }

    pub(super) fn now() -> MockInstant {
        MOCK_CLOCK.with(|cell| MockInstant(cell.borrow().now_ms))
    }

    pub(super) fn elapsed_ms(start: MockInstant) -> u64 {
        now().0 - start.0
    }

    pub(super) fn schedule_wake(at_ms: u64) {
        MOCK_CLOCK.with(|cell| {
            let mut st = cell.borrow_mut();
            st.next_wake_ms = Some(match st.next_wake_ms {
                Some(cur) => cur.min(at_ms),
                None => at_ms,
            });
        });
    }

    pub(super) fn snapshot_events() -> Vec<ClockEvent> {
        MOCK_CLOCK.with(|cell| cell.borrow().events.clone())
    }

    pub(super) async fn after_millis(ms: u64) {
        let start_ms = now().0;
        let deadline_ms = start_ms + ms;
        MOCK_CLOCK.with(|cell| {
            cell.borrow_mut().events.push(ClockEvent::AfterMillis {
                at_ms: start_ms,
                ms,
            });
        });
        let mut yielded_once = false;
        poll_fn(|_cx| {
            let cur = now().0;
            if yielded_once && cur >= deadline_ms {
                return Poll::Ready(());
            }
            yielded_once = true;
            schedule_wake(deadline_ms);
            Poll::Pending
        })
        .await;
    }

    pub(super) async fn with_timeout<F: Future>(
        duration: Duration,
        fut: F,
    ) -> Result<F::Output, ()> {
        let start_ms = now().0;
        let timeout_ms = duration.as_millis();
        let deadline_ms = start_ms + timeout_ms;
        MOCK_CLOCK.with(|cell| {
            cell.borrow_mut().events.push(ClockEvent::WithTimeout {
                at_ms: start_ms,
                timeout_ms,
            });
        });
        let mut fut = pin!(fut);
        let mut yielded_once = false;
        poll_fn(|cx| {
            if let Poll::Ready(out) = fut.as_mut().poll(cx) {
                return Poll::Ready(Ok(out));
            }
            let cur = now().0;
            if yielded_once && cur >= deadline_ms {
                return Poll::Ready(Err(()));
            }
            yielded_once = true;
            schedule_wake(deadline_ms);
            Poll::Pending
        })
        .await
    }

    pub(super) fn run_until_ready<F: Future>(fut: F) -> (F::Output, Vec<ClockEvent>) {
        let _guard = ResetGuard::new();
        let mut fut = pin!(fut);
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..10_000 {
            MOCK_CLOCK.with(|cell| cell.borrow_mut().next_wake_ms = None);
            if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
                return (out, snapshot_events());
            }
            let next = MOCK_CLOCK
                .with(|cell| cell.borrow_mut().next_wake_ms.take())
                .expect("mock future yielded Poll::Pending without scheduling a wake");
            MOCK_CLOCK.with(|cell| {
                let mut st = cell.borrow_mut();
                st.now_ms = st.now_ms.max(next);
            });
        }
        panic!("mock future exceeded maximum poll iterations");
    }
}

#[cfg(test)]
mod tests {
    use super::clock::{self, ClockEvent};
    use super::*;
    use core::convert::Infallible;
    use core::future::poll_fn;
    use core::task::Poll;
    use embedded_hal::digital::{Error as DigitalError, ErrorKind, ErrorType as DigitalErrorType};
    use embedded_hal::spi::ErrorType as SpiErrorType;
    use std::vec;
    use std::vec::Vec;

    struct DummySpi;

    impl SpiErrorType for DummySpi {
        type Error = Infallible;
    }

    impl SpiBus for DummySpi {
        async fn read(&mut self, _words: &mut [u8]) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn write(&mut self, _words: &[u8]) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn transfer(&mut self, _read: &mut [u8], _write: &[u8]) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn transfer_in_place(&mut self, _words: &mut [u8]) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    struct MockOutputPin {
        high: bool,
        high_calls: usize,
        low_calls: usize,
    }

    impl MockOutputPin {
        fn new(initial_high: bool) -> Self {
            Self {
                high: initial_high,
                high_calls: 0,
                low_calls: 0,
            }
        }
    }

    impl DigitalErrorType for MockOutputPin {
        type Error = Infallible;
    }

    impl OutputPin for MockOutputPin {
        fn set_low(&mut self) -> Result<(), Self::Error> {
            self.high = false;
            self.low_calls += 1;
            Ok(())
        }

        fn set_high(&mut self) -> Result<(), Self::Error> {
            self.high = true;
            self.high_calls += 1;
            Ok(())
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct MockPinError;

    impl DigitalError for MockPinError {
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum BusyPinEvent {
        LowResolved(u64),
        HighResolved(u64),
    }

    struct MockBusyPin {
        is_high: bool,
        low_at: Option<(u64, Result<(), MockPinError>)>,
        high_at: Option<(u64, Result<(), MockPinError>)>,
        events: Vec<BusyPinEvent>,
    }

    impl MockBusyPin {
        fn new(
            low_at: Option<(u64, Result<(), MockPinError>)>,
            high_at: Option<(u64, Result<(), MockPinError>)>,
        ) -> Self {
            Self {
                is_high: true,
                low_at,
                high_at,
                events: Vec::new(),
            }
        }
    }

    impl DigitalErrorType for MockBusyPin {
        type Error = MockPinError;
    }

    impl InputPin for MockBusyPin {
        fn is_high(&mut self) -> Result<bool, Self::Error> {
            Ok(self.is_high)
        }

        fn is_low(&mut self) -> Result<bool, Self::Error> {
            Ok(!self.is_high)
        }
    }

    impl Wait for MockBusyPin {
        async fn wait_for_high(&mut self) -> Result<(), Self::Error> {
            poll_fn(|_cx| {
                let Some((at_ms, res)) = self.high_at else {
                    return Poll::Pending;
                };
                let now_ms = clock::now().0;
                if now_ms >= at_ms {
                    if res.is_ok() {
                        self.is_high = true;
                    }
                    self.events.push(BusyPinEvent::HighResolved(now_ms));
                    Poll::Ready(res)
                } else {
                    clock::schedule_wake(at_ms);
                    Poll::Pending
                }
            })
            .await
        }

        async fn wait_for_low(&mut self) -> Result<(), Self::Error> {
            poll_fn(|_cx| {
                let Some((at_ms, res)) = self.low_at else {
                    return Poll::Pending;
                };
                let now_ms = clock::now().0;
                if now_ms >= at_ms {
                    if res.is_ok() {
                        self.is_high = false;
                    }
                    self.events.push(BusyPinEvent::LowResolved(now_ms));
                    Poll::Ready(res)
                } else {
                    clock::schedule_wake(at_ms);
                    Poll::Pending
                }
            })
            .await
        }

        async fn wait_for_rising_edge(&mut self) -> Result<(), Self::Error> {
            self.wait_for_high().await
        }

        async fn wait_for_falling_edge(&mut self) -> Result<(), Self::Error> {
            self.wait_for_low().await
        }

        async fn wait_for_any_edge(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn make_bus(
        busy: MockBusyPin,
    ) -> EpdBus<DummySpi, MockOutputPin, MockOutputPin, MockBusyPin, MockOutputPin> {
        EpdBus::new(
            DummySpi,
            MockOutputPin::new(true),
            MockOutputPin::new(false),
            busy,
            MockOutputPin::new(true),
        )
    }

    #[test]
    fn rst_mut_provides_mutable_access_to_reset_pin() {
        let mut bus = make_bus(MockBusyPin::new(None, None));
        assert!(bus.rst_mut().high);
        assert_eq!(bus.rst_mut().low_calls, 0);
        assert_eq!(bus.rst_mut().high_calls, 0);

        bus.rst_mut().set_low().unwrap();
        assert!(!bus.rst_mut().high);
        assert_eq!(bus.rst_mut().low_calls, 1);

        bus.rst_mut().set_high().unwrap();
        assert!(bus.rst_mut().high);
        assert_eq!(bus.rst_mut().high_calls, 1);
    }

    #[test]
    fn wait_uc_idle_high_succeeds_on_falling_then_rising_busy() {
        let mut bus = make_bus(MockBusyPin::new(Some((3, Ok(()))), Some((420, Ok(())))));
        let (result, clock_events) = clock::run_until_ready(bus.wait_uc_idle_high());
        assert_eq!(result, Ok(420));
        assert_eq!(
            bus.busy.events,
            vec![
                BusyPinEvent::LowResolved(3),
                BusyPinEvent::HighResolved(420),
            ]
        );
        assert_eq!(
            clock_events,
            vec![
                ClockEvent::WithTimeout {
                    at_ms: 0,
                    timeout_ms: 50,
                },
                ClockEvent::AfterMillis { at_ms: 3, ms: 1 },
                ClockEvent::WithTimeout {
                    at_ms: 4,
                    timeout_ms: 30_000,
                },
            ]
        );
    }

    #[test]
    fn wait_uc_idle_high_continues_after_50ms_falling_edge_timeout_or_low_error() {
        // Missed falling edge times out at 50 ms, pauses 1 ms, then completes when HIGH.
        let mut bus = make_bus(MockBusyPin::new(None, Some((0, Ok(())))));
        let (result, clock_events) = clock::run_until_ready(bus.wait_uc_idle_high());
        assert_eq!(result, Ok(51));
        assert_eq!(bus.busy.events, vec![BusyPinEvent::HighResolved(51)]);
        assert_eq!(
            clock_events,
            vec![
                ClockEvent::WithTimeout {
                    at_ms: 0,
                    timeout_ms: 50,
                },
                ClockEvent::AfterMillis { at_ms: 50, ms: 1 },
                ClockEvent::WithTimeout {
                    at_ms: 51,
                    timeout_ms: 30_000,
                },
            ]
        );

        // Pin error during wait_for_low is ignored and still settles on wait_for_high.
        let mut bus = make_bus(MockBusyPin::new(
            Some((2, Err(MockPinError))),
            Some((15, Ok(()))),
        ));
        let (result, _) = clock::run_until_ready(bus.wait_uc_idle_high());
        assert_eq!(result, Ok(15));
    }

    #[test]
    fn wait_uc_idle_high_returns_timed_out_on_30s_expiry_or_high_error() {
        // BUSY drops at 2 ms, settles 1 ms to t=3 ms, then never returns HIGH -> times out at 30,003 ms.
        let mut bus = make_bus(MockBusyPin::new(Some((2, Ok(()))), None));
        let (result, clock_events) = clock::run_until_ready(bus.wait_uc_idle_high());
        assert_eq!(result, Err(BusyError::TimedOut));
        assert_eq!(bus.busy.events, vec![BusyPinEvent::LowResolved(2)]);
        assert_eq!(
            clock_events,
            vec![
                ClockEvent::WithTimeout {
                    at_ms: 0,
                    timeout_ms: 50,
                },
                ClockEvent::AfterMillis { at_ms: 2, ms: 1 },
                ClockEvent::WithTimeout {
                    at_ms: 3,
                    timeout_ms: 30_000,
                },
            ]
        );

        // Pin error during wait_for_high also surfaces as BusyError::TimedOut.
        let mut bus = make_bus(MockBusyPin::new(
            Some((2, Ok(()))),
            Some((10, Err(MockPinError))),
        ));
        let (result, _) = clock::run_until_ready(bus.wait_uc_idle_high());
        assert_eq!(result, Err(BusyError::TimedOut));
    }

    #[test]
    fn wait_uc_idle_high_asserted_rejects_low_wait_pin_error() {
        let mut bus = make_bus(MockBusyPin::new(
            Some((2, Err(MockPinError))),
            Some((15, Ok(()))),
        ));
        let (result, _) = clock::run_until_ready(bus.wait_uc_idle_high_asserted());
        assert_eq!(result, Err(BusyError::NeverAsserted));
    }
}
