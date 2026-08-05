//! Source-identity computation: exact identity (length plus full SHA-256)
//! and the bounded quick fingerprint, as caller-driven resumable jobs.
//!
//! The PRD splits source trust into two tiers with very different costs:
//!
//! - **Exact identity** — full length plus full SHA-256 — is the only
//!   proof that authorizes mutation, artifact publication, or source
//!   decode. Hashing a whole EPUB is the expensive path, so [`Sha256Job`]
//!   is incremental: the executor feeds bounded chunks across cooperative
//!   turns and no step exceeds its I/O budget.
//! - **The quick fingerprint** hashes three bounded regions (head, middle,
//!   tail, up to [`QUICK_REGION_BYTES`] each) plus the length, under a
//!   domain-separation tag and a policy version. It exists for one
//!   purpose: the provisional fast cached open, where a match permits
//!   *read-only display of previously committed state* while full
//!   validation runs behind it. It is explicitly not an integrity proof —
//!   [`tests`] pin the property that a byte outside its regions changes
//!   nothing — and nothing here lets it authorize more.
//!
//! Both jobs are pull-shaped: the job says what to read next, the caller
//! reads (through whatever bounded SD path it owns) and feeds bytes back.
//! Chunk boundaries carry no meaning — any split produces the same digest,
//! which the metamorphic tests pin — so the executor can size reads purely
//! by its work-slice budget.

use sha2::{Digest, Sha256};

use crate::bodies::SHA256_BYTES;

/// The current quick-fingerprint region policy. Stored in source metadata;
/// a record fingerprinted under a different policy simply forces full
/// validation instead of the quick path.
pub const QUICK_FINGERPRINT_POLICY_V1: u16 = 1;

/// Per-region byte budget. Three regions plus the tag: a quick check reads
/// at most 12 KB regardless of source size.
pub const QUICK_REGION_BYTES: u64 = 4096;

/// Domain-separation tag hashed ahead of the policy version, length, and
/// region bytes, so a quick fingerprint can never collide by construction
/// with a full-file SHA-256 of anything.
const QUICK_TAG: &[u8; 4] = b"XTQF";

/// Exact source identity — the pair every authoritative decision compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceIdentity {
    pub length: u64,
    pub sha256: [u8; SHA256_BYTES],
}

/// The deterministic policy-v1 regions for a source of `length` bytes:
/// head, centered middle (512-aligned for SD friendliness), and tail, each
/// clamped to the file. Regions may overlap on small files; overlap is
/// deterministic and therefore fine.
pub fn quick_regions_v1(length: u64) -> [(u64, u64); 3] {
    let clamp = |offset: u64| {
        let offset = offset.min(length);
        (offset, QUICK_REGION_BYTES.min(length - offset))
    };
    let middle = (length / 2).saturating_sub(QUICK_REGION_BYTES / 2) & !511;
    [
        clamp(0),
        clamp(middle),
        clamp(length.saturating_sub(QUICK_REGION_BYTES)),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashJobError {
    /// More bytes fed than the declared length.
    Overrun,
    /// Finished before the declared length was fed.
    Short,
    /// Bytes fed with no read outstanding.
    NotExpecting,
}

/// Incremental full-file SHA-256 against a declared length. The declared
/// length is a *contract*: feeding past it or finishing short is an error,
/// because every caller compares the digest against an identity that pairs
/// the hash with exactly that length.
pub struct Sha256Job {
    hasher: Sha256,
    processed: u64,
    expected: u64,
}

impl Sha256Job {
    pub fn new(expected_length: u64) -> Self {
        Self {
            hasher: Sha256::new(),
            processed: 0,
            expected: expected_length,
        }
    }

    pub fn processed(&self) -> u64 {
        self.processed
    }

    /// Bytes still to feed — what the caller sizes its next bounded read
    /// from.
    pub fn remaining(&self) -> u64 {
        self.expected - self.processed
    }

    pub fn update(&mut self, chunk: &[u8]) -> Result<(), HashJobError> {
        let len = chunk.len() as u64;
        if len > self.remaining() {
            return Err(HashJobError::Overrun);
        }
        self.hasher.update(chunk);
        self.processed += len;
        Ok(())
    }

    pub fn finish(self) -> Result<[u8; SHA256_BYTES], HashJobError> {
        if self.processed != self.expected {
            return Err(HashJobError::Short);
        }
        Ok(self.hasher.finalize().into())
    }
}

/// One-shot convenience for hosts and tests; firmware uses the job.
pub fn sha256_of(bytes: &[u8]) -> [u8; SHA256_BYTES] {
    let mut job = Sha256Job::new(bytes.len() as u64);
    // Both calls are infallible by construction: the length matches.
    let _ = job.update(bytes);
    job.finish().unwrap_or([0; SHA256_BYTES])
}

/// The quick-fingerprint job. Pull-driven: [`next_read`][Self::next_read]
/// names the exact offset and remaining length of the current region, the
/// caller reads some prefix of it and feeds the bytes back in order.
pub struct QuickFingerprintJob {
    hasher: Sha256,
    regions: [(u64, u64); 3],
    region: usize,
    region_fed: u64,
}

impl QuickFingerprintJob {
    pub fn new(length: u64) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(QUICK_TAG);
        hasher.update(QUICK_FINGERPRINT_POLICY_V1.to_le_bytes());
        hasher.update(length.to_le_bytes());
        let mut job = Self {
            hasher,
            regions: quick_regions_v1(length),
            region: 0,
            region_fed: 0,
        };
        job.skip_empty_regions();
        job
    }

    fn skip_empty_regions(&mut self) {
        while self.region < self.regions.len() && self.region_fed == self.regions[self.region].1 {
            self.region += 1;
            self.region_fed = 0;
        }
    }

    /// The next `(offset, remaining)` to read, or `None` when every region
    /// has been fed and [`finish`][Self::finish] may be called.
    pub fn next_read(&self) -> Option<(u64, u64)> {
        let (offset, len) = *self.regions.get(self.region)?;
        Some((offset + self.region_fed, len - self.region_fed))
    }

    /// Feed bytes read at the offset [`next_read`][Self::next_read] gave.
    /// Any prefix length is fine; chunk splits do not affect the digest.
    pub fn update(&mut self, chunk: &[u8]) -> Result<(), HashJobError> {
        let Some((_, remaining)) = self.next_read() else {
            return Err(HashJobError::NotExpecting);
        };
        if chunk.len() as u64 > remaining {
            return Err(HashJobError::Overrun);
        }
        self.hasher.update(chunk);
        self.region_fed += chunk.len() as u64;
        self.skip_empty_regions();
        Ok(())
    }

    pub fn finish(self) -> Result<[u8; SHA256_BYTES], HashJobError> {
        if self.region < self.regions.len() {
            return Err(HashJobError::Short);
        }
        Ok(self.hasher.finalize().into())
    }
}

/// One-shot quick fingerprint over an in-memory source; hosts and tests.
pub fn quick_fingerprint_of(bytes: &[u8]) -> [u8; SHA256_BYTES] {
    let mut job = QuickFingerprintJob::new(bytes.len() as u64);
    while let Some((offset, remaining)) = job.next_read() {
        let start = offset as usize;
        let end = start + remaining as usize;
        if job.update(&bytes[start..end]).is_err() {
            return [0; SHA256_BYTES];
        }
    }
    job.finish().unwrap_or([0; SHA256_BYTES])
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec::Vec;

    fn fixture(len: usize) -> Vec<u8> {
        (0..len).map(|n| (n * 31 % 251) as u8).collect()
    }

    #[test]
    fn sha_job_matches_reference_and_enforces_length() {
        let bytes = fixture(10_000);
        let mut job = Sha256Job::new(bytes.len() as u64);
        for chunk in bytes.chunks(777) {
            job.update(chunk).unwrap();
        }
        let expected: [u8; 32] = {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            hasher.finalize().into()
        };
        assert_eq!(job.finish().unwrap(), expected);
        assert_eq!(sha256_of(&bytes), expected);

        let mut short = Sha256Job::new(10);
        short.update(&[0; 5]).unwrap();
        assert_eq!(short.finish(), Err(HashJobError::Short));
        let mut over = Sha256Job::new(4);
        assert_eq!(over.update(&[0; 5]), Err(HashJobError::Overrun));
    }

    #[test]
    fn quick_fingerprint_is_chunking_invariant() {
        let bytes = fixture(50_000);
        let whole = quick_fingerprint_of(&bytes);
        // Byte-at-a-time delivery must produce the identical digest.
        let mut job = QuickFingerprintJob::new(bytes.len() as u64);
        while let Some((offset, _)) = job.next_read() {
            job.update(&bytes[offset as usize..offset as usize + 1])
                .unwrap();
        }
        assert_eq!(job.finish().unwrap(), whole);
    }

    #[test]
    fn quick_fingerprint_covers_its_regions_and_only_them() {
        let bytes = fixture(50_000);
        let baseline = quick_fingerprint_of(&bytes);

        // A change inside any region changes the fingerprint.
        for (offset, len) in quick_regions_v1(bytes.len() as u64) {
            let mut changed = bytes.clone();
            changed[(offset + len / 2) as usize] ^= 0xFF;
            assert_ne!(
                quick_fingerprint_of(&changed),
                baseline,
                "change at region {offset}+{len} went unseen"
            );
        }

        // A change outside every region does NOT change it — the pinned
        // proof that a quick match is not exact identity and must never
        // authorize mutation.
        let regions = quick_regions_v1(bytes.len() as u64);
        let outside = 10_000u64; // between head (0..4096) and middle
        assert!(regions
            .iter()
            .all(|(offset, len)| outside < *offset || outside >= offset + len));
        let mut changed = bytes.clone();
        changed[outside as usize] ^= 0xFF;
        assert_eq!(quick_fingerprint_of(&changed), baseline);

        // Same bytes, different length: never equal (length is hashed).
        assert_ne!(quick_fingerprint_of(&bytes[..49_999]), baseline);
    }

    #[test]
    fn quick_fingerprint_handles_small_and_empty_sources() {
        for len in [0usize, 1, 511, 512, 4096, 8192, 12_288, 12_289] {
            let bytes = fixture(len);
            // Must terminate and be reproducible; regions overlap freely.
            assert_eq!(quick_fingerprint_of(&bytes), quick_fingerprint_of(&bytes));
            for (offset, region_len) in quick_regions_v1(len as u64) {
                assert!(offset + region_len <= len as u64, "region out of bounds");
            }
        }
        // Distinct tiny sources get distinct fingerprints (regions cover
        // everything below 4 KB).
        assert_ne!(
            quick_fingerprint_of(&fixture(100)),
            quick_fingerprint_of(&{
                let mut b = fixture(100);
                b[50] ^= 1;
                b
            })
        );
    }

    #[test]
    fn quick_job_rejects_misuse() {
        let mut done = QuickFingerprintJob::new(0);
        assert_eq!(done.next_read(), None);
        assert_eq!(done.update(&[1]), Err(HashJobError::NotExpecting));

        let mut job = QuickFingerprintJob::new(100);
        // Region is 100 bytes (clamped); feeding 101 overruns.
        assert_eq!(job.update(&[0; 101]), Err(HashJobError::Overrun));
        job.update(&[0; 100]).unwrap();
    }
}
