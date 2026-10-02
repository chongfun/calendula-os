//! The card as the storage code sees it: a session that lends out the root
//! directory for one closure.

use embedded_sdmmc::{Directory, TimeSource, Timestamp};

use crate::progress::{ProgressSink, Silent};

/// Why a session did not reach the root directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// The card would not initialize.
    CardInit,
    /// The volume would not open.
    Volume,
    /// The root directory would not open.
    Root,
}

/// The fixed timestamp on every directory entry, since the device keeps no
/// wall time.
#[derive(Clone, Copy, Debug, Default)]
pub struct StaticTime;

impl TimeSource for StaticTime {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 56,
            zero_indexed_month: 4,
            zero_indexed_day: 19,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

/// The root directory of one session.
pub type Root<'a, D> = Directory<'a, D, StaticTime, 8, 8, 1>;

/// A card the storage code can open a session on.
///
/// The firmware's session takes the SPI bus from the panel for the closure;
/// a test's is a volume manager over a FAT image in RAM. The closure cannot
/// open a second session.
pub trait Card {
    /// The block device a session reads and writes through.
    type Device<'a>: embedded_sdmmc::BlockDevice
    where
        Self: 'a;

    /// Run `f` against the card's root directory.
    fn with_root<R>(
        &mut self,
        f: impl for<'a> FnOnce(&Root<'a, Self::Device<'a>>) -> R,
    ) -> Result<R, SessionError>;

    /// [`Card::with_root`], with somewhere for `f` to report progress. A card
    /// with nobody watching hands it [`Silent`].
    fn with_root_reporting<R>(
        &mut self,
        f: impl for<'a> FnOnce(&Root<'a, Self::Device<'a>>, &mut dyn ProgressSink) -> R,
    ) -> Result<R, SessionError> {
        self.with_root(|root| f(root, &mut Silent))
    }
}
