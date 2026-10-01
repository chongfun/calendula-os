//! The card, as the storage code sees it: a session that hands out the root
//! directory for the length of one closure.

use embedded_sdmmc::{Directory, TimeSource, Timestamp};

/// Why a session did not reach the root directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// The card would not initialise.
    CardInit,
    /// The volume would not open.
    Volume,
    /// The root directory would not open.
    Root,
}

/// The fixed clock directory entries are stamped with. The device keeps no
/// wall time, and an honest constant beats a guess that looks real.
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
/// The firmware's session brings the SPI bus up for the card and puts it
/// back for the panel around the closure; a test's is a volume manager over
/// a FAT image in RAM. Nothing inside the closure can open a second session,
/// which is the rule the firmware's bus sharing already imposes.
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
}
