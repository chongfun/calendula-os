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
//! parsing — live in `proto::upload` so host `cargo test` covers them
//! (this crate only compiles for the firmware target).

// riscv32imc has no CAS; portable-atomic provides it on single-core.
use portable_atomic::AtomicBool;

pub use proto::upload::{hash_identity, sanitized_name, UploadName};

/// True while a book body is streaming; the session-ending reset waits
/// for it so a done press cannot truncate a file mid-write.
pub static UPLOAD_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// True from the moment Wi-Fi requests the upload session until board I/O
/// has closed it. Set before the storage command is queued, which closes
/// the Exit race where the reset could otherwise beat the SD owner into
/// the session and skip the stop handshake entirely.
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
