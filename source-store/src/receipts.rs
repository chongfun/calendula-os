//! Idempotency state: the epoch pair and the compact operation receipts
//! that make create, replace, delete, and recovery safe to retry after an
//! unknown HTTP outcome.
//!
//! One A/B authoritative record holds the whole state — the current and
//! previous epochs plus every retained receipt — because the retry
//! guarantees are *joint*: a receipt is only as durable as the epoch pair
//! that scopes it, and the PRD's rule that "an operation receipt is not
//! reclaimed while any delayed request in an accepted epoch could be
//! interpreted as new" is a property of the state as a whole. Publishing it
//! through the same commit-sector protocol as source metadata means a
//! half-written receipt table can never be consulted.
//!
//! Receipts are *compact result bindings*, not transcripts: enough identity
//! to prove a request was seen with these exact parameters
//! ([`OperationReceipt::matches_parameters`]) and enough result to answer it
//! again ([`result_book_token_or_zero`][OperationReceipt::result_book_token_or_zero],
//! [`result_status`][OperationReceipt::result_status]). Deletion replay
//! safety rides on these — a tombstone can be reclaimed once its delete's
//! receipt survives here, which is why receipts carry their own CRC on top
//! of the record body CRC: the field list is pinned byte-for-byte, and a
//! future partial-salvage path gets per-receipt integrity for free.
//!
//! The receipt table is sorted by `(epoch, nonce)` and lookups are binary
//! searches; insertion keeps order. Capacity is compile-time
//! ([`MAX_RECEIPTS`]) and rotation is the caller's move: when
//! [`IdempotencyState::insert`] reports full, the owner rotates the epoch —
//! which retires receipts older than the *previous* epoch and re-scopes
//! "genuinely new" — and the capabilities response advertises both numbers
//! so the browser can pace itself.

use crate::record::{RecordView, BODY_CRC_BYTES, BODY_PREFIX_BYTES};

use crate::bodies::{
    validate_display_label, DisplayLabel, RequestBinding, BINDING_TAG_RECOVER, BINDING_TAG_UPLOAD,
    BOOK_TOKEN_BYTES, DISPLAY_LABEL_MAX_BYTES, LOGICAL_BOOK_ID_BYTES, REQUEST_ID_BYTES,
    SHA256_BYTES,
};

pub const IDEMPOTENCY_MAGIC: [u8; 4] = *b"XTID";
pub const IDEMPOTENCY_SCHEMA: u16 = 1;

/// Retained receipts across the current and previous epochs. Provisional
/// v1 constant (a PRD measurement gate): 32 bounds the record at one
/// ~7 KB file while comfortably exceeding what a browser session issues
/// between rotations.
pub const MAX_RECEIPTS: usize = 32;

/// New operations accepted per epoch — the "maximum new operation requests
/// per epoch" the capabilities response advertises. Half the retained
/// total, so a full current epoch plus a full previous epoch always fit:
/// rotation therefore always restores headroom, and
/// [`IdempotencyState::insert`] can never be wedged by receipts that
/// retention rules forbid dropping.
pub const MAX_RECEIPTS_PER_EPOCH: usize = MAX_RECEIPTS / 2;

/// The browser-facing request nonce: 128 random bits.
pub const REQUEST_NONCE_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptOperation {
    Create = 1,
    Replace = 2,
    Delete = 3,
    RecoverExternallyModified = 4,
}

impl ReceiptOperation {
    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Create,
            2 => Self::Replace,
            3 => Self::Delete,
            4 => Self::RecoverExternallyModified,
            _ => return None,
        })
    }
}

/// Result statuses a receipt can replay. v1 keeps this to success — failed
/// operations are not receipted (a failed request is safe to re-execute) —
/// but the field is wire-visible, so it is an enum from day one.
pub const RECEIPT_STATUS_SUCCESS: u8 = 1;

/// Byte layout (little-endian, fixed offsets):
///
/// ```text
/// 0    epoch u64
/// 8    request_nonce [16]
/// 24   operation u8
/// 25   logical_book_id [16]
/// 41   base_book_token_or_zero [16]
/// 57   source_generation u64
/// 65   source_length_or_zero u64
/// 73   source_sha256_or_zero [32]
/// 105  display_label_length u8 (0 = empty)
/// 106  display_label [64], zero-padded
/// 170  result_book_token_or_zero [16]
/// 186  result_status u8
/// 187  receipt_crc32 u32 (CRC-32/ISO-HDLC over bytes 0..187)
/// ```
pub const RECEIPT_BYTES: usize = 191;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationReceipt {
    pub epoch: u64,
    pub request_nonce: [u8; REQUEST_NONCE_BYTES],
    pub operation: ReceiptOperation,
    pub logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
    pub base_book_token_or_zero: [u8; BOOK_TOKEN_BYTES],
    pub source_generation: u64,
    pub source_length_or_zero: u64,
    pub source_sha256_or_zero: [u8; SHA256_BYTES],
    /// Zero length means no label was bound (delete).
    pub display_label_len: u8,
    pub display_label: [u8; DISPLAY_LABEL_MAX_BYTES],
    pub result_book_token_or_zero: [u8; BOOK_TOKEN_BYTES],
    pub result_status: u8,
}

impl OperationReceipt {
    /// A receipt binding a validated label.
    pub fn label_from(label: &DisplayLabel) -> (u8, [u8; DISPLAY_LABEL_MAX_BYTES]) {
        let mut bytes = [0u8; DISPLAY_LABEL_MAX_BYTES];
        bytes[..label.as_bytes().len()].copy_from_slice(label.as_bytes());
        (label.as_bytes().len() as u8, bytes)
    }

    /// Reconstruct the request ID (`epoch || request_nonce`) bound by this receipt.
    pub fn request_id(&self) -> [u8; REQUEST_ID_BYTES] {
        let mut id = [0u8; REQUEST_ID_BYTES];
        id[..8].copy_from_slice(&self.epoch.to_le_bytes());
        id[8..].copy_from_slice(&self.request_nonce);
        id
    }

    /// Reconstruct the canonical request binding from this receipt and return its digest.
    /// Returns `None` for delete receipts (which have no source metadata request binding).
    pub fn binding_digest(&self) -> Option<[u8; SHA256_BYTES]> {
        let tag = match self.operation {
            ReceiptOperation::Create | ReceiptOperation::Replace => BINDING_TAG_UPLOAD,
            ReceiptOperation::RecoverExternallyModified => BINDING_TAG_RECOVER,
            ReceiptOperation::Delete => return None,
        };
        let parsed_label;
        let label = if self.display_label_len == 0 {
            None
        } else {
            parsed_label = DisplayLabel::from_record(self.display_label_len, &self.display_label)?;
            Some(&parsed_label)
        };
        let request_id = self.request_id();
        Some(
            RequestBinding {
                tag,
                operation: self.operation as u8,
                request_id: &request_id,
                base_book_token_or_zero: &self.base_book_token_or_zero,
                declared_length: self.source_length_or_zero,
                declared_sha256: &self.source_sha256_or_zero,
                label,
            }
            .digest(),
        )
    }

    /// Request-ID parameter consistency: everything the client bound,
    /// nothing the device produced. A retry matching on `(epoch, nonce)`
    /// but not on these is misuse and is rejected, per the PRD.
    ///
    /// `logical_book_id` is deliberately absent: the device assigns it, so
    /// a legitimate replay probe cannot know it (a delete retry carries
    /// only the token). It is stored in the receipt as result identity,
    /// not compared as a parameter.
    pub fn matches_parameters(&self, other: &Self) -> bool {
        self.operation == other.operation
            && self.base_book_token_or_zero == other.base_book_token_or_zero
            && self.source_length_or_zero == other.source_length_or_zero
            && self.source_sha256_or_zero == other.source_sha256_or_zero
            && self.display_label_len == other.display_label_len
            && self.display_label == other.display_label
    }

    fn is_valid(&self) -> bool {
        let label_ok = if self.display_label_len == 0 {
            self.display_label == [0u8; DISPLAY_LABEL_MAX_BYTES]
        } else {
            usize::from(self.display_label_len) <= DISPLAY_LABEL_MAX_BYTES
                && validate_display_label(
                    &self.display_label[..usize::from(self.display_label_len)],
                )
                && self.display_label[usize::from(self.display_label_len)..]
                    .iter()
                    .all(|byte| *byte == 0)
        };
        label_ok
            && self.epoch >= 1
            && self.logical_book_id != [0u8; LOGICAL_BOOK_ID_BYTES]
            && self.result_status == RECEIPT_STATUS_SUCCESS
    }

    fn encode(&self) -> Option<[u8; RECEIPT_BYTES]> {
        if !self.is_valid() {
            return None;
        }
        let mut out = [0u8; RECEIPT_BYTES];
        out[0..8].copy_from_slice(&self.epoch.to_le_bytes());
        out[8..24].copy_from_slice(&self.request_nonce);
        out[24] = self.operation as u8;
        out[25..41].copy_from_slice(&self.logical_book_id);
        out[41..57].copy_from_slice(&self.base_book_token_or_zero);
        out[57..65].copy_from_slice(&self.source_generation.to_le_bytes());
        out[65..73].copy_from_slice(&self.source_length_or_zero.to_le_bytes());
        out[73..105].copy_from_slice(&self.source_sha256_or_zero);
        out[105] = self.display_label_len;
        out[106..170].copy_from_slice(&self.display_label);
        out[170..186].copy_from_slice(&self.result_book_token_or_zero);
        out[186] = self.result_status;
        let crc = crc32(&out[..RECEIPT_BYTES - 4]);
        out[187..191].copy_from_slice(&crc.to_le_bytes());
        Some(out)
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != RECEIPT_BYTES {
            return None;
        }
        let stored = u32::from_le_bytes(bytes[187..191].try_into().ok()?);
        if crc32(&bytes[..RECEIPT_BYTES - 4]) != stored {
            return None;
        }
        let receipt = Self {
            epoch: u64::from_le_bytes(bytes[0..8].try_into().ok()?),
            request_nonce: bytes[8..24].try_into().ok()?,
            operation: ReceiptOperation::from_u8(bytes[24])?,
            logical_book_id: bytes[25..41].try_into().ok()?,
            base_book_token_or_zero: bytes[41..57].try_into().ok()?,
            source_generation: u64::from_le_bytes(bytes[57..65].try_into().ok()?),
            source_length_or_zero: u64::from_le_bytes(bytes[65..73].try_into().ok()?),
            source_sha256_or_zero: bytes[73..105].try_into().ok()?,
            display_label_len: bytes[105],
            display_label: bytes[106..170].try_into().ok()?,
            result_book_token_or_zero: bytes[170..186].try_into().ok()?,
            result_status: bytes[186],
        };
        if !receipt.is_valid() {
            return None;
        }
        Some(receipt)
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(bytes)
}

// ---------------------------------------------------------------------------
// The state record
// ---------------------------------------------------------------------------

const STATE_FIXED_FIELD_BYTES: usize = 8 // current_epoch
    + 8                          // previous_epoch_or_zero
    + 2                          // receipt_count
    + 2; // receipt_record_size
pub const IDEMPOTENCY_MAX_LOGICAL_BYTES: usize =
    BODY_PREFIX_BYTES + STATE_FIXED_FIELD_BYTES + MAX_RECEIPTS * RECEIPT_BYTES + BODY_CRC_BYTES;

/// What a lookup found. `ParameterMismatch` is terminal for the request —
/// a reused request ID with different bound parameters is client misuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptLookup {
    Unknown,
    Replay(OperationReceipt),
    ParameterMismatch,
}

/// The in-memory state. At ~6 KB this must live in a static or the Wi-Fi
/// session's loaned memory, never a stack frame — same rule as
/// `ReaderStore`.
pub struct IdempotencyState {
    pub current_epoch: u64,
    pub previous_epoch_or_zero: u64,
    receipts: [OperationReceipt; MAX_RECEIPTS],
    count: usize,
}

impl IdempotencyState {
    /// The state a device starts with before any epoch has been published:
    /// epoch 1, no history.
    pub fn initial() -> Self {
        Self {
            current_epoch: 1,
            previous_epoch_or_zero: 0,
            receipts: [EMPTY_RECEIPT; MAX_RECEIPTS],
            count: 0,
        }
    }

    pub fn receipts(&self) -> &[OperationReceipt] {
        &self.receipts[..self.count]
    }

    pub fn is_full(&self) -> bool {
        self.count == MAX_RECEIPTS
    }

    /// An epoch a genuinely new request may use: only the current one.
    pub fn epoch_is_current(&self, epoch: u64) -> bool {
        epoch == self.current_epoch
    }

    /// An epoch whose *known* request IDs must still resolve: current or
    /// previous. Known IDs are resolved before epoch freshness, so a
    /// delayed retry from the previous epoch replays instead of failing.
    pub fn epoch_is_accepted(&self, epoch: u64) -> bool {
        epoch == self.current_epoch
            || (self.previous_epoch_or_zero != 0 && epoch == self.previous_epoch_or_zero)
    }

    /// Resolve a request ID, checking parameter consistency against the
    /// probe receipt's bound fields. Runs *before* token validation in
    /// every operation, per the PRD's mandatory lookup order.
    pub fn lookup(&self, probe: &OperationReceipt) -> ReceiptLookup {
        match self.position(probe.epoch, &probe.request_nonce) {
            Err(_) => ReceiptLookup::Unknown,
            Ok(at) => {
                let stored = &self.receipts[at];
                if stored.matches_parameters(probe) {
                    ReceiptLookup::Replay(*stored)
                } else {
                    ReceiptLookup::ParameterMismatch
                }
            }
        }
    }

    /// Whether this request ID is recorded at all, regardless of whether
    /// its parameters would agree.
    ///
    /// Retention rules ask this rather than [`lookup`][Self::lookup]: a
    /// stored receipt answers a retry *either* way — replay or
    /// `ParameterMismatch` — and both are definitive answers that make
    /// re-execution impossible. What retention protects against is a
    /// request ID resolving to *nothing*, which is what turns a retry into
    /// a second execution.
    pub fn contains_request(&self, epoch: u64, nonce: &[u8; REQUEST_NONCE_BYTES]) -> bool {
        self.position(epoch, nonce).is_ok()
    }

    /// Retrieve the stored receipt for a request ID, if present.
    pub fn get_receipt(
        &self,
        epoch: u64,
        nonce: &[u8; REQUEST_NONCE_BYTES],
    ) -> Option<&OperationReceipt> {
        match self.position(epoch, nonce) {
            Ok(at) => Some(&self.receipts[at]),
            Err(_) => None,
        }
    }

    /// Receipts already issued against the current epoch. Operations check
    /// this *before* committing anything: a rejection for an exhausted
    /// epoch must arrive before the operation runs, never after.
    pub fn current_epoch_receipts(&self) -> usize {
        self.receipts()
            .iter()
            .filter(|receipt| receipt.epoch == self.current_epoch)
            .count()
    }

    /// Whether one more operation may be accepted in the current epoch.
    pub fn has_epoch_headroom(&self) -> bool {
        self.current_epoch_receipts() < MAX_RECEIPTS_PER_EPOCH
    }

    /// Record a committed operation's receipt. The caller has already
    /// resolved the lookup as `Unknown`, verified epoch freshness and
    /// [`has_epoch_headroom`][Self::has_epoch_headroom], and *committed the
    /// operation itself*. A full epoch here means the caller skipped the
    /// headroom check. Duplicate `(epoch, nonce)` is a caller bug and is
    /// refused.
    pub fn insert(&mut self, receipt: OperationReceipt) -> Result<(), ReceiptInsertError> {
        if !receipt.is_valid() || !self.epoch_is_current(receipt.epoch) {
            return Err(ReceiptInsertError::Invalid);
        }
        let at = match self.position(receipt.epoch, &receipt.request_nonce) {
            Ok(_) => return Err(ReceiptInsertError::Duplicate),
            Err(at) => at,
        };
        if self.is_full() || self.current_epoch_receipts() >= MAX_RECEIPTS_PER_EPOCH {
            return Err(ReceiptInsertError::Full);
        }
        self.receipts.copy_within(at..self.count, at + 1);
        self.receipts[at] = receipt;
        self.count += 1;
        Ok(())
    }

    /// Rotate to `new_epoch`: the current epoch becomes previous, receipts
    /// older than the new previous epoch are retired, and `new_epoch`
    /// becomes the only epoch new requests may use. Epochs are monotonic
    /// and never reused — the device derives them from a persistent
    /// counter, and this refuses anything not strictly newer.
    pub fn rotate_epoch(&mut self, new_epoch: u64) -> Result<(), EpochRotationError> {
        if new_epoch <= self.current_epoch {
            return Err(EpochRotationError::NotNewer);
        }
        let retiring_before = self.current_epoch;
        self.previous_epoch_or_zero = self.current_epoch;
        self.current_epoch = new_epoch;
        // Retain in place: receipts are sorted by (epoch, nonce), so the
        // survivors are a suffix.
        let first_kept = self.receipts[..self.count]
            .iter()
            .position(|receipt| receipt.epoch >= retiring_before)
            .unwrap_or(self.count);
        self.receipts.copy_within(first_kept..self.count, 0);
        self.count -= first_kept;
        Ok(())
    }

    fn position(&self, epoch: u64, nonce: &[u8; REQUEST_NONCE_BYTES]) -> Result<usize, usize> {
        self.receipts[..self.count].binary_search_by(|receipt| {
            (receipt.epoch, receipt.request_nonce).cmp(&(epoch, *nonce))
        })
    }

    /// Write the type-specific fields; returns the logical length for
    /// [`seal_body`][crate::record::seal_body]. Variable-length: the file
    /// only carries `count` receipts.
    pub fn encode_into(&self, buf: &mut [u8]) -> Option<usize> {
        if self.current_epoch == 0
            || self.previous_epoch_or_zero >= self.current_epoch
            || self.count > MAX_RECEIPTS
        {
            return None;
        }
        let logical = BODY_PREFIX_BYTES
            + STATE_FIXED_FIELD_BYTES
            + self.count * RECEIPT_BYTES
            + BODY_CRC_BYTES;
        if buf.len() < logical {
            return None;
        }
        let mut at = BODY_PREFIX_BYTES;
        buf[at..at + 8].copy_from_slice(&self.current_epoch.to_le_bytes());
        buf[at + 8..at + 16].copy_from_slice(&self.previous_epoch_or_zero.to_le_bytes());
        let count = u16::try_from(self.count).ok()?;
        buf[at + 16..at + 18].copy_from_slice(&count.to_le_bytes());
        let size = u16::try_from(RECEIPT_BYTES).ok()?;
        buf[at + 18..at + 20].copy_from_slice(&size.to_le_bytes());
        at += STATE_FIXED_FIELD_BYTES;
        let mut previous: Option<(u64, [u8; REQUEST_NONCE_BYTES])> = None;
        for receipt in self.receipts() {
            // Sortedness and epoch scoping are invariants of this struct;
            // encoding revalidates them so a corrupted in-memory state
            // cannot become a valid-looking record.
            let key = (receipt.epoch, receipt.request_nonce);
            if previous.is_some_and(|prior| prior >= key) || !self.epoch_is_accepted(receipt.epoch)
            {
                return None;
            }
            previous = Some(key);
            buf[at..at + RECEIPT_BYTES].copy_from_slice(&receipt.encode()?);
            at += RECEIPT_BYTES;
        }
        Some(at + BODY_CRC_BYTES)
    }

    pub fn decode(view: &RecordView<'_>) -> Option<Self> {
        if view.schema_version != IDEMPOTENCY_SCHEMA {
            return None;
        }
        let body = view.logical_body;
        let fixed_end = BODY_PREFIX_BYTES + STATE_FIXED_FIELD_BYTES;
        if body.len() < fixed_end + BODY_CRC_BYTES {
            return None;
        }
        let current_epoch = u64::from_le_bytes(
            body[BODY_PREFIX_BYTES..BODY_PREFIX_BYTES + 8]
                .try_into()
                .ok()?,
        );
        let previous_epoch_or_zero = u64::from_le_bytes(
            body[BODY_PREFIX_BYTES + 8..BODY_PREFIX_BYTES + 16]
                .try_into()
                .ok()?,
        );
        let count = usize::from(u16::from_le_bytes(
            body[BODY_PREFIX_BYTES + 16..BODY_PREFIX_BYTES + 18]
                .try_into()
                .ok()?,
        ));
        let size = usize::from(u16::from_le_bytes(
            body[BODY_PREFIX_BYTES + 18..BODY_PREFIX_BYTES + 20]
                .try_into()
                .ok()?,
        ));
        if size != RECEIPT_BYTES
            || count > MAX_RECEIPTS
            || current_epoch == 0
            || previous_epoch_or_zero >= current_epoch
            || body.len() != fixed_end + count * RECEIPT_BYTES + BODY_CRC_BYTES
        {
            return None;
        }
        let mut state = Self {
            current_epoch,
            previous_epoch_or_zero,
            receipts: [EMPTY_RECEIPT; MAX_RECEIPTS],
            count,
        };
        let mut previous: Option<(u64, [u8; REQUEST_NONCE_BYTES])> = None;
        for slot in 0..count {
            let at = fixed_end + slot * RECEIPT_BYTES;
            let receipt = OperationReceipt::decode(&body[at..at + RECEIPT_BYTES])?;
            let key = (receipt.epoch, receipt.request_nonce);
            if previous.is_some_and(|prior| prior >= key) || !state.epoch_is_accepted(receipt.epoch)
            {
                return None;
            }
            previous = Some(key);
            state.receipts[slot] = receipt;
        }
        Some(state)
    }
}

/// Filler for unused receipt slots; never observable through
/// [`IdempotencyState::receipts`].
const EMPTY_RECEIPT: OperationReceipt = OperationReceipt {
    epoch: 0,
    request_nonce: [0; REQUEST_NONCE_BYTES],
    operation: ReceiptOperation::Create,
    logical_book_id: [0; LOGICAL_BOOK_ID_BYTES],
    base_book_token_or_zero: [0; BOOK_TOKEN_BYTES],
    source_generation: 0,
    source_length_or_zero: 0,
    source_sha256_or_zero: [0; SHA256_BYTES],
    display_label_len: 0,
    display_label: [0; DISPLAY_LABEL_MAX_BYTES],
    result_book_token_or_zero: [0; BOOK_TOKEN_BYTES],
    result_status: RECEIPT_STATUS_SUCCESS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochRotationError {
    /// Epochs are monotonic; the proposed epoch does not advance.
    NotNewer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptInsertError {
    /// Invalid receipt, or its epoch is not the current one.
    Invalid,
    /// `(epoch, nonce)` already present — the caller skipped lookup.
    Duplicate,
    /// Rotation required before this receipt can be retained.
    Full,
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::record::{classify_record, record_file_len, seal_body, RecordState};
    use std::vec;

    fn receipt(epoch: u64, nonce_seed: u8) -> OperationReceipt {
        OperationReceipt {
            epoch,
            request_nonce: [nonce_seed; REQUEST_NONCE_BYTES],
            operation: ReceiptOperation::Delete,
            logical_book_id: [1; LOGICAL_BOOK_ID_BYTES],
            base_book_token_or_zero: [2; BOOK_TOKEN_BYTES],
            source_generation: 1,
            source_length_or_zero: 0,
            source_sha256_or_zero: [0; SHA256_BYTES],
            display_label_len: 0,
            display_label: [0; DISPLAY_LABEL_MAX_BYTES],
            result_book_token_or_zero: [0; BOOK_TOKEN_BYTES],
            result_status: RECEIPT_STATUS_SUCCESS,
        }
    }

    #[test]
    fn lookup_resolves_before_epoch_freshness() {
        let mut state = IdempotencyState::initial();
        state.insert(receipt(1, 5)).unwrap();
        state.rotate_epoch(2).unwrap();
        // Epoch 1 is now stale for new requests but its receipt replays.
        assert!(!state.epoch_is_current(1));
        assert_eq!(
            state.lookup(&receipt(1, 5)),
            ReceiptLookup::Replay(receipt(1, 5))
        );
        assert_eq!(state.lookup(&receipt(2, 5)), ReceiptLookup::Unknown);
    }

    #[test]
    fn parameter_mismatch_is_terminal() {
        let mut state = IdempotencyState::initial();
        state.insert(receipt(1, 5)).unwrap();
        let mut reused = receipt(1, 5);
        reused.base_book_token_or_zero = [9; BOOK_TOKEN_BYTES];
        assert_eq!(state.lookup(&reused), ReceiptLookup::ParameterMismatch);
    }

    #[test]
    fn insert_rejects_duplicates_stale_epochs_and_overflow() {
        let mut state = IdempotencyState::initial();
        state.insert(receipt(1, 5)).unwrap();
        assert_eq!(
            state.insert(receipt(1, 5)),
            Err(ReceiptInsertError::Duplicate)
        );
        state.rotate_epoch(2).unwrap();
        assert_eq!(
            state.insert(receipt(1, 6)),
            Err(ReceiptInsertError::Invalid)
        );
        // Fill the current epoch to its cap; the next insert is refused
        // even though total capacity remains.
        for n in 0..MAX_RECEIPTS_PER_EPOCH {
            state.insert(receipt(2, n as u8)).unwrap();
        }
        assert!(!state.has_epoch_headroom());
        assert!(!state.is_full());
        assert_eq!(
            state.insert(receipt(2, MAX_RECEIPTS_PER_EPOCH as u8)),
            Err(ReceiptInsertError::Full)
        );
        // Rotation restores headroom: the full current epoch becomes the
        // retained previous epoch, and both together fit in MAX_RECEIPTS.
        state.rotate_epoch(3).unwrap();
        assert!(state.has_epoch_headroom());
        for n in 0..MAX_RECEIPTS_PER_EPOCH {
            state.insert(receipt(3, n as u8)).unwrap();
        }
        assert!(state.is_full());
        assert_eq!(
            state.lookup(&receipt(2, 0)),
            ReceiptLookup::Replay(receipt(2, 0))
        );
    }

    #[test]
    fn rotation_retires_only_pre_previous_epochs() {
        let mut state = IdempotencyState::initial();
        state.insert(receipt(1, 1)).unwrap();
        state.rotate_epoch(2).unwrap();
        state.insert(receipt(2, 2)).unwrap();
        state.rotate_epoch(3).unwrap();
        // Epoch 1's receipt is gone; epoch 2's (now previous) survives.
        assert_eq!(state.lookup(&receipt(1, 1)), ReceiptLookup::Unknown);
        assert_eq!(
            state.lookup(&receipt(2, 2)),
            ReceiptLookup::Replay(receipt(2, 2))
        );
        // Epochs never rewind or repeat.
        assert_eq!(state.rotate_epoch(3), Err(EpochRotationError::NotNewer));
        assert_eq!(state.rotate_epoch(2), Err(EpochRotationError::NotNewer));
    }

    #[test]
    fn state_roundtrips_through_record() {
        let mut state = IdempotencyState::initial();
        state.insert(receipt(1, 3)).unwrap();
        state.rotate_epoch(2).unwrap();
        state.insert(receipt(2, 1)).unwrap();

        let mut buf = vec![0u8; record_file_len(IDEMPOTENCY_MAX_LOGICAL_BYTES).unwrap()];
        let logical = state.encode_into(&mut buf).unwrap();
        seal_body(IDEMPOTENCY_MAGIC, IDEMPOTENCY_SCHEMA, 7, logical, &mut buf).unwrap();
        let RecordState::Prepared(view) =
            classify_record(&buf[..record_file_len(logical).unwrap()], IDEMPOTENCY_MAGIC)
        else {
            panic!("expected prepared");
        };
        let decoded = IdempotencyState::decode(&view).expect("decode");
        assert_eq!(decoded.current_epoch, 2);
        assert_eq!(decoded.previous_epoch_or_zero, 1);
        assert_eq!(decoded.receipts(), state.receipts());
    }

    #[test]
    fn corrupt_receipt_fails_whole_decode() {
        let mut state = IdempotencyState::initial();
        state.insert(receipt(1, 3)).unwrap();
        let mut buf = vec![0u8; record_file_len(IDEMPOTENCY_MAX_LOGICAL_BYTES).unwrap()];
        let logical = state.encode_into(&mut buf).unwrap();
        // Corrupt one receipt byte, then re-seal so only the per-receipt
        // CRC can catch it. Fails closed: the whole state is rejected.
        buf[BODY_PREFIX_BYTES + STATE_FIXED_FIELD_BYTES + 30] ^= 1;
        seal_body(IDEMPOTENCY_MAGIC, IDEMPOTENCY_SCHEMA, 1, logical, &mut buf).unwrap();
        let RecordState::Prepared(view) =
            classify_record(&buf[..record_file_len(logical).unwrap()], IDEMPOTENCY_MAGIC)
        else {
            panic!("expected prepared");
        };
        assert!(IdempotencyState::decode(&view).is_none());
    }
}
