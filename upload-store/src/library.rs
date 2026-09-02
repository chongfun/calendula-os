//! Resolving a [`LibraryPath`] to something the driver can open.
//!
//! `embedded-sdmmc` opens a name inside a directory it already has, so an
//! arbitrary long path has to be walked one component at a time: enumerate
//! with long names visible, match the component, take the 8.3 alias the entry
//! actually answers to, and descend through that. This is the one place that
//! does it, so callers hold paths rather than scattering LFN scans.
//!
//! Matching is exact, per [`LibraryPath`]: a component matches the displayed
//! long name, or the rendered alias text when there is no long name, byte
//! for byte. A card holding `Foo.epub` beside `foo.epub` holds two locators,
//! each opening its own entry.
//!
//! The one forgiving lookup left is the shelf: `/BOOKS` is a fixed product
//! name being discovered rather than a locator being resolved, and a
//! computer can legally leave it spelled `Books`. Plain ASCII case, owned
//! here, refusing ambiguity; see [`open_library_root`].

use core::fmt::Write as _;
use core::ops::ControlFlow;

use embedded_sdmmc::{Directory, TimeSource};
use proto::library_path::{BookRoot, LibraryPath};

use crate::install::InstallError;

/// Storage for one long name while a component is matched against it.
///
/// Matching is exact, so a name longer than the longest legal component can
/// equal no component and no listable child; the driver hands such a name
/// back empty, and the walk reads the entry as unmatchable, which under
/// exact semantics it is. The forgiving model needed four bytes per
/// component byte here, because a scalar can lowercase to a shorter one;
/// exactness retired that arithmetic with the rule that required it.
const LFN_SCAN_BYTES: usize = proto::library_path::MAX_COMPONENT_BYTES;

/// One entry, as the driver can reach it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The 8.3 alias, which is how it is opened. The driver's own value
    /// rather than a rendering of it: a short name is ISO-8859-1, so an alias
    /// of accented characters is wider as UTF-8 than as bytes on the card,
    /// and a buffer sized for the rendering is a buffer an entry can overflow
    /// and vanish through.
    pub alias: embedded_sdmmc::ShortFileName,
    pub is_dir: bool,
}

/// Which entry a component names, decided across a whole directory.
///
/// Lifted out of the walk because the driver refuses to create two names
/// differing only in case, and the test formatter reuses the first entry
/// instead. Such a directory comes from another operating system, which is
/// exactly the card this feature supports, so the rule is unit tested here;
/// the image tests forge such directories by rewriting LFN bytes.
#[derive(Default)]
struct Selector {
    exact: Option<Entry>,
    /// The case-equivalent directories, counted apart from the files:
    /// only a directory can be the shelf, so the shelf reading consults
    /// these and ignores the rest. See [`Selector::finish_for_shelf`].
    forgiving_dir: Option<Entry>,
    forgiving_dirs: usize,
    /// Whether an exact spelling settles the walk. For an ordinary locator
    /// it does: exact wins whatever else the directory holds. The shelf scan
    /// keeps walking past an exact non-directory, because a case-variant
    /// directory further on changes what that file means.
    settle_on_exact: bool,
}

impl Selector {
    /// For an ordinary locator component.
    fn locator() -> Self {
        Self {
            settle_on_exact: true,
            ..Self::default()
        }
    }

    /// For the fixed shelf name.
    fn shelf() -> Self {
        Self::default()
    }

    /// Offer one entry. `Break` means the answer is settled.
    fn offer(&mut self, long: Option<&str>, entry: Entry, component: &str) -> ControlFlow<()> {
        match long {
            Some(long) => {
                // The spelling the card holds, which is the spelling a listing
                // showed and a locator was built from. It wins; whether it
                // also ends the walk depends on who is asking.
                if long == component {
                    let settled = self.settle_on_exact || entry.is_dir;
                    self.exact = Some(entry);
                    return if settled {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    };
                }
                // The forgiving bucket serves only the shelf's fixed-name
                // discovery, so the rule is plain ASCII case, not the
                // driver's Unicode equivalence: `BOOKS` is ASCII, and a
                // durable locator never reads from this bucket.
                if !self.settle_on_exact && long.eq_ignore_ascii_case(component) && entry.is_dir {
                    // Only a directory can be the shelf, so only directories
                    // are kept: the readings that consult this bucket are
                    // about which directory, or how many.
                    self.forgiving_dirs += 1;
                    if self.forgiving_dir.is_none() {
                        self.forgiving_dir = Some(entry);
                    }
                }
            }
            None => {
                // A short-only entry's name is its rendered alias text, and
                // a locator built from a listing stores exactly that
                // rendering, so the match is exact here too. The driver
                // forgives ASCII case when opening by name; that is a lookup
                // convenience, not locator semantics.
                let mut rendered =
                    heapless::String::<{ proto::storage::MAX_ALIAS_UTF8_BYTES }>::new();
                if write!(rendered, "{}", entry.alias).is_ok() && rendered.as_str() == component {
                    let settled = self.settle_on_exact || entry.is_dir;
                    self.exact = Some(entry);
                    return if settled {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    };
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn finish(self) -> Lookup {
        // A locator resolves exactly or not at all; the forgiving bucket
        // belongs to the shelf's own reading, `finish_for_shelf`.
        match self.exact {
            Some(entry) => Lookup::Found(entry),
            None => Lookup::Missing,
        }
    }

    /// [`Selector::finish`], read the way the fixed shelf name must read it.
    /// Only a directory can be the shelf, so the entry's type joins the
    /// classification:
    ///
    /// - an exact directory is the shelf, whatever else the card holds;
    /// - an exact non-directory with no case-equivalent directory anywhere
    ///   is a card with no shelf, the documented file-squats-the-name case;
    /// - an exact non-directory beside a case-equivalent directory is
    ///   ambiguous: the directory may be the shelf, and reading the file as
    ///   "no shelf" would commit an empty catalog while books sit under it;
    /// - with no exact spelling, exactly one case-equivalent directory is
    ///   the shelf, and case-equivalent files do not compete, since a file
    ///   could not have been it;
    /// - two or more case-equivalent directories are a question nothing
    ///   here can answer.
    ///
    /// `Found` therefore always holds a directory.
    fn finish_for_shelf(self) -> Lookup {
        match self.exact {
            Some(entry) if entry.is_dir => Lookup::Found(entry),
            Some(_) => {
                if self.forgiving_dirs == 0 {
                    Lookup::Missing
                } else {
                    Lookup::Ambiguous
                }
            }
            None => match (self.forgiving_dirs, self.forgiving_dir) {
                (1, Some(entry)) => Lookup::Found(entry),
                (0, _) => Lookup::Missing,
                _ => Lookup::Ambiguous,
            },
        }
    }
}

/// Which entry a component names: present, absent, or claimed by more than
/// one case variant with no exact spelling.
///
/// Ambiguity is kept apart from absence because the right reading differs by
/// caller. An ordinary locator maps it to absence: picking one candidate
/// would open a book the reader did not choose, and a row that is not shown
/// cannot be pressed. The fixed product root cannot afford that reading,
/// because an absent shelf is what commits an empty catalog; see
/// [`open_library_root`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum Lookup {
    Missing,
    Found(Entry),
    Ambiguous,
}

impl Lookup {
    fn into_entry(self) -> Option<Entry> {
        match self {
            Lookup::Found(entry) => Some(entry),
            Lookup::Missing | Lookup::Ambiguous => None,
        }
    }
}

/// Walk `dir` once, offering every entry to `selector`.
fn scan_into<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    component: &str,
    selector: &mut Selector,
) -> Result<(), InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut storage = [0u8; LFN_SCAN_BYTES];
    let mut lfn = embedded_sdmmc::LfnBuffer::new(&mut storage);
    let walked = dir.iterate_dir_lfn(&mut lfn, |entry, long| {
        if entry.attributes.is_volume() {
            return ControlFlow::Continue(());
        }
        let alias = entry.name;
        selector.offer(
            long,
            Entry {
                alias,
                is_dir: entry.attributes.is_directory(),
            },
            component,
        )
    });
    if walked.is_err() {
        return Err(InstallError::Card);
    }
    Ok(())
}

/// Find one component inside an open directory, with the full three-way
/// classification.
fn lookup_in<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    component: &str,
) -> Result<Lookup, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut selector = Selector::locator();
    scan_into(dir, component, &mut selector)?;
    Ok(selector.finish())
}

/// Find the shelf inside the card root, with the type-aware reading.
fn shelf_lookup_in<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Lookup, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut selector = Selector::shelf();
    scan_into(dir, crate::SHELF_DIR, &mut selector)?;
    Ok(selector.finish_for_shelf())
}

/// Find one component inside an open directory.
///
/// `Ok(None)` is a name that is not there. An unreadable directory is `Err`,
/// because reading it as an absence would report a book as missing on a card
/// that merely would not answer.
///
/// A name claimed by several case variants and no exact spelling also reads
/// as an absence here, deliberately: picking one would open a book the
/// reader did not choose. The one caller for whom that reading is unsafe is
/// [`open_library_root`], which uses the richer lookup.
pub fn entry_in<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    component: &str,
) -> Result<Option<Entry>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    Ok(lookup_in(dir, component)?.into_entry())
}

/// Run `f` against the directory a path names, starting from the library
/// root.
///
/// A closure rather than a returned handle, because the root is borrowed
/// rather than owned and a walk of zero components has to hand back the root
/// itself. It also keeps the directory table shallow: each level is dropped
/// as the next opens.
///
/// `Ok(None)` is a component that is not there, or one that turned out to be
/// a file. Walking through a file is a caller's mistake rather than a card
/// fault, so it reads as an absence.
pub fn with_dir<D, T, R, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    path: &LibraryPath,
    f: impl FnOnce(&Directory<'_, D, T, MD, MF, MV>) -> R,
) -> Result<Option<R>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut here: Option<Directory<'_, D, T, MD, MF, MV>> = None;
    for component in path.components() {
        let dir = here.as_ref().unwrap_or(root);
        let Some(entry) = entry_in(dir, component)? else {
            return Ok(None);
        };
        if !entry.is_dir {
            return Ok(None);
        }
        let next = dir.open_dir(entry.alias).map_err(|_| InstallError::Card)?;
        here = Some(next);
    }
    Ok(Some(f(here.as_ref().unwrap_or(root))))
}

/// Run `f` against the directory holding a book, and the alias to open it by.
///
/// The parent comes with the alias because opening a file needs the directory
/// it lives in, and resolving the path again to get there would walk every
/// component twice.
///
/// `Ok(None)` is a path that does not lead to a file.
pub fn with_book<D, T, R, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    path: &LibraryPath,
    f: impl FnOnce(&Directory<'_, D, T, MD, MF, MV>, &embedded_sdmmc::ShortFileName) -> R,
) -> Result<Option<R>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let (Some(name), Some(parent)) = (path.file_name(), path.parent()) else {
        return Ok(None);
    };
    let found = with_dir(root, &parent, |dir| match entry_in(dir, name) {
        Ok(Some(entry)) if !entry.is_dir => Ok(Some(f(dir, &entry.alias))),
        Ok(_) => Ok(None),
        Err(error) => Err(error),
    })?;
    match found {
        Some(inner) => inner,
        None => Ok(None),
    }
}

/// Open the library root, resolved the way every other component is.
///
/// The library directory is an entry like any other: a card that spells it
/// with a long name gives it an alias that is not its name, so opening it by
/// name misses it. Every path that looks for the shelf comes here, so
/// scanning, recovery and opening agree about whether a card has one. The
/// answer is load-bearing, because a library read as absent is committed as
/// an empty catalog and the orphan sweep then reclaims the caches of every
/// book it left out.
///
/// `Ok(None)` is a card with no library: only loose EPUBs, or a lone file
/// sitting where the library should be. `Err` is a card that would not
/// answer, which must not be recorded as the same thing. A contested shelf
/// name is `Err` too ([`InstallError::Ambiguous`]): several case-variant
/// directories with no exact spelling, or an exact file beside a
/// case-variant directory. A plausible shelf is not zero shelves.
pub fn open_library_root<'a, D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'a, D, T, MD, MF, MV>,
) -> Result<Option<Directory<'a, D, T, MD, MF, MV>>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let entry = match shelf_lookup_in(root)? {
        Lookup::Found(entry) => entry,
        Lookup::Missing => return Ok(None),
        // Case variants of the shelf with no exact spelling, or an exact
        // non-directory squatting on the name beside a case-variant
        // directory. A computer can legally leave either behind. An
        // ordinary locator reads ambiguity as absence, but an absent shelf
        // is what commits an empty catalog and lets the orphan sweep
        // reclaim the caches of every book the scan left out. A card
        // holding a plausible shelf is not a card holding none, so it is
        // refused until a computer settles which directory is the shelf.
        Lookup::Ambiguous => return Err(InstallError::Ambiguous),
    };
    // `Found` from the shelf lookup is a directory by construction; the
    // file-squats-the-name case is classified inside it, as `Missing`.
    match root.open_dir(entry.alias) {
        Ok(library) => Ok(Some(library)),
        // Resolved a moment ago, so this is the card changing under the walk
        // rather than an absence.
        Err(_) => Err(InstallError::Card),
    }
}

/// Run `f` against the book a location names, opening the root it is
/// relative to.
///
/// The card root is the one place a book can sit outside the library, and it
/// is spelled as a root rather than as a first component so a locator keeps
/// meaning one thing. `root` is the card's root directory, since that is the
/// only handle a caller can have before either root is opened.
pub fn with_book_at<D, T, R, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    at: BookRoot,
    path: &LibraryPath,
    f: impl FnOnce(&Directory<'_, D, T, MD, MF, MV>, &embedded_sdmmc::ShortFileName) -> R,
) -> Result<Option<R>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match at {
        BookRoot::CardRoot => with_book(root, path, f),
        BookRoot::Library => match open_library_root(root)? {
            Some(library) => with_book(&library, path, f),
            None => Ok(None),
        },
    }
}

/// One child of a directory, as browsing shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Child {
    /// What to display, and what a locator component holds: the long name when
    /// the entry has one, the alias otherwise.
    pub name: heapless::String<{ proto::library_path::MAX_COMPONENT_BYTES }>,
    /// The 8.3 alias, which is how the driver opens it.
    pub alias: embedded_sdmmc::ShortFileName,
    /// Whether `name` is the entry's long name. A short-only entry shows its
    /// alias, and the two forms are matched by different rules, so anything
    /// comparing names later has to know which it holds.
    pub long_name: bool,
    pub is_dir: bool,
    /// Bytes, from the directory entry. Zero for a directory.
    pub size: u32,
}

/// Hand every book and folder in a directory to `on_child`, in the order the
/// card stores them.
///
/// Order is the card's and sorting is the caller's, because sorting needs
/// storage proportional to the folder and this collects nothing.
///
/// Left out: anything that is not a directory or an EPUB, and anything
/// starting with a dot, both by the rules the catalog scan uses so the two
/// agree about what a book is. Also anything with no locator from here, a
/// name the driver could not decode or one the depth and length limits
/// cannot fit, since a book that cannot be named cannot be opened.
///
/// `Ok(None)` is a path that is not a directory. `Err` is a card that would
/// not answer.
pub fn for_each_child<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    path: &LibraryPath,
    mut on_child: impl FnMut(&Child) -> ControlFlow<()>,
) -> Result<Option<()>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let listed = with_dir(root, path, |dir| -> Result<(), InstallError> {
        let mut storage = [0u8; LFN_SCAN_BYTES];
        let mut lfn = embedded_sdmmc::LfnBuffer::new(&mut storage);
        let walked = dir.iterate_dir_lfn(&mut lfn, |entry, long| {
            if entry.attributes.is_volume() {
                return ControlFlow::Continue(());
            }
            let alias = entry.name;
            // A short-only entry shows its alias, so it is rendered here.
            // The buffer is sized so it cannot overflow, since an alias that
            // did not fit would take its book out of the listing.
            let mut rendered = heapless::String::<{ proto::storage::MAX_ALIAS_UTF8_BYTES }>::new();
            if write!(rendered, "{alias}").is_err() {
                return ControlFlow::Continue(());
            }
            let is_dir = entry.attributes.is_directory();
            // An entry with a long name is found by that name, so one this
            // build could not decode has no locator, whatever its alias says.
            let shown = match long {
                Some("") => return ControlFlow::Continue(()),
                Some(long) => long,
                None => rendered.as_str(),
            };
            if proto::storage::is_hidden_entry(shown) {
                return ControlFlow::Continue(());
            }
            if !is_dir && !proto::storage::is_epub_path(shown) {
                return ControlFlow::Continue(());
            }
            // The whole locator, not just this component. A name can fit
            // comfortably and still have no path from here: the folder may
            // already be at the depth limit, or long enough that one more
            // component overruns a serialized locator. Either way the row
            // would do nothing when pressed.
            if path.child(shown).is_err() {
                return ControlFlow::Continue(());
            }
            let mut name = heapless::String::new();
            if name.push_str(shown).is_err() {
                return ControlFlow::Continue(());
            }
            let child = Child {
                name,
                alias,
                long_name: long.is_some(),
                is_dir,
                size: if is_dir { 0 } else { entry.size },
            };
            on_child(&child)
        });
        // A caller's stop is not a failure: the driver leaves the loop and
        // reports success, so an error here is the card.
        if walked.is_err() {
            return Err(InstallError::Card);
        }
        Ok(())
    })?;
    match listed {
        Some(inner) => inner.map(Some),
        None => Ok(None),
    }
}

impl Default for Child {
    /// An unfilled slot in a caller's window. The alias is the one short name
    /// that means "this directory", since a short name has no empty value and
    /// a slot past `filled` names nothing.
    fn default() -> Self {
        Self {
            name: heapless::String::new(),
            alias: embedded_sdmmc::ShortFileName::this_dir(),
            long_name: false,
            is_dir: false,
            size: 0,
        }
    }
}

/// Hand every book in a directory to `on_book`, in the order the card
/// stores them, each with its full locator built on `path`.
///
/// The same filters as [`for_each_child`]: dot-led names, non-EPUBs, names
/// the driver could not decode, and any child whose whole locator would be
/// illegal are all left out, so the catalog can only ever hold what
/// browsing can reach.
fn visit_books_in<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    path: &LibraryPath,
    on_book: &mut impl FnMut(&LibraryPath, &embedded_sdmmc::ShortFileName, u32),
) -> Result<(), InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut storage = [0u8; LFN_SCAN_BYTES];
    let mut lfn = embedded_sdmmc::LfnBuffer::new(&mut storage);
    let walked = dir.iterate_dir_lfn(&mut lfn, |entry, long| {
        if entry.attributes.is_directory() || entry.attributes.is_volume() {
            return ControlFlow::Continue(());
        }
        let mut rendered = heapless::String::<{ proto::storage::MAX_ALIAS_UTF8_BYTES }>::new();
        if write!(rendered, "{}", entry.name).is_err() {
            return ControlFlow::Continue(());
        }
        let Some(shown) = proto::storage::catalog_scan_name(long, rendered.as_str()) else {
            return ControlFlow::Continue(());
        };
        let Ok(locator) = path.child(shown) else {
            return ControlFlow::Continue(());
        };
        on_book(&locator, &entry.name, entry.size);
        ControlFlow::Continue(())
    });
    walked.map_err(|_| InstallError::Card)
}

/// The `n`th subfolder of a directory the walk may descend into, as its
/// component and its alias.
///
/// A subfolder qualifies by the listing's own rules: not dot-led, which
/// also covers the `.` and `..` entries every FAT subdirectory carries, its
/// name decoded and small enough to be a component, and its whole locator
/// legal. It must also sit above the depth floor: a folder at the maximum
/// depth is itself addressable, but nothing inside it can be, so the walk
/// has no business going in.
fn nth_walkable_subdir<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    path: &LibraryPath,
    n: usize,
) -> Result<
    Option<(
        heapless::String<{ proto::library_path::MAX_COMPONENT_BYTES }>,
        embedded_sdmmc::ShortFileName,
    )>,
    InstallError,
>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut storage = [0u8; LFN_SCAN_BYTES];
    let mut lfn = embedded_sdmmc::LfnBuffer::new(&mut storage);
    let mut seen: usize = 0;
    let mut found = None;
    let walked = dir.iterate_dir_lfn(&mut lfn, |entry, long| {
        if !entry.attributes.is_directory() || entry.attributes.is_volume() {
            return ControlFlow::Continue(());
        }
        let mut rendered = heapless::String::<{ proto::storage::MAX_ALIAS_UTF8_BYTES }>::new();
        if write!(rendered, "{}", entry.name).is_err() {
            return ControlFlow::Continue(());
        }
        let shown = match long {
            // A folder whose long name this build could not decode has no
            // component, so nothing below it has a locator: skip the
            // subtree.
            Some("") => return ControlFlow::Continue(()),
            Some(long) => long,
            None => rendered.as_str(),
        };
        if proto::storage::is_hidden_entry(shown) {
            return ControlFlow::Continue(());
        }
        let Ok(child) = path.child(shown) else {
            return ControlFlow::Continue(());
        };
        if child.depth() >= proto::library_path::MAX_DEPTH {
            return ControlFlow::Continue(());
        }
        if seen == n {
            let mut component = heapless::String::new();
            if component.push_str(shown).is_err() {
                return ControlFlow::Continue(());
            }
            found = Some((component, entry.name));
            return ControlFlow::Break(());
        }
        seen += 1;
        ControlFlow::Continue(())
    });
    if walked.is_err() {
        return Err(InstallError::Card);
    }
    Ok(found)
}

/// Hand every book below the library root to `on_book`, depth first, a
/// directory's files before its subfolders' contents, in the order the card
/// stores them.
///
/// One directory handle walks the whole tree, descending through a child's
/// alias and ascending through the `..` entry every FAT subdirectory
/// carries, so the walk holds one slot at any depth and keeps no per-level
/// storage. Finding the next subfolder re-iterates the current directory, so
/// one with `s` subfolders is read `s + 1` times: bounded extra block reads
/// for a flat memory footprint and no recursion.
///
/// Deterministic for an unchanged card, which the scan's walk fingerprint
/// depends on.
///
/// `library` is consumed: descending mutates the handle, and handing it
/// back mid-tree would be handing back an arbitrary subfolder.
///
/// `Err` is a card that would not answer, anywhere in the tree: a scan must
/// not commit a catalog missing whatever went unread.
pub fn for_each_book_depth_first<D, T, const MD: usize, const MF: usize, const MV: usize>(
    mut library: Directory<'_, D, T, MD, MF, MV>,
    on_book: &mut impl FnMut(&LibraryPath, &embedded_sdmmc::ShortFileName, u32),
) -> Result<(), InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut path = LibraryPath::root();
    // The subfolder ordinal to hunt next, per level below the root. A book
    // sits at most at MAX_DEPTH, so a descended-into folder sits at most at
    // MAX_DEPTH - 1 and the root's slot makes the array whole.
    //
    // Counted in `usize` rather than something sized to what a directory
    // ought to hold: nothing here bounds a directory's subfolder count, and
    // an ordinal that wrapped would send the walk back to the first child
    // and around that directory forever. Eight words of stack, against a
    // 128-byte name buffer and a 256-byte path in the same walk.
    let mut next_child = [0usize; proto::library_path::MAX_DEPTH];
    let mut level: usize = 0;
    visit_books_in(&library, &path, on_book)?;
    loop {
        match nth_walkable_subdir(&library, &path, next_child[level])? {
            Some((component, alias)) => {
                next_child[level] += 1;
                let child = path.child(component.as_str()).map_err(|_| {
                    // `nth_walkable_subdir` proved this legal a moment ago,
                    // so failing here is the card changing under the walk.
                    InstallError::Card
                })?;
                library.change_dir(alias).map_err(|_| InstallError::Card)?;
                path = child;
                level += 1;
                next_child[level] = 0;
                visit_books_in(&library, &path, on_book)?;
            }
            None => {
                if level == 0 {
                    return Ok(());
                }
                library
                    .change_dir(embedded_sdmmc::ShortFileName::parent_dir())
                    .map_err(|_| InstallError::Card)?;
                let Some(parent) = path.parent() else {
                    return Err(InstallError::Card);
                };
                path = parent;
                level -= 1;
            }
        }
    }
}

/// Which of a folder's children a listing is asking about.
///
/// A screen orders the two apart, so it also counts and pages them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Book,
    Folder,
}

impl Kind {
    /// Whether `child` is of this kind.
    const fn holds(self, child: &Child) -> bool {
        match self {
            Self::Book => !child.is_dir,
            Self::Folder => child.is_dir,
        }
    }
}

/// How many books and how many folders a directory shows, in that order.
///
/// Counts by walking, since the answer is what [`for_each_child`] would hand
/// over and no total is stored anywhere. Both come from one walk: a caller
/// showing books above folders needs the split to know which row is which,
/// and asking twice would read the directory twice to learn one number.
pub fn count_children_split<D, T, const MD: usize, const MF: usize, const MV: usize>(
    library: &Directory<'_, D, T, MD, MF, MV>,
    path: &LibraryPath,
) -> Result<Option<(usize, usize)>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut books = 0usize;
    let mut folders = 0usize;
    let listed = for_each_child(library, path, |child| {
        if child.is_dir {
            folders += 1;
        } else {
            books += 1;
        }
        ControlFlow::Continue(())
    })?;
    Ok(listed.map(|()| (books, folders)))
}

/// How many books and folders a directory shows, together.
pub fn count_children<D, T, const MD: usize, const MF: usize, const MV: usize>(
    library: &Directory<'_, D, T, MD, MF, MV>,
    path: &LibraryPath,
) -> Result<Option<usize>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    Ok(count_children_split(library, path)?.map(|(books, folders)| books + folders))
}

/// One row of the Library listing: a child, and which root its locator is
/// relative to.
///
/// The root travels with the row because the library root's own listing shows
/// two of them at once. A locator alone cannot say which, by design: it is
/// relative to a root rather than absolute, so the pair is the address.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibraryRow {
    pub child: Child,
    pub at: BookRoot,
}

/// How many rows of each kind a library listing shows, in the order it shows
/// them: the shelf's books, then the card root's, then the shelf's folders.
///
/// Card-root books appear only in the library root's own listing. Nothing
/// nests at the card root by contract, so there is no folder there to descend
/// into and no deeper listing that could hold one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowCounts {
    pub shelf_books: usize,
    pub root_books: usize,
    pub shelf_folders: usize,
}

impl RowCounts {
    pub const fn total(self) -> usize {
        self.shelf_books + self.root_books + self.shelf_folders
    }

    /// Rows below this are books, rows from here on are folders.
    pub const fn books(self) -> usize {
        self.shelf_books + self.root_books
    }
}

/// Count the card-root books, which are the library's oldest half: loose
/// EPUBs copied on before the shelf existed. Folders there are not counted,
/// because nothing nests at the card root and the catalog scan does not look.
fn count_root_books<D, T, const MD: usize, const MF: usize, const MV: usize>(
    card_root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<usize, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut books = 0usize;
    let listed = for_each_child(card_root, &LibraryPath::root(), |child| {
        if !child.is_dir {
            books += 1;
        }
        ControlFlow::Continue(())
    })?;
    Ok(listed.map_or(0, |()| books))
}

/// How many rows the Library screen shows for `path`, split by region.
///
/// Takes the card's own root and opens the shelf itself, so a caller cannot
/// hand a library-root-relative locator to the wrong directory. That is the
/// whole reason this exists rather than the caller composing the pieces: a
/// locator says which root it belongs to only by being paired with one.
///
/// `Ok(None)` is a path that is not a directory under the shelf. A card with
/// no shelf at all still has a library, made of whatever sits loose at its
/// root.
pub fn count_library_rows<D, T, const MD: usize, const MF: usize, const MV: usize>(
    card_root: &Directory<'_, D, T, MD, MF, MV>,
    path: &LibraryPath,
) -> Result<Option<RowCounts>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let root_books = if path.is_root() {
        count_root_books(card_root)?
    } else {
        0
    };
    let Some(shelf) = open_library_root(card_root)? else {
        return Ok(path.is_root().then_some(RowCounts {
            shelf_books: 0,
            root_books,
            shelf_folders: 0,
        }));
    };
    let Some((shelf_books, shelf_folders)) = count_children_split(&shelf, path)? else {
        return Ok(None);
    };
    Ok(Some(RowCounts {
        shelf_books,
        root_books,
        shelf_folders,
    }))
}

/// Fill what is left of `window` from one region, and say how many landed.
fn fill_region<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    path: &LibraryPath,
    kind: Kind,
    at: BookRoot,
    skip: usize,
    window: &mut [LibraryRow],
    filled: &mut usize,
) -> Result<Option<()>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut seen = 0usize;
    for_each_child(dir, path, |child| {
        if !kind.holds(child) {
            return ControlFlow::Continue(());
        }
        if seen >= skip && *filled < window.len() {
            window[*filled].child = child.clone();
            window[*filled].at = at;
            *filled += 1;
        }
        seen += 1;
        if *filled == window.len() {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    })
}

/// Fill `window` with the Library rows after `skip`, and say how many landed.
///
/// The order the screen shows: the shelf's books, then the card root's loose
/// ones, then the shelf's folders. A reader who made no folders sees the list
/// they always saw, and one who did sees their books above the folders they
/// made.
///
/// The catalog's order is its own, card root first. The two may differ
/// because a row resolves to a book by where it is rather than by position.
///
/// `counts` comes from [`count_library_rows`], read once on entering a
/// folder rather than per page, and tells a row number which region it is
/// in. A stale count shows a list shifted by the drift, which the next
/// listing corrects; it cannot name a child that is not there, since every
/// row comes from a walk taken now.
///
/// One walk per region the window reaches, which is the price of ordering
/// regions apart without storage proportional to the folder.
pub fn page_library_rows<D, T, const MD: usize, const MF: usize, const MV: usize>(
    card_root: &Directory<'_, D, T, MD, MF, MV>,
    path: &LibraryPath,
    counts: RowCounts,
    skip: usize,
    window: &mut [LibraryRow],
) -> Result<Option<usize>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let shelf = open_library_root(card_root)?;
    let mut filled = 0usize;
    // The shelf's books, then the card root's, then the shelf's folders. Each
    // region takes whatever of the skip it covers; what is left over belongs
    // to the next.
    let mut at = skip;
    for (region, kind, root) in [
        (counts.shelf_books, Kind::Book, BookRoot::Library),
        (counts.root_books, Kind::Book, BookRoot::CardRoot),
        (counts.shelf_folders, Kind::Folder, BookRoot::Library),
    ] {
        if filled == window.len() {
            break;
        }
        if at >= region {
            at -= region;
            continue;
        }
        let listed = match root {
            BookRoot::CardRoot => fill_region(
                card_root,
                &LibraryPath::root(),
                kind,
                root,
                at,
                window,
                &mut filled,
            )?,
            BookRoot::Library => match shelf.as_ref() {
                Some(shelf) => fill_region(shelf, path, kind, root, at, window, &mut filled)?,
                None => Some(()),
            },
        };
        if listed.is_none() {
            return Ok(None);
        }
        at = 0;
    }
    Ok(Some(filled))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(alias: &str, is_dir: bool) -> Entry {
        Entry {
            alias: embedded_sdmmc::ShortFileName::create_from_str(alias).expect("a short name"),
            is_dir,
        }
    }

    /// Feed a directory's entries to the selector and take the raw
    /// three-way classification, the way an ordinary locator scans.
    fn classify(entries: &[(Option<&str>, &str)], component: &str) -> Lookup {
        let mut selector = Selector::locator();
        for (long, alias) in entries {
            if selector
                .offer(*long, entry(alias, false), component)
                .is_break()
            {
                break;
            }
        }
        selector.finish()
    }

    /// Feed a shelf scan, entries carrying their type, and take the
    /// type-aware classification.
    fn classify_shelf(entries: &[(Option<&str>, &str, bool)]) -> Lookup {
        let mut selector = Selector::shelf();
        for (long, alias, is_dir) in entries {
            if selector
                .offer(*long, entry(alias, *is_dir), "BOOKS")
                .is_break()
            {
                break;
            }
        }
        selector.finish_for_shelf()
    }

    /// [`classify`], read the way an ordinary locator reads it.
    fn resolve(entries: &[(Option<&str>, &str)], component: &str) -> Option<Entry> {
        classify(entries, component).into_entry()
    }

    #[test]
    fn the_cards_own_spelling_wins_over_one_that_differs_in_case() {
        // A directory another operating system wrote, holding both.
        let card = [
            (Some("Foo.epub"), "FOO~1.EPU"),
            (Some("foo.epub"), "FOO~2.EPU"),
        ];

        assert_eq!(
            resolve(&card, "foo.epub").map(|e| e.alias),
            Some(entry("FOO~2.EPU", false).alias),
            "the reader chose the row spelled this way",
        );
        assert_eq!(
            resolve(&card, "Foo.epub").map(|e| e.alias),
            Some(entry("FOO~1.EPU", false).alias),
        );
    }

    #[test]
    fn an_exact_match_later_in_the_directory_still_wins() {
        let card = [
            (Some("foo.epub"), "FOO~1.EPU"),
            (Some("FOO.EPUB"), "FOO~2.EPU"),
        ];

        assert_eq!(
            resolve(&card, "FOO.EPUB").map(|e| e.alias),
            Some(entry("FOO~2.EPU", false).alias),
            "order of entries cannot decide which book opens",
        );
    }

    #[test]
    fn a_case_variant_is_a_different_locator() {
        let card = [(Some("Dune.epub"), "DUNE~1.EPU")];

        assert!(
            resolve(&card, "DUNE.EPUB").is_none(),
            "a locator names the entry it was obtained from, exactly",
        );
        assert!(resolve(&card, "Dune.epub").is_some());
    }

    #[test]
    fn case_variant_twins_each_resolve_to_their_own_entry() {
        let card = [
            (Some("Foo.epub"), "FOO~1.EPU"),
            (Some("foo.epub"), "FOO~2.EPU"),
        ];

        assert_eq!(
            resolve(&card, "Foo.epub").map(|e| e.alias),
            Some(entry("FOO~1.EPU", false).alias),
        );
        assert_eq!(
            resolve(&card, "foo.epub").map(|e| e.alias),
            Some(entry("FOO~2.EPU", false).alias),
        );
        assert_eq!(
            resolve(&card, "FOO.EPUB"),
            None,
            "a spelling the card does not hold names nothing",
        );
    }

    /// A locator reads a case variant as absence, full stop; the shelf's own
    /// reading is where variants and their ambiguity mean something, and it
    /// is tested through `classify_shelf` below.
    #[test]
    fn a_locator_never_reads_from_the_forgiving_bucket() {
        let card = [(Some("Books"), "BOOKS~1"), (Some("books"), "BOOKS~2")];

        assert_eq!(classify(&card, "BOOKS"), Lookup::Missing);
        assert_eq!(classify(&[], "BOOKS"), Lookup::Missing);
        assert_eq!(
            classify_shelf(&[(Some("Books"), "BOOKS~1", true)]),
            Lookup::Found(entry("BOOKS~1", true)),
            "one case variant alone is still the shelf",
        );
    }

    /// The shelf reading consults the entry's type, because only a
    /// directory can be the shelf. An exact file must not end the scan and
    /// hide a case-variant directory behind it, and case-variant files must
    /// not manufacture ambiguity they cannot be party to.
    #[test]
    fn the_shelf_reading_is_type_aware() {
        // An exact directory is the shelf, whatever else the card holds.
        assert!(matches!(
            classify_shelf(&[
                (Some("books"), "BOOKS~1", true),
                (Some("BOOKS"), "BOOKS~2", true),
            ]),
            Lookup::Found(Entry { is_dir: true, .. })
        ));
        // A lone exact file squats the name: no shelf.
        assert_eq!(
            classify_shelf(&[(Some("BOOKS"), "BOOKS~1", false)]),
            Lookup::Missing
        );
        // An exact file beside a case-variant directory is a question, not
        // an absence: the directory may be the shelf.
        assert_eq!(
            classify_shelf(&[
                (Some("BOOKS"), "BOOKS~1", false),
                (Some("books"), "BOOKS~2", true),
            ]),
            Lookup::Ambiguous
        );
        // Order cannot decide it: the directory first, the exact file after.
        assert_eq!(
            classify_shelf(&[
                (Some("books"), "BOOKS~1", true),
                (Some("BOOKS"), "BOOKS~2", false),
            ]),
            Lookup::Ambiguous
        );
        // A case-variant file does not compete with a case-variant
        // directory: the file could not have been the shelf.
        assert!(matches!(
            classify_shelf(&[
                (Some("Books"), "BOOKS~1", true),
                (Some("books"), "BOOKS~2", false),
            ]),
            Lookup::Found(Entry { is_dir: true, .. })
        ));
        // An exact file beside only case-variant files is still no shelf.
        assert_eq!(
            classify_shelf(&[
                (Some("BOOKS"), "BOOKS~1", false),
                (Some("books"), "BOOKS~2", false),
            ]),
            Lookup::Missing
        );
        // Two case-variant directories stay ambiguous.
        assert_eq!(
            classify_shelf(&[
                (Some("Books"), "BOOKS~1", true),
                (Some("books"), "BOOKS~2", true),
            ]),
            Lookup::Ambiguous
        );
    }

    #[test]
    fn a_short_only_entry_is_matched_by_its_rendered_text_exactly() {
        let card = [(None, "SHORT.EPU")];

        assert!(resolve(&card, "SHORT.EPU").is_some());
        assert!(
            resolve(&card, "short.epu").is_none(),
            "the rendering is uppercase, and a locator stores the rendering",
        );
        assert!(
            resolve(&card, "Short.epub").is_none(),
            "a long name is not this entry's name",
        );
    }
}
