//! Mount-session source-integrity tracking: what this mount has proved
//! about each book's bytes, and what each level of proof permits.
//!
//! The PRD splits trust in a source into levels because the proofs have
//! wildly different costs. A full SHA-256 over a 40 MB EPUB is seconds of
//! SD reads; the bounded quick check is at most 12 KB. The cached-open
//! contract exists to spend the cheap proof at open time and the expensive
//! one in the background:
//!
//! - **Unchecked** — committed identity exists, nothing verified this
//!   mount. Nothing source-bound may be shown or done.
//! - **QuickChecked** — the bounded quick check matched. *Provisional*
//!   read-only display of previously committed state (text caches, covers,
//!   committed artifacts) is permitted while full validation runs behind
//!   it; mutation, decode, and publication stay prohibited. This is the
//!   PRD's explicit product trade: a bounded chance of briefly showing
//!   stale cached content, in exchange for the fast reopen.
//! - **FullyValidated** — exact length plus complete SHA-256 matched this
//!   mount. Everything is permitted.
//! - **Mismatch** — exact validation *failed*: the bytes are not the
//!   committed generation. A managed book in this state is the PRD's
//!   `ExternallyModified` quarantine — display stops, mutation from these
//!   bytes is prohibited, and the observed identity recorded here is what
//!   the list exposes so a client can authorize explicit recovery.
//! - **Unavailable** — the file is missing, or could not be read well
//!   enough to establish any identity.
//! - **UnsupportedContainer** — the bytes exceed container bounds or the
//!   classic-ZIP gate refused them. Identified, never opened.
//!
//! Levels live in resident memory only — they are claims about *this
//! mount*, so unmount, remount, reboot, or a detected filesystem change
//! resets them ([`MountSession::reset`]) and everything degrades to
//! `Unchecked`. Committed metadata is never touched: a mismatch does not
//! rewrite history, it quarantines the present.
//!
//! Successful commits seed the session: a managed upload's persisted-file
//! reread, a recovery's in-closure rehash, and an adoption's full hash are
//! each an exact-identity proof of the bytes the new generation vouches
//! for, taken at most one commit ago under the storage-owner
//! serialization. The PRD names this explicitly ("seed the current-mount
//! exact-validation set"), and it is what makes a freshly uploaded book
//! readable without immediately rehashing it.

use embedded_sdmmc::{BlockDevice, Directory, Mode, TimeSource};

use crate::bodies::{LOGICAL_BOOK_ID_BYTES, SHA256_BYTES};
use crate::select::{SlotEntry, MAX_SOURCE_SLOTS};
use crate::validate::{QuickFingerprintJob, Sha256Job, QUICK_FINGERPRINT_POLICY_V1};

/// How far this mount's proof about a book's bytes goes. Ordered by
/// neither trust nor severity — use the permission methods, not `>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrityLevel {
    Unchecked,
    QuickChecked,
    FullyValidated,
    Mismatch,
    Unavailable,
    UnsupportedContainer,
}

impl IntegrityLevel {
    /// May previously committed source-bound state (text caches, covers,
    /// committed artifacts) be displayed? True for the provisional tier
    /// and above — and *only* display: see [`may_use_source`].
    pub fn may_display_cached(self) -> bool {
        matches!(self, Self::QuickChecked | Self::FullyValidated)
    }

    /// May the source be opened, decoded, or used to create/replace any
    /// persistent state — caches, artifacts, negative results, metadata?
    /// Exact identity only. A quick check never authorizes mutation.
    pub fn may_use_source(self) -> bool {
        matches!(self, Self::FullyValidated)
    }
}

#[derive(Clone, Copy)]
struct SessionEntry {
    logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
    source_generation: u64,
    level: IntegrityLevel,
    /// The full identity actually observed when `level` is `Mismatch` —
    /// what the list exposes and a recovery request must quote back.
    observed_length: u64,
    observed_sha256: [u8; SHA256_BYTES],
}

/// The per-mount validation table. One entry per logical book (the
/// authoritative generation's), sized by the same bound as the catalog, so
/// it cannot overflow while the catalog invariants hold; if it ever were
/// full, new conclusions are dropped and the affected book simply stays
/// `Unchecked` — the fail-safe direction, since `Unchecked` permits
/// nothing.
pub struct MountSession {
    entries: [Option<SessionEntry>; MAX_SOURCE_SLOTS],
}

impl MountSession {
    /// `const` for the same reason as `OpsWorkspace::new`: the session
    /// table is embedded in the workspace static.
    pub const fn new() -> Self {
        Self {
            entries: [None; MAX_SOURCE_SLOTS],
        }
    }

    /// Forget every proof. Call on unmount, remount, reboot, or any
    /// detected filesystem change — the events after which "validated this
    /// mount" stops being true.
    pub fn reset(&mut self) {
        self.entries = [None; MAX_SOURCE_SLOTS];
    }

    /// The level this mount has established for exactly this generation.
    /// An entry for another generation of the same book does not answer —
    /// proofs are about specific bytes, and a generation change means
    /// different bytes.
    pub fn level(
        &self,
        logical_book_id: &[u8; LOGICAL_BOOK_ID_BYTES],
        source_generation: u64,
    ) -> IntegrityLevel {
        self.find(logical_book_id)
            .filter(|entry| entry.source_generation == source_generation)
            .map(|entry| entry.level)
            .unwrap_or(IntegrityLevel::Unchecked)
    }

    /// The observed identity behind a `Mismatch`, for the list and for
    /// recovery authorization. `None` at every other level.
    pub fn observed_identity(
        &self,
        logical_book_id: &[u8; LOGICAL_BOOK_ID_BYTES],
        source_generation: u64,
    ) -> Option<(u64, [u8; SHA256_BYTES])> {
        self.find(logical_book_id)
            .filter(|entry| {
                entry.source_generation == source_generation
                    && entry.level == IntegrityLevel::Mismatch
            })
            .map(|entry| (entry.observed_length, entry.observed_sha256))
    }

    /// Seed `FullyValidated` from a commit path whose publication just
    /// proved exact identity (upload reread, recovery rehash, adoption
    /// hash). Only meaningful immediately after committed selection.
    pub fn seed_full_validation(
        &mut self,
        logical_book_id: &[u8; LOGICAL_BOOK_ID_BYTES],
        source_generation: u64,
    ) {
        self.conclude(
            logical_book_id,
            source_generation,
            IntegrityLevel::FullyValidated,
            0,
            [0; SHA256_BYTES],
        );
    }

    /// Record that the container gate refused this generation's bytes.
    pub fn note_unsupported_container(
        &mut self,
        logical_book_id: &[u8; LOGICAL_BOOK_ID_BYTES],
        source_generation: u64,
    ) {
        self.conclude(
            logical_book_id,
            source_generation,
            IntegrityLevel::UnsupportedContainer,
            0,
            [0; SHA256_BYTES],
        );
    }

    /// Record a quick-check match. Deliberately upgrades `Unchecked` only:
    /// a quick match must never overwrite a full proof in either
    /// direction — not `FullyValidated` (it proves less) and not
    /// `Mismatch`/`Unavailable` (a 12 KB probe does not un-discover what a
    /// full pass discovered).
    fn note_quick_match(
        &mut self,
        logical_book_id: &[u8; LOGICAL_BOOK_ID_BYTES],
        source_generation: u64,
    ) {
        if self.level(logical_book_id, source_generation) == IntegrityLevel::Unchecked {
            self.conclude(
                logical_book_id,
                source_generation,
                IntegrityLevel::QuickChecked,
                0,
                [0; SHA256_BYTES],
            );
        }
    }

    /// Overwrite the book's entry with a completed conclusion. Full-pass
    /// conclusions replace anything, including an earlier `Mismatch`: a
    /// user who restores the original bytes gets their book back, because
    /// exact identity is content-addressed and cares about bytes, not
    /// history.
    fn conclude(
        &mut self,
        logical_book_id: &[u8; LOGICAL_BOOK_ID_BYTES],
        source_generation: u64,
        level: IntegrityLevel,
        observed_length: u64,
        observed_sha256: [u8; SHA256_BYTES],
    ) {
        let entry = SessionEntry {
            logical_book_id: *logical_book_id,
            source_generation,
            level,
            observed_length,
            observed_sha256,
        };
        if let Some(slot) = self
            .entries
            .iter()
            .position(|held| held.is_some_and(|held| held.logical_book_id == *logical_book_id))
            .or_else(|| self.entries.iter().position(Option::is_none))
        {
            self.entries[slot] = Some(entry);
        }
    }

    fn find(&self, logical_book_id: &[u8; LOGICAL_BOOK_ID_BYTES]) -> Option<&SessionEntry> {
        self.entries
            .iter()
            .flatten()
            .find(|entry| entry.logical_book_id == *logical_book_id)
    }
}

impl Default for MountSession {
    fn default() -> Self {
        Self::new()
    }
}

/// What the bounded quick check concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuickCheckOutcome {
    /// Presence, exact length, and quick fingerprint all matched. The
    /// session now permits provisional display; full validation is still
    /// owed and must start no later than the first reader open.
    Match,
    /// Something disagreed — length, fingerprint, or a fingerprint policy
    /// this build does not compute. Not a mismatch verdict: a quick check
    /// cannot prove modification any more than it can prove identity. It
    /// only means the cheap path is closed and full validation must run
    /// before anything source-bound is displayed.
    RequiresFullValidation,
    /// The file is gone. Recorded in the session: absence is a conclusion,
    /// not a probe failure.
    Unavailable,
}

/// The PRD's bounded quick check for one book: expected file present at
/// its expected location, exact file length, stored quick fingerprint over
/// the policy's regions. Reads at most ~12 KB regardless of source size.
///
/// `dir`/`name` locate the source bytes (managed slot file or unmanaged
/// books-directory file — the caller owns that resolution, as everywhere
/// in this crate). `entry` is the committed metadata being checked
/// against.
pub fn quick_check<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    name: &str,
    entry: &SlotEntry,
    session: &mut MountSession,
) -> QuickCheckOutcome
where
    D: BlockDevice,
    T: TimeSource,
{
    let meta = &entry.metadata;
    // A fingerprint computed under a policy this build does not implement
    // cannot be compared; the quick path is closed, nothing is concluded.
    if meta.quick_fingerprint_policy_version != QUICK_FINGERPRINT_POLICY_V1 {
        return QuickCheckOutcome::RequiresFullValidation;
    }
    let file = match dir.open_file_in_dir(name, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => {
            // Only a confirmed absence concludes; a card that could not
            // answer closes the quick path without recording anything.
            if crate::publish::confirm_absent(dir, name).is_ok() {
                session.conclude(
                    &meta.logical_book_id,
                    meta.source_generation,
                    IntegrityLevel::Unavailable,
                    0,
                    [0; SHA256_BYTES],
                );
                return QuickCheckOutcome::Unavailable;
            }
            return QuickCheckOutcome::RequiresFullValidation;
        }
        Err(_) => return QuickCheckOutcome::RequiresFullValidation,
    };
    let length = u64::from(file.length());
    if length != meta.source_length {
        let _ = file.close();
        return QuickCheckOutcome::RequiresFullValidation;
    }
    let mut job = QuickFingerprintJob::new(length);
    let mut chunk = [0u8; QUICK_CHECK_CHUNK];
    while let Some((offset, remaining)) = job.next_read() {
        let want = (remaining as usize).min(chunk.len());
        let seek = u32::try_from(offset)
            .ok()
            .and_then(|offset| file.seek_from_start(offset).ok());
        if seek.is_none()
            || read_exact(&file, &mut chunk[..want]).is_err()
            || job.update(&chunk[..want]).is_err()
        {
            let _ = file.close();
            return QuickCheckOutcome::RequiresFullValidation;
        }
    }
    let closed = file.close();
    match job.finish() {
        Ok(fingerprint) if closed.is_ok() && fingerprint == meta.quick_fingerprint_sha256 => {
            session.note_quick_match(&meta.logical_book_id, meta.source_generation);
            QuickCheckOutcome::Match
        }
        _ => QuickCheckOutcome::RequiresFullValidation,
    }
}

/// Quick-check read chunk: one SD sector's worth on the stack.
const QUICK_CHECK_CHUNK: usize = 512;

/// One bounded unit of background full validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationStep {
    /// More bytes remain; call again with the next budget.
    Pending,
    /// The concluded level, already recorded in the session:
    /// `FullyValidated`, `Mismatch`, or `Unavailable`.
    Concluded(IntegrityLevel),
}

/// The background full-validation job behind the provisional cached open:
/// a resumable full SHA-256 over the *current* file, compared against the
/// committed identity only at the end.
///
/// The whole current file is hashed even when its length already disagrees
/// with the committed length, because the point of a mismatch conclusion
/// is the *observed* identity it records — the exact `(length, sha256)`
/// the list exposes and an explicit recovery request must quote back. A
/// cheap "length differs, done" answer would leave recovery with nothing
/// to authorize against.
///
/// Each [`step`][Self::step] reopens the file, seeks to its cursor, and
/// reads at most `budget_bytes` (clamped to its scratch), so the job holds
/// no handle between executor turns and one in-flight bounded SD request
/// is the most a cancellation ever waits for. A file whose length changes
/// between steps is being written *right now*; the job concludes
/// `Unavailable` — no identity can be established from a moving target —
/// and a later fresh job settles it.
pub struct FullValidationJob {
    logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
    source_generation: u64,
    expected_length: u64,
    expected_sha256: [u8; SHA256_BYTES],
    hasher: Option<Sha256Job>,
    observed_length: u64,
    cursor: u64,
}

impl FullValidationJob {
    /// Validate `entry`'s committed identity against whatever bytes its
    /// source location currently holds.
    pub fn new(entry: &SlotEntry) -> Self {
        Self {
            logical_book_id: entry.metadata.logical_book_id,
            source_generation: entry.metadata.source_generation,
            expected_length: entry.metadata.source_length,
            expected_sha256: entry.metadata.source_sha256,
            hasher: None,
            observed_length: 0,
            cursor: 0,
        }
    }

    /// Hash up to `budget_bytes` more (clamped to `scratch.len()` per SD
    /// read). Conclusions are written to `session` before they are
    /// returned, so a caller that drops the job mid-run loses progress but
    /// never a verdict.
    pub fn step<D, T, const MD: usize, const MF: usize, const MV: usize>(
        &mut self,
        dir: &Directory<'_, D, T, MD, MF, MV>,
        name: &str,
        session: &mut MountSession,
        budget_bytes: usize,
        scratch: &mut [u8],
    ) -> ValidationStep
    where
        D: BlockDevice,
        T: TimeSource,
    {
        if scratch.is_empty() || budget_bytes == 0 {
            return ValidationStep::Pending;
        }
        let file = match dir.open_file_in_dir(name, Mode::ReadOnly) {
            Ok(file) => file,
            Err(_) => return self.conclude_unavailable(session),
        };
        let length = u64::from(file.length());
        if self.hasher.is_none() {
            self.observed_length = length;
            self.hasher = Some(Sha256Job::new(length));
        } else if length != self.observed_length {
            let _ = file.close();
            return self.conclude_unavailable(session);
        }
        // Split borrow: the hasher is owned; take it back on every path.
        let Some(hasher) = self.hasher.as_mut() else {
            return ValidationStep::Pending;
        };
        let mut spent = 0usize;
        while spent < budget_bytes && hasher.remaining() > 0 {
            let want = (hasher.remaining() as usize)
                .min(scratch.len())
                .min(budget_bytes - spent);
            let seek = u32::try_from(self.cursor)
                .ok()
                .and_then(|cursor| file.seek_from_start(cursor).ok());
            if seek.is_none()
                || read_exact(&file, &mut scratch[..want]).is_err()
                || hasher.update(&scratch[..want]).is_err()
            {
                let _ = file.close();
                return self.conclude_unavailable(session);
            }
            self.cursor += want as u64;
            spent += want;
        }
        let done = hasher.remaining() == 0;
        if file.close().is_err() {
            return self.conclude_unavailable(session);
        }
        if !done {
            return ValidationStep::Pending;
        }
        let observed = match self.hasher.take().map(Sha256Job::finish) {
            Some(Ok(digest)) => digest,
            _ => return self.conclude_unavailable(session),
        };
        let level =
            if self.observed_length == self.expected_length && observed == self.expected_sha256 {
                IntegrityLevel::FullyValidated
            } else {
                IntegrityLevel::Mismatch
            };
        session.conclude(
            &self.logical_book_id,
            self.source_generation,
            level,
            self.observed_length,
            observed,
        );
        ValidationStep::Concluded(level)
    }

    fn conclude_unavailable(&mut self, session: &mut MountSession) -> ValidationStep {
        self.hasher = None;
        session.conclude(
            &self.logical_book_id,
            self.source_generation,
            IntegrityLevel::Unavailable,
            0,
            [0; SHA256_BYTES],
        );
        ValidationStep::Concluded(IntegrityLevel::Unavailable)
    }
}

fn read_exact<D, T, const MD: usize, const MF: usize, const MV: usize>(
    file: &embedded_sdmmc::File<'_, D, T, MD, MF, MV>,
    buf: &mut [u8],
) -> Result<(), ()>
where
    D: BlockDevice,
    T: TimeSource,
{
    let mut at = 0usize;
    while at < buf.len() {
        match file.read(&mut buf[at..]) {
            Ok(0) | Err(_) => return Err(()),
            Ok(n) => at += n,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_gate_display_and_use() {
        assert!(!IntegrityLevel::Unchecked.may_display_cached());
        assert!(IntegrityLevel::QuickChecked.may_display_cached());
        assert!(IntegrityLevel::FullyValidated.may_display_cached());
        assert!(!IntegrityLevel::Mismatch.may_display_cached());
        assert!(!IntegrityLevel::Unavailable.may_display_cached());
        assert!(!IntegrityLevel::UnsupportedContainer.may_display_cached());

        // The provisional tier must never authorize source use — that is
        // the one-line contract the whole cached-open trade rests on.
        assert!(!IntegrityLevel::QuickChecked.may_use_source());
        assert!(IntegrityLevel::FullyValidated.may_use_source());
    }

    #[test]
    fn session_tracks_per_generation_and_upgrades_conservatively() {
        let mut session = MountSession::new();
        let book = [7u8; LOGICAL_BOOK_ID_BYTES];
        assert_eq!(session.level(&book, 1), IntegrityLevel::Unchecked);

        session.note_quick_match(&book, 1);
        assert_eq!(session.level(&book, 1), IntegrityLevel::QuickChecked);
        // A proof about generation 1 says nothing about generation 2.
        assert_eq!(session.level(&book, 2), IntegrityLevel::Unchecked);

        // Full conclusions overwrite; quick matches never downgrade them.
        session.conclude(&book, 1, IntegrityLevel::Mismatch, 9, [1; SHA256_BYTES]);
        session.note_quick_match(&book, 1);
        assert_eq!(session.level(&book, 1), IntegrityLevel::Mismatch);
        assert_eq!(
            session.observed_identity(&book, 1),
            Some((9, [1; SHA256_BYTES]))
        );

        // A later full pass that matches gets the book back.
        session.conclude(
            &book,
            1,
            IntegrityLevel::FullyValidated,
            0,
            [0; SHA256_BYTES],
        );
        assert_eq!(session.level(&book, 1), IntegrityLevel::FullyValidated);
        assert_eq!(session.observed_identity(&book, 1), None);

        // A new generation replaces the book's entry outright.
        session.seed_full_validation(&book, 2);
        assert_eq!(session.level(&book, 2), IntegrityLevel::FullyValidated);
        assert_eq!(session.level(&book, 1), IntegrityLevel::Unchecked);

        session.reset();
        assert_eq!(session.level(&book, 2), IntegrityLevel::Unchecked);
    }
}
