//! Moving through the library tree, against a card that may stop answering.
//!
//! The state machine is [`app_core::browse::Browse`] and the listing is
//! [`upload_store::library`]; what lives here is the transaction between
//! them. A move either lands with a page of rows in front of the reader, or
//! it is put back exactly as it was, because the failure the app hears means
//! standstill and it keeps its own depth and rows on that word. A recovery
//! that quietly left this side somewhere else would have the two halves
//! describing different folders, with the screen naming one and every later
//! command landing on the other.
//!
//! Sited here rather than in the firmware for the reason the publish tail
//! was: a fault after the state has already moved is exactly the shape that
//! keeps getting written wrong, and it cannot be tested inside a
//! `#![no_main]` binary. The caller supplies an open card and the visible row
//! count; everything that decides whether a move landed is here.

use app_core::browse::{Chosen, Row};
use embedded_sdmmc::{Directory, TimeSource};
use proto::library_path::{BookRoot, LibraryPath};
use upload_store::library::{open_listing, Kind, LibraryRow, OpenListing};

use crate::store::{ReaderStore, LIBRARY_WINDOW};

/// The row count for a folder holding `rows`, or `None` when it holds more
/// than a cursor can address.
///
/// Refused rather than clamped, for the reason the catalog refuses a library
/// past its own count field: a listing that stops at 65,535 hides the
/// children after it, and hidden rows are unreachable rather than merely
/// unlisted. A folder's rows are its files and its subdirectories together,
/// so this limit is reachable on a card whose catalog is comfortably legal.
pub fn addressable_rows(rows: usize) -> Option<u16> {
    u16::try_from(rows).ok()
}

/// What one listing produced, as the app needs to hear it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listing {
    pub depth: u8,
    pub count: u16,
    pub books: u16,
    pub selection: u16,
}

/// What the row a reader pressed turned out to be.
///
/// `Book` carries a whole locator, which makes this a few hundred bytes, the
/// same trade [`app_core::browse::Chosen`] makes and for the same reason:
/// there is no allocator here, and an out-parameter would move the cost to
/// every caller. It is built once per press and dropped by the end of it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum RowChoice {
    /// A folder, now listed.
    Entered(Listing),
    /// A book. The caller resolves it to a catalog row, which needs the
    /// catalog and so belongs where the catalog is read.
    Book {
        at: BookRoot,
        locator: LibraryPath,
        size: u32,
    },
    /// Gone since the listing, unnameable from here, or a card that would not
    /// answer. Nothing moved.
    Failed,
}

/// Where in the listing the visible rows start, given the cursor.
fn page_start(store: &ReaderStore, selection: u16, portrait: bool) -> Option<usize> {
    let total = store.browse().count() as usize;
    if total == 0 {
        return None;
    }
    let selection = (selection as usize).min(total - 1);
    Some(ui::render::library_scroll_start(selection, total, portrait))
}

/// Read the page at `start` into the store, or say the card would not answer.
///
/// The fallible half of [`ensure_page`]. A move has to know: a page it could
/// not read is a folder it cannot show, and reporting the move as landed
/// would commit both halves to a place with no rows in it.
fn read_page<D, T, const MD: usize, const MF: usize, const MV: usize>(
    store: &mut ReaderStore,
    card_root: &Directory<'_, D, T, MD, MF, MV>,
    listing: &OpenListing<'_, D, T, MD, MF, MV>,
    start: usize,
) -> Option<usize>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let counts = store.folder_counts();
    let mut cursor = store.folder_cursor_for(start);
    let mut window: [LibraryRow; LIBRARY_WINDOW] = Default::default();
    let filled = listing
        .page(card_root, counts, start, &mut window, &mut cursor)
        .ok()
        .flatten()?;
    if let Some(cursor) = cursor {
        store.record_folder_checkpoint(&cursor);
    }
    store.begin_folder_page(start);
    for row in window.iter().take(filled) {
        store.push_folder_row(
            row.child.name.as_str(),
            row.child.alias,
            row.child.is_dir,
            row.child.size,
            row.at,
        );
    }
    Some(filled)
}

/// Slide the resident page over the rows a render is about to draw, and say
/// whether that cost a read.
///
/// Best-effort on purpose: this runs before every Library paint, and a card
/// that stalls during one costs that paint its rows rather than the reader
/// their place. A move uses [`read_page`] instead, which reports the fault.
///
/// The answer is whether the card was touched, not whether it answered. A
/// caller timing the Library path needs to tell a scroll inside the loaded
/// page, which reads nothing, from the crossing that refills it.
pub fn ensure_page<D, T, const MD: usize, const MF: usize, const MV: usize>(
    store: &mut ReaderStore,
    card_root: &Directory<'_, D, T, MD, MF, MV>,
    selection: u16,
    portrait: bool,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some(start) = page_start(store, selection, portrait) else {
        store.begin_folder_page(0);
        return false;
    };
    let need =
        ui::render::library_visible_rows(portrait).min(store.browse().count() as usize - start);
    if store.folder_covers(start, need) {
        return false;
    }
    let path = store.browse().path().clone();
    let read = match open_listing(card_root, &path).ok().flatten() {
        Some(listing) => read_page(store, card_root, &listing, start).is_some(),
        None => false,
    };
    if !read {
        // Best-effort, as the doc above says: a card that would not answer
        // costs this paint its rows. Both ways of failing cost the same,
        // whether the card would not open the folder or would not read the
        // page: the resident rows belong to the offset this call was asked
        // to move away from, and leaving them would keep serving a page the
        // reader has scrolled off. `read_page` mutates nothing until its own
        // read has succeeded, so there is no half-filled page to undo.
        store.begin_folder_page(start);
    }
    true
}

/// Count where browsing is and put the page around the cursor in front of the
/// reader.
///
/// `None` is a folder that would not answer, count or page. Nothing is moved
/// to recover from it: where the reader is belongs to the caller, which is the
/// only place that knows what to put back.
pub fn list_here<D, T, const MD: usize, const MF: usize, const MV: usize>(
    store: &mut ReaderStore,
    card_root: &Directory<'_, D, T, MD, MF, MV>,
    portrait: bool,
) -> Option<Listing>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    list_landing(store, card_root, portrait, None)
}

/// [`list_here`], landing on the book at `locator` under `at` when this
/// folder holds it: what a pick whose rescan cleared the page owes.
///
/// This side's selection moves on entering and leaving, not on scrolling, so
/// relisting from it put the reader back at the top. The book is found by
/// name in the walk that counts the folder, and the page is read from the
/// cursor that walk leaves.
pub fn relist_on_book<D, T, const MD: usize, const MF: usize, const MV: usize>(
    store: &mut ReaderStore,
    card_root: &Directory<'_, D, T, MD, MF, MV>,
    portrait: bool,
    at: BookRoot,
    locator: &LibraryPath,
) -> Option<Listing>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // A book in another folder is not on this listing, and a same-named one
    // here would be the wrong row to land on.
    let here = locator.parent().as_ref() == Some(store.browse().path());
    if let Some(name) = locator.file_name().filter(|_| here) {
        store.browse_mut().land_on(name);
    }
    list_landing(store, card_root, portrait, Some((Kind::Book, at)))
}

/// List where browsing is. With `region`, the row [`Browse::returning`] names
/// is looked for among that kind of row under that root, in the same walk
/// that counts the folder, and the listing lands on it and reads its page
/// from there: a few walks wherever it sorts.
///
/// [`Browse::returning`]: app_core::browse::Browse::returning
fn list_landing<D, T, const MD: usize, const MF: usize, const MV: usize>(
    store: &mut ReaderStore,
    card_root: &Directory<'_, D, T, MD, MF, MV>,
    portrait: bool,
    region: Option<(Kind, BookRoot)>,
) -> Option<Listing>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let path = store.browse().path().clone();
    // One resolution for both halves. The count and the page address the
    // same folder, and opening it twice walked the card root for the shelf
    // and then every component again.
    let listing = open_listing(card_root, &path).ok().flatten()?;
    let (counts, landing) = match region.zip(store.browse().returning()) {
        Some(((kind, at), name)) => listing.counts_ranking(card_root, kind, at, name).ok()?,
        None => (listing.counts(card_root).ok()?, None),
    };
    let total = addressable_rows(counts.total())?;
    store.set_folder_counts(counts);
    store.clear_folder_page();
    if let Some(cursor) = landing {
        if let Some(row) = cursor.row(counts).and_then(|row| u16::try_from(row).ok()) {
            store.browse_mut().note_row(row, cursor.name.as_str());
        }
        store.record_folder_checkpoint(&cursor);
    }
    // After the row is offered: the count ends the return, and a name it did
    // not find leaves the fallback row, clamped.
    store.browse_mut().set_count(total);
    let selection = store.browse().selection();
    if let Some(start) = page_start(store, selection, portrait) {
        // The count said there are rows, so a page that will not read is the
        // card going away between the two reads, not an empty folder.
        read_page(store, card_root, &listing, start)?;
    }
    Some(Listing {
        depth: path.depth() as u8,
        count: total,
        books: counts.books().min(usize::from(total)) as u16,
        selection,
    })
}

/// Take browsing back to the library root and list it, which is what a scan
/// that replaced the catalog underneath it owes.
///
/// `None` is a card that answered the scan and then would not answer for the
/// rows. Browsing is left at the root with nothing listed, so both halves
/// agree about where the reader is and how much is loaded.
///
/// What the caller must not do with that `None` is call it an empty library.
/// A card with no books answers this call with a real listing of zero rows,
/// and the two states are different: one is a library to add books to, the
/// other is a library that could not be read. Only the caller can say which,
/// because only the caller carries the status the screen reads.
pub fn relist_root<D, T, const MD: usize, const MF: usize, const MV: usize>(
    store: &mut ReaderStore,
    card_root: &Directory<'_, D, T, MD, MF, MV>,
    portrait: bool,
) -> Option<Listing>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // A rescan is a new library. Browsing goes back to the root rather than
    // holding a folder the card may no longer have, and every row number
    // picked before that is retired with it: a move issued from the folder
    // this just left would otherwise be read against the root, and the same
    // row number would enter a different folder or open a different book.
    //
    // The catalog epoch cannot stand in for that. A scan whose recovery is
    // unfinished declines to rebuild, so the catalog and its epoch survive
    // while this reset happens anyway.
    store.reposition_browse();
    let point = store.browse_checkpoint();
    match list_here(store, card_root, portrait) {
        Some(listing) => Some(listing),
        None => {
            // The count may already have landed before the page failed, and
            // a store claiming rows it has none of is the mismatch this whole
            // transaction exists to prevent.
            store.restore_browse(point);
            None
        }
    }
}

/// What book the row at `index` is, without moving anything.
///
/// `None` for a folder, and for a row the resident page does not cover. What
/// a book row is worth to a caller is the three things its catalog identity
/// was derived from, so that is what comes back.
pub fn row_book(store: &ReaderStore, index: u16) -> Option<(BookRoot, LibraryPath, u32)> {
    let row = store.folder_row(index as usize)?;
    if row.is_dir {
        return None;
    }
    let locator = store.browse().path().child(row.name.as_str()).ok()?;
    Some((row.at, locator, row.size))
}

/// Act on the row at `index`: enter a folder, or name the book it is.
pub fn choose_row<D, T, const MD: usize, const MF: usize, const MV: usize>(
    store: &mut ReaderStore,
    card_root: &Directory<'_, D, T, MD, MF, MV>,
    index: u16,
    portrait: bool,
) -> RowChoice
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some(row) = store.folder_row(index as usize) else {
        // A row the resident page never covered. The press acts on nothing
        // rather than on a neighbour.
        return RowChoice::Failed;
    };
    let mut name = heapless::String::<{ proto::library_path::MAX_COMPONENT_BYTES }>::new();
    if name.push_str(row.name.as_str()).is_err() {
        return RowChoice::Failed;
    }
    let size = row.size;
    let at = row.at;
    let kind = if row.is_dir { Row::Folder } else { Row::Book };
    let point = store.browse_checkpoint();
    match store.browse_mut().choose(name.as_str(), kind) {
        Chosen::Entered => {
            store.clear_folder_page();
            match list_here(store, card_root, portrait) {
                Some(listing) => RowChoice::Entered(listing),
                None => {
                    store.restore_browse(point);
                    RowChoice::Failed
                }
            }
        }
        // Choosing a book moves nothing, so there is nothing to put back.
        Chosen::Open(locator) => RowChoice::Book { at, locator, size },
        Chosen::Refused(_) => RowChoice::Failed,
    }
}

/// Go up one folder and list the parent, landing back on the folder just
/// left: by name where the parent still holds it, else on the row it was
/// entered from.
pub fn leave_folder<D, T, const MD: usize, const MF: usize, const MV: usize>(
    store: &mut ReaderStore,
    card_root: &Directory<'_, D, T, MD, MF, MV>,
    portrait: bool,
) -> Option<Listing>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let point = store.browse_checkpoint();
    if !store.browse_mut().leave() {
        return None;
    }
    // Every way of failing after the move has to put the move back, or the
    // app and this state describe different folders. One rollback here, so a
    // path added later cannot forget it: the listing below returns `None` and
    // says nothing about restoring.
    //
    // The folder is found by name in the walk that counts the parent, so a
    // departure that lands has counted all of it: one that stopped early
    // could pass over the very name it was going back to.
    match list_landing(
        store,
        card_root,
        portrait,
        Some((Kind::Folder, BookRoot::Library)),
    ) {
        Some(listing) => Some(listing),
        None => {
            store.restore_browse(point);
            None
        }
    }
}
