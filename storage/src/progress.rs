//! How far a library scan has got, for a caller that shows it.
//!
//! The scan is one card session, so the panel shows progress only on a bus
//! the session lends it. [`ProgressSink::report`] is that loan: called between
//! card operations, at most every [`REPORT_INTERVAL_MS`], with a percentage
//! that only rises and stays below 100.

use upload_store::ledger::AssignProgress;

/// The least time between two reports, measured from the end of the last.
/// Each one costs the scan a fast refresh, about 380 ms on the X3, so this
/// keeps the panel's share of the scan's time near a sixth.
pub const REPORT_INTERVAL_MS: u64 = 2_000;

/// The highest percentage a scan reports. The frame after the scan replaces
/// the note, so 100 is shown by the scan ending rather than by a report.
pub const MAX_REPORTED_PERCENT: u8 = 99;

/// Where a scan's progress goes.
///
/// `report` is called only between card operations, not inside a block
/// transfer or a directory iteration: the card is deselected and its volume
/// manager free, so an implementation may lend the bus to the panel.
pub trait ProgressSink {
    /// Milliseconds on the clock reports are spaced by.
    fn now_ms(&mut self) -> u64;
    /// The scan is `percent` done.
    fn report(&mut self, percent: u8);
}

/// A sink that shows nothing, for every scan nobody is watching.
pub struct Silent;

impl ProgressSink for Silent {
    fn now_ms(&mut self) -> u64 {
        0
    }

    fn report(&mut self, _percent: u8) {}
}

// Cost estimates in X3 milliseconds, from one 1,100-book card with one moved
// 11.7 MB book: 13.3 s in all, 8.7 s of it hashing and 1.9 s the carry. Only
// their ratios matter, and only to keep the rate of the percentage steady.
const WALK_MS_PER_BOOK: u64 = 1;
const MATCH_MS_PER_BOOK: u64 = 1;
const WRITE_MS_PER_BOOK: u64 = 1;
const FINISH_MS: u64 = 100;
const HASH_BYTES_PER_MS: u64 = 1_350;
const CARRY_MS: u64 = 1_900;
/// Room kept for a move proof until the join says whether one is owed, so
/// a scan that turns out to hash has not already shown most of its bar.
const UNPROVED_MS: u64 = 4_000;

/// One scan's progress: estimated work done against estimated work left,
/// rate-limited onto a sink.
///
/// A revision (the book count, then the bytes a move needs hashed) keeps the
/// percentage earned and spreads what is left over the new estimate: it
/// cannot fall back, and a revision alone does not move it.
pub struct ScanProgress<'s> {
    sink: &'s mut dyn ProgressSink,
    /// The fraction already earned when the estimate was last revised, in
    /// hundredths of a percent.
    base: u32,
    /// Estimated milliseconds done since that revision.
    done: u64,
    /// Estimated milliseconds left at that revision.
    left: u64,
    /// Books the scan expects.
    books: u64,
    /// Bytes the move search expects to hash, and the bytes reported so far.
    hash_planned: u64,
    hashed: u64,
    /// Rows the join has reported.
    matched: u64,
    shown: u8,
    /// When the last report ended, or the scan began.
    since_ms: u64,
}

/// The whole scan's estimate before the join says whether a move is owed.
const fn unproved_ms(books: u64) -> u64 {
    books * (WALK_MS_PER_BOOK + MATCH_MS_PER_BOOK + WRITE_MS_PER_BOOK) + FINISH_MS + UNPROVED_MS
}

impl<'s> ScanProgress<'s> {
    /// A scan expected to find about `books` books.
    pub fn new(sink: &'s mut dyn ProgressSink, books: usize) -> Self {
        let books = books.max(1) as u64;
        let since_ms = sink.now_ms();
        Self {
            sink,
            base: 0,
            done: 0,
            left: unproved_ms(books),
            books,
            hash_planned: 0,
            hashed: 0,
            matched: 0,
            shown: 0,
            since_ms,
        }
    }

    /// The walk counted `books` books; the rest of the estimate follows them.
    pub fn counted(&mut self, books: usize) {
        self.books = books.max(1) as u64;
        self.revise(unproved_ms(self.books));
    }

    /// `rows` more catalog rows are written.
    pub fn walked(&mut self, rows: usize) {
        self.advance(rows as u64 * WALK_MS_PER_BOOK);
    }

    /// The ledger said how far it has got.
    pub fn assign(&mut self, step: AssignProgress) {
        match step {
            AssignProgress::Matched { rows } => {
                let rows = u64::from(rows).min(self.books);
                let new = rows.saturating_sub(self.matched);
                self.matched = rows;
                self.advance(new * MATCH_MS_PER_BOOK);
            }
            AssignProgress::Proving { bytes } => {
                // Whatever of the join was not reported is done now.
                let unreported = self.books.saturating_sub(self.matched);
                self.matched = self.books;
                self.advance(unreported * MATCH_MS_PER_BOOK);
                self.hash_planned = bytes;
                // A copy proved is a copy carried, one per file expected.
                let carries = if bytes > 0 { CARRY_MS } else { 0 };
                let left = bytes / HASH_BYTES_PER_MS
                    + carries
                    + self.books * WRITE_MS_PER_BOOK
                    + FINISH_MS;
                self.revise(left);
            }
            AssignProgress::Hashed { bytes } => {
                // Past the plan, the search earns nothing more: a second file
                // of one length is work the estimate did not hold.
                let bytes = bytes.min(self.hash_planned);
                let new = bytes.saturating_sub(self.hashed);
                if new >= HASH_BYTES_PER_MS {
                    self.hashed += new - new % HASH_BYTES_PER_MS;
                    self.advance(new / HASH_BYTES_PER_MS);
                }
            }
        }
    }

    /// One proved move's reading state is carried.
    pub fn carried(&mut self) {
        self.advance(CARRY_MS);
    }

    /// The ledger's new generation is written: all but the finish is done,
    /// whatever the ledger reported on the way.
    pub fn assigned(&mut self) {
        self.done = self.done.max(self.left.saturating_sub(FINISH_MS));
        self.advance(0);
    }

    /// The percentage earned so far, before rate-limiting.
    pub fn percent(&self) -> u8 {
        (self.fraction() / 100).min(u32::from(MAX_REPORTED_PERCENT)) as u8
    }

    /// Every percentage reported so far has been at most this.
    pub fn shown(&self) -> u8 {
        self.shown
    }

    fn fraction(&self) -> u32 {
        if self.left == 0 {
            return self.base;
        }
        let rest = u64::from(10_000 - self.base);
        let earned = rest * self.done.min(self.left) / self.left;
        self.base + earned as u32
    }

    fn revise(&mut self, left: u64) {
        self.base = self.fraction();
        self.done = 0;
        self.left = left.max(1);
    }

    fn advance(&mut self, ms: u64) {
        self.done = self.done.saturating_add(ms);
        let percent = self.percent();
        // The clock is read only when there is something new to show.
        if percent <= self.shown {
            return;
        }
        if self.sink.now_ms().saturating_sub(self.since_ms) < REPORT_INTERVAL_MS {
            return;
        }
        self.sink.report(percent);
        self.shown = percent;
        self.since_ms = self.sink.now_ms();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock that moves `step` milliseconds each time it is read.
    struct Ticking {
        now: u64,
        step: u64,
        reports: [(u64, u8); 128],
        count: usize,
    }

    impl Ticking {
        fn new(step: u64) -> Self {
            Self {
                now: 0,
                step,
                reports: [(0, 0); 128],
                count: 0,
            }
        }

        fn reports(&self) -> &[(u64, u8)] {
            &self.reports[..self.count]
        }
    }

    impl ProgressSink for Ticking {
        fn now_ms(&mut self) -> u64 {
            self.now += self.step;
            self.now
        }

        fn report(&mut self, percent: u8) {
            self.reports[self.count] = (self.now, percent);
            self.count += 1;
        }
    }

    fn assert_well_formed(reports: &[(u64, u8)]) {
        let mut last: Option<(u64, u8)> = None;
        for &(at, percent) in reports {
            assert!(percent <= MAX_REPORTED_PERCENT, "{reports:?}");
            if let Some((was_at, was)) = last {
                assert!(percent > was, "rising: {reports:?}");
                assert!(at - was_at >= REPORT_INTERVAL_MS, "spaced: {reports:?}");
            }
            last = Some((at, percent));
        }
    }

    /// The measured X3 scan, replayed: the hash dominates the percentage, as
    /// it dominates the time.
    #[test]
    fn a_move_is_mostly_its_hash() {
        let mut sink = Ticking::new(400);
        let mut progress = ScanProgress::new(&mut sink, 1_100);
        progress.counted(1_100);
        for _ in 0..1_100 / 37 {
            progress.walked(37);
        }
        progress.assign(AssignProgress::Matched { rows: 1_100 });
        let before_hash = progress.percent();
        let bytes = 11_700_000;
        progress.assign(AssignProgress::Proving { bytes });
        assert_eq!(
            progress.percent(),
            before_hash,
            "a revision alone moves nothing"
        );
        let mut hashed = 0;
        while hashed < bytes {
            hashed = (hashed + 4_096).min(bytes);
            progress.assign(AssignProgress::Hashed { bytes: hashed });
        }
        let after_hash = progress.percent();
        assert!(before_hash < 35, "{before_hash}");
        assert!(after_hash - before_hash > 40, "{before_hash}..{after_hash}");
        progress.carried();
        progress.assigned();
        assert!(progress.percent() <= MAX_REPORTED_PERCENT);
        let shown = progress.shown();
        assert!(shown > after_hash.saturating_sub(10), "{shown}");
        assert_well_formed(sink.reports());
        assert!(sink.reports().len() > 3, "{:?}", sink.reports());
    }

    /// No move: the phases alone carry it, and still stop at 99.
    #[test]
    fn phases_alone_carry_a_scan_with_nothing_to_prove() {
        let mut sink = Ticking::new(900);
        let mut progress = ScanProgress::new(&mut sink, 50);
        progress.counted(1_000);
        for _ in 0..1_000 / 10 {
            progress.walked(10);
        }
        for rows in (100..=1_000).step_by(100) {
            progress.assign(AssignProgress::Matched { rows });
        }
        progress.assign(AssignProgress::Proving { bytes: 0 });
        progress.assigned();
        assert!(progress.percent() >= 90, "{}", progress.percent());
        assert!(progress.percent() <= MAX_REPORTED_PERCENT);
        assert_well_formed(sink.reports());
        assert!(!sink.reports().is_empty());
    }

    /// A clock that hardly moves reports nothing: the interval holds even
    /// when every step earns a percent.
    #[test]
    fn reports_wait_out_the_interval() {
        let mut sink = Ticking::new(1);
        let mut progress = ScanProgress::new(&mut sink, 100);
        for _ in 0..100 {
            progress.walked(1);
        }
        assert!(sink.reports().is_empty(), "{:?}", sink.reports());
    }

    /// More hashing than planned holds the percentage rather than running it
    /// past the phases still to come.
    #[test]
    fn an_overrun_hash_holds_its_share() {
        let mut sink = Ticking::new(5_000);
        let mut progress = ScanProgress::new(&mut sink, 10);
        progress.assign(AssignProgress::Proving { bytes: 1_000_000 });
        progress.assign(AssignProgress::Hashed { bytes: 1_000_000 });
        let planned = progress.percent();
        progress.assign(AssignProgress::Hashed { bytes: 5_000_000 });
        assert_eq!(progress.percent(), planned);
        assert!(planned < MAX_REPORTED_PERCENT);
        assert_well_formed(sink.reports());
    }
}
