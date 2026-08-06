//! M0S storage-owner plumbing: the message contract between the Wi-Fi
//! task's logical-book endpoints and the display task (the single SD
//! owner), plus the owner's resident state.
//!
//! The shape mirrors the legacy upload pipeline deliberately: small
//! `Copy` operations go one way on `SOURCE_OPS`, small `Copy` events come
//! back on `SOURCE_EVENTS`, and bulk EPUB bytes ride the existing
//! `UPLOAD_CHUNKS` ping-pong — the storage owner streams them into a
//! `source-store` transaction instead of a bare `/BOOKS` file. All JSON
//! rendering happens on the Wi-Fi side (`proto::source_http`) from these
//! `Copy` values, so no channel ever carries a buffer it did not loan.
//!
//! The owner's working state — the ~43 KB `OpsWorkspace` plus the
//! resident idempotency store — lives here as a compile-time-initialized
//! static. That size may never exist on the ~43 KB stack, and no-alloc
//! firmware has no heap for it outside the wireless session, so `.bss`
//! is the only sound home; the display task takes it once at boot and
//! lends it to each upload session. Loaded lazily on the first
//! logical-book operation of a session, and the mount session resets at
//! that load, because a session's proofs die with its mount.

use source_store::bodies::{DisplayLabel, BOOK_TOKEN_BYTES, LOGICAL_BOOK_ID_BYTES, SHA256_BYTES};
use source_store::list::{BookListEntry, SourceIntegrityStatus};
use source_store::ops::DeleteOutcome;
use source_store::ops::{IdempotencyStore, OpsWorkspace};
use source_store::publish::PublishError;
use source_store::recover::RecoveryOutcome;
use source_store::upload::{UploadBeginOutcome, UploadError};

/// The two crates deliberately do not depend on each other, so the build
/// that owns both is where their advertised ceiling is proven equal.
const _: () = assert!(
    source_store::upload::MAX_SOURCE_BYTES
        == proto::epub::SourceContainerLimits::V1.max_epub_bytes as u64
);

/// The profile string capabilities and operation responses advertise.
pub const BOARD_PROFILE: &str = if cfg!(feature = "device-x3") {
    "x3"
} else {
    "x4"
};

// ---------------------------------------------------------------------------
// Operations (Wi-Fi task -> storage owner)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct SourceUploadOp {
    pub epoch: u64,
    pub nonce: [u8; 16],
    pub declared_length: u64,
    pub declared_sha256: [u8; SHA256_BYTES],
    pub display_label: DisplayLabel,
    /// `None` creates; `Some` replaces the generation this token names.
    pub replace_token: Option<[u8; BOOK_TOKEN_BYTES]>,
}

#[derive(Clone, Copy)]
pub struct SourceDeleteOp {
    pub epoch: u64,
    pub nonce: [u8; 16],
    pub book_token: [u8; BOOK_TOKEN_BYTES],
}

#[derive(Clone, Copy)]
pub struct SourceRecoverOp {
    pub epoch: u64,
    pub nonce: [u8; 16],
    pub book_token: [u8; BOOK_TOKEN_BYTES],
    pub observed_length: u64,
    pub observed_sha256: [u8; SHA256_BYTES],
    pub display_label: Option<DisplayLabel>,
}

#[derive(Clone, Copy)]
pub enum SourceOp {
    /// Create or replace. On `SourceEvent::UploadStarted` the Wi-Fi task
    /// streams the body over `UPLOAD_CHUNKS`; any other reply means no
    /// byte may be sent.
    Upload(SourceUploadOp),
    Delete(SourceDeleteOp),
    Recover(SourceRecoverOp),
    /// Stream every listable book back as `ListEntry` events, then
    /// `ListEnd`.
    List,
    /// Rotate the idempotency epoch if it lacks headroom, then report it.
    Capabilities,
}

// ---------------------------------------------------------------------------
// Events (storage owner -> Wi-Fi task)
// ---------------------------------------------------------------------------

/// A committed create, replace, or recovery — everything the JSON
/// response needs, so the Wi-Fi task renders without asking again.
#[derive(Clone, Copy)]
pub struct SourceCommit {
    pub logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
    pub book_token: [u8; BOOK_TOKEN_BYTES],
    pub source_generation: u64,
    pub source_length: u64,
    pub source_sha256: [u8; SHA256_BYTES],
    pub display_label: DisplayLabel,
}

#[derive(Clone, Copy)]
pub struct SourceCaps {
    pub idempotency_epoch: u64,
    pub max_new_requests_this_epoch: u64,
    pub retained_previous_epoch: u64,
}

#[derive(Clone, Copy)]
pub enum SourceEvent {
    UploadStarted,
    Committed(SourceCommit),
    Deleted {
        logical_book_id: [u8; LOGICAL_BOOK_ID_BYTES],
    },
    Refused(Refusal),
    ListEntry(BookListEntry),
    ListEnd {
        count: u16,
    },
    Capabilities(SourceCaps),
}

/// Every way an operation is refused, as the stable machine-readable
/// vocabulary the PRD requires: one code per distinct client action
/// ("re-fetch capabilities", "use recovery instead", "give up"), never a
/// filesystem path or a transient's guts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The catalog or idempotency state could not be loaded; retry after
    /// the storage owner recovers.
    StorageUnavailable,
    /// No authoritative book carries the given token.
    UnknownToken,
    /// Declared length exceeds the advertised ceiling.
    SourceTooLarge,
    /// The replace base is externally modified/unavailable; recovery or
    /// delete are the repairs.
    ExternallyModified,
    /// The request's epoch is not current; re-fetch capabilities.
    StaleEpoch,
    /// The request ID was already used with different parameters, or at a
    /// different endpoint.
    RequestIdMisuse,
    /// The epoch's new-request budget is spent; re-fetch capabilities
    /// (which rotates) and retry with a fresh ID.
    EpochExhausted,
    /// Every managed slot is occupied.
    NoFreeSlot,
    /// Every tombstone slot is occupied; cleanup frees them.
    NoTombstoneSlot,
    /// Committed records disagree about this request ID; the card needs
    /// attention, not retries.
    AmbiguousRequestEvidence,
    /// Streamed bytes disagreed with the declared length.
    LengthMismatch,
    /// Streamed bytes hashed differently than declared.
    DigestMismatch,
    /// The persisted bytes reread differently than received.
    PersistMismatch,
    /// The classic-ZIP gate refused (ZIP64, bounds, malformed).
    UnsupportedContainer,
    /// Authority changed under the operation; safe to retry.
    Conflict,
    /// The book's bytes still match its committed identity.
    NotExternallyModified,
    /// The bytes on the card match neither committed nor observed
    /// identity; re-observe and retry with a new request ID.
    ObservedMismatch,
    /// Recovery was asked of an unmanaged book.
    UnmanagedBook,
    /// The client aborted its own upload stream.
    ClientAborted,
    /// Card I/O failed mid-operation.
    StorageIo,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Self::StorageUnavailable => "storage_unavailable",
            Self::UnknownToken => "unknown_token",
            Self::SourceTooLarge => "source_too_large",
            Self::ExternallyModified => "externally_modified",
            Self::StaleEpoch => "stale_epoch",
            Self::RequestIdMisuse => "request_id_misuse",
            Self::EpochExhausted => "epoch_exhausted",
            Self::NoFreeSlot => "no_free_slot",
            Self::NoTombstoneSlot => "no_tombstone_slot",
            Self::AmbiguousRequestEvidence => "ambiguous_request_evidence",
            Self::LengthMismatch => "length_mismatch",
            Self::DigestMismatch => "digest_mismatch",
            Self::PersistMismatch => "persist_mismatch",
            Self::UnsupportedContainer => "unsupported_container",
            Self::Conflict => "conflict",
            Self::NotExternallyModified => "not_externally_modified",
            Self::ObservedMismatch => "observed_mismatch",
            Self::UnmanagedBook => "unmanaged_book",
            Self::ClientAborted => "client_aborted",
            Self::StorageIo => "storage_io",
        }
    }

    /// Whether the *same* request can succeed later without the client
    /// changing anything. Stale epochs and exhausted budgets are
    /// retryable because re-fetching capabilities is part of the retry;
    /// mismatches and misuse are not, because the same bytes will always
    /// get the same answer.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::StorageUnavailable
                | Self::StaleEpoch
                | Self::EpochExhausted
                | Self::NoTombstoneSlot
                | Self::Conflict
                | Self::PersistMismatch
                | Self::StorageIo
        )
    }

    pub fn http_status(self) -> &'static str {
        match self {
            Self::StorageUnavailable | Self::StorageIo => "503 Service Unavailable",
            Self::UnknownToken => "404 Not Found",
            Self::SourceTooLarge => "413 Content Too Large",
            Self::StaleEpoch | Self::ExternallyModified | Self::Conflict => "409 Conflict",
            Self::EpochExhausted => "429 Too Many Requests",
            _ => "400 Bad Request",
        }
    }
}

pub fn refusal_for_begin(outcome: &UploadBeginOutcome) -> Refusal {
    match outcome {
        UploadBeginOutcome::Started(_) | UploadBeginOutcome::Replayed(_) => Refusal::Conflict,
        UploadBeginOutcome::RejectedUnknownToken => Refusal::UnknownToken,
        UploadBeginOutcome::RejectedTooLarge => Refusal::SourceTooLarge,
        UploadBeginOutcome::RejectedExternallyModified => Refusal::ExternallyModified,
        UploadBeginOutcome::RejectedStaleEpoch => Refusal::StaleEpoch,
        UploadBeginOutcome::RejectedParameterMismatch => Refusal::RequestIdMisuse,
        UploadBeginOutcome::RejectedEpochExhausted => Refusal::EpochExhausted,
        UploadBeginOutcome::RejectedIdentityCollision => Refusal::Conflict,
        UploadBeginOutcome::RejectedNoFreeSlot => Refusal::NoFreeSlot,
        UploadBeginOutcome::CatalogUnavailable | UploadBeginOutcome::IdempotencyUnavailable => {
            Refusal::StorageUnavailable
        }
        UploadBeginOutcome::AmbiguousRequestEvidence => Refusal::AmbiguousRequestEvidence,
        UploadBeginOutcome::Failed(error) => refusal_for_publish(*error),
    }
}

pub fn refusal_for_upload_error(error: UploadError) -> Refusal {
    match error {
        UploadError::LengthMismatch => Refusal::LengthMismatch,
        UploadError::DigestMismatch => Refusal::DigestMismatch,
        UploadError::PersistMismatch => Refusal::PersistMismatch,
        UploadError::UnsupportedContainer => Refusal::UnsupportedContainer,
        UploadError::RevalidationRefused => Refusal::Conflict,
        UploadError::CatalogUnavailable | UploadError::IdempotencyUnavailable => {
            Refusal::StorageUnavailable
        }
        UploadError::Io(error) => refusal_for_publish(error),
    }
}

pub fn refusal_for_delete(outcome: &DeleteOutcome) -> Refusal {
    match outcome {
        DeleteOutcome::Deleted { .. } => Refusal::Conflict,
        DeleteOutcome::RejectedUnknownToken => Refusal::UnknownToken,
        DeleteOutcome::RejectedStaleEpoch => Refusal::StaleEpoch,
        DeleteOutcome::RejectedParameterMismatch => Refusal::RequestIdMisuse,
        DeleteOutcome::RejectedEpochExhausted => Refusal::EpochExhausted,
        DeleteOutcome::RejectedNoTombstoneSlot => Refusal::NoTombstoneSlot,
        DeleteOutcome::CatalogUnavailable | DeleteOutcome::IdempotencyUnavailable => {
            Refusal::StorageUnavailable
        }
        DeleteOutcome::AmbiguousRequestEvidence => Refusal::AmbiguousRequestEvidence,
        DeleteOutcome::Failed(error) => refusal_for_publish(*error),
    }
}

pub fn refusal_for_recovery(outcome: &RecoveryOutcome) -> Refusal {
    match outcome {
        RecoveryOutcome::Recovered(_) => Refusal::Conflict,
        RecoveryOutcome::RejectedUnknownToken => Refusal::UnknownToken,
        RecoveryOutcome::RejectedUnmanagedBook => Refusal::UnmanagedBook,
        RecoveryOutcome::RejectedNotExternallyModified => Refusal::NotExternallyModified,
        RecoveryOutcome::RejectedObservedMismatch => Refusal::ObservedMismatch,
        RecoveryOutcome::RejectedStaleEpoch => Refusal::StaleEpoch,
        RecoveryOutcome::RejectedParameterMismatch => Refusal::RequestIdMisuse,
        RecoveryOutcome::RejectedEpochExhausted => Refusal::EpochExhausted,
        RecoveryOutcome::RejectedIdentityCollision => Refusal::Conflict,
        RecoveryOutcome::RejectedUnsupportedContainer => Refusal::UnsupportedContainer,
        RecoveryOutcome::CatalogUnavailable | RecoveryOutcome::IdempotencyUnavailable => {
            Refusal::StorageUnavailable
        }
        RecoveryOutcome::AmbiguousRequestEvidence => Refusal::AmbiguousRequestEvidence,
        RecoveryOutcome::Failed(error) => refusal_for_publish(*error),
    }
}

pub fn refusal_for_publish(error: PublishError) -> Refusal {
    match error {
        PublishError::RevalidationRefused => Refusal::Conflict,
        PublishError::AmbiguousAuthority
        | PublishError::CorruptAuthority
        | PublishError::UnsupportedSchema => Refusal::StorageUnavailable,
        _ => Refusal::StorageIo,
    }
}

/// The wire string for a list entry's integrity status.
pub fn integrity_status_str(status: SourceIntegrityStatus) -> &'static str {
    match status {
        SourceIntegrityStatus::UncheckedThisMount => "unchecked_this_mount",
        SourceIntegrityStatus::ValidatedThisMount => "validated_this_mount",
        SourceIntegrityStatus::Unavailable => "unavailable",
        SourceIntegrityStatus::ExternallyModified => "externally_modified",
        SourceIntegrityStatus::UnsupportedSourceContainer => "unsupported_source_container",
    }
}

// ---------------------------------------------------------------------------
// Owner state
// ---------------------------------------------------------------------------

/// The storage owner's resident M0S state. `idem: None` doubles as "this
/// session has not loaded the catalog yet".
pub struct SourceOwnerState {
    pub ws: OpsWorkspace,
    pub idem: Option<IdempotencyStore>,
}

/// The initial owner state as a flash-resident image (an immutable static
/// lands in `.rodata`, which the C3 maps from flash). ~12 KB: the
/// workspace's record scratch, catalog view, and mount session, plus the
/// receipt table. It exists because no sound path builds a value this
/// size at runtime — the stack budget is 27 KB and `ptr::write` would
/// transit it — while an untyped copy of a linker-materialized image
/// costs nothing but flash.
///
/// This is the M0S RAM budget decision, made three times over: a
/// permanent `.bss` static was tried first and failed the X4 link (DRAM
/// overflow, then the reader's stack-headroom assertion); then the ~23 KB
/// image at 32 slots/16 receipts did not fit the X3's wireless session
/// heap at all (27,264 B free); and even the shrunken image must be
/// carved out *before* the radio fragments the donated regions — total
/// free is meaningless when no single hole is image-sized. The state is
/// only ever touched between `ReceiveUpload` and the session-ending
/// reset — exactly the lifetime of the loaned session heap, so that heap
/// is where the live copy belongs.
static OWNER_INIT: SourceOwnerState = SourceOwnerState {
    ws: OpsWorkspace::new(),
    idem: None,
};

static OWNER_CLAIMED: portable_atomic::AtomicBool = portable_atomic::AtomicBool::new(false);

/// The parked claim, between the wifi task's early carve-out and the
/// storage owner's pickup at upload-session entry. Null when unclaimed,
/// already taken, or the claim failed.
static OWNER_HANDOFF: portable_atomic::AtomicPtr<SourceOwnerState> =
    portable_atomic::AtomicPtr::new(core::ptr::null_mut());

/// Carve the owner state out of the wireless session's loaned heap and
/// park it for the storage owner, exactly once per boot. The wifi task
/// calls this immediately after `sync_mem::donate_heap`, while the
/// donated regions are still pristine: the image needs one *contiguous*
/// block, and once radio init and the upload server's stream pool have
/// carved the regions up, no image-sized hole survives — measured on X3
/// hardware 2026-08-06, where 19 KB of total free heap refused a 12.5 KB
/// claim. A failed claim only logs: the logical-book endpoints then
/// answer `storage_unavailable`, and the legacy shelf is unaffected.
pub fn claim_session_owner_early() {
    use portable_atomic::Ordering;

    if OWNER_CLAIMED.swap(true, Ordering::SeqCst) {
        return;
    }
    let layout = core::alloc::Layout::new::<SourceOwnerState>();
    // SAFETY: the layout is non-zero-sized; a null return is handled. The
    // global allocator hands back memory aligned for the layout and owned
    // exclusively by this call, and the claim flag above makes this the
    // only live pointer to it. `copy_nonoverlapping` is an untyped copy,
    // so the image's padding bytes transfer without being read as values,
    // and the destination afterwards holds a valid `SourceOwnerState`
    // because the source is one. The allocation is leaked by design: the
    // session heap dies with the session-ending reset.
    #[allow(unsafe_code)]
    unsafe {
        let ptr = alloc::alloc::alloc(layout);
        if ptr.is_null() {
            esp_println::println!("source: owner state allocation refused (heap exhausted)");
            return;
        }
        core::ptr::copy_nonoverlapping(
            core::ptr::from_ref(&OWNER_INIT).cast::<u8>(),
            ptr,
            layout.size(),
        );
        esp_println::println!(
            "source: owner claimed {} B, heap used={} free={}",
            layout.size(),
            esp_alloc::HEAP.used(),
            esp_alloc::HEAP.free()
        );
        OWNER_HANDOFF.store(ptr.cast::<SourceOwnerState>(), Ordering::SeqCst);
    }
}

/// Take the parked owner state, exactly once. The display task's upload
/// session calls this at entry; `None` means the early claim failed, the
/// wireless session never donated, or a second session of one boot asks
/// again — sessions end in a software reset, so a second session only
/// follows a non-terminal sleep, whose stale mount proofs must not be
/// reused; refusing beats aliasing.
pub fn take_session_owner() -> Option<&'static mut SourceOwnerState> {
    use portable_atomic::Ordering;

    let ptr = OWNER_HANDOFF.swap(core::ptr::null_mut(), Ordering::SeqCst);
    // SAFETY: the swap-to-null makes this call the only extraction of the
    // parked pointer, which (when non-null) is the leaked, initialized
    // allocation made by `claim_session_owner_early` — uniquely referenced
    // and 'static by design.
    #[allow(unsafe_code)]
    unsafe {
        ptr.as_mut()
    }
}
