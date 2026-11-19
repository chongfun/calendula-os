//! Browser-to-shelf book upload plumbing.
//!
//! The wifi task receives raw EPUB bytes over HTTP and streams them to
//! the display task (the single SD owner) through a two-buffer
//! ping-pong: chunks carry loaned 4 KB buffers one way, the buffers
//! come back on the return channel once written. The display task holds
//! one SD session for the whole upload phase and writes /BOOKS/<name>,
//! where <name> is the browser's filename as a VFAT long name with a
//! hashed 8.3 alias beside it.
//!
//! The pure pieces — name derivation, label shaping, identity-sidecar
//! parsing — live in `proto::upload` so host `cargo test` covers them.

// riscv32imc has no CAS; portable-atomic provides it on single-core.
use portable_atomic::{AtomicBool, AtomicU8, Ordering};

pub use proto::upload::{hash_identity, sanitized_name, UploadName};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SessionState {
    Inactive = 0,
    Active = 1,
    Closing = 2,
}

pub static UPLOAD_SESSION_STATE: AtomicU8 = AtomicU8::new(SessionState::Inactive as u8);

pub fn session_state() -> SessionState {
    match UPLOAD_SESSION_STATE.load(Ordering::SeqCst) {
        1 => SessionState::Active,
        2 => SessionState::Closing,
        _ => SessionState::Inactive,
    }
}

pub fn set_session_state(state: SessionState) {
    UPLOAD_SESSION_STATE.store(state as u8, Ordering::SeqCst);
    UPLOAD_SESSION_ACTIVE.store(state == SessionState::Active, Ordering::SeqCst);
}

/// A frontlight edit may be pending even if no HTTP request ever opened an
/// SD upload session. An Exit must open and stop a session to flush the light
/// value before reset; this also retries a failed previous teardown.
pub const fn exit_needs_stop(state: SessionState, light_needs_flush: bool) -> bool {
    !matches!(state, SessionState::Inactive) || light_needs_flush
}

/// True while an upload or browser request is in flight; the power task
/// checks it before sleep to avoid interrupting active transfers.
pub static UPLOAD_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Backward-compatible flag: true when the upload session is Active.
pub static UPLOAD_SESSION_ACTIVE: AtomicBool = AtomicBool::new(false);

pub struct UploadBegin {
    /// The 8.3 name to delete. Empty for uploads: which alias a book lands
    /// under is the installer's to choose, once the file is complete and it
    /// can see which aliases are free.
    pub name: UploadName,
    /// The VFAT long name the book is installed under — the name a computer
    /// shows, and the one an upload of the same book replaces. Empty for
    /// deletions.
    pub long_name: proto::upload::UploadFilename,
    /// True removes the named book instead of writing one.
    pub delete: bool,
    /// Whether the name lives in /BOOKS (uploads always do; deletions
    /// follow the catalog's location flag).
    pub in_books: bool,
    /// How the same book would have been stored before uploads carried a
    /// long name, so re-uploading one of those replaces it instead of
    /// landing beside it. `None` for deletions.
    pub legacy: Option<upload_store::install::LegacyKey>,
}

pub struct UploadChunk {
    /// `None` only on aborts that have no buffer left to hand over.
    pub buffer: Option<&'static mut [u8]>,
    pub len: usize,
    pub last: bool,
    pub abort: bool,
}

#[cfg(test)]
mod tests {
    use super::{exit_needs_stop, SessionState};

    #[test]
    fn any_pending_light_change_requires_a_storage_stop_before_wifi_reset() {
        assert!(!exit_needs_stop(SessionState::Inactive, false));
        // Also covers a fresh Wi-Fi session with no HTTP upload, and a
        // previous teardown whose save failed.
        assert!(exit_needs_stop(SessionState::Inactive, true));
        assert!(exit_needs_stop(SessionState::Active, false));
        assert!(exit_needs_stop(SessionState::Closing, false));
    }
}
