//! Firmware side of the reader page plan: the store-backed fast paths
//! (cached page records, section-relative page mapping) and TOC page
//! targets over the bounded [`ReaderStore`]. Heights, pagination walks,
//! and page-body drawing live in [`ui::reading`] behind the
//! `ReadingBlocks` trait so firmware and host tools render one way.

use crate::store::ReaderStore;
pub use display::font::{style_marker_code, STYLE_MARKER};
use proto::cache::PageRecord;
use ui::reading::{apply_block_placement, page_record_at, paginate_block_pages, PageIndexCursor};
pub use ui::reading::{
    first_styled_line_style, paragraph_indent, reader_layout_config, READER_WRAP_SAFETY,
};

pub struct ReaderPagePlan {
    page_count: u32,
    page: PageRecord,
}

impl ReaderPagePlan {
    pub fn new(sd_library: &ReaderStore, requested_page: u32) -> Self {
        let page_count = reader_page_count(sd_library);
        let requested_page = sd_library.local_page_for_global(requested_page.min(page_count - 1));
        let page = reader_page_at(sd_library, requested_page);
        Self { page_count, page }
    }

    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    pub fn page(&self) -> PageRecord {
        self.page
    }
}

pub(crate) fn reader_page_count(sd_library: &ReaderStore) -> u32 {
    if sd_library.book_total_pages > 0 {
        return sd_library.book_total_pages;
    }
    if sd_library.page_count > 0 {
        return sd_library.page_count as u32;
    }
    paginate_block_pages(sd_library).max(1) as u32
}

pub(crate) fn reader_page_at(sd_library: &ReaderStore, page_index: usize) -> PageRecord {
    if page_index < sd_library.page_count {
        return sd_library.pages[page_index];
    }
    page_record_at(sd_library, page_index)
}

/// Rebuild the section's page records from scratch by walking every block
/// through the shared [`PageIndexCursor`] — the same cursor the streaming
/// cache build advances incrementally, so the full walk and the per-line
/// path cannot drift. Returns the finished cursor plus whether the page
/// records overflowed their capacity, so a builder can adopt them and keep
/// appending incrementally (the carry path does exactly that).
pub fn rebuild_page_index(library: &mut ReaderStore) -> (PageIndexCursor, bool) {
    library.page_count = 0;
    let mut cursor = PageIndexCursor::start(library.page_box());
    let mut overflowed = false;
    for index in 0..library.block_count {
        let placement = cursor.place_next_block(library, index);
        let spine = library.block_spine.get(index).copied().unwrap_or(0);
        apply_block_placement(
            placement,
            index,
            spine,
            &mut library.pages,
            &mut library.page_spine,
            &mut library.page_count,
            &mut overflowed,
        );
    }
    rebuild_page_offsets(library);
    (cursor, overflowed)
}

/// A page opens where its first line opens. Taken from the line rather than
/// computed from the cached text, whose byte length is a rendering's.
pub(crate) fn rebuild_page_offsets(library: &mut ReaderStore) {
    for page in 0..library.page_count {
        let first = library.pages[page].first_block as usize;
        library.page_offset[page] = library.block_offset.get(first).copied().unwrap_or(0);
    }
}

pub fn rebuild_toc_page_targets(library: &mut ReaderStore) {
    for toc_index in 0..library.toc_count {
        let spine_index = library.toc[toc_index].spine_index;
        if spine_index < 0 {
            library.toc_page[toc_index] = 0;
            continue;
        }
        let spine = spine_index as u16;
        let page = library
            .book_sections
            .iter()
            .take(library.book_section_count)
            .find(|section| section.spine == spine)
            .map(|section| section.start_page as usize)
            .or_else(|| {
                library
                    .page_spine
                    .iter()
                    .take(library.page_count)
                    .position(|page_spine| *page_spine == spine)
            })
            .unwrap_or(0);
        library.toc_page[toc_index] = page.min(u16::MAX as usize) as u16;
    }
}

/// Mirror the block just appended into the page index through the shared
/// incremental cursor — O(1) per line, the streaming counterpart of
/// [`rebuild_page_index`].
///
/// Exists so the three fields a placement touches (`pages`, `page_spine`,
/// `page_count`) stay inside this crate: a caller that borrows all three at once
/// can leave the index describing a different arena than the one it holds.
/// Returns whether the page records overflowed their capacity.
pub fn place_appended_block(
    library: &mut ReaderStore,
    cursor: &mut PageIndexCursor,
    index: usize,
) -> bool {
    let placement = cursor.place_next_block(library, index);
    let spine = library.block_spine.get(index).copied().unwrap_or(0);
    let mut overflowed = false;
    apply_block_placement(
        placement,
        index,
        spine,
        &mut library.pages,
        &mut library.page_spine,
        &mut library.page_count,
        &mut overflowed,
    );
    overflowed
}

/// Re-place just the last block after `mark_last_block_paragraph_end` grew it.
/// The gap it grew by is not ink, so the block stays on its page and what
/// moves is the running `y` the next block is placed against; the decision is
/// re-taken all the same, and a `NewPage` answer is mirrored into the records
/// as a full rebuild would arrive at it. Returns whether the records
/// overflowed.
pub fn replace_last_block(
    library: &mut ReaderStore,
    cursor: &mut PageIndexCursor,
    index: usize,
) -> bool {
    let placement = cursor.replace_last_block(library, index);
    if placement != ui::reading::BlockPlacement::NewPage {
        return false;
    }
    let spine = library.block_spine.get(index).copied().unwrap_or(0);
    let mut overflowed = false;
    let before = library.page_count;
    ui::reading::apply_last_block_move(
        index,
        spine,
        &mut library.pages,
        &mut library.page_spine,
        &mut library.page_count,
        &mut overflowed,
    );
    // The move can open a page, and the moved block is the only thing on it,
    // so that is where the page opens. The walk that sets offsets as pages
    // open has already been past this one.
    if library.page_count > before {
        let offset = library.block_offset.get(index).copied().unwrap_or(0);
        library.set_last_page_offset(offset);
    }
    overflowed
}
