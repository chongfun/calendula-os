//! Bounded external sorting for a rebuildable catalog-row index.
//!
//! The storage implementation owns publication: `begin` must make any
//! prior output unpublishable before changing either run, and `publish` must
//! expose the completed run only after its contents are durable. A failed
//! build is never published by this algorithm. The comparator reads the
//! referenced catalog rows and must return a total order for their titles,
//! physical names and locators.

use core::cmp::Ordering;

/// Backing store for the two runs and their source catalog.
///
/// Runs hold catalog row numbers, not identity. Each has room for `count`
/// entries; offsets and lengths must be checked by the implementation.
pub trait IndexStorage {
    type Error;

    /// Retire any previously published index and prepare both scratch runs.
    fn begin(&mut self, count: u16) -> Result<(), Self::Error>;
    /// Refuse an unreadable or unaddressable catalog row, even in a one-row
    /// catalog where merge sort would never invoke the comparator.
    fn check(&mut self, row: u16) -> Result<(), Self::Error>;
    fn read(&mut self, run: bool, position: u16) -> Result<u16, Self::Error>;
    fn write(&mut self, run: bool, position: u16, row: u16) -> Result<(), Self::Error>;
    fn compare(&mut self, left: u16, right: u16) -> Result<Ordering, Self::Error>;
    /// Publish only the complete run, tied to the exact catalog it indexed.
    fn publish(&mut self, run: bool, count: u16) -> Result<(), Self::Error>;
}

/// Resumable merge state. Scratch runs belong to the caller and must remain
/// unchanged between slices. Drop this state if the catalog changes or any
/// operation fails; its partially written runs are not publishable.
#[derive(Clone, Copy, Debug)]
pub struct Sorter {
    count: u16,
    initialized: u16,
    source: bool,
    width: u32,
    start: u32,
    left: u32,
    right: u32,
    out: u32,
    left_row: Option<u16>,
    right_row: Option<u16>,
}

impl Sorter {
    pub const fn new(count: u16) -> Self {
        Self {
            count,
            initialized: 0,
            source: false,
            width: 1,
            start: 0,
            left: 0,
            right: 1,
            out: 0,
            left_row: None,
            right_row: None,
        }
    }

    /// Do at most `budget` row writes and return the completed run, if any.
    /// `begin` and `publish` are intentionally separate so disk-backed
    /// implementations can divide those operations into bounded slices too.
    pub fn step<S: IndexStorage>(
        &mut self,
        store: &mut S,
        budget: usize,
    ) -> Result<Option<bool>, S::Error> {
        let len = u32::from(self.count);
        let mut written = 0;
        while written < budget {
            if self.initialized < self.count {
                let row = self.initialized;
                store.check(row)?;
                store.write(false, row, row)?;
                self.initialized += 1;
                written += 1;
                continue;
            }
            if self.width >= len {
                return Ok(Some(self.source));
            }
            let middle = (self.start + self.width).min(len);
            let end = (middle + self.width).min(len);
            if self.out == end {
                self.start = end;
                if self.start == len {
                    self.source = !self.source;
                    self.width *= 2;
                    self.start = 0;
                }
                self.left = self.start;
                self.right = (self.start + self.width).min(len);
                self.out = self.start;
                self.left_row = None;
                self.right_row = None;
                continue;
            }
            if self.left < middle && self.left_row.is_none() {
                self.left_row = Some(store.read(self.source, self.left as u16)?);
            }
            if self.right < end && self.right_row.is_none() {
                self.right_row = Some(store.read(self.source, self.right as u16)?);
            }
            let take_left = match (self.left_row, self.right_row) {
                (Some(a), Some(b)) => store.compare(a, b)? != Ordering::Greater,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                // All rows in this merge were already written.
                (None, None) => continue,
            };
            let row = if take_left {
                self.left_row
            } else {
                self.right_row
            };
            if let Some(row) = row {
                store.write(!self.source, self.out as u16, row)?;
                if take_left {
                    self.left += 1;
                    self.left_row = None;
                } else {
                    self.right += 1;
                    self.right_row = None;
                }
                self.out += 1;
                written += 1;
            }
        }
        Ok(None)
    }
}

/// Sort all catalog rows using bottom-up merge sort and constant RAM.
///
/// Every pass reads one complete run and writes the other. Once an operation
/// fails, no later write or publication is attempted. A failed `publish`
/// may nevertheless have reached the card, so the storage implementation
/// must make its validity marker the last write after both runs are durable.
pub fn build<S: IndexStorage>(store: &mut S, count: u16) -> Result<(), S::Error> {
    store.begin(count)?;
    let mut sorter = Sorter::new(count);
    loop {
        if let Some(run) = sorter.step(store, usize::MAX)? {
            return store.publish(run, count);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Memory {
        runs: [[u16; 40]; 2],
        keys: [u8; 40],
        published: Option<bool>,
        operation: usize,
        fail_at: Option<usize>,
    }

    impl Memory {
        fn new() -> Self {
            let mut keys = [0; 40];
            for (i, key) in keys.iter_mut().enumerate() {
                *key = ((i * 17) % 37) as u8;
            }
            Self {
                runs: [[0; 40]; 2],
                keys,
                published: None,
                operation: 0,
                fail_at: None,
            }
        }

        fn step(&mut self) -> Result<(), ()> {
            let operation = self.operation;
            self.operation += 1;
            if self.fail_at == Some(operation) {
                Err(())
            } else {
                Ok(())
            }
        }
    }

    impl IndexStorage for Memory {
        type Error = ();

        fn begin(&mut self, _count: u16) -> Result<(), ()> {
            self.published = None;
            self.step()
        }

        fn read(&mut self, run: bool, position: u16) -> Result<u16, ()> {
            self.step()?;
            Ok(self.runs[usize::from(run)][usize::from(position)])
        }

        fn check(&mut self, _row: u16) -> Result<(), ()> {
            self.step()
        }

        fn write(&mut self, run: bool, position: u16, row: u16) -> Result<(), ()> {
            self.step()?;
            self.runs[usize::from(run)][usize::from(position)] = row;
            Ok(())
        }

        fn compare(&mut self, left: u16, right: u16) -> Result<Ordering, ()> {
            self.step()?;
            Ok(self.keys[usize::from(left)].cmp(&self.keys[usize::from(right)]))
        }

        fn publish(&mut self, run: bool, _count: u16) -> Result<(), ()> {
            self.step()?;
            self.published = Some(run);
            Ok(())
        }
    }

    #[test]
    fn merge_orders_every_row_across_runs_and_keeps_ties_stable() {
        let mut disk = Memory::new();
        build(&mut disk, 40).unwrap();
        let rows = &disk.runs[usize::from(disk.published.unwrap())];
        let mut seen = [false; 40];
        for (position, &row) in rows.iter().enumerate() {
            assert!(!seen[usize::from(row)]);
            seen[usize::from(row)] = true;
            if position > 0 {
                let previous = rows[position - 1];
                assert!(disk.keys[usize::from(previous)] <= disk.keys[usize::from(row)]);
                if disk.keys[usize::from(previous)] == disk.keys[usize::from(row)] {
                    assert!(previous < row);
                }
            }
        }
        assert!(seen.into_iter().all(|value| value));
    }

    #[test]
    fn failed_build_does_not_publish_a_partial_run() {
        let mut successful = Memory::new();
        build(&mut successful, 37).unwrap();
        for failed_operation in 0..successful.operation {
            let mut disk = Memory::new();
            disk.fail_at = Some(failed_operation);
            assert!(
                build(&mut disk, 37).is_err(),
                "operation {failed_operation}"
            );
            assert_eq!(disk.published, None, "operation {failed_operation}");
            assert_eq!(disk.operation, failed_operation + 1);
        }
    }

    #[test]
    fn empty_and_single_row_indexes_publish() {
        for count in [0, 1] {
            let mut disk = Memory::new();
            build(&mut disk, count).unwrap();
            assert_eq!(disk.published, Some(false));
            if count == 1 {
                assert_eq!(disk.runs[0][0], 0);
            }
        }
    }

    #[test]
    fn single_row_slices_match_the_uninterrupted_sort() {
        for count in 0..=40 {
            let mut expected = Memory::new();
            build(&mut expected, count).unwrap();
            let mut actual = Memory::new();
            actual.begin(count).unwrap();
            let mut sorter = Sorter::new(count);
            let run = loop {
                if let Some(run) = sorter.step(&mut actual, 1).unwrap() {
                    break run;
                }
            };
            actual.publish(run, count).unwrap();
            assert_eq!(
                &actual.runs[usize::from(run)][..usize::from(count)],
                &expected.runs[usize::from(expected.published.unwrap())][..usize::from(count)],
                "count {count}"
            );
        }
    }
}
