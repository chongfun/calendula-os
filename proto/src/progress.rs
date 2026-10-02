//! Structural job progress types.

/// Structural progress of a long-running job measured in facts (done of total).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct JobProgress {
    pub done: u16,
    pub total: u16,
}

impl JobProgress {
    pub const fn new(done: u16, total: u16) -> Self {
        Self { done, total }
    }

    /// Convert the structural completion ratio into permille (parts per thousand, 0..=1000).
    pub const fn permille(self) -> u16 {
        let total = if self.total > 0 { self.total as u32 } else { 1 };
        let permille = (self.done as u32 * 1000) / total;
        if permille > 1000 {
            1000
        } else {
            permille as u16
        }
    }
}

/// A projection of a book's background build progress held for rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildProgressView {
    pub book_id: u32,
    pub done: u16,
    pub total: u16,
}

impl BuildProgressView {
    pub const fn new(book_id: u32, progress: JobProgress) -> Self {
        Self {
            book_id,
            done: progress.done,
            total: progress.total,
        }
    }

    pub const fn progress(self) -> JobProgress {
        JobProgress {
            done: self.done,
            total: self.total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_progress_permille() {
        assert_eq!(JobProgress::new(0, 10).permille(), 0);
        assert_eq!(JobProgress::new(5, 10).permille(), 500);
        assert_eq!(JobProgress::new(10, 10).permille(), 1000);
        assert_eq!(JobProgress::new(15, 10).permille(), 1000);
        assert_eq!(JobProgress::new(0, 0).permille(), 0);
        assert_eq!(JobProgress::new(7, 23).permille(), 304);
    }
}
