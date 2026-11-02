//! Presentation keys for the catalog-backed library view.
//!
//! A label is never an address: the root and full locator remain part of the
//! key so two copies with the same title are independently ordered.

use core::cmp::Ordering;

use heapless::String;
use proto::library_path::BookRoot;

/// A book's display label, using its filename when no title has been stored.
/// A title longer than 64 bytes is cut at a character boundary.
pub fn display_label(title: &str, filename: &str) -> String<64> {
    let mut label = String::new();
    if title.is_empty() {
        proto::upload::derive_catalog_label(filename, &mut label);
    } else {
        // Catalog titles have already been decoded into a 64-byte string.
        // Iterating characters also keeps this helper safe for longer inputs.
        for ch in title.chars() {
            if label.push(ch).is_err() {
                break;
            }
        }
    }
    label
}

/// Borrowed fields needed to order one catalog book.
pub struct BookKey<'a> {
    pub label: &'a str,
    pub filename: &'a str,
    pub root: BookRoot,
    pub locator: &'a str,
}

fn compare_names(a: &str, b: &str) -> Ordering {
    a.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(b.bytes().map(|byte| byte.to_ascii_lowercase()))
        .then_with(|| a.cmp(b))
}

/// Compare presentation first, retaining deterministic physical tie-breakers.
/// Labels, then filenames, are compared with ASCII case folding and their
/// original bytes as a tie-breaker; root and locator break any remaining ties.
pub fn compare_books(a: &BookKey<'_>, b: &BookKey<'_>) -> Ordering {
    fn root_order(root: BookRoot) -> u8 {
        match root {
            BookRoot::Library => 0,
            BookRoot::CardRoot => 1,
        }
    }
    compare_names(a.label, b.label)
        .then_with(|| compare_names(a.filename, b.filename))
        .then_with(|| root_order(a.root).cmp(&root_order(b.root)))
        .then_with(|| a.locator.cmp(b.locator))
}

/// A candidate retained from a streamed catalog pass. The catalog row is a
/// hint for reading the record, not an identity or a filesystem address.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub label: String<64>,
    pub locator: String<{ proto::library_path::MAX_PATH_BYTES }>,
    pub root: BookRoot,
    pub catalog_row: u16,
    pub byte_size: u32,
}

impl Candidate {
    pub fn key(&self) -> BookKey<'_> {
        BookKey {
            label: self.label.as_str(),
            filename: self.locator.rsplit('/').next().unwrap_or(""),
            root: self.root,
            locator: self.locator.as_str(),
        }
    }

    /// `None` means the record cannot be addressed under a known root.
    pub fn from_record(record: &proto::catalog::CatalogRecord, catalog_row: u16) -> Option<Self> {
        let root = record.root?;
        let path = proto::library_path::LibraryPath::parse(record.path.as_str()).ok()?;
        if path.as_str() != record.path.as_str()
            || (root == BookRoot::CardRoot && !path.parent()?.is_root())
        {
            return None;
        }
        let filename = record
            .path
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())?;
        Some(Self {
            label: display_label(record.title.as_str(), filename),
            locator: record.path.clone(),
            root,
            catalog_row,
            byte_size: record.byte_size,
        })
    }
}

/// Catalog-wide order for the on-card index: exact parent path first, then
/// the display order inside it. Root-level books from both roots share the
/// empty parent, so they form one section. The parent is compared exactly:
/// case-distinct directories are different filesystem locations.
pub fn compare_catalog_candidates(a: &Candidate, b: &Candidate) -> Ordering {
    fn parent(path: &str) -> &str {
        path.rsplit_once('/').map_or("", |(parent, _)| parent)
    }
    parent(a.locator.as_str())
        .cmp(parent(b.locator.as_str()))
        .then_with(|| compare_books(&a.key(), &b.key()))
}

/// Which side of an exclusive cursor a streamed page selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Reverse,
}

/// An order-independent freshness hint, never proof of file identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Membership {
    count: u32,
    sum: u64,
    xor: u64,
}

impl Membership {
    pub fn add(&mut self, root: BookRoot, locator: &str, byte_size: u32) {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let root_byte = match root {
            BookRoot::Library => 0,
            BookRoot::CardRoot => 1,
        };
        for byte in core::iter::once(root_byte)
            .chain(locator.bytes())
            .chain(byte_size.to_le_bytes())
        {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
        }
        self.count = self.count.wrapping_add(1);
        self.sum = self.sum.wrapping_add(hash);
        self.xor ^= hash.rotate_left(23);
    }

    pub(crate) fn totals(self) -> (u64, u64) {
        (self.sum, self.xor)
    }

    pub(crate) fn from_totals(count: u32, sum: u64, xor: u64) -> Self {
        Self { count, sum, xor }
    }
}

/// Offer a row to a caller-owned bounded window, kept in ascending order.
/// Reverse selection keeps the greatest rows below the cursor; forward
/// selection keeps the least rows above it. No partial window is published by
/// this helper: the caller must finish and validate the whole catalog pass.
pub fn offer<const N: usize>(
    window: &mut heapless::Vec<Candidate, N>,
    candidate: Candidate,
    cursor: Option<&Candidate>,
    direction: Direction,
) {
    if let Some(cursor) = cursor {
        let ordering = compare_books(&candidate.key(), &cursor.key());
        if (direction == Direction::Forward && ordering != Ordering::Greater)
            || (direction == Direction::Reverse && ordering != Ordering::Less)
        {
            return;
        }
    }
    if N == 0 {
        return;
    }
    let index = window
        .partition_point(|entry| compare_books(&entry.key(), &candidate.key()) == Ordering::Less);
    let was_full = window.len() == N;
    if was_full {
        match direction {
            Direction::Forward if index == N => return,
            Direction::Forward => {
                window.pop();
            }
            Direction::Reverse if index == 0 => return,
            Direction::Reverse => {
                window.remove(0);
            }
        }
    }
    let index = if direction == Direction::Reverse && was_full {
        // A full reverse window just discarded its first entry.
        index - 1
    } else {
        index
    };
    let _ = window.insert(index, candidate);
}

/// Count a known book's position in a complete pass. The caller first finds
/// its exact root, locator, and size, then offers every book in the folder.
pub struct Rank<'a> {
    target: &'a Candidate,
    before: usize,
    matches: usize,
    key_changed: bool,
}

impl<'a> Rank<'a> {
    pub fn new(target: &'a Candidate) -> Self {
        Self {
            target,
            before: 0,
            matches: 0,
            key_changed: false,
        }
    }

    pub fn offer(&mut self, candidate: &Candidate) {
        let order = compare_books(&candidate.key(), &self.target.key());
        if order == Ordering::Less {
            self.before += 1;
        }
        if candidate.root == self.target.root
            && candidate.locator == self.target.locator
            && candidate.byte_size == self.target.byte_size
        {
            self.matches += 1;
            self.key_changed |= order != Ordering::Equal;
        }
    }

    /// Return the zero-based rank, or `None` if the physical address is absent,
    /// duplicated, or now has a different presentation key.
    pub fn finish(self) -> Option<usize> {
        (self.matches == 1 && !self.key_changed).then_some(self.before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key<'a>(label: &'a str, filename: &'a str, root: BookRoot, locator: &'a str) -> BookKey<'a> {
        BookKey {
            label,
            filename,
            root,
            locator,
        }
    }

    #[test]
    fn labels_use_titles_or_the_pretty_filename() {
        assert_eq!(display_label("Dune", "A_DUNE.epub"), "Dune");
        assert_eq!(display_label("", "B_HOBBIT.epub"), "B HOBBIT");
        let long = "é".repeat(33);
        assert_eq!(display_label(&long, "x.epub").as_str(), "é".repeat(32));
    }

    #[test]
    fn order_is_case_insensitive_and_breaks_ties_by_physical_address() {
        let library = BookRoot::Library;
        let card = BookRoot::CardRoot;
        let mut books = [
            key("Dune", "z.epub", card, "z.epub"),
            key("dune", "a.epub", library, "a.epub"),
            key("Dune", "z.epub", library, "shelf/z.epub"),
            key("A Christmas Carol", "c.epub", library, "c.epub"),
            key("Dune", "a.epub", library, "a.epub"),
        ];
        books.sort_by(compare_books);
        assert_eq!(
            books.map(|b| (b.label, b.filename, b.root)),
            [
                ("A Christmas Carol", "c.epub", library),
                ("Dune", "a.epub", library),
                ("Dune", "z.epub", library),
                ("Dune", "z.epub", card),
                ("dune", "a.epub", library),
            ]
        );
    }

    #[test]
    fn bounded_windows_page_forward_and_backward_with_exclusive_cursors() {
        fn candidate(n: u16) -> Candidate {
            let mut label = String::new();
            let mut locator = String::new();
            use core::fmt::Write;
            write!(label, "Title {n:02}").unwrap();
            write!(locator, "Book {n:02}.epub").unwrap();
            Candidate {
                label,
                locator,
                root: BookRoot::Library,
                catalog_row: n,
                byte_size: 1,
            }
        }
        let mut window = heapless::Vec::<Candidate, 4>::new();
        for n in (0..37).rev() {
            offer(&mut window, candidate(n), None, Direction::Forward);
        }
        assert_eq!(
            window
                .iter()
                .map(|c| c.catalog_row)
                .collect::<heapless::Vec<_, 4>>()
                .as_slice(),
            &[0, 1, 2, 3]
        );
        let target = candidate(28);
        let mut rank = Rank::new(&target);
        for n in (0..37).rev() {
            rank.offer(&candidate(n));
        }
        assert_eq!(rank.finish(), Some(28));
        let mut renamed = candidate(28);
        renamed.label = candidate(0).label;
        let mut moved = Rank::new(&renamed);
        for n in 0..37 {
            if n == 28 {
                moved.offer(&renamed);
            } else {
                moved.offer(&candidate(n));
            }
        }
        assert_eq!(moved.finish(), Some(1));
        let mut stale_target = Rank::new(&target);
        stale_target.offer(&renamed);
        assert_eq!(stale_target.finish(), None);
        let mut duplicate = Rank::new(&target);
        duplicate.offer(&target);
        duplicate.offer(&target);
        assert_eq!(duplicate.finish(), None);
        let cursor = window[3].clone();
        window.clear();
        for n in 0..37 {
            offer(&mut window, candidate(n), Some(&cursor), Direction::Forward);
        }
        assert_eq!(
            window
                .iter()
                .map(|c| c.catalog_row)
                .collect::<heapless::Vec<_, 4>>()
                .as_slice(),
            &[4, 5, 6, 7]
        );
        let cursor = window[0].clone();
        window.clear();
        for n in (0..37).rev() {
            offer(&mut window, candidate(n), Some(&cursor), Direction::Reverse);
        }
        assert_eq!(
            window
                .iter()
                .map(|c| c.catalog_row)
                .collect::<heapless::Vec<_, 4>>()
                .as_slice(),
            &[0, 1, 2, 3]
        );
    }

    #[test]
    fn membership_is_order_independent_and_includes_root_and_size() {
        let mut a = Membership::default();
        a.add(BookRoot::Library, "shelf/a.epub", 42);
        a.add(BookRoot::CardRoot, "b.epub", 9);
        let mut b = Membership::default();
        b.add(BookRoot::CardRoot, "b.epub", 9);
        b.add(BookRoot::Library, "shelf/a.epub", 42);
        assert_eq!(a, b);
        let mut changed = Membership::default();
        changed.add(BookRoot::Library, "b.epub", 9);
        changed.add(BookRoot::Library, "shelf/a.epub", 43);
        assert_ne!(a, changed);
    }
}
