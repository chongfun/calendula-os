#![no_std]
#![forbid(unsafe_code)]

//! MCU-neutral peripheral drivers. Chip RTC, pad and probe ownership lives
//! in the concrete firmware executable.
pub mod bm8563;
pub mod bq27220;
pub mod cw2017;
pub mod gt911;
pub mod spi_dma;

#[cfg(test)]
extern crate std;

#[cfg(test)]
pub(crate) mod test_support {
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use embedded_hal_async::delay::DelayNs;
    use embedded_hal_async::i2c::{Error, ErrorKind, ErrorType, I2c, Operation};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::vec::Vec;

    pub fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = pin!(fut);
        let mut cx = Context::from_waker(Waker::noop());
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(out) => out,
            Poll::Pending => panic!("host test future unexpectedly yielded Poll::Pending"),
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MockI2cError(pub ErrorKind);

    impl Error for MockI2cError {
        fn kind(&self) -> ErrorKind {
            self.0
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum I2cExpectation {
        Write {
            addr: u8,
            data: Vec<u8>,
            result: Result<(), MockI2cError>,
        },
        WriteRead {
            addr: u8,
            write: Vec<u8>,
            read: Result<Vec<u8>, MockI2cError>,
        },
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum TimelineEvent {
        Write { addr: u8, data: Vec<u8> },
        WriteRead { addr: u8, write: Vec<u8> },
        DelayMs(u32),
    }

    #[derive(Clone, Default)]
    pub struct Timeline(Rc<RefCell<Vec<TimelineEvent>>>);

    impl Timeline {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn push(&self, ev: TimelineEvent) {
            self.0.borrow_mut().push(ev);
        }

        pub fn snapshot(&self) -> Vec<TimelineEvent> {
            self.0.borrow().clone()
        }
    }

    pub struct MockDelay {
        pub delays_ms: Vec<u32>,
        timeline: Option<Timeline>,
    }

    impl MockDelay {
        pub fn new() -> Self {
            Self {
                delays_ms: Vec::new(),
                timeline: None,
            }
        }

        pub fn with_timeline(timeline: Timeline) -> Self {
            Self {
                delays_ms: Vec::new(),
                timeline: Some(timeline),
            }
        }
    }

    impl DelayNs for MockDelay {
        async fn delay_ns(&mut self, ns: u32) {
            let ms = ns / 1_000_000;
            self.delays_ms.push(ms);
            if let Some(tl) = &self.timeline {
                tl.push(TimelineEvent::DelayMs(ms));
            }
        }

        async fn delay_ms(&mut self, ms: u32) {
            self.delays_ms.push(ms);
            if let Some(tl) = &self.timeline {
                tl.push(TimelineEvent::DelayMs(ms));
            }
        }
    }

    pub struct ScriptedI2c {
        expectations: VecDeque<I2cExpectation>,
        timeline: Option<Timeline>,
    }

    impl ScriptedI2c {
        pub fn new(expectations: impl IntoIterator<Item = I2cExpectation>) -> Self {
            Self {
                expectations: expectations.into_iter().collect(),
                timeline: None,
            }
        }

        pub fn with_timeline(
            expectations: impl IntoIterator<Item = I2cExpectation>,
            timeline: Timeline,
        ) -> Self {
            Self {
                expectations: expectations.into_iter().collect(),
                timeline: Some(timeline),
            }
        }

        pub fn assert_exhausted(&self) {
            assert!(
                self.expectations.is_empty(),
                "unconsumed mock I2C expectations: {:?}",
                self.expectations
            );
        }
    }

    impl ErrorType for ScriptedI2c {
        type Error = MockI2cError;
    }

    impl I2c for ScriptedI2c {
        async fn transaction(
            &mut self,
            address: u8,
            operations: &mut [Operation<'_>],
        ) -> Result<(), Self::Error> {
            let expected = self
                .expectations
                .pop_front()
                .expect("unexpected I2C transaction");
            match (expected, operations) {
                (I2cExpectation::Write { addr, data, result }, [Operation::Write(actual)]) => {
                    assert_eq!(address, addr, "I2C write address mismatch");
                    assert_eq!(*actual, data.as_slice(), "I2C write payload mismatch");
                    if let Some(tl) = &self.timeline {
                        tl.push(TimelineEvent::Write {
                            addr: address,
                            data: actual.to_vec(),
                        });
                    }
                    result
                }
                (
                    I2cExpectation::WriteRead { addr, write, read },
                    [Operation::Write(actual_write), Operation::Read(actual_read)],
                ) => {
                    assert_eq!(address, addr, "I2C write_read address mismatch");
                    assert_eq!(
                        *actual_write,
                        write.as_slice(),
                        "I2C write_read command mismatch"
                    );
                    if let Some(tl) = &self.timeline {
                        tl.push(TimelineEvent::WriteRead {
                            addr: address,
                            write: actual_write.to_vec(),
                        });
                    }
                    match read {
                        Ok(bytes) => {
                            assert_eq!(
                                actual_read.len(),
                                bytes.len(),
                                "I2C write_read length mismatch"
                            );
                            actual_read.copy_from_slice(&bytes);
                            Ok(())
                        }
                        Err(err) => Err(err),
                    }
                }
                (exp, ops) => panic!(
                    "mismatched I2C operation: expected {exp:?}, got {} op(s)",
                    ops.len()
                ),
            }
        }
    }
}
