//! Block transactions on their way to the card, for bench telemetry. The
//! firmware's counting device bumps these; the storage code reads them around
//! the operations it reports on.

use core::sync::atomic::{AtomicU32, Ordering};

pub static READ_CALLS: AtomicU32 = AtomicU32::new(0);
pub static READ_BLOCKS: AtomicU32 = AtomicU32::new(0);
pub static WRITE_CALLS: AtomicU32 = AtomicU32::new(0);
pub static WRITE_BLOCKS: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub read_calls: u32,
    pub read_blocks: u32,
    pub write_calls: u32,
    pub write_blocks: u32,
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        read_calls: READ_CALLS.load(Ordering::Relaxed),
        read_blocks: READ_BLOCKS.load(Ordering::Relaxed),
        write_calls: WRITE_CALLS.load(Ordering::Relaxed),
        write_blocks: WRITE_BLOCKS.load(Ordering::Relaxed),
    }
}

impl Snapshot {
    pub fn since(self, start: Snapshot) -> Snapshot {
        Snapshot {
            read_calls: self.read_calls.wrapping_sub(start.read_calls),
            read_blocks: self.read_blocks.wrapping_sub(start.read_blocks),
            write_calls: self.write_calls.wrapping_sub(start.write_calls),
            write_blocks: self.write_blocks.wrapping_sub(start.write_blocks),
        }
    }
}

/// Add to a counter. A load and a store rather than `fetch_add`: the C3 has no
/// atomic read-modify-write, and only the storage task counts.
pub fn bump(counter: &AtomicU32, amount: u32) {
    let value = counter.load(Ordering::Relaxed).wrapping_add(amount);
    counter.store(value, Ordering::Relaxed);
}
