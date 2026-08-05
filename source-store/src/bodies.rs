//! Typed logical bodies for the M0S authoritative records: source metadata,
//! deletion tombstones, and managed-upload staging markers.
//!
//! Each type owns its field layout and semantic validation; the framing
//! (prefix, padding, CRC, commit sector) is [`crate::record`]'s. An encoder
//! fills only the type-specific fields and hands the buffer to
//! [`seal_body`][crate::record::seal_body]; a decoder takes a
//! [`RecordView`][crate::record::RecordView] the classifier already proved
//! structurally sound and applies the *semantic* rules — enum validity,
//! provenance consistency, label shape, canonical zero tails. Decoders
//! reject rather than repair: a record that fails semantic validation is
//! exactly as untrustworthy as one that fails its checksum.
//!
//! Layouts are fixed little-endian at fixed offsets. Encoding is canonical —
//! one byte sequence per logical value, unused label tail zeroed — because
//! the body CRC covers every byte and the publish path compares rereads
//! byte-for-byte.

use crate::record::{RecordView, BODY_CRC_BYTES, BODY_PREFIX_BYTES};

pub const LOGICAL_BOOK_ID_BYTES: usize = 16;
pub const BOOK_TOKEN_BYTES: usize = 16;
pub const SHA256_BYTES: usize = 32;
/// Epoch-scoped operation request ID: u64 idempotency epoch plus the
/// browser's 128-bit nonce — the PRD's 48-hex-character wire format, stored
/// binary.
pub const REQUEST_ID_BYTES: usize = 24;
pub const DISPLAY_LABEL_MAX_BYTES: usize = 64;

/// Display-label contract: valid UTF-8, 1–64 bytes, no NUL, no C0 controls.
/// DEL (0x7F) is rejected with them — it is exactly as unprintable, and the
/// PRD's "disallowed C0" floor is a floor, not a ceiling.
pub fn validate_display_label(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > DISPLAY_LABEL_MAX_BYTES {
        return false;
    }
    if core::str::from_utf8(bytes).is_err() {
        return false;
    }
    !bytes.iter().any(|byte| *byte < 0x20 || *byte == 0x7F)
}

/// A validated display label: length plus a zero-padded fixed buffer, the
/// exact on-record representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayLabel {
    len: u8,
    bytes: [u8; DISPLAY_LABEL_MAX_BYTES],
}

impl DisplayLabel {
    pub fn new(label: &[u8]) -> Option<Self> {
        if !validate_display_label(label) {
            return None;
        }
        let mut bytes = [0u8; DISPLAY_LABEL_MAX_BYTES];
        bytes[..label.len()].copy_from_slice(label);
        Some(Self {
            len: label.len() as u8,
            bytes,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// A minimal valid label for filler entries in fixed-capacity arrays
    /// that are sized but never read. Avoids an `unwrap` on the validating
    /// constructor in panic-free code paths.
    pub fn placeholder() -> Self {
        let mut bytes = [0u8; DISPLAY_LABEL_MAX_BYTES];
        bytes[0] = b'-';
        Self { len: 1, bytes }
    }

    /// Decode the on-record form: the tail beyond `len` must be canonical
    /// zeros, so equal labels are equal bytes and the CRC pins the whole
    /// field.
    fn from_record(len: u8, bytes: &[u8; DISPLAY_LABEL_MAX_BYTES]) -> Option<Self> {
        let label = bytes.get(..usize::from(len))?;
        if !validate_display_label(label) {
            return None;
        }
        if bytes[usize::from(len)..].iter().any(|byte| *byte != 0) {
            return None;
        }
        Some(Self { len, bytes: *bytes })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceOrigin {
    ManagedUpload = 1,
    UnmanagedSd = 2,
}

/// Where an unmanaged book's bytes live: its 8.3 file name in the books
/// directory. Managed sources carry [`UnmanagedName::none`] — their bytes
/// live in the slot the metadata already names.
pub const UNMANAGED_NAME_MAX_BYTES: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnmanagedName {
    len: u8,
    bytes: [u8; UNMANAGED_NAME_MAX_BYTES],
}

impl UnmanagedName {
    pub const fn none() -> Self {
        Self {
            len: 0,
            bytes: [0; UNMANAGED_NAME_MAX_BYTES],
        }
    }

    /// A validated 8.3 name: ASCII graphic characters, one dot, stem of
    /// 1–8, extension of 1–3 — the shape the catalog scan discovers and
    /// `embedded-sdmmc` reopens.
    pub fn new(name: &str) -> Option<Self> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > UNMANAGED_NAME_MAX_BYTES {
            return None;
        }
        if !bytes.iter().all(|byte| byte.is_ascii_graphic()) {
            return None;
        }
        let (stem, ext) = name.split_once('.')?;
        if stem.is_empty() || stem.len() > 8 || ext.is_empty() || ext.len() > 3 || ext.contains('.')
        {
            return None;
        }
        let mut stored = [0u8; UNMANAGED_NAME_MAX_BYTES];
        stored[..bytes.len()].copy_from_slice(bytes);
        Some(Self {
            len: bytes.len() as u8,
            bytes: stored,
        })
    }

    pub fn is_none(&self) -> bool {
        self.len == 0
    }

    pub fn as_str(&self) -> Option<&str> {
        if self.is_none() {
            return None;
        }
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).ok()
    }

    fn from_record(len: u8, bytes: &[u8; UNMANAGED_NAME_MAX_BYTES]) -> Option<Self> {
        if len == 0 {
            if bytes.iter().any(|byte| *byte != 0) {
                return None;
            }
            return Some(Self::none());
        }
        let name = core::str::from_utf8(bytes.get(..usize::from(len))?).ok()?;
        if bytes[usize::from(len)..].iter().any(|byte| *byte != 0) {
            return None;
        }
        Self::new(name)
    }
}

/// How the current generation came to be — independent of where the bytes
/// live. `ManagedUpload` origin does not mean the card never changed; the
/// provenance kind records which *operation* produced the generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    ManagedUploadRequest = 1,
    LocalUnmanagedOperation = 2,
    ExternalRecoveryRequest = 3,
}

/// The provenance combinations a generation can legitimately carry.
/// Unmanaged books are identified locally; managed generations come from
/// uploads or explicit recovery. Everything else is a forged or corrupted
/// record.
fn provenance_is_valid(
    origin: SourceOrigin,
    kind: OperationKind,
    externally_recovered: bool,
) -> bool {
    let combination_ok = match origin {
        SourceOrigin::ManagedUpload => matches!(
            kind,
            OperationKind::ManagedUploadRequest | OperationKind::ExternalRecoveryRequest
        ),
        SourceOrigin::UnmanagedSd => matches!(kind, OperationKind::LocalUnmanagedOperation),
    };
    // The flag is the kind, persisted: set exactly when this generation was
    // adopted through explicit recovery.
    combination_ok
        && (externally_recovered == matches!(kind, OperationKind::ExternalRecoveryRequest))
}

// ---------------------------------------------------------------------------
// Source metadata
// ---------------------------------------------------------------------------

pub const SOURCE_METADATA_MAGIC: [u8; 4] = *b"XTSM";
pub const SOURCE_METADATA_SCHEMA: u16 = 1;

/// Type-specific field bytes; the logical body adds the framing prefix and
/// CRC around them.
const SOURCE_METADATA_FIELD_BYTES: usize = LOGICAL_BOOK_ID_BYTES // logical_book_id
    + 8                          // source_generation
    + 1                          // source_origin
    + 1                          // source_operation_kind
    + REQUEST_ID_BYTES           // operation request id or local id
    + 1                          // externally_recovered
    + 1                          // physical_slot
    + 8                          // source_length
    + SHA256_BYTES               // source_sha256
    + 2                          // quick_fingerprint_policy_version
    + SHA256_BYTES               // quick_fingerprint_sha256
    + BOOK_TOKEN_BYTES           // book_token
    + 1                          // display_label_length
    + DISPLAY_LABEL_MAX_BYTES    // display_label
    + 1                          // unmanaged_name_length
    + UNMANAGED_NAME_MAX_BYTES; // unmanaged_name
pub const SOURCE_METADATA_LOGICAL_BYTES: usize =
    BODY_PREFIX_BYTES + SOURCE_METADATA_FIELD_BYTES + BODY_CRC_BYTES;

/// One committed source generation of one logical book, on one physical
/// slot. The record generation in the framing prefix is the *metadata*
/// generation — monotonic per slot, distinct from `source_generation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceMetadata {
    pub logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
    pub source_generation: u64,
    pub source_origin: SourceOrigin,
    pub operation_kind: OperationKind,
    pub operation_request_id: [u8; REQUEST_ID_BYTES],
    pub externally_recovered: bool,
    pub physical_slot: u8,
    pub source_length: u64,
    pub source_sha256: [u8; SHA256_BYTES],
    pub quick_fingerprint_policy_version: u16,
    pub quick_fingerprint_sha256: [u8; SHA256_BYTES],
    pub book_token: [u8; BOOK_TOKEN_BYTES],
    pub display_label: DisplayLabel,
    /// Where the bytes live for `UnmanagedSd` sources; none for managed.
    pub unmanaged_name: UnmanagedName,
}

impl SourceMetadata {
    /// Semantic validity, shared by encode and decode so a record this
    /// firmware writes is always one it would accept back.
    fn is_valid(&self) -> bool {
        let name_matches_origin = match self.source_origin {
            SourceOrigin::ManagedUpload => self.unmanaged_name.is_none(),
            SourceOrigin::UnmanagedSd => !self.unmanaged_name.is_none(),
        };
        name_matches_origin
            && self.logical_book_id != [0u8; LOGICAL_BOOK_ID_BYTES]
            && self.book_token != [0u8; BOOK_TOKEN_BYTES]
            && self.source_generation >= 1
            && self.source_length >= 1
            && provenance_is_valid(
                self.source_origin,
                self.operation_kind,
                self.externally_recovered,
            )
    }

    /// Write the type-specific fields into `buf` (after the framing
    /// prefix); returns the logical length for
    /// [`seal_body`][crate::record::seal_body].
    pub fn encode_into(&self, buf: &mut [u8]) -> Option<usize> {
        if !self.is_valid() || buf.len() < SOURCE_METADATA_LOGICAL_BYTES {
            return None;
        }
        let mut c = Cursor::new(buf, BODY_PREFIX_BYTES);
        c.put(&self.logical_book_id)?;
        c.put_u64(self.source_generation)?;
        c.put_u8(self.source_origin as u8)?;
        c.put_u8(self.operation_kind as u8)?;
        c.put(&self.operation_request_id)?;
        c.put_u8(u8::from(self.externally_recovered))?;
        c.put_u8(self.physical_slot)?;
        c.put_u64(self.source_length)?;
        c.put(&self.source_sha256)?;
        c.put_u16(self.quick_fingerprint_policy_version)?;
        c.put(&self.quick_fingerprint_sha256)?;
        c.put(&self.book_token)?;
        c.put_u8(self.display_label.len)?;
        c.put(&self.display_label.bytes)?;
        c.put_u8(self.unmanaged_name.len)?;
        c.put(&self.unmanaged_name.bytes)?;
        c.finish_at(SOURCE_METADATA_LOGICAL_BYTES - BODY_CRC_BYTES)
    }

    pub fn decode(view: &RecordView<'_>) -> Option<Self> {
        if view.schema_version != SOURCE_METADATA_SCHEMA
            || view.logical_body.len() != SOURCE_METADATA_LOGICAL_BYTES
        {
            return None;
        }
        let mut c = Reader::new(view.logical_body, BODY_PREFIX_BYTES);
        let decoded = Self {
            logical_book_id: c.take()?,
            source_generation: c.take_u64()?,
            source_origin: match c.take_u8()? {
                1 => SourceOrigin::ManagedUpload,
                2 => SourceOrigin::UnmanagedSd,
                _ => return None,
            },
            operation_kind: match c.take_u8()? {
                1 => OperationKind::ManagedUploadRequest,
                2 => OperationKind::LocalUnmanagedOperation,
                3 => OperationKind::ExternalRecoveryRequest,
                _ => return None,
            },
            operation_request_id: c.take()?,
            externally_recovered: match c.take_u8()? {
                0 => false,
                1 => true,
                _ => return None,
            },
            physical_slot: c.take_u8()?,
            source_length: c.take_u64()?,
            source_sha256: c.take()?,
            quick_fingerprint_policy_version: c.take_u16()?,
            quick_fingerprint_sha256: c.take()?,
            book_token: c.take()?,
            display_label: {
                let len = c.take_u8()?;
                let bytes: [u8; DISPLAY_LABEL_MAX_BYTES] = c.take()?;
                DisplayLabel::from_record(len, &bytes)?
            },
            unmanaged_name: {
                let len = c.take_u8()?;
                let bytes: [u8; UNMANAGED_NAME_MAX_BYTES] = c.take()?;
                UnmanagedName::from_record(len, &bytes)?
            },
        };
        if !decoded.is_valid() {
            return None;
        }
        Some(decoded)
    }
}

// ---------------------------------------------------------------------------
// Deletion tombstone
// ---------------------------------------------------------------------------

pub const TOMBSTONE_MAGIC: [u8; 4] = *b"XTTB";
pub const TOMBSTONE_SCHEMA: u16 = 1;

const TOMBSTONE_FIELD_BYTES: usize = LOGICAL_BOOK_ID_BYTES // logical_book_id
    + 8                          // deleted_source_generation
    + BOOK_TOKEN_BYTES           // deleted_book_token
    + REQUEST_ID_BYTES           // delete_request_id
    + 1; // delete_result_status
pub const TOMBSTONE_LOGICAL_BYTES: usize =
    BODY_PREFIX_BYTES + TOMBSTONE_FIELD_BYTES + BODY_CRC_BYTES;

/// The only status a v1 tombstone records: the deletion succeeded. The
/// field exists so a future schema can distinguish outcomes without moving
/// bytes.
pub const TOMBSTONE_STATUS_DELETED: u8 = 1;

/// A committed tombstone hides every source generation of the book at or
/// below `deleted_source_generation`, plus all state bound to them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tombstone {
    pub logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
    pub deleted_source_generation: u64,
    pub deleted_book_token: [u8; BOOK_TOKEN_BYTES],
    pub delete_request_id: [u8; REQUEST_ID_BYTES],
    pub delete_result_status: u8,
}

impl Tombstone {
    fn is_valid(&self) -> bool {
        self.logical_book_id != [0u8; LOGICAL_BOOK_ID_BYTES]
            && self.deleted_source_generation >= 1
            && self.deleted_book_token != [0u8; BOOK_TOKEN_BYTES]
            && self.delete_result_status == TOMBSTONE_STATUS_DELETED
    }

    pub fn encode_into(&self, buf: &mut [u8]) -> Option<usize> {
        if !self.is_valid() || buf.len() < TOMBSTONE_LOGICAL_BYTES {
            return None;
        }
        let mut c = Cursor::new(buf, BODY_PREFIX_BYTES);
        c.put(&self.logical_book_id)?;
        c.put_u64(self.deleted_source_generation)?;
        c.put(&self.deleted_book_token)?;
        c.put(&self.delete_request_id)?;
        c.put_u8(self.delete_result_status)?;
        c.finish_at(TOMBSTONE_LOGICAL_BYTES - BODY_CRC_BYTES)
    }

    pub fn decode(view: &RecordView<'_>) -> Option<Self> {
        if view.schema_version != TOMBSTONE_SCHEMA
            || view.logical_body.len() != TOMBSTONE_LOGICAL_BYTES
        {
            return None;
        }
        let mut c = Reader::new(view.logical_body, BODY_PREFIX_BYTES);
        let decoded = Self {
            logical_book_id: c.take()?,
            deleted_source_generation: c.take_u64()?,
            deleted_book_token: c.take()?,
            delete_request_id: c.take()?,
            delete_result_status: c.take_u8()?,
        };
        if !decoded.is_valid() {
            return None;
        }
        Some(decoded)
    }
}

// ---------------------------------------------------------------------------
// Managed-upload staging marker
// ---------------------------------------------------------------------------

pub const STAGING_MARKER_MAGIC: [u8; 4] = *b"XTMK";
pub const STAGING_MARKER_SCHEMA: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagedOperation {
    Create = 1,
    Replace = 2,
}

const STAGING_MARKER_FIELD_BYTES: usize = 1 // operation
    + REQUEST_ID_BYTES           // operation_request_id
    + LOGICAL_BOOK_ID_BYTES      // logical_book_id
    + BOOK_TOKEN_BYTES           // base_book_token_or_zero
    + 8                          // candidate_source_generation
    + 1                          // candidate_physical_slot
    + 8                          // expected_source_length
    + SHA256_BYTES               // expected_source_sha256
    + 1                          // display_label_length
    + DISPLAY_LABEL_MAX_BYTES; // display_label
pub const STAGING_MARKER_LOGICAL_BYTES: usize =
    BODY_PREFIX_BYTES + STAGING_MARKER_FIELD_BYTES + BODY_CRC_BYTES;

/// Durably committed *before* the candidate EPUB is created or truncated,
/// so a reserved managed slot with no committed source metadata is always
/// explained: either its marker names it (transaction in flight or
/// abandoned — resumable or cleanable), or it is an orphan to quarantine.
/// Never adoptable as an unmanaged book either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StagingMarker {
    pub operation: StagedOperation,
    pub operation_request_id: [u8; REQUEST_ID_BYTES],
    pub logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
    pub base_book_token_or_zero: [u8; BOOK_TOKEN_BYTES],
    pub candidate_source_generation: u64,
    pub candidate_physical_slot: u8,
    pub expected_source_length: u64,
    pub expected_source_sha256: [u8; SHA256_BYTES],
    pub display_label: DisplayLabel,
}

impl StagingMarker {
    fn is_valid(&self) -> bool {
        let base_token_ok = match self.operation {
            // A create has no base authority to name; a replace must name it.
            StagedOperation::Create => self.base_book_token_or_zero == [0u8; BOOK_TOKEN_BYTES],
            StagedOperation::Replace => self.base_book_token_or_zero != [0u8; BOOK_TOKEN_BYTES],
        };
        base_token_ok
            && self.logical_book_id != [0u8; LOGICAL_BOOK_ID_BYTES]
            && self.candidate_source_generation >= 1
            && self.expected_source_length >= 1
    }

    pub fn encode_into(&self, buf: &mut [u8]) -> Option<usize> {
        if !self.is_valid() || buf.len() < STAGING_MARKER_LOGICAL_BYTES {
            return None;
        }
        let mut c = Cursor::new(buf, BODY_PREFIX_BYTES);
        c.put_u8(self.operation as u8)?;
        c.put(&self.operation_request_id)?;
        c.put(&self.logical_book_id)?;
        c.put(&self.base_book_token_or_zero)?;
        c.put_u64(self.candidate_source_generation)?;
        c.put_u8(self.candidate_physical_slot)?;
        c.put_u64(self.expected_source_length)?;
        c.put(&self.expected_source_sha256)?;
        c.put_u8(self.display_label.len)?;
        c.put(&self.display_label.bytes)?;
        c.finish_at(STAGING_MARKER_LOGICAL_BYTES - BODY_CRC_BYTES)
    }

    pub fn decode(view: &RecordView<'_>) -> Option<Self> {
        if view.schema_version != STAGING_MARKER_SCHEMA
            || view.logical_body.len() != STAGING_MARKER_LOGICAL_BYTES
        {
            return None;
        }
        let mut c = Reader::new(view.logical_body, BODY_PREFIX_BYTES);
        let decoded = Self {
            operation: match c.take_u8()? {
                1 => StagedOperation::Create,
                2 => StagedOperation::Replace,
                _ => return None,
            },
            operation_request_id: c.take()?,
            logical_book_id: c.take()?,
            base_book_token_or_zero: c.take()?,
            candidate_source_generation: c.take_u64()?,
            candidate_physical_slot: c.take_u8()?,
            expected_source_length: c.take_u64()?,
            expected_source_sha256: c.take()?,
            display_label: {
                let len = c.take_u8()?;
                let bytes: [u8; DISPLAY_LABEL_MAX_BYTES] = c.take()?;
                DisplayLabel::from_record(len, &bytes)?
            },
        };
        if !decoded.is_valid() {
            return None;
        }
        Some(decoded)
    }
}

// ---------------------------------------------------------------------------
// Bounds-checked field cursors
// ---------------------------------------------------------------------------

/// Sequential writer over a body buffer. Every put is bounds-checked and
/// the encoder's final offset is asserted against the layout constant, so a
/// field added to a struct without its layout bytes (or vice versa) fails
/// every roundtrip test instead of silently shifting offsets.
struct Cursor<'a> {
    buf: &'a mut [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a mut [u8], at: usize) -> Self {
        Self { buf, at }
    }

    fn put(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.at.checked_add(bytes.len())?;
        self.buf.get_mut(self.at..end)?.copy_from_slice(bytes);
        self.at = end;
        Some(())
    }

    fn put_u8(&mut self, value: u8) -> Option<()> {
        self.put(&[value])
    }

    fn put_u16(&mut self, value: u16) -> Option<()> {
        self.put(&value.to_le_bytes())
    }

    fn put_u64(&mut self, value: u64) -> Option<()> {
        self.put(&value.to_le_bytes())
    }

    /// The encoder is done; its cursor must sit exactly where the CRC
    /// field begins. Returns the logical length (fields plus CRC).
    fn finish_at(self, crc_offset: usize) -> Option<usize> {
        if self.at != crc_offset {
            return None;
        }
        crc_offset.checked_add(BODY_CRC_BYTES)
    }
}

/// Sequential reader mirroring [`Cursor`].
struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8], at: usize) -> Self {
        Self { buf, at }
    }

    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let end = self.at.checked_add(N)?;
        let bytes = self.buf.get(self.at..end)?;
        self.at = end;
        bytes.try_into().ok()
    }

    fn take_u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|bytes| bytes[0])
    }

    fn take_u16(&mut self) -> Option<u16> {
        self.take::<2>().map(u16::from_le_bytes)
    }

    fn take_u64(&mut self) -> Option<u64> {
        self.take::<8>().map(u64::from_le_bytes)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::record::{classify_record, record_file_len, seal_body, RecordState};
    use std::vec;
    use std::vec::Vec;

    fn sample_metadata() -> SourceMetadata {
        SourceMetadata {
            logical_book_id: [1; LOGICAL_BOOK_ID_BYTES],
            source_generation: 3,
            source_origin: SourceOrigin::ManagedUpload,
            operation_kind: OperationKind::ManagedUploadRequest,
            operation_request_id: [2; REQUEST_ID_BYTES],
            externally_recovered: false,
            physical_slot: 4,
            source_length: 123_456,
            source_sha256: [5; SHA256_BYTES],
            quick_fingerprint_policy_version: 1,
            quick_fingerprint_sha256: [6; SHA256_BYTES],
            book_token: [7; BOOK_TOKEN_BYTES],
            display_label: DisplayLabel::new(b"A Book").unwrap(),
            unmanaged_name: UnmanagedName::none(),
        }
    }

    /// Seal a body into a full record file image (prepared, no commit
    /// sector) and hand back the file plus the classifier's view.
    fn seal_to_file(
        magic: [u8; 4],
        schema: u16,
        generation: u64,
        logical_len: usize,
        encode: impl FnOnce(&mut [u8]) -> Option<usize>,
    ) -> Vec<u8> {
        let mut file = vec![0u8; record_file_len(logical_len).unwrap()];
        assert_eq!(encode(&mut file), Some(logical_len));
        seal_body(magic, schema, generation, logical_len, &mut file).unwrap();
        file
    }

    #[test]
    fn metadata_roundtrip() {
        let meta = sample_metadata();
        let file = seal_to_file(
            SOURCE_METADATA_MAGIC,
            SOURCE_METADATA_SCHEMA,
            9,
            SOURCE_METADATA_LOGICAL_BYTES,
            |buf| meta.encode_into(buf),
        );
        let RecordState::Prepared(view) = classify_record(&file, SOURCE_METADATA_MAGIC) else {
            panic!("expected prepared");
        };
        assert_eq!(SourceMetadata::decode(&view), Some(meta));
    }

    #[test]
    fn metadata_rejects_bad_provenance() {
        // An unmanaged origin claiming a managed upload operation.
        let mut meta = sample_metadata();
        meta.source_origin = SourceOrigin::UnmanagedSd;
        let mut buf = vec![0u8; SOURCE_METADATA_LOGICAL_BYTES];
        assert_eq!(meta.encode_into(&mut buf), None);

        // Recovery provenance must carry the externally_recovered flag.
        let mut meta = sample_metadata();
        meta.operation_kind = OperationKind::ExternalRecoveryRequest;
        assert_eq!(meta.encode_into(&mut buf), None);
        meta.externally_recovered = true;
        assert!(meta.encode_into(&mut buf).is_some());
    }

    #[test]
    fn metadata_rejects_zero_identity() {
        let mut buf = vec![0u8; SOURCE_METADATA_LOGICAL_BYTES];
        let mut meta = sample_metadata();
        meta.logical_book_id = [0; LOGICAL_BOOK_ID_BYTES];
        assert_eq!(meta.encode_into(&mut buf), None);
        let mut meta = sample_metadata();
        meta.book_token = [0; BOOK_TOKEN_BYTES];
        assert_eq!(meta.encode_into(&mut buf), None);
    }

    #[test]
    fn label_contract() {
        assert!(DisplayLabel::new(b"x").is_some());
        assert!(DisplayLabel::new("β-reader".as_bytes()).is_some());
        assert!(DisplayLabel::new(&[b'a'; 64]).is_some());
        assert!(DisplayLabel::new(b"").is_none());
        assert!(DisplayLabel::new(&[b'a'; 65]).is_none());
        assert!(DisplayLabel::new(b"nul\0byte").is_none());
        assert!(DisplayLabel::new(b"tab\tbyte").is_none());
        assert!(DisplayLabel::new(&[0xFF, 0xFE]).is_none());
    }

    #[test]
    fn label_noncanonical_tail_rejected() {
        let meta = sample_metadata();
        let mut file = seal_to_file(
            SOURCE_METADATA_MAGIC,
            SOURCE_METADATA_SCHEMA,
            1,
            SOURCE_METADATA_LOGICAL_BYTES,
            |buf| meta.encode_into(buf),
        );
        // Poke a byte into the label buffer beyond its length, then re-seal
        // so only the semantic check can catch it.
        // The label buffer sits just ahead of the trailing unmanaged-name
        // field (1 length byte + buffer) and the CRC.
        let label_tail =
            SOURCE_METADATA_LOGICAL_BYTES - BODY_CRC_BYTES - 1 - UNMANAGED_NAME_MAX_BYTES - 1;
        file[label_tail] = b'x';
        seal_body(
            SOURCE_METADATA_MAGIC,
            SOURCE_METADATA_SCHEMA,
            1,
            SOURCE_METADATA_LOGICAL_BYTES,
            &mut file,
        )
        .unwrap();
        let RecordState::Prepared(view) = classify_record(&file, SOURCE_METADATA_MAGIC) else {
            panic!("expected prepared");
        };
        assert_eq!(SourceMetadata::decode(&view), None);
    }

    #[test]
    fn unmanaged_name_contract_and_roundtrip() {
        assert!(UnmanagedName::new("MOBY.EPU").is_some());
        assert!(UnmanagedName::new("A.E").is_some());
        assert!(UnmanagedName::new("12345678.EPU").is_some());
        assert!(UnmanagedName::new("").is_none());
        assert!(UnmanagedName::new("NODOT").is_none());
        assert!(UnmanagedName::new("TOOLONGXX.EPU").is_none());
        assert!(UnmanagedName::new("TWO.DOT.S").is_none());
        assert!(UnmanagedName::new("SP CE.EPU").is_none());

        // An unmanaged record must carry a name and unmanaged provenance.
        let mut meta = sample_metadata();
        meta.source_origin = SourceOrigin::UnmanagedSd;
        meta.operation_kind = OperationKind::LocalUnmanagedOperation;
        let mut buf = vec![0u8; SOURCE_METADATA_LOGICAL_BYTES];
        assert_eq!(meta.encode_into(&mut buf), None, "name required");
        meta.unmanaged_name = UnmanagedName::new("MOBY.EPU").unwrap();
        assert!(meta.encode_into(&mut buf).is_some());
        let file = seal_to_file(
            SOURCE_METADATA_MAGIC,
            SOURCE_METADATA_SCHEMA,
            1,
            SOURCE_METADATA_LOGICAL_BYTES,
            |buf| meta.encode_into(buf),
        );
        let RecordState::Prepared(view) = classify_record(&file, SOURCE_METADATA_MAGIC) else {
            panic!("expected prepared");
        };
        assert_eq!(SourceMetadata::decode(&view), Some(meta));

        // A managed record must not carry one.
        let mut meta = sample_metadata();
        meta.unmanaged_name = UnmanagedName::new("MOBY.EPU").unwrap();
        assert_eq!(meta.encode_into(&mut buf), None);
    }

    #[test]
    fn tombstone_roundtrip() {
        let stone = Tombstone {
            logical_book_id: [3; LOGICAL_BOOK_ID_BYTES],
            deleted_source_generation: 2,
            deleted_book_token: [4; BOOK_TOKEN_BYTES],
            delete_request_id: [5; REQUEST_ID_BYTES],
            delete_result_status: TOMBSTONE_STATUS_DELETED,
        };
        let file = seal_to_file(
            TOMBSTONE_MAGIC,
            TOMBSTONE_SCHEMA,
            1,
            TOMBSTONE_LOGICAL_BYTES,
            |buf| stone.encode_into(buf),
        );
        let RecordState::Prepared(view) = classify_record(&file, TOMBSTONE_MAGIC) else {
            panic!("expected prepared");
        };
        assert_eq!(Tombstone::decode(&view), Some(stone));
    }

    #[test]
    fn marker_roundtrip_and_base_token_rule() {
        let marker = StagingMarker {
            operation: StagedOperation::Replace,
            operation_request_id: [8; REQUEST_ID_BYTES],
            logical_book_id: [9; LOGICAL_BOOK_ID_BYTES],
            base_book_token_or_zero: [10; BOOK_TOKEN_BYTES],
            candidate_source_generation: 5,
            candidate_physical_slot: 2,
            expected_source_length: 42,
            expected_source_sha256: [11; SHA256_BYTES],
            display_label: DisplayLabel::new(b"Replacement").unwrap(),
        };
        let file = seal_to_file(
            STAGING_MARKER_MAGIC,
            STAGING_MARKER_SCHEMA,
            1,
            STAGING_MARKER_LOGICAL_BYTES,
            |buf| marker.encode_into(buf),
        );
        let RecordState::Prepared(view) = classify_record(&file, STAGING_MARKER_MAGIC) else {
            panic!("expected prepared");
        };
        assert_eq!(StagingMarker::decode(&view), Some(marker));

        // A create must not name a base token; a replace must.
        let mut buf = vec![0u8; STAGING_MARKER_LOGICAL_BYTES];
        let mut create = marker;
        create.operation = StagedOperation::Create;
        assert_eq!(create.encode_into(&mut buf), None);
        create.base_book_token_or_zero = [0; BOOK_TOKEN_BYTES];
        assert!(create.encode_into(&mut buf).is_some());
    }

    #[test]
    fn wrong_type_magic_never_decodes() {
        // A tombstone-typed slot holding a (valid) metadata record: the
        // classifier already rejects it on magic before decode is reached.
        let meta = sample_metadata();
        let file = seal_to_file(
            SOURCE_METADATA_MAGIC,
            SOURCE_METADATA_SCHEMA,
            1,
            SOURCE_METADATA_LOGICAL_BYTES,
            |buf| meta.encode_into(buf),
        );
        assert_eq!(
            classify_record(&file, TOMBSTONE_MAGIC),
            RecordState::Corrupt
        );
    }
}
