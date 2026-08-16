//! Upload naming and identity-sidecar formats shared by the firmware's
//! browser-to-shelf upload path.
//!
//! These are the pure pieces of that path: 8.3 name derivation, label
//! shaping, and the identity-sidecar wire format. They live here rather
//! than in `fw` so host `cargo test` actually exercises them — `fw` only
//! compiles for the riscv32 firmware target and is excluded from the CI
//! test job.

use core::fmt::Write;
use heapless::String;

/// 8.3 names cap at twelve characters.
pub type UploadName = String<12>;

/// Display-name budget for the upload label sidecar, matched to the catalog
/// label width.
pub type UploadLabel = String<64>;

/// Derives an 8.3 upload name from percent-decoded filename bytes:
/// the first four ASCII alphanumerics uppercased (default BOOK),
/// four base-36 digits hashed from the whole decoded stem, and extension
/// `.EPU` (which the catalog scan accepts alongside `.epub`). A prefix
/// alone is not enough — book filenames often share their first eight
/// characters (author, series), and the write path replaces an
/// identity-matched existing name, so a library of same-prefix uploads
/// collapsed to one file. The hash spreads those apart while staying
/// deterministic: re-uploading the same filename still replaces the same
/// book.
pub fn sanitized_name(client_name: &[u8]) -> UploadName {
    let stem_end = client_name
        .iter()
        .rposition(|byte| *byte == b'.')
        .unwrap_or(client_name.len());
    let stem = &client_name[..stem_end];
    let mut name = UploadName::new();
    let mut hash: u32 = 0x811c_9dc5;
    let mut at = 0;
    while at < stem.len() {
        let byte = stem[at];
        hash = (hash ^ byte as u32).wrapping_mul(0x0100_0193);
        if name.len() < 4 && byte.is_ascii_alphanumeric() {
            let _ = name.push(byte.to_ascii_uppercase() as char);
        }
        at += 1;
    }
    if name.is_empty() {
        let _ = name.push_str("BOOK");
    } else {
        while name.len() < 4 {
            let _ = name.push('X');
        }
    }
    let digits = base36_tail(hash);
    for digit in digits {
        let _ = name.push(digit as char);
    }
    let _ = name.push_str(".EPU");
    name
}

/// Creates a readable label source from pre-decoded client filename bytes,
/// preserving spaces and case (unlike `sanitized_name`, which forces 8.3).
/// The catalog label derivation later strips the extension and prettifies it,
/// so the result is shaped exactly like a copied book's filename label.
/// Falls back to ASCII-only if the bytes aren't valid UTF-8 (e.g. a
/// multibyte character truncated at the buffer edge).
pub fn readable_filename(client_name: &[u8]) -> UploadLabel {
    let mut bytes = [0u8; 64];
    let len = client_name.len().min(bytes.len());
    bytes[..len].copy_from_slice(&client_name[..len]);
    let mut out = UploadLabel::new();
    match core::str::from_utf8(&bytes[..len]) {
        Ok(text) => {
            let _ = out.push_str(text);
        }
        Err(err) => {
            let valid_len = err.valid_up_to();
            if let Ok(valid_text) = core::str::from_utf8(&bytes[..valid_len]) {
                let _ = out.push_str(valid_text);
            }
            for &byte in &bytes[valid_len..len] {
                if byte.is_ascii() && byte >= 0x20 {
                    let _ = out.push(byte as char);
                }
            }
        }
    }
    out
}

/// The list label a catalog record shows, derived from its file name: strip
/// the epub extension and prettify the stem the same way for copied and
/// uploaded books. Non-injective — distinct filenames can map to the same
/// label — so identity must come from the sidecar hash, never a label match.
pub fn derive_catalog_label(display_name: &str, open_name: &str, out: &mut String<64>) {
    if open_name.eq_ignore_ascii_case("HPMOR.EPU") || open_name.eq_ignore_ascii_case("HPMOR.EPUB") {
        let _ = out.push_str("Harry Potter and the Methods of Rationality");
        return;
    }

    let file_name = display_name
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(display_name);
    let stem = strip_epub_suffix(file_name).unwrap_or(file_name);
    push_pretty_file_stem(stem, out);
    if out.is_empty() {
        let _ = out.push_str(display_name);
    }
}

fn strip_epub_suffix(name: &str) -> Option<&str> {
    let bytes = name.as_bytes();
    if bytes.len() >= 5 && bytes[bytes.len() - 5..].eq_ignore_ascii_case(b".epub") {
        return Some(&name[..name.len() - 5]);
    }
    if bytes.len() >= 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".epu") {
        return Some(&name[..name.len() - 4]);
    }
    None
}

/// Append a readable catalog label from a filename stem. Iterates characters,
/// not bytes: stepping a multi-byte character one byte at a time would push
/// each byte as its own `char` and turn "Café" into "CafÃ©".
fn push_pretty_file_stem(stem: &str, out: &mut String<64>) {
    let mut capitalize_next = true;
    for ch in stem.chars() {
        let ch = match ch {
            '-' | '_' => {
                capitalize_next = true;
                ' '
            }
            'a'..='z' if capitalize_next => {
                capitalize_next = false;
                ch.to_ascii_uppercase()
            }
            'A'..='Z' | '0'..='9' => {
                capitalize_next = false;
                ch
            }
            '.' => break,
            _ => {
                if ch.is_alphanumeric() {
                    capitalize_next = false;
                }
                ch
            }
        };
        if ch == ' ' && (out.is_empty() || out.as_str().ends_with(' ')) {
            continue;
        }
        if out.push(ch).is_err() {
            break;
        }
    }
    while out.as_str().ends_with(' ') {
        out.pop();
    }
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub fn base36_tail(hash: u32) -> [u8; 4] {
    const BASE36: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut tail = hash % 36u32.pow(4);
    let mut digits = [0u8; 4];
    for digit in digits.iter_mut().rev() {
        *digit = BASE36[(tail % 36) as usize];
        tail /= 36;
    }
    digits
}

pub fn hash_identity(client_name: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in client_name {
        hash = (hash ^ byte as u64).wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn percent_decode_in_place(bytes: &mut [u8]) -> usize {
    let mut read = 0;
    let mut write = 0;
    while read < bytes.len() {
        if bytes[read] == b'%' && read + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_nibble(bytes[read + 1]), hex_nibble(bytes[read + 2]))
            {
                bytes[write] = (high << 4) | low;
                read += 3;
                write += 1;
                continue;
            }
        }
        bytes[write] = bytes[read];
        read += 1;
        write += 1;
    }
    write
}

/// Returns true if the exact query parameter is present in the path's query string.
pub fn has_query_param(path: &[u8], param: &[u8]) -> bool {
    let Some(query_at) = path.iter().position(|byte| *byte == b'?') else {
        return false;
    };
    path[query_at + 1..]
        .split(|byte| *byte == b'&')
        .any(|pair| pair == param)
}

/// Raw (still percent-encoded) value of a named query parameter.
///
/// Matches on the whole `name=` prefix of a parameter, so a request whose
/// query carries `reset_after_ms=` does not answer a lookup for `after_ms`.
pub fn query_param<'a>(path: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let query_at = path.iter().position(|byte| *byte == b'?')? + 1;
    path[query_at..]
        .split(|byte| *byte == b'&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix(b"="))
}

/// Percent-decoded `name=` value from a path's query string.
pub fn raw_query_name(path: &mut [u8]) -> Option<&mut [u8]> {
    let query_at = path.iter().position(|byte| *byte == b'?')? + 1;
    let pair = path[query_at..]
        .split_mut(|byte| *byte == b'&')
        .find(|pair| pair.starts_with(b"name="))?;
    let raw_name = &mut pair[5..];
    let len = percent_decode_in_place(raw_name);
    if len == 0 {
        return None;
    }
    Some(&mut raw_name[..len])
}

/// Interprets an identity sidecar read: an 8-byte file whose read returns all
/// 8 bytes is a valid little-endian identity hash.
///
/// The two failure shapes get different verdicts because one is deterministic
/// and one is transient. A sidecar of any other length is malformed for good —
/// retrying can't fix it — so it reads as `Ok(None)` (no identity) and the
/// collision probe moves on; the worst outcome is a visible duplicate book
/// instead of every upload probing that slot failing forever. A short read or
/// I/O error on a correctly-sized file means the card can't be trusted right
/// now, so it surfaces as `Err` and the upload aborts and can be retried.
// The unit error mirrors fw's sidecar helpers (read_upload_identity et al.),
// where the only response to an I/O failure is aborting the upload.
#[allow(clippy::result_unit_err)]
pub fn parse_identity_read<E>(
    file_len: u32,
    read_result: Result<usize, E>,
    buf: &[u8; 8],
) -> Result<Option<u64>, ()> {
    if file_len != 8 {
        return Ok(None);
    }
    match read_result {
        Ok(8) => Ok(Some(u64::from_le_bytes(*buf))),
        _ => Err(()),
    }
}

// ---------------------------------------------------------------------------
// VFAT long names
// ---------------------------------------------------------------------------
//
// Ported from upstream `7f9856d` ("Write wireless uploads as EPUB long
// names"), alongside the driver fork that can create them. These supersede
// `sanitized_name` for the wireless path: the long name is what the user
// sees on a computer, and the 8.3 alias below exists only because FAT
// requires every long-named file to have one.

/// Longest long-name we build. Bounded rather than heap-allocated, like
/// everything else on this path.
pub const UPLOAD_FILENAME_BYTES: usize = 64;
pub type UploadFilename = String<UPLOAD_FILENAME_BYTES>;
pub type UploadShortName = String<12>;

const EPUB_SUFFIX: &str = ".epub";
const MAX_DECODED_BASENAME_BYTES: usize = 256;
const FNV_OFFSET: u32 = 0x811C_9DC5;
const FNV_PRIME: u32 = 0x0100_0193;

/// Turn an **already percent-decoded** browser filename into a portable
/// VFAT long name.
///
/// Path components and the supplied extension are discarded, FAT-invalid
/// characters are replaced, and the result always ends in lowercase `.epub`.
///
/// Decoding is deliberately not done here. [`raw_query_name`] decodes the
/// query value in place, and this project's upload path calls it first, so
/// decoding again would consume a second round of escapes: a file genuinely
/// named `Novel%2FPart.epub` arrives as `Novel%252FPart.epub`, decodes once
/// to its real name, and a second pass would read that `%2F` as a path
/// separator and truncate the name to `Part.epub`. One decoding boundary,
/// and it is the caller's.
pub fn wireless_epub_filename(client_name: &[u8]) -> UploadFilename {
    let mut basename = [0u8; MAX_DECODED_BASENAME_BYTES];
    let mut basename_len = 0;
    for &byte in client_name {
        if byte == b'/' || byte == b'\\' {
            basename_len = 0;
        } else if basename_len < basename.len() {
            basename[basename_len] = byte;
            basename_len += 1;
        }
    }

    let decoded = match core::str::from_utf8(&basename[..basename_len]) {
        Ok(text) => text,
        Err(error) => core::str::from_utf8(&basename[..error.valid_up_to()]).unwrap_or(""),
    };
    let decoded = decoded.trim_matches(|ch| ch == ' ' || ch == '.');
    let stem = decoded
        .rfind('.')
        .map(|extension_at| &decoded[..extension_at])
        .unwrap_or(decoded)
        .trim_matches(|ch| ch == ' ' || ch == '.');

    let mut out = UploadFilename::new();
    for ch in stem.chars() {
        let ch = if ch.is_control()
            || matches!(ch, '"' | '*' | '/' | ':' | '<' | '>' | '?' | '\\' | '|')
        {
            '_'
        } else {
            ch
        };
        if out.len() + ch.len_utf8() + EPUB_SUFFIX.len() > out.capacity() {
            break;
        }
        let _ = out.push(ch);
    }
    while out.ends_with([' ', '.']) {
        out.pop();
    }
    if out.is_empty() {
        let _ = out.push_str("Book");
    }
    // Windows reserves these device names even with an extension, so a card
    // holding `NUL.epub` cannot be manipulated normally there — and this
    // helper promises a *portable* name. FAT itself permits them, and the
    // driver's validator checks only characters and length, so the guard
    // belongs here.
    //
    // The reservation attaches to the part before the *first* dot, so
    // `NUL.txt` is reserved too and the guard has to look there rather than
    // at the whole stem. The suffix that follows is kept: `NUL.txt` becomes
    // `NUL_.txt`, not `NUL.txt_`.
    let head_len = out.as_str().split('.').next().map_or(0, str::len);
    if is_reserved_dos_name(&out[..head_len]) {
        let mut guarded = UploadFilename::new();
        let _ = guarded.push_str(&out[..head_len]);
        let _ = guarded.push('_');
        for ch in out[head_len..].chars() {
            if guarded.len() + ch.len_utf8() + EPUB_SUFFIX.len() > guarded.capacity() {
                break;
            }
            let _ = guarded.push(ch);
        }
        out = guarded;
    }
    let _ = out.push_str(EPUB_SUFFIX);
    out
}

/// Whether a name component is a reserved DOS device name,
/// case-insensitively. Compares by `char` rather than by byte because the
/// port numbers may be superscripts, which are multi-byte.
fn is_reserved_dos_name(component: &str) -> bool {
    const BARE: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    const NUMBERED: [&str; 2] = ["COM", "LPT"];

    if BARE.iter().any(|name| component.eq_ignore_ascii_case(name)) {
        return true;
    }
    let mut chars = component.chars();
    let (Some(a), Some(b), Some(c), Some(port)) =
        (chars.next(), chars.next(), chars.next(), chars.next())
    else {
        return false;
    };
    if chars.next().is_some() {
        return false;
    }
    // Windows recognizes the superscript forms of 1-3 here as well as the
    // ASCII digits, so `COM\u{b9}` names the same device as `COM1`.
    if !matches!(port, '1'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}') {
        return false;
    }
    NUMBERED.iter().any(|name| {
        let mut want = name.chars();
        [a, b, c].iter().all(|ch| {
            want.next()
                .is_some_and(|expected| ch.eq_ignore_ascii_case(&expected))
        })
    })
}

/// Build a deterministic, legal 8.3 alias for a long upload filename.
///
/// FAT gives every long-named file a short alias, so one has to exist; it is
/// never shown to the user. `probe` is incremented only when the alias is
/// already occupied by another directory entry. The `.EPU` extension keeps
/// the file openable by the ordinary catalog scan if its long-name records
/// are ever damaged.
pub fn upload_short_alias(long_name: &str, probe: u16) -> UploadShortName {
    let mut hash = FNV_OFFSET;
    for byte in long_name
        .as_bytes()
        .iter()
        .copied()
        .chain(probe.to_le_bytes())
    {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    let mut out = UploadShortName::new();
    write!(out, "{hash:08X}.EPU").expect("8.3 alias always fits");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_device_names_are_guarded_before_any_suffix() {
        // The reservation attaches to the component before the first dot, so
        // an inner suffix must be kept rather than swallowed.
        assert_eq!(wireless_epub_filename(b"NUL.txt.epub"), "NUL_.txt.epub");
        assert_eq!(
            wireless_epub_filename("LPT\u{b3}.foo.epub".as_bytes()),
            "LPT\u{b3}_.foo.epub"
        );
        // Superscript port numbers name the same devices on Windows.
        assert_eq!(
            wireless_epub_filename("COM\u{b9}.epub".as_bytes()),
            "COM\u{b9}_.epub"
        );
    }

    #[test]
    fn reserved_dos_names_are_made_portable() {
        // Reserved even with an extension on Windows.
        assert_eq!(wireless_epub_filename(b"NUL.epub"), "NUL_.epub");
        assert_eq!(wireless_epub_filename(b"con.epub"), "con_.epub");
        assert_eq!(wireless_epub_filename(b"COM1.epub"), "COM1_.epub");
        assert_eq!(wireless_epub_filename(b"lpt9.epub"), "lpt9_.epub");
        // Not reserved: bare COM/LPT, a zero suffix, and ordinary names that
        // merely start the same way.
        assert_eq!(wireless_epub_filename(b"COM.epub"), "COM.epub");
        assert_eq!(wireless_epub_filename(b"COM0.epub"), "COM0.epub");
        assert_eq!(wireless_epub_filename(b"CON1.epub"), "CON1.epub");
        assert_eq!(wireless_epub_filename(b"Contact.epub"), "Contact.epub");
    }

    /// Upstream `9b0123d` ("Preserve UTF-8 in catalog labels") fixed a
    /// byte-wise stem prettifier that could split a multi-byte character.
    /// This tree derives labels through a char-wise implementation already;
    /// the test is ported so the property is pinned here rather than only
    /// upstream, and so a future rewrite cannot quietly reintroduce it.
    #[test]
    fn catalog_labels_preserve_utf8() {
        let mut label = String::<64>::new();
        derive_catalog_label(
            "/books/marigold_wireless_Caf\u{e9}_Test.epub",
            "X.EPU",
            &mut label,
        );
        assert_eq!(label, "Marigold Wireless Caf\u{e9} Test");

        label.clear();
        derive_catalog_label("/books/M\u{e4}rchen \u{1f600}.epub", "X.EPU", &mut label);
        assert_eq!(label, "M\u{e4}rchen \u{1f600}");

        // A stem long enough to hit the 64-byte cap mid-character must stop
        // on a boundary, not split one.
        label.clear();
        let mut long_path = String::<256>::new();
        let _ = long_path.push_str("/books/");
        for _ in 0..40 {
            let _ = long_path.push('\u{1f600}');
        }
        let _ = long_path.push_str(".epub");
        derive_catalog_label(long_path.as_str(), "X.EPU", &mut label);
        assert!(core::str::from_utf8(label.as_bytes()).is_ok());
    }

    /// The filename must be percent-decoded exactly once. `raw_query_name`
    /// decodes in place, so a helper that decodes again turns a literal `%`
    /// in a filename into a second round of escapes — and `%252F` into a
    /// path separator that silently truncates the name.
    #[test]
    fn upload_filename_decodes_exactly_once() {
        // `Novel%2FPart.epub` is the real filename; the browser escapes its
        // percent sign, so the query carries `Novel%252FPart.epub`.
        let mut path = *b"/upload?name=Novel%252FPart.epub";
        let decoded = raw_query_name(&mut path).expect("query name");
        assert_eq!(decoded, b"Novel%2FPart.epub");
        assert_eq!(
            wireless_epub_filename(decoded),
            "Novel%2FPart.epub",
            "a literal percent escape must not be decoded a second time"
        );
    }

    #[test]
    fn keeps_a_readable_epub_long_name() {
        assert_eq!(
            wireless_epub_filename(b"The Left Hand of Darkness.epub"),
            "The Left Hand of Darkness.epub"
        );
        assert_eq!(
            wireless_epub_filename("M\u{e4}rchen \u{1f600}.EPUB".as_bytes()),
            "M\u{e4}rchen \u{1f600}.epub"
        );
    }

    #[test]
    fn removes_paths_and_sanitizes_fat_characters() {
        assert_eq!(
            wireless_epub_filename(b"../unsafe:book?.epub"),
            "unsafe_book_.epub"
        );
        assert_eq!(
            wireless_epub_filename(br"C:\fakepath\Novel.zip"),
            "Novel.epub"
        );
        assert_eq!(wireless_epub_filename(b"..."), "Book.epub");
    }

    #[test]
    fn truncates_on_a_utf8_boundary_and_keeps_the_suffix() {
        let name = wireless_epub_filename(
            "\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}\u{1f600}.epub".as_bytes(),
        );
        assert!(name.len() <= UPLOAD_FILENAME_BYTES);
        assert!(name.ends_with(EPUB_SUFFIX));
        assert!(core::str::from_utf8(name.as_bytes()).is_ok());
    }

    #[test]
    fn aliases_are_legal_deterministic_and_probeable() {
        let first = upload_short_alias("A Book.epub", 0);
        assert_eq!(first, upload_short_alias("A Book.epub", 0));
        assert_ne!(first, upload_short_alias("A Book.epub", 1));
        assert_eq!(first.len(), 12);
        assert!(first.ends_with(".EPU"));
        assert!(first[..8].bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn test_sanitized_name() {
        let input1 = b"MyCoolBook.epub";
        let res1 = sanitized_name(input1);
        let res1_again = sanitized_name(input1);
        assert_eq!(res1, res1_again);
        assert!(res1.ends_with(".EPU"));

        let input2 = b"MyCoSecondBook.epub";
        let res2 = sanitized_name(input2);
        assert!(res2.ends_with(".EPU"));

        assert_eq!(&res1.as_str()[0..4], "MYCO");
        assert_eq!(&res2.as_str()[0..4], "MYCO");
        assert_ne!(res1, res2);

        let input_short = b"abc.epub";
        let res_short = sanitized_name(input_short);
        assert_eq!(&res_short.as_str()[0..4], "ABCX");
        assert!(res_short.ends_with(".EPU"));
        assert_eq!(res_short.len(), 12);

        let input_empty = b".epub";
        let res_empty = sanitized_name(input_empty);
        assert_eq!(&res_empty.as_str()[0..4], "BOOK");
        assert!(res_empty.ends_with(".EPU"));
        assert_eq!(res_empty.len(), 12);
    }

    #[test]
    fn test_derive_catalog_label_ambiguity() {
        // This test documents that `derive_catalog_label` is non-injective and
        // maps distinct original filenames to the exact same normalized catalog label.
        // Because of this ambiguity, we cannot safely migrate or overwrite a legacy book
        // based on a normalized label match alone.
        let mut label1 = String::<64>::new();
        derive_catalog_label("MyCoolBook-One.epub", "MYCOOLBO.EPU", &mut label1);

        let mut label2 = String::<64>::new();
        derive_catalog_label("MyCoolBook_One.epub", "MYCOOLBO.EPU", &mut label2);

        assert_eq!(label1.as_str(), "MyCoolBook One");
        assert_eq!(label1, label2);
    }

    #[test]
    fn pretty_file_stems_preserve_utf8() {
        let mut label = String::<64>::new();
        push_pretty_file_stem("calendula_wireless_Café_Test", &mut label);
        assert_eq!(label.as_str(), "Calendula Wireless Café Test");

        label.clear();
        push_pretty_file_stem("Märchen 😀", &mut label);
        assert_eq!(label.as_str(), "Märchen 😀");
    }

    #[test]
    fn pretty_file_stems_preserve_non_ascii_boundaries() {
        let mut label = String::<64>::new();
        push_pretty_file_stem("élan_café", &mut label);
        assert_eq!(label.as_str(), "élan Café");

        label.clear();
        push_pretty_file_stem("_leading_separator", &mut label);
        assert_eq!(label.as_str(), "Leading Separator");
    }

    #[test]
    fn pretty_file_stems_stop_at_the_label_budget() {
        // A stem of multi-byte characters can exhaust the 64-byte budget
        // mid-character; the push must fail rather than truncate one.
        let mut label = String::<64>::new();
        push_pretty_file_stem(&"é".repeat(40), &mut label);
        assert!(label.len() <= 64);
        assert!(label.chars().all(|ch| ch == 'é' || ch == 'É'));
    }

    #[test]
    fn test_parse_identity_read() {
        let buf = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];

        // An 8-byte file read in full is a valid identity.
        assert_eq!(
            parse_identity_read::<()>(8, Ok(8), &buf),
            Ok(Some(0x8877665544332211))
        );

        // A wrong-length sidecar is deterministically malformed: report "no
        // identity" so the probe skips the slot instead of aborting every
        // future upload whose probe window crosses it. The read result is
        // irrelevant — the file can't hold a valid hash.
        assert_eq!(parse_identity_read::<()>(0, Ok(0), &buf), Ok(None));
        assert_eq!(parse_identity_read::<()>(7, Ok(7), &buf), Ok(None));
        assert_eq!(parse_identity_read::<()>(9, Ok(8), &buf), Ok(None));
        assert_eq!(parse_identity_read::<()>(7, Err(()), &buf), Ok(None));

        // A short read or I/O error on a correctly-sized file is transient
        // card trouble: abort the upload so a retry can succeed.
        assert_eq!(parse_identity_read::<()>(8, Ok(7), &buf), Err(()));
        assert_eq!(parse_identity_read::<()>(8, Ok(0), &buf), Err(()));
        assert_eq!(parse_identity_read::<()>(8, Err(()), &buf), Err(()));
    }

    #[test]
    fn test_hash_identity() {
        let input1 = b"MyCoolBook.epub";
        let res1 = hash_identity(input1);
        let res1_again = hash_identity(input1);
        assert_eq!(res1, res1_again);

        let input2 = b"MyCoolBook_v2.epub";
        let res2 = hash_identity(input2);
        assert_ne!(res1, res2);

        // Files that collide in legacy 8.3 naming must have distinct identity hashes
        let collide1 = b"MyCoolBook One.epub";
        let collide2 = b"MyCoolBook Two.epub";
        assert_ne!(hash_identity(collide1), hash_identity(collide2));
    }

    #[test]
    fn test_percent_decode() {
        let mut buf1 = b"?name=My%20Cool%20Book.epub&other=1".to_vec();
        let name1 = raw_query_name(&mut buf1).unwrap();
        assert_eq!(name1, b"My Cool Book.epub");

        let mut buf2 = b"?name=A%26B%3D%2B%3F.epub".to_vec();
        let name2 = raw_query_name(&mut buf2).unwrap();
        assert_eq!(name2, b"A&B=+?.epub");

        let mut buf3 = b"other=1".to_vec();
        assert!(raw_query_name(&mut buf3).is_none());

        let mut buf4 = b"?name=&other=1".to_vec();
        assert!(raw_query_name(&mut buf4).is_none());
    }

    #[test]
    fn test_has_query_param() {
        assert!(has_query_param(b"?root=1", b"root=1"));
        assert!(has_query_param(b"?name=book.epu&root=1", b"root=1"));
        assert!(has_query_param(b"?root=1&name=book.epu", b"root=1"));

        // Encoded or embedded strings must not trigger the param
        assert!(!has_query_param(b"?name=root%3D1.epu", b"root=1"));
        assert!(!has_query_param(b"?name=root=1.epu", b"root=1"));
        assert!(!has_query_param(b"?root=2", b"root=1"));
        assert!(!has_query_param(b"/delete", b"root=1"));
    }

    #[test]
    fn query_param_reads_a_value_wherever_it_sits() {
        assert_eq!(
            query_param(b"/p?after_ms=250", b"after_ms"),
            Some(&b"250"[..])
        );
        assert_eq!(
            query_param(b"/p?seed=1&after_ms=40&tail=x", b"after_ms"),
            Some(&b"40"[..])
        );
        // Present but empty is a value, not an absence; parsing it is the
        // caller's job.
        assert_eq!(query_param(b"/p?after_ms=", b"after_ms"), Some(&b""[..]));
    }

    #[test]
    fn query_param_refuses_absent_and_partial_matches() {
        assert_eq!(query_param(b"/p", b"after_ms"), None);
        assert_eq!(query_param(b"/p?seed=1", b"after_ms"), None);
        // A parameter that merely ends in the name is a different parameter.
        assert_eq!(query_param(b"/p?reset_after_ms=250", b"after_ms"), None);
        // As is one that merely starts with it.
        assert_eq!(query_param(b"/p?after_ms_max=250", b"after_ms"), None);
        // A bare flag has no value to return, and must not stop the scan.
        assert_eq!(query_param(b"/p?after_ms", b"after_ms"), None);
        assert_eq!(
            query_param(b"/p?after_ms&after_ms=7", b"after_ms"),
            Some(&b"7"[..])
        );
    }
}
