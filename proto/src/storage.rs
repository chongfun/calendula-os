use crate::book::{BookId, BookMeta, BookSource, CoverStatus};
use heapless::String;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanRoot {
    BooksDir,
    CardRoot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileCandidate<'a> {
    pub root: ScanRoot,
    pub path: &'a str,
    pub byte_size: u32,
}

impl<'a> FileCandidate<'a> {
    pub fn as_book(self, id: BookId) -> Option<BookMeta<'a>> {
        if !is_epub_path(self.path) || is_hidden_entry(self.path) {
            return None;
        }
        let file_name = self.path.rsplit('/').next().unwrap_or(self.path);
        let title = file_name.strip_suffix(".epub").unwrap_or(file_name);
        Some(BookMeta {
            id,
            title,
            author: "Unknown Author",
            source_path: self.path,
            byte_size: self.byte_size,
            source: BookSource::MicroSd,
            cover_status: CoverStatus::Unknown,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageError {
    NoCard,
    UnsupportedFilesystem,
    Io,
    TooManyBooks,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReaderProgressRecord<'a> {
    pub book_path: &'a str,
    pub book_id: BookId,
    pub spine_index: u16,
    pub screen_index: u32,
    pub text_run_index: u16,
    pub text_byte_offset: u16,
    pub reading_orientation: u8,
    pub refresh_policy: u8,
}

pub trait BookStorage {
    fn scan_epubs(
        &mut self,
        on_candidate: impl FnMut(FileCandidate<'_>) -> Result<(), StorageError>,
    ) -> Result<(), StorageError>;

    fn read_at(&mut self, path: &str, offset: u32, out: &mut [u8]) -> Result<usize, StorageError>;
}

pub trait ProgressStorage {
    fn load_progress<'a>(
        &mut self,
        scratch: &'a mut [u8],
    ) -> Result<Option<ReaderProgressRecord<'a>>, StorageError>;

    fn store_progress(&mut self, record: ReaderProgressRecord<'_>) -> Result<(), StorageError>;
}

pub fn is_epub_path(path: &str) -> bool {
    // Uploads now create VFAT long names ending in ".epub", but releases
    // before that wrote only 8.3 ".epu" names, and sideloaded books use
    // either; accept both spellings everywhere EPUBs are discovered.
    if path.len() >= 4 {
        let tail = &path.as_bytes()[path.len() - 4..];
        if tail[0] == b'.'
            && tail[1].eq_ignore_ascii_case(&b'e')
            && tail[2].eq_ignore_ascii_case(&b'p')
            && tail[3].eq_ignore_ascii_case(&b'u')
        {
            return true;
        }
    }
    path.as_bytes()
        .windows(5)
        .last()
        .map(|suffix| suffix.eq_ignore_ascii_case(b".epub"))
        .unwrap_or(false)
}

/// Whether the entry is platform metadata rather than a book.
///
/// macOS writes an AppleDouble sidecar named `._<original>` beside every file
/// it copies to a FAT volume, holding the resource fork. The sidecar keeps the
/// `.epub` extension, so an extension test alone accepts it, and the card then
/// lists a phantom duplicate of every book which cannot open: the sidecar is
/// not an archive, so the reader fails with `Zip(MissingEndOfCentralDirectory)`
/// and nothing on screen points at the real cause. Every book copied from a
/// Mac in Finder has one.
///
/// The rule is any leading dot rather than `._` specifically, which also
/// covers `.DS_Store`-style clutter and anything else a platform hides. No
/// book wants a name starting with a dot.
///
/// Takes a name or a path: only the segment after the last `/` is examined, so
/// `/books/._x.epub` is rejected while `/.hidden/x.epub` -- a hidden directory
/// the scan was told to walk -- is not.
pub fn is_hidden_entry(path: &str) -> bool {
    path.rsplit('/').next().unwrap_or(path).starts_with('.')
}

/// How many UTF-8 bytes a FAT long filename can occupy, and so how large a
/// buffer the SD scan must lend `embedded-sdmmc` to assemble one.
///
/// FAT stores a long name as at most 255 UTF-16 code units. A unit that is
/// not part of a surrogate pair encodes to at most 3 UTF-8 bytes; a surrogate
/// pair spends two units on 4 bytes, which is cheaper per unit. So 3 bytes per
/// unit bounds it: 255 * 3.
///
/// The size matters because it is what keeps [`is_hidden_entry`] reachable
/// during a scan. A long name that does not fit is one the scan never sees:
/// `embedded-sdmmc` documents that such an entry is presented as if it had
/// only its short name, and at the pinned revision it reports an empty long
/// name instead. Either way the leading `._` is gone by the time the filter
/// runs, and `._<book>.epub` is left with a short name like `_BOOK~1.EPU`
/// that no rule can separate from a book legitimately starting with an
/// underscore. A buffer that cannot overflow removes the question.
pub const MAX_LFN_UTF8_BYTES: usize = 255 * 3;

/// The name a scanned directory entry belongs in the catalog under, or `None`
/// when the entry is not a book the library should list.
///
/// Takes the pair an `iterate_dir_lfn` callback receives: the entry's long
/// name when the scan could read one, and its 8.3 short name, which uploaded
/// books have as their only name. The long name wins when present, since it
/// is what the reader recognizes and the only spelling that still carries a
/// sidecar's leading dot.
pub fn catalog_scan_name<'a>(long_name: Option<&'a str>, short_name: &'a str) -> Option<&'a str> {
    let name = match long_name {
        // The entry has a long name and this scan did not get it: at the
        // pinned `embedded-sdmmc` revision an empty string is what a long
        // name too large for the buffer comes back as. Refuse the entry
        // rather than fall back to the short name, which cannot be
        // classified -- see `MAX_LFN_UTF8_BYTES`. Sizing the buffer to that
        // maximum makes this unreachable for any valid FAT name; it costs one
        // comparison not to depend on that being true.
        Some("") => return None,
        Some(name) => name,
        // No long name at all, which is an ordinary 8.3-only entry: books
        // uploaded before long-name support, and anything copied on as 8.3
        // from a computer. The short name is the whole name. A short name
        // cannot begin with a dot, so the hidden-entry test below never fires
        // on this branch -- it is applied uniformly rather than skipped,
        // because a rule that runs on one branch and not the other is the
        // shape this bug already took once.
        None => short_name,
    };
    (is_epub_path(name) && !is_hidden_entry(name)).then_some(name)
}

/// Store the catalog's display path in its fixed-size field. The FAT short
/// name remains the open handle; this only provides the user-facing label and
/// a stable cache identity.
pub fn catalog_display_path<const N: usize>(prefix: &str, name: &str, out: &mut String<N>) {
    out.clear();
    push_utf8_prefix(prefix, N, out);

    // Keep the EPUB suffix when a long FAT name needs trimming. The Library's
    // fallback label uses it to remove the extension, while the beginning of
    // the filename remains the most useful part for the reader.
    let suffix = if name.len() >= 5
        && name.as_bytes()[name.len() - 5..].eq_ignore_ascii_case(b".epub")
    {
        &name[name.len() - 5..]
    } else if name.len() >= 4 && name.as_bytes()[name.len() - 4..].eq_ignore_ascii_case(b".epu") {
        &name[name.len() - 4..]
    } else {
        ""
    };
    let stem = &name[..name.len() - suffix.len()];
    let stem_capacity = N.saturating_sub(out.len() + suffix.len());

    // A trimmed path must still name exactly one book. This string is not
    // only shown: `source_hash` and `cache_key_for` both hash it, so two
    // files whose names agree up to the trim and whose sizes match would
    // otherwise share a catalog identity *and* a cache. Uploads make that
    // reachable — they permit a 59-byte stem while `/books/` leaves 52 — so
    // a trim spends its last few bytes on a discriminator over the whole
    // name instead of more of a prefix the two already share.
    if out.len() + stem.len() + suffix.len() > N {
        let tag = discriminator(name);
        let kept = stem_capacity.saturating_sub(tag.len());
        push_utf8_prefix(stem, out.len() + kept, out);
        let _ = out.push_str(&tag);
    } else {
        push_utf8_prefix(stem, out.len() + stem_capacity, out);
    }
    let _ = out.push_str(suffix);
}

/// A short, filename-legal tag distinguishing names that share a trimmed
/// prefix: base-36 over an FNV-1a of the whole name.
///
/// Seven digits, because 36^7 exceeds `u32::MAX` and so carries the hash
/// whole. That is what keeps trimming from weakening identity: this string
/// feeds `source_hash` and `cache_key_for`, so a trimmed path must not
/// collide more readily than those 32-bit hashes do by themselves. Five
/// digits folded the hash into 36^5 and made trims collide roughly seventy
/// times sooner than the identity they feed — a real pair being
/// `A*46 + "000000007328"` and `A*46 + "000000085285"`, distinct names with
/// one display path.
///
/// Exactness is not on offer at this layer while identity is a 32-bit FNV;
/// content-addressed identity is the fix, and it belongs to the
/// user-managed-library milestone rather than here.
fn discriminator(name: &str) -> String<8> {
    const BASE36: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut hash = 0x811c_9dc5u32;
    for byte in name.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let mut out = String::new();
    let _ = out.push('~');
    let mut digits = [0u8; 7];
    for slot in digits.iter_mut().rev() {
        *slot = BASE36[(hash % 36) as usize];
        hash /= 36;
    }
    for digit in digits {
        let _ = out.push(digit as char);
    }
    out
}

fn push_utf8_prefix<const N: usize>(text: &str, end: usize, out: &mut String<N>) {
    for ch in text.chars() {
        if out.len() + ch.len_utf8() > end {
            break;
        }
        let _ = out.push(ch);
    }
}

#[cfg(test)]
mod tests {

    /// Two names that shared a display path under a five-digit tag. The tag
    /// feeds `source_hash` and `cache_key_for`, so an identical path here
    /// means one catalog identity and one cache for two books.
    #[test]
    fn trimmed_paths_survive_a_tag_that_would_have_folded() {
        let stem = "A".repeat(46);
        let mut first = String::<64>::new();
        let mut second = String::<64>::new();
        catalog_display_path(
            "/books/",
            &std::format!("{stem}000000007328.epub"),
            &mut first,
        );
        catalog_display_path(
            "/books/",
            &std::format!("{stem}000000085285.epub"),
            &mut second,
        );
        assert_ne!(
            first, second,
            "distinct names must not share a display path"
        );
        assert!(first.len() <= 64 && second.len() <= 64);
        assert!(first.ends_with(".epub") && second.ends_with(".epub"));
    }

    /// Uploads permit a 59-byte stem while `/books/` leaves 52, so two legal
    /// uploads can agree through the trim. The trimmed path is hashed for
    /// both catalog identity and cache key, so letting them collide would
    /// hand two books one cache.
    #[test]
    fn trimmed_paths_stay_distinct_for_distinct_names() {
        let shared = "A".repeat(52);
        let mut first = String::<64>::new();
        let mut second = String::<64>::new();
        catalog_display_path("/books/", &std::format!("{shared}1234567.epub"), &mut first);
        catalog_display_path(
            "/books/",
            &std::format!("{shared}7654321.epub"),
            &mut second,
        );
        assert_ne!(
            first, second,
            "names differing only past the trim must not share an identity"
        );
        assert!(first.len() <= 64 && second.len() <= 64);
        assert!(first.ends_with(".epub") && second.ends_with(".epub"));

        // A name that fits is untouched, so existing caches keep their keys.
        let mut short = String::<64>::new();
        catalog_display_path("/books/", "Novel.epub", &mut short);
        assert_eq!(short.as_str(), "/books/Novel.epub");
    }

    /// Long upload names made this path load-bearing: display names are now
    /// routinely multi-byte and long enough to trim, and a trim that lands
    /// mid-character would put invalid UTF-8 in every catalog record.
    #[test]
    fn display_paths_trim_on_character_boundaries() {
        let mut path = String::<64>::new();
        let mut name = String::<128>::new();
        for _ in 0..30 {
            let _ = name.push('\u{1f600}');
        }
        let _ = name.push_str(".epub");
        catalog_display_path("/books/", name.as_str(), &mut path);
        assert!(path.len() <= 64);
        assert!(core::str::from_utf8(path.as_bytes()).is_ok());

        let mut path = String::<64>::new();
        catalog_display_path("/books/", "M\u{e4}rchen.epub", &mut path);
        assert_eq!(path.as_str(), "/books/M\u{e4}rchen.epub");
    }

    extern crate std;

    use super::*;

    #[test]
    fn recognizes_epub_suffix_case_insensitively() {
        assert!(is_epub_path("/books/Alice.EPUB"));
        assert!(is_epub_path("book.epub"));
        assert!(!is_epub_path("book.epub.tmp"));
    }

    /// A Mac writes `._<name>` beside every file copied to the card, and the
    /// sidecar keeps the `.epub` extension. Catalogued, it becomes a phantom
    /// duplicate of a real book that can never open.
    #[test]
    fn appledouble_sidecars_are_not_books() {
        assert!(is_hidden_entry("._book.epub"));
        assert!(is_hidden_entry("/._book.epub"));
        assert!(is_hidden_entry("/books/._book.epub"));
        // The 8.3 spelling of the same sidecar, which uploads also produce.
        assert!(is_hidden_entry("/books/._book.epu"));
    }

    #[test]
    fn ordinary_books_are_not_hidden() {
        assert!(!is_hidden_entry("book.epub"));
        assert!(!is_hidden_entry("/books/book.epub"));
        // A dot inside the name, and a leading underscore, are both ordinary.
        assert!(!is_hidden_entry("/books/vol._2.epub"));
        assert!(!is_hidden_entry("/books/_book.epub"));
        // A hidden directory the scan was pointed at does not hide its books.
        assert!(!is_hidden_entry("/.stuff/book.epub"));
    }

    #[test]
    fn hidden_entries_are_rejected_as_book_candidates() {
        let sidecar = FileCandidate {
            root: ScanRoot::CardRoot,
            path: "/._algernon.epub",
            byte_size: 4096,
        };
        assert!(sidecar.as_book(BookId(1)).is_none());

        let real = FileCandidate {
            root: ScanRoot::CardRoot,
            path: "/algernon.epub",
            byte_size: 4096,
        };
        assert!(real.as_book(BookId(1)).is_some());
    }

    /// The scan's own decision, made on the pair its callback is handed.
    #[test]
    fn scan_catalogues_books_and_skips_sidecars() {
        // A long name is what the reader sees, and what carries the dot.
        assert_eq!(
            catalog_scan_name(Some("algernon.epub"), "ALGERN~1.EPU"),
            Some("algernon.epub")
        );
        assert_eq!(
            catalog_scan_name(Some("._algernon.epub"), "_ALGER~1.EPU"),
            None
        );
        assert_eq!(catalog_scan_name(Some("notes.txt"), "NOTES.TXT"), None);

        // An 8.3-only entry, which is what an upload writes: the short name
        // is the whole name, and a leading underscore is an ordinary book.
        assert_eq!(
            catalog_scan_name(None, "ALGERN~1.EPU"),
            Some("ALGERN~1.EPU")
        );
        assert_eq!(catalog_scan_name(None, "_BOOK~1.EPU"), Some("_BOOK~1.EPU"));
        assert_eq!(catalog_scan_name(None, "NOTES.TXT"), None);
    }

    /// A long name the scan could not read comes back empty, and the short
    /// name it would otherwise fall back to cannot be classified: the 8.3
    /// spelling of `._<book>.epub` has lost its dot and reads as a book. The
    /// entry is refused instead.
    #[test]
    fn unreadable_long_names_are_refused_rather_than_guessed() {
        assert_eq!(catalog_scan_name(Some(""), "_ALGER~1.EPU"), None);
        assert_eq!(catalog_scan_name(Some(""), "ALGERN~1.EPU"), None);
    }

    /// The buffer the scan lends the FAT layer holds any name FAT can store,
    /// so no real sidecar can reach the fallback above. A 255-code-unit name
    /// of 3-byte characters is the largest a long name gets.
    #[test]
    fn the_longest_fat_name_still_fits_the_scan_buffer() {
        let longest: std::string::String = core::iter::repeat_n('\u{4e00}', 255).collect();
        assert_eq!(longest.len(), MAX_LFN_UTF8_BYTES);

        // The same length, spelled as a sidecar: ASCII stem, `._` prefix,
        // `.epub` suffix, padded out past the buffer the scan used to lend.
        let mut sidecar = std::string::String::from("._");
        sidecar.extend(core::iter::repeat_n(
            'a',
            MAX_LFN_UTF8_BYTES - "._.epub".len(),
        ));
        sidecar.push_str(".epub");
        assert_eq!(sidecar.len(), MAX_LFN_UTF8_BYTES);
        assert!(sidecar.len() > 192, "must exceed the buffer this fixed");
        assert!(is_hidden_entry(&sidecar));
        assert_eq!(catalog_scan_name(Some(&sidecar), "_AAAAA~1.EPU"), None);
    }

    #[test]
    fn file_candidate_becomes_minimal_book_meta() {
        let candidate = FileCandidate {
            root: ScanRoot::BooksDir,
            path: "/books/algernon.epub",
            byte_size: 42,
        };

        let book = candidate.as_book(BookId(3)).expect("epub candidate");

        assert_eq!(book.title, "algernon");
        assert_eq!(book.source, BookSource::MicroSd);
        assert_eq!(book.byte_size, 42);
    }

    #[test]
    fn long_epub_names_do_not_collapse_to_the_root_path() {
        for name in [
            "L'Istituto per la Regolazione degli Orologi - Ahmet Hamdi Tanpinar_748.epub",
            "The Weird_ A Compendium of Stra - Jeff Vandermeer; Ann Vandermeer.epub",
        ] {
            let mut path = String::<64>::new();
            catalog_display_path("/", name, &mut path);

            assert_ne!(path.as_str(), "/");
            assert!(path.ends_with(".epub"));
            assert!(path.len() <= 64);
        }
    }
}
