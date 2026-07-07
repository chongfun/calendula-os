//! Browser-to-shelf book upload plumbing.
//!
//! The wifi task receives raw EPUB bytes over HTTP and streams them to
//! the display task (the single SD owner) through a two-buffer
//! ping-pong: chunks carry loaned 4 KB buffers one way, the buffers
//! come back on the return channel once written. The display task holds
//! one SD session for the whole upload phase and writes /BOOKS/<8.3>.

use heapless::String;
// riscv32imc has no CAS; portable-atomic provides it on single-core.
use portable_atomic::AtomicBool;

/// True while a book body is streaming; the session-ending reset waits
/// for it so a done press cannot truncate a file mid-write.
pub static UPLOAD_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// 8.3 names cap at twelve characters.
pub type UploadName = String<12>;

pub struct UploadBegin {
    pub name: UploadName,
    /// True removes the named book instead of writing one.
    pub delete: bool,
    /// Whether the name lives in /BOOKS (uploads always do; deletions
    /// follow the catalog's location flag).
    pub in_books: bool,
    /// The client's original filename, decoded but otherwise untouched.
    /// The 8.3 `name` can't carry a real title, so this is stashed in a
    /// label sidecar and shown in the Library until the book is first opened
    /// (which learns the EPUB title). Empty for deletions.
    pub label: UploadLabel,
}

/// Display-name budget for the upload label sidecar, matched to the catalog
/// label width.
pub type UploadLabel = String<64>;

pub struct UploadChunk {
    /// `None` only on aborts that have no buffer left to hand over.
    pub buffer: Option<&'static mut [u8]>,
    pub len: usize,
    pub last: bool,
    pub abort: bool,
}

/// Derives an 8.3 upload name from raw (still percent-encoded) filename
/// bytes: keep the first eight ASCII alphanumerics uppercased, default
/// to BOOK, extension `.EPU` (which the catalog scan accepts alongside
/// `.epub`). Working on raw bytes sidesteps any decode-buffer limit;
/// percent escapes simply contribute their hex letters.
pub fn sanitized_name(client_name: &[u8]) -> UploadName {
    let stem_end = client_name
        .iter()
        .rposition(|byte| *byte == b'.')
        .unwrap_or(client_name.len());
    let stem = &client_name[..stem_end];
    let mut name = UploadName::new();
    let mut at = 0;
    while at < stem.len() && name.len() < 8 {
        // Decode %XX escapes so "High%20Output" stems as HIGHOUTP, not
        // HIGH20OU; undecodable escapes fall through as literal bytes.
        let byte = if stem[at] == b'%' && at + 2 < stem.len() {
            match (hex_nibble(stem[at + 1]), hex_nibble(stem[at + 2])) {
                (Some(high), Some(low)) => {
                    at += 2;
                    (high << 4) | low
                }
                _ => stem[at],
            }
        } else {
            stem[at]
        };
        if byte.is_ascii_alphanumeric() {
            let _ = name.push(byte.to_ascii_uppercase() as char);
        }
        at += 1;
    }
    if name.is_empty() {
        let _ = name.push_str("BOOK");
    }
    let _ = name.push_str(".EPU");
    name
}

/// Decodes a percent-encoded client filename into a readable label source,
/// preserving spaces and case (unlike `sanitized_name`, which forces 8.3).
/// The catalog label derivation later strips the extension and prettifies it,
/// so the result is shaped exactly like a copied book's filename label.
/// Falls back to ASCII-only if the decoded bytes aren't valid UTF-8 (e.g. a
/// multibyte escape truncated at the buffer edge).
pub fn readable_filename(client_name: &[u8]) -> UploadLabel {
    let mut bytes = [0u8; 64];
    let mut len = 0;
    let mut at = 0;
    while at < client_name.len() && len < bytes.len() {
        let byte = if client_name[at] == b'%' && at + 2 < client_name.len() {
            match (
                hex_nibble(client_name[at + 1]),
                hex_nibble(client_name[at + 2]),
            ) {
                (Some(high), Some(low)) => {
                    at += 2;
                    (high << 4) | low
                }
                _ => client_name[at],
            }
        } else {
            client_name[at]
        };
        at += 1;
        bytes[len] = byte;
        len += 1;
    }
    let mut out = UploadLabel::new();
    match core::str::from_utf8(&bytes[..len]) {
        Ok(text) => {
            let _ = out.push_str(text);
        }
        Err(_) => {
            for &byte in &bytes[..len] {
                if byte.is_ascii() && byte >= 0x20 {
                    let _ = out.push(byte as char);
                }
            }
        }
    }
    out
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
