//! Structural job progress types.

/// Structural progress of a long-running job measured in facts (done of total).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobProgress {
    pub done: u16,
    pub total: u16,
}

impl JobProgress {
    pub const fn new(done: u16, total: u16) -> Self {
        Self { done, total }
    }

    /// The completion ratio in whole percent, 0..=100, the unit a render
    /// request carries it in.
    pub const fn percent(self) -> u8 {
        let total = if self.total > 0 { self.total as u32 } else { 1 };
        let percent = (self.done as u32 * 100) / total;
        if percent > 100 {
            100
        } else {
            percent as u8
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_progress_percent() {
        assert_eq!(JobProgress::new(0, 10).percent(), 0);
        assert_eq!(JobProgress::new(5, 10).percent(), 50);
        assert_eq!(JobProgress::new(10, 10).percent(), 100);
        assert_eq!(JobProgress::new(15, 10).percent(), 100);
        assert_eq!(JobProgress::new(0, 0).percent(), 0);
        assert_eq!(JobProgress::new(7, 23).percent(), 30);
    }
}
