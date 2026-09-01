//! Resolving locators against a real FAT image.
//!
//! The path rules themselves are unit tested in `proto::library_path`. What
//! needs a card is the walk: long names, mixed long and 8.3 components, and
//! the ways a path can fail to name a file.

use std::cell::RefCell;
use std::rc::Rc;

use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx, Mode, TimeSource, Timestamp};
use embedded_sdmmc::{Directory, VolumeIdx, VolumeManager};
use proto::library_path::{BookRoot, LibraryPath};
use upload_store::library::{
    count_children, entry_in, for_each_child, open_library_root, page_children, with_book,
    with_book_at, with_dir,
};

const BLOCK_BYTES: usize = 512;
// Large enough that fatfs picks FAT16, which the driver supports.
const DISK_BLOCKS: u32 = 32 * 1024;
const PART_START_BLOCK: u32 = 64;

struct RamDisk {
    data: RefCell<Vec<u8>>,
    /// Reads fail from this one onward, so a directory walk can fail the way
    /// a card does rather than the way a missing name does.
    fail_reads_from: RefCell<Option<u32>>,
    reads_seen: RefCell<u32>,
}

#[derive(Debug)]
struct DiskError;

impl core::fmt::Display for DiskError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "disk error")
    }
}

impl std::error::Error for DiskError {}

#[derive(Clone)]
struct SharedDisk(Rc<RamDisk>);

impl BlockDevice for SharedDisk {
    type Error = DiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), DiskError> {
        {
            let mut seen = self.0.reads_seen.borrow_mut();
            *seen += 1;
            if self
                .0
                .fail_reads_from
                .borrow()
                .is_some_and(|at| *seen >= at)
            {
                return Err(DiskError);
            }
        }
        let data = self.0.data.borrow();
        for (i, block) in blocks.iter_mut().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            block.copy_from_slice(&data[at..at + BLOCK_BYTES]);
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), DiskError> {
        let mut data = self.0.data.borrow_mut();
        for (i, block) in blocks.iter().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            data[at..at + BLOCK_BYTES].copy_from_slice(&block[..]);
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, DiskError> {
        Ok(BlockCount(DISK_BLOCKS))
    }
}

struct StaticTime;

impl TimeSource for StaticTime {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 55,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

type Mgr = VolumeManager<SharedDisk, StaticTime, 8, 8, 1>;
type Dir<'a> = Directory<'a, SharedDisk, StaticTime, 8, 8, 1>;

fn format_disk() -> Vec<u8> {
    let mut data = vec![0u8; DISK_BLOCKS as usize * BLOCK_BYTES];
    data[446 + 4] = 0x06;
    data[446 + 8..446 + 12].copy_from_slice(&PART_START_BLOCK.to_le_bytes());
    let sectors = DISK_BLOCKS - PART_START_BLOCK;
    data[446 + 12..446 + 16].copy_from_slice(&sectors.to_le_bytes());
    data[510] = 0x55;
    data[511] = 0xAA;
    let part_start = PART_START_BLOCK as usize * BLOCK_BYTES;
    let part_len = sectors as usize * BLOCK_BYTES;
    let cursor = std::io::Cursor::new(&mut data[part_start..part_start + part_len]);
    fatfs::format_volume(cursor, fatfs::FormatVolumeOptions::new()).expect("format");
    data
}

fn new_card() -> SharedDisk {
    SharedDisk(Rc::new(RamDisk {
        data: RefCell::new(format_disk()),
        fail_reads_from: RefCell::new(None),
        reads_seen: RefCell::new(0),
    }))
}

impl SharedDisk {
    /// Refuse reads from the `n`th from now on.
    fn fail_reads_from(&self, n: Option<u32>) {
        *self.0.reads_seen.borrow_mut() = 0;
        *self.0.fail_reads_from.borrow_mut() = n;
    }

    /// Start counting reads again, so a walk can be measured on its own.
    fn reset_reads(&self) {
        *self.0.reads_seen.borrow_mut() = 0;
    }

    fn reads(&self) -> u32 {
        *self.0.reads_seen.borrow()
    }
}

fn open_mgr(disk: SharedDisk) -> Mgr {
    VolumeManager::new_with_limits(disk, StaticTime, 7000)
}

fn open_root(mgr: &Mgr) -> Dir<'_> {
    let volume = mgr.open_volume(VolumeIdx(0)).expect("volume");
    let raw = volume.to_raw_volume();
    let raw_root = mgr.open_root_dir(raw).expect("root");
    Directory::new(raw_root, mgr)
}

/// Descend by long name, since making a directory hands back nothing to
/// descend through.
fn child<'a>(dir: &Dir<'a>, name: &str) -> Dir<'a> {
    let entry = entry_in(dir, name).expect("read").expect("present");
    dir.open_dir(entry.alias).expect("open")
}

/// Build `Fiction/Long Folder Name/Dune.epub` plus an 8.3-only book.
fn seed(root: &Dir<'_>) {
    root.make_dir_in_dir_lfn("Fiction").expect("mkdir");
    let fiction = child(root, "Fiction");
    fiction
        .make_dir_in_dir_lfn("Long Folder Name")
        .expect("mkdir");
    let nested = child(&fiction, "Long Folder Name");
    let book = nested.create_file_in_dir_lfn("Dune.epub").expect("create");
    book.write(b"dune").expect("write");
    book.close().expect("close");

    let short = fiction
        .open_file_in_dir("SHORT.EPU", Mode::ReadWriteCreate)
        .expect("create");
    short.write(b"short").expect("write");
    short.close().expect("close");
}

fn path(text: &str) -> LibraryPath {
    LibraryPath::parse(text).expect("parse")
}

#[test]
fn a_nested_book_resolves_through_long_components() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed(&root);

    let read = with_book(
        &root,
        &path("Fiction/Long Folder Name/Dune.epub"),
        |dir, alias| {
            let file = dir.open_file_in_dir(alias, Mode::ReadOnly).expect("open");
            let mut buf = [0u8; 8];
            let n = file.read(&mut buf).expect("read");
            file.close().expect("close");
            buf[..n].to_vec()
        },
    )
    .expect("walk")
    .expect("the book is there");

    assert_eq!(read, b"dune".to_vec());
}

#[test]
fn a_book_with_no_long_name_is_found_under_its_alias() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed(&root);

    assert!(with_book(&root, &path("Fiction/SHORT.EPU"), |_, _| ())
        .expect("walk")
        .is_some());
}

#[test]
fn a_case_variant_locator_does_not_resolve() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed(&root);

    assert!(
        with_book(
            &root,
            &path("FICTION/long folder name/DUNE.EPUB"),
            |_, _| ()
        )
        .expect("walk")
        .is_none(),
        "a locator names the spelling it was obtained from, exactly",
    );
    assert!(
        with_book(
            &root,
            &path("Fiction/Long Folder Name/Dune.epub"),
            |_, _| ()
        )
        .expect("walk")
        .is_some(),
        "and that spelling still resolves",
    );
}

/// The exact-locator property, on the pathological card that motivates it:
/// two entries whose names differ only by case are two locators, and each
/// opens its own bytes. The driver refuses to create the pair, so the image
/// is forged.
#[test]
fn case_variant_entries_each_resolve_to_their_own_bytes() {
    let disk = new_card();
    {
        let mgr = open_mgr(disk.clone());
        let root = open_root(&mgr);
        let mixed = root.create_file_in_dir_lfn("DuneA.epub").expect("create");
        mixed.write(b"mixed").expect("write");
        mixed.close().expect("close");
        let upper = root.create_file_in_dir_lfn("DUNEB.EPUB").expect("create");
        upper.write(b"upper").expect("write");
        upper.close().expect("close");
    }
    {
        let mut data = disk.0.data.borrow_mut();
        rewrite_long_name(&mut data, "DuneA.epub", "Dune.epub");
        rewrite_long_name(&mut data, "DUNEB.EPUB", "DUNE.EPUB");
    }
    let mgr = open_mgr(disk);
    let root = open_root(&mgr);

    let read_at = |locator: &str| {
        with_book(&root, &path(locator), |dir, alias| {
            let file = dir.open_file_in_dir(alias, Mode::ReadOnly).expect("open");
            let mut buf = [0u8; 8];
            let n = file.read(&mut buf).expect("read");
            buf[..n].to_vec()
        })
        .expect("walk")
        .expect("the locator resolves")
    };
    assert_eq!(read_at("Dune.epub"), b"mixed".to_vec());
    assert_eq!(read_at("DUNE.EPUB"), b"upper".to_vec());
}

#[test]
fn the_root_path_resolves_to_the_root() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed(&root);

    let found = with_dir(&root, &LibraryPath::root(), |dir| {
        entry_in(dir, "Fiction").expect("read").is_some()
    })
    .expect("walk")
    .expect("the root is always there");

    assert!(found, "a zero-component walk hands back the root itself");
}

#[test]
fn a_missing_component_is_an_absence() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed(&root);

    assert!(with_dir(&root, &path("Nonfiction"), |_| ())
        .expect("walk")
        .is_none());
    assert!(with_book(
        &root,
        &path("Fiction/Long Folder Name/Missing.epub"),
        |_, _| ()
    )
    .expect("walk")
    .is_none());
}

#[test]
fn a_file_where_a_directory_was_expected_is_an_absence() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed(&root);

    assert!(
        with_dir(&root, &path("Fiction/SHORT.EPU"), |_| ())
            .expect("walk")
            .is_none(),
        "walking through a file finds nothing rather than failing the card",
    );
}

#[test]
fn a_directory_is_not_a_book() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed(&root);

    assert!(with_book(&root, &path("Fiction"), |_, _| ())
        .expect("walk")
        .is_none());
}

/// Two names the filesystem keeps apart stay apart here, and each locator
/// opens its own book.
///
/// `\u{130}` lowercases to `i` plus a combining dot, so comparing whole
/// lowercased strings would call these one name and hand back whichever entry
/// the walk met first.
#[test]
fn names_that_differ_only_by_a_lowercase_expansion_are_two_books() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);

    for (name, body) in [
        ("\u{130}.epub", &b"dotted"[..]),
        ("i\u{307}.epub", &b"expanded"[..]),
    ] {
        let file = root.create_file_in_dir_lfn(name).expect("create");
        file.write(body).expect("write");
        file.close().expect("close");
    }

    for (name, expected) in [
        ("\u{130}.epub", &b"dotted"[..]),
        ("i\u{307}.epub", &b"expanded"[..]),
    ] {
        let read = with_book(&root, &path(name), |dir, alias| {
            let file = dir.open_file_in_dir(alias, Mode::ReadOnly).expect("open");
            let mut buf = [0u8; 16];
            let n = file.read(&mut buf).expect("read");
            file.close().expect("close");
            buf[..n].to_vec()
        })
        .expect("walk")
        .expect("present");

        assert_eq!(read, expected.to_vec(), "{name} opened the wrong book");
    }
}

/// A card that will not answer is a card fault, not a missing book. Later
/// reconciliation treats those very differently: one is a book to look for,
/// the other is a card to wait on.
#[test]
fn a_directory_that_cannot_be_read_is_an_error() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let root = open_root(&mgr);
    seed(&root);

    // Everything up to here is cached or already read; fail the next read so
    // the enumeration itself is what breaks.
    disk.fail_reads_from(Some(1));
    let outcome = entry_in(&root, "Fiction");

    assert!(
        matches!(outcome, Err(upload_store::install::InstallError::Card)),
        "an unreadable directory reported {outcome:?}, which a caller would \
         read as the book being gone",
    );
}

/// Two short-name entries the driver keeps apart stay apart here.
///
/// It builds a short name with `to_ascii_uppercase` over ISO-8859-1 and then
/// matches the bytes, so `\u{dc}.EPU` and `\u{fc}.EPU` are two files. The
/// long-name rule would call them one, and open whichever came first.
#[test]
fn short_names_differing_beyond_ascii_case_are_two_books() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);

    for (name, body) in [("\u{dc}.EPU", &b"upper"[..]), ("\u{fc}.EPU", &b"lower"[..])] {
        let file = root
            .open_file_in_dir(name, Mode::ReadWriteCreate)
            .expect("create");
        file.write(body).expect("write");
        file.close().expect("close");
    }

    for (name, expected) in [("\u{dc}.EPU", &b"upper"[..]), ("\u{fc}.EPU", &b"lower"[..])] {
        let read = with_book(&root, &path(name), |dir, alias| {
            let file = dir.open_file_in_dir(alias, Mode::ReadOnly).expect("open");
            let mut buf = [0u8; 16];
            let n = file.read(&mut buf).expect("read");
            file.close().expect("close");
            buf[..n].to_vec()
        })
        .expect("walk")
        .expect("present");

        assert_eq!(read, expected.to_vec(), "{name} opened the wrong book");
    }
}

/// A short-only entry's locator is its rendered alias text, exactly. The
/// driver would forgive ASCII case on an open-by-name; a locator does not,
/// because a locator stores the rendering a listing produced.
#[test]
fn a_short_only_locator_is_the_rendered_alias_exactly() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed(&root);

    assert!(with_book(&root, &path("Fiction/short.epu"), |_, _| ())
        .expect("walk")
        .is_none());
    assert!(with_book(&root, &path("Fiction/SHORT.EPU"), |_, _| ())
        .expect("walk")
        .is_some());
}

/// A name larger than any legal component cannot be named by a locator at
/// all under exact matching, so the walk reads it as absent rather than
/// resolving it through a case-folded shorthand. `\u{212a}` lowercases to
/// `k`, which the forgiving model once used to open this book through a
/// 128-byte locator; the exact model has no such shorthand, and that is the
/// point.
#[test]
fn a_name_too_large_for_a_component_is_absent_not_reachable() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);

    let on_card: String = "\u{212a}".repeat(123) + ".epub";
    let shorthand: String = "k".repeat(123) + ".epub";
    assert!(on_card.len() > 256, "far larger than any component");

    let file = root.create_file_in_dir_lfn(&on_card).expect("create");
    file.write(b"kelvin").expect("write");
    file.close().expect("close");

    assert!(
        with_book(&root, &path(&shorthand), |_, _| ())
            .expect("walk")
            .is_none(),
        "a case-folded shorthand is not the entry's name",
    );
}

/// Build a folder holding one of everything browsing has an opinion about.
fn seed_mixed(root: &Dir<'_>) {
    root.make_dir_in_dir_lfn("Fiction").expect("mkdir");
    let fiction = child(root, "Fiction");
    fiction.make_dir_in_dir_lfn("Space Opera").expect("mkdir");

    for name in [
        "Dune.epub",
        "Old Book.EPU",
        "notes.txt",
        "._Dune.epub",
        ".DS_Store",
    ] {
        let file = fiction.create_file_in_dir_lfn(name).expect("create");
        file.write(b"x").expect("write");
        file.close().expect("close");
    }
}

fn names_in(root: &Dir<'_>, at: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for_each_child(root, &path(at), |child| {
        out.push((child.name.as_str().to_string(), child.is_dir));
        core::ops::ControlFlow::Continue(())
    })
    .expect("walk")
    .expect("a directory");
    out
}

#[test]
fn a_folder_shows_its_books_and_folders_and_nothing_else() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed_mixed(&root);

    let mut listed = names_in(&root, "Fiction");
    listed.sort();

    assert_eq!(
        listed,
        vec![
            ("Dune.epub".to_string(), false),
            ("Old Book.EPU".to_string(), false),
            ("Space Opera".to_string(), true),
        ],
        "notes.txt is not a book, and the dot names are a Mac's leavings",
    );
}

#[test]
fn long_names_are_shown_rather_than_aliases() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed_mixed(&root);

    let listed = names_in(&root, "Fiction");
    let opera = listed
        .iter()
        .find(|(name, _)| name == "Space Opera")
        .expect("the folder is listed");
    assert!(opera.1, "and it is a folder");
}

#[test]
fn a_child_says_which_form_its_name_came_from() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed(&root);

    let mut forms = Vec::new();
    for_each_child(&root, &path("Fiction"), |child| {
        forms.push((child.name.as_str().to_string(), child.long_name));
        core::ops::ControlFlow::Continue(())
    })
    .expect("listing")
    .expect("the folder is there");
    forms.sort();

    // SHORT.EPU was written without a long name, so its row shows an alias.
    // Whoever compares these names later matches the two forms by different
    // rules, and cannot tell them apart from the text alone.
    assert_eq!(
        forms,
        vec![
            ("Long Folder Name".to_string(), true),
            ("SHORT.EPU".to_string(), false),
        ],
    );
}

#[test]
fn a_listed_name_is_a_locator_that_resolves() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed_mixed(&root);

    for (name, is_dir) in names_in(&root, "Fiction") {
        let child_path = path("Fiction").child(&name).expect("component fits");
        let found = if is_dir {
            with_dir(&root, &child_path, |_| ())
                .expect("walk")
                .is_some()
        } else {
            with_book(&root, &child_path, |_, _| ())
                .expect("walk")
                .is_some()
        };
        assert!(found, "{name} was listed and then could not be opened");
    }
}

#[test]
fn an_empty_folder_is_empty_rather_than_absent() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    root.make_dir_in_dir_lfn("Empty").expect("mkdir");

    assert_eq!(
        count_children(&root, &path("Empty")).expect("walk"),
        Some(0)
    );
}

#[test]
fn listing_a_book_is_an_absence() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed_mixed(&root);

    assert_eq!(
        count_children(&root, &path("Fiction/Dune.epub")).expect("walk"),
        None,
        "a book is not a folder, which is an absence rather than a fault",
    );
}

#[test]
fn a_folder_that_cannot_be_read_is_an_error() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let root = open_root(&mgr);
    seed_mixed(&root);

    disk.fail_reads_from(Some(1));
    assert!(matches!(
        count_children(&root, &path("Fiction")),
        Err(upload_store::install::InstallError::Card)
    ));
}

#[test]
fn a_window_walks_a_large_folder_a_page_at_a_time() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    root.make_dir_in_dir_lfn("Many").expect("mkdir");
    let many = child(&root, "Many");
    for i in 0..100 {
        let file = many
            .create_file_in_dir_lfn(&format!("Book {i:03}.epub"))
            .expect("create");
        file.close().expect("close");
    }

    assert_eq!(
        count_children(&root, &path("Many")).expect("walk"),
        Some(100)
    );

    let mut seen: Vec<String> = Vec::new();
    let mut window = vec![upload_store::library::Child::default(); 8];
    let mut skip = 0;
    loop {
        let filled = page_children(&root, &path("Many"), skip, &mut window)
            .expect("walk")
            .expect("a directory");
        if filled == 0 {
            break;
        }
        seen.extend(window[..filled].iter().map(|c| c.name.as_str().to_string()));
        skip += filled;
    }

    seen.sort();
    assert_eq!(seen.len(), 100, "every book was handed over exactly once");
    assert_eq!(seen.first().map(String::as_str), Some("Book 000.epub"));
    assert_eq!(seen.last().map(String::as_str), Some("Book 099.epub"));
}

/// A folder at the depth limit lists nothing, because nothing inside it has a
/// locator. The books are on the card; what is missing is a way to name them.
#[test]
fn a_folder_at_the_depth_limit_lists_no_children() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);

    let depth = proto::library_path::MAX_DEPTH;
    let mut parts: Vec<String> = Vec::new();
    // Built from the root each time, since a directory handle cannot be
    // cloned and the walk is cheap at this depth.
    root.make_dir_in_dir_lfn("Level0").expect("mkdir");
    parts.push("Level0".to_string());
    let mut here = child(&root, "Level0");
    for level in 1..depth {
        let name = format!("Level{level}");
        here.make_dir_in_dir_lfn(&name).expect("mkdir");
        here = child(&here, &name);
        parts.push(name);
    }
    let book = here.create_file_in_dir_lfn("Deep.epub").expect("create");
    book.write(b"deep").expect("write");
    book.close().expect("close");

    let deepest = path(&parts.join("/"));
    assert_eq!(deepest.depth(), depth, "the folder itself is nameable");
    assert_eq!(
        count_children(&root, &deepest).expect("walk"),
        Some(0),
        "one component further has no locator, so the book is not offered",
    );
}

/// A component can fit while the path to it does not.
#[test]
fn a_child_is_skipped_when_the_whole_locator_would_be_too_long() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);

    let first = "x".repeat(120);
    let second = "y".repeat(120);
    root.make_dir_in_dir_lfn(&first).expect("mkdir");
    let outer = child(&root, &first);
    outer.make_dir_in_dir_lfn(&second).expect("mkdir");
    let inner = child(&outer, &second);

    let book_name = "z".repeat(100) + ".epub";
    assert!(
        book_name.len() <= proto::library_path::MAX_COMPONENT_BYTES,
        "the component itself is legal",
    );
    let book = inner.create_file_in_dir_lfn(&book_name).expect("create");
    book.close().expect("close");

    let folder = path(&format!("{first}/{second}"));
    assert!(
        folder.child(&book_name).is_err(),
        "and the whole locator is not",
    );
    assert_eq!(
        count_children(&root, &folder).expect("walk"),
        Some(0),
        "so it is not offered as a row that could not be opened",
    );
}

/// The library root and the card root are two roots, not one path with a
/// prefix, so the same locator names a different book under each.
#[test]
fn a_locator_names_a_book_under_the_root_it_belongs_to() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed_two_roots(&root);

    let shelved = read_book_at(&root, BookRoot::Library, "Dune.epub");
    let loose = read_book_at(&root, BookRoot::CardRoot, "Dune.epub");

    assert_eq!(shelved.as_deref(), Some(&b"shelved"[..]));
    assert_eq!(loose.as_deref(), Some(&b"loose"[..]));
}

/// A locator spends none of its depth naming the library directory, so the
/// library's own name is not a component and does not resolve as one.
#[test]
fn the_library_directory_is_not_a_component_of_its_own_locators() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed_two_roots(&root);

    assert_eq!(
        read_book_at(&root, BookRoot::Library, "BOOKS/Dune.epub"),
        None,
        "that would be /BOOKS/BOOKS/Dune.epub"
    );
}

#[test]
fn a_card_with_no_library_holds_no_shelved_book() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    let file = root.create_file_in_dir_lfn("Dune.epub").expect("create");
    file.write(b"loose").expect("write");
    file.close().expect("close");

    // Absent is not a card fault: it is what a card holding only loose EPUBs
    // looks like.
    assert_eq!(read_book_at(&root, BookRoot::Library, "Dune.epub"), None);
    assert_eq!(
        read_book_at(&root, BookRoot::CardRoot, "Dune.epub").as_deref(),
        Some(&b"loose"[..])
    );
}

fn seed_two_roots(root: &Dir<'_>) {
    root.make_dir_in_dir_lfn("BOOKS").expect("mkdir");
    let books = child(root, "BOOKS");
    let shelved = books.create_file_in_dir_lfn("Dune.epub").expect("create");
    shelved.write(b"shelved").expect("write");
    shelved.close().expect("close");

    let loose = root.create_file_in_dir_lfn("Dune.epub").expect("create");
    loose.write(b"loose").expect("write");
    loose.close().expect("close");
}

fn read_book_at(root: &Dir<'_>, at: BookRoot, locator: &str) -> Option<Vec<u8>> {
    with_book_at(root, at, &path(locator), |dir, alias| {
        let file = dir.open_file_in_dir(alias, Mode::ReadOnly).expect("open");
        let mut buf = [0u8; 32];
        let read = file.read(&mut buf).expect("read");
        buf[..read].to_vec()
    })
    .expect("walk")
}

/// A card whose library directory carries a long name sits under an alias
/// that is not its name, which is what `make_dir_in_dir_lfn` writes and what
/// opening it by name misses.
#[test]
fn the_library_root_is_found_under_an_alias_that_is_not_its_name() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    seed_two_roots(&root);

    let alias = entry_in(&root, "BOOKS")
        .expect("read")
        .expect("present")
        .alias;
    assert_ne!(
        alias,
        embedded_sdmmc::ShortFileName::create_from_str("BOOKS").expect("a short name"),
        "the fixture has to be the awkward card, or it proves nothing"
    );
    assert!(
        root.open_dir("BOOKS").is_err(),
        "and opening by name misses"
    );

    assert!(open_library_root(&root).expect("read").is_some());
}

/// The scan's absence answer decides whether a catalog is committed without
/// the shelved books, so a card that would not answer must not look empty.
#[test]
fn a_library_root_that_cannot_be_read_is_an_error_not_an_absence() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let root = open_root(&mgr);
    // Enough entries that iterating the root spans more than the block the
    // seeding left cached, so the lookup has to go back to the card.
    for index in 0..48 {
        let mut name = std::string::String::from("Filler ");
        name.push_str(&index.to_string());
        name.push_str(".epub");
        let file = root.create_file_in_dir_lfn(&name).expect("create");
        file.close().expect("close");
    }
    seed_two_roots(&root);

    disk.fail_reads_from(Some(1));

    assert!(
        open_library_root(&root).is_err(),
        "a card that would not answer must not read as a card with no library"
    );
}

#[test]
fn a_card_with_no_library_root_says_so_rather_than_failing() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    let file = root.create_file_in_dir_lfn("Dune.epub").expect("create");
    file.write(b"loose").expect("write");
    file.close().expect("close");

    assert!(open_library_root(&root).expect("read").is_none());
}

/// A file where the library should be is not a library, and reads as one
/// being absent rather than as a card fault.
#[test]
fn a_file_named_like_the_library_root_is_not_one() {
    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    let file = root.create_file_in_dir_lfn("BOOKS").expect("create");
    file.write(b"x").expect("write");
    file.close().expect("close");

    assert!(open_library_root(&root).expect("read").is_none());
}

/// The first page of a large folder stops when the window is full, which is
/// the whole reason paging exists rather than walking a folder into a
/// caller's buffer. A name-only assertion cannot see the difference: a walk
/// that read every entry and threw most away would list the same eight.
///
/// Both walks start from a fresh volume so neither is measured through the
/// other's cached blocks.
#[test]
fn a_first_page_stops_reading_once_its_window_is_full() {
    let disk = new_card();
    {
        let mgr = open_mgr(disk.clone());
        let root = open_root(&mgr);
        seed_many(&root, 100);
    }

    let paging = {
        let mgr = open_mgr(disk.clone());
        let root = open_root(&mgr);
        let mut window = vec![upload_store::library::Child::default(); 8];
        disk.reset_reads();
        let filled = page_children(&root, &path("Many"), 0, &mut window)
            .expect("walk")
            .expect("a directory");
        assert_eq!(filled, 8);
        disk.reads()
    };

    let counting = {
        let mgr = open_mgr(disk.clone());
        let root = open_root(&mgr);
        disk.reset_reads();
        assert_eq!(
            count_children(&root, &path("Many")).expect("walk"),
            Some(100)
        );
        disk.reads()
    };

    assert!(
        paging < counting,
        "a full window read {paging} blocks and the whole folder read \
         {counting}; the page walked the folder to its end"
    );
}

fn seed_many(root: &Dir<'_>, count: usize) {
    root.make_dir_in_dir_lfn("Many").expect("mkdir");
    let many = child(root, "Many");
    for index in 0..count {
        let file = many
            .create_file_in_dir_lfn(&format!("Book {index:03}.epub"))
            .expect("create");
        file.close().expect("close");
    }
}

/// A short name is ISO-8859-1 on the card, and the driver renders each byte as
/// the scalar of the same value, so an accented character takes two UTF-8
/// bytes. Eight of them plus an extension render to 20, over the 12 an ASCII
/// 8.3 name needs. A buffer sized for the ASCII case drops this book from
/// resolution and from listings, which is a book gone missing rather than one
/// spelled oddly.
#[test]
fn a_short_only_name_of_accented_characters_is_listed_and_opens() {
    let awkward = "\u{dc}\u{dc}\u{dc}\u{dc}\u{dc}\u{dc}\u{dc}\u{dc}.EPU";
    assert_eq!(awkward.len(), 20, "the rendering is what overflows");

    let mgr = open_mgr(new_card());
    let root = open_root(&mgr);
    root.make_dir_in_dir_lfn("Fiction").expect("mkdir");
    let fiction = child(&root, "Fiction");
    // Created through a short name, so the entry carries no long name and is
    // found by its alias alone.
    let short = embedded_sdmmc::ShortFileName::create_from_str(awkward).expect("a valid 8.3 name");
    let file = fiction
        .open_file_in_dir(short, Mode::ReadWriteCreate)
        .expect("create");
    file.write(b"accented").expect("write");
    file.close().expect("close");

    assert_eq!(
        names_in(&root, "Fiction"),
        vec![(awkward.to_string(), false)],
        "the listing shows it"
    );

    let locator = path("Fiction").child(awkward).expect("component fits");
    let read = with_book(&root, &locator, |dir, alias| {
        let opened = dir
            .open_file_in_dir(alias, Mode::ReadOnly)
            .expect("open by alias");
        let mut buf = [0u8; 16];
        let read = opened.read(&mut buf).expect("read");
        buf[..read].to_vec()
    })
    .expect("walk")
    .expect("the book is there");

    assert_eq!(read, b"accented".to_vec(), "and the row opens the book");
}

/// Rewrite a six-character long name the driver created into a
/// five-character case variant, directly in the image bytes.
///
/// The driver refuses to create two names differing only in case, which is
/// exactly the directory another operating system can leave behind, so the
/// test forges one. A single LFN entry stores UTF-16 units 0..5 at bytes
/// 1..11 and units 5..11 at bytes 14..26; a six-unit name is shortened by
/// writing the NUL terminator over unit 5 and pad over unit 6, and re-cased
/// by rewriting units 0..5. The 8.3 alias and its checksum are untouched: an
/// alias unrelated to its long name is legal FAT, and is exactly what any
/// `~1` alias already looks like.
/// The byte offset of UTF-16 unit `i` inside a 32-byte LFN entry, whose
/// thirteen units live in three regions.
fn lfn_unit_offset(i: usize) -> usize {
    match i {
        0..=4 => 1 + 2 * i,
        5..=10 => 14 + 2 * (i - 5),
        _ => 28 + 2 * (i - 11),
    }
}

/// Rewrite a single-entry long name the driver created into another name of
/// the same or shorter length, directly in the image bytes.
///
/// The driver refuses to create two names differing only in case, which is
/// exactly the directory another operating system can leave behind, so the
/// tests forge one. The 8.3 alias and its checksum are untouched: an alias
/// unrelated to its long name is legal FAT, and is what any `~1` alias
/// already looks like.
fn rewrite_long_name(data: &mut [u8], created: &str, forged: &str) {
    let created_units: Vec<u16> = created.encode_utf16().collect();
    let forged_units: Vec<u16> = forged.encode_utf16().collect();
    assert!(created_units.len() <= 13, "one LFN entry holds 13 units");
    assert!(forged_units.len() <= created_units.len());
    let mut patched = 0;
    for at in (0..data.len().saturating_sub(31)).step_by(32) {
        let entry = &data[at..at + 32];
        // One terminal LFN entry: sequence 1 with the last-in-chain flag.
        if entry[0] != 0x41 || entry[11] != 0x0F || entry[12] != 0 {
            continue;
        }
        let unit_at = |i: usize| {
            let offset = lfn_unit_offset(i);
            u16::from_le_bytes([entry[offset], entry[offset + 1]])
        };
        let matches = (0..created_units.len()).all(|i| unit_at(i) == created_units[i])
            && (created_units.len() == 13 || unit_at(created_units.len()) == 0x0000);
        if !matches {
            continue;
        }
        for (i, unit) in forged_units.iter().enumerate() {
            let offset = lfn_unit_offset(i);
            data[at + offset..at + offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        // Terminator where the forged name ends, padding over the rest of
        // what the created name used.
        for i in forged_units.len()..=created_units.len().min(12) {
            let unit: u16 = if i == forged_units.len() {
                0x0000
            } else {
                0xFFFF
            };
            let offset = lfn_unit_offset(i);
            data[at + offset..at + offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        patched += 1;
    }
    assert_eq!(patched, 1, "exactly one directory entry should match");
}

fn forge_case_variant(data: &mut [u8], created: &str, forged: &str) {
    rewrite_long_name(data, created, forged);
}

/// A computer can legally leave case variants of the shelf with no exact
/// `BOOKS`. Two candidate libraries is not zero libraries: reading the
/// ambiguity as absence commits an empty catalog, and the orphan sweep then
/// reclaims the caches of every shelved book. The resolver must refuse.
#[test]
fn case_variants_of_the_shelf_are_refused_rather_than_read_as_absent() {
    let disk = new_card();
    {
        let mgr = open_mgr(disk.clone());
        let root = open_root(&mgr);
        root.make_dir_in_dir_lfn("BooksQ").expect("mkdir");
        root.make_dir_in_dir_lfn("booksZ").expect("mkdir");
    }
    {
        let mut data = disk.0.data.borrow_mut();
        forge_case_variant(&mut data, "BooksQ", "Books");
        forge_case_variant(&mut data, "booksZ", "books");
    }
    let mgr = open_mgr(disk);
    let root = open_root(&mgr);

    let outcome = open_library_root(&root);
    assert!(
        !matches!(outcome, Ok(None)),
        "two candidate shelves must not read as no shelf"
    );
    assert!(outcome.is_err(), "the ambiguity is a refusal");

    // The ordinary locator rule is unchanged: a component two case variants
    // answer to reads as absent, because picking one would open a folder the
    // reader did not choose.
    assert!(entry_in(&root, "BOOKS").expect("read").is_none());

    // The card's own spelling still wins exactly, ambiguity or not.
    assert!(entry_in(&root, "Books").expect("read").is_some());
}

/// An exact `BOOKS` file must not hide a case-variant library directory.
/// The file cannot be the shelf and the directory can, so reading the pair
/// as "no shelf" would commit a catalog that omits every book under the
/// directory. A lone squatting file, with nothing case-equivalent beside
/// it, still reads as no shelf
/// (`a_file_named_like_the_library_root_is_not_one`).
#[test]
fn an_exact_shelf_file_does_not_hide_a_case_variant_library() {
    let disk = new_card();
    {
        let mgr = open_mgr(disk.clone());
        let root = open_root(&mgr);
        let file = root.create_file_in_dir_lfn("BOOKS").expect("create");
        file.write(b"x").expect("write");
        file.close().expect("close");
        root.make_dir_in_dir_lfn("booksZ").expect("mkdir");
    }
    {
        let mut data = disk.0.data.borrow_mut();
        forge_case_variant(&mut data, "booksZ", "books");
    }
    let mgr = open_mgr(disk);
    let root = open_root(&mgr);

    let outcome = open_library_root(&root);
    assert!(
        !matches!(outcome, Ok(None)),
        "a plausible shelf beside the squatting file must not read as none"
    );
    assert!(outcome.is_err(), "the mix is a refusal");
}

/// The tree the depth-first walk tests build under a shelf: books at three
/// depths, a short-only book, and every kind of entry the walk must skip.
fn seed_shelf_tree(root: &Dir<'_>) {
    root.make_dir_in_dir_lfn("BOOKS").expect("mkdir");
    let books = child(root, "BOOKS");

    let file = books.create_file_in_dir_lfn("A.epub").expect("create");
    file.write(b"a").expect("write");
    file.close().expect("close");
    let skip = books.create_file_in_dir_lfn("notes.txt").expect("create");
    skip.write(b"n").expect("write");
    skip.close().expect("close");
    let short = books
        .open_file_in_dir("SHORT.EPU", Mode::ReadWriteCreate)
        .expect("create");
    short.write(b"sh").expect("write");
    short.close().expect("close");

    books.make_dir_in_dir_lfn("Fiction").expect("mkdir");
    let fiction = child(&books, "Fiction");
    let dune = fiction.create_file_in_dir_lfn("Dune.epub").expect("create");
    dune.write(b"du").expect("write");
    dune.close().expect("close");
    fiction.make_dir_in_dir_lfn("Classics").expect("mkdir");
    let classics = child(&fiction, "Classics");
    let iliad = classics
        .create_file_in_dir_lfn("Iliad.epub")
        .expect("create");
    iliad.write(b"il").expect("write");
    iliad.close().expect("close");

    books.make_dir_in_dir_lfn(".hidden").expect("mkdir");
    let hidden = child(&books, ".hidden");
    let secret = hidden
        .create_file_in_dir_lfn("Secret.epub")
        .expect("create");
    secret.write(b"se").expect("write");
    secret.close().expect("close");

    books.make_dir_in_dir_lfn("Empty").expect("mkdir");
}

/// Collect one full walk of the shelf as (locator, size) pairs.
fn walked_books(root: &Dir<'_>) -> Vec<(String, u32)> {
    let books = open_library_root(root)
        .expect("read")
        .expect("the shelf is there");
    let mut seen = Vec::new();
    upload_store::library::for_each_book_depth_first(books, &mut |path, _alias, size| {
        seen.push((path.as_str().to_string(), size));
    })
    .expect("walk");
    seen
}

/// Milestone 2's contract: every book below the shelf is found at its full
/// locator, depth first, a directory's files before its subfolders'
/// contents, in the order the card stores them, and identically on every
/// walk, which the scan's fingerprint depends on. Hidden entries, non-EPUBs,
/// and everything below a hidden folder stay out, exactly as browsing would
/// leave them out.
#[test]
fn the_shelf_walk_finds_every_book_at_its_locator_depth_first() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let root = open_root(&mgr);
    seed_shelf_tree(&root);

    let seen = walked_books(&root);
    assert_eq!(
        seen,
        vec![
            ("A.epub".to_string(), 1),
            ("SHORT.EPU".to_string(), 2),
            ("Fiction/Dune.epub".to_string(), 2),
            ("Fiction/Classics/Iliad.epub".to_string(), 2),
        ],
        "files before subfolder contents, card order, filters applied",
    );

    assert_eq!(
        seen,
        walked_books(&root),
        "an unchanged card walks identically"
    );
}

/// The walk honors the locator bounds the way browsing does: a book whose
/// locator would be deeper than MAX_DEPTH is not cataloged, and the folder
/// that could only hold such books is not even entered.
#[test]
fn the_shelf_walk_stops_at_the_locator_depth_floor() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let root = open_root(&mgr);
    root.make_dir_in_dir_lfn("BOOKS").expect("mkdir");
    {
        let mut dir = child(&root, "BOOKS");
        // d1 through d8: a folder at every depth the locator rules allow.
        for depth in 1..=proto::library_path::MAX_DEPTH {
            let name = format!("d{depth}");
            dir.make_dir_in_dir_lfn(&name).expect("mkdir");
            let next = child(&dir, &name);
            dir = next;
            if depth == proto::library_path::MAX_DEPTH - 1 {
                // Depth 8 for the book itself: the deepest legal locator.
                let book = dir.create_file_in_dir_lfn("Edge.epub").expect("create");
                book.write(b"edge").expect("write");
                book.close().expect("close");
            }
            if depth == proto::library_path::MAX_DEPTH {
                // Depth 9 for this one: no legal locator can name it.
                let book = dir.create_file_in_dir_lfn("Below.epub").expect("create");
                book.write(b"below").expect("write");
                book.close().expect("close");
            }
        }
    }

    let seen = walked_books(&root);
    assert_eq!(
        seen,
        vec![("d1/d2/d3/d4/d5/d6/d7/Edge.epub".to_string(), 4)],
        "the deepest legal book is found and the unreachable one is not",
    );
}

/// A card that stops answering mid-walk fails the walk: committing a
/// catalog missing whatever went unread would hand the orphan sweep every
/// unread book's caches.
#[test]
fn a_walk_the_card_interrupts_is_an_error_not_a_short_catalog() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let root = open_root(&mgr);
    seed_shelf_tree(&root);

    let books = open_library_root(&root)
        .expect("read")
        .expect("the shelf is there");
    disk.fail_reads_from(Some(4));
    let walked = upload_store::library::for_each_book_depth_first(books, &mut |_, _, _| {});
    disk.fail_reads_from(None);
    assert!(walked.is_err(), "an unread subtree must fail the walk");
}
