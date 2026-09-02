//! Where the reader is in the library, and what pressing a button does there.
//!
//! The library is the card's own folder tree, so browsing has a position in
//! that tree rather than an index into one flat list. This holds that
//! position and the rules for moving through it. It reads no card and stores
//! no entries: the caller lists the current folder and hands over what the
//! selected row turned out to be.
//!
//! Leaving a folder returns to the row it was entered from. Without that,
//! stepping into a folder and back out drops the reader at the top of a long
//! list, having lost the place they were working through.

use proto::library_path::{LibraryPath, PathError, MAX_COMPONENT_BYTES, MAX_DEPTH};

/// What the selected row is, which the caller knows from the listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Folder,
    Book,
}

/// What choosing the selected row did.
///
/// `Open` carries a whole locator, which makes this a few hundred bytes.
/// Boxing it is the usual answer and there is no allocator here, and an
/// out-parameter would move the cost to every caller. It is built once per
/// button press and dropped by the end of it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum Chosen {
    /// Moved into a folder. The caller lists the new path.
    Entered,
    /// A book to open, at this locator.
    Open(LibraryPath),
    /// The row cannot be turned into a locator from here, so nothing moved.
    /// A listing that only offers legal rows makes this unreachable, and it
    /// is returned rather than ignored so a caller that lists differently
    /// finds out.
    Refused(PathError),
}

/// The reader's position in the library tree.
#[derive(Clone, Debug, Default)]
pub struct Browse {
    path: LibraryPath,
    selection: u16,
    count: u16,
    /// The row each ancestor was entered from, innermost last.
    trail: heapless::Vec<u16, MAX_DEPTH>,
    /// The folder [`Browse::leave`] left, until the parent's listing places it
    /// or ends without it. The fallback row lives in `selection`, which
    /// `leave` sets before the listing starts.
    returning: Option<heapless::String<MAX_COMPONENT_BYTES>>,
}

impl Browse {
    /// At the library root, with nothing listed yet.
    ///
    /// `const` because the firmware holds this inside a store that is all
    /// zero bytes by construction, so the whole thing lives in `.bss` rather
    /// than being copied out of flash at boot.
    pub const fn root() -> Self {
        Self {
            path: LibraryPath::root(),
            selection: 0,
            count: 0,
            trail: heapless::Vec::new(),
            returning: None,
        }
    }

    pub fn path(&self) -> &LibraryPath {
        &self.path
    }

    pub fn selection(&self) -> u16 {
        self.selection
    }

    pub fn count(&self) -> u16 {
        self.count
    }

    pub fn is_root(&self) -> bool {
        self.path.is_root()
    }

    /// Tell it how many rows the current folder has, ending a listing.
    ///
    /// A pending return ends here. If [`Browse::note_row`] placed the folder
    /// that was left, the selection sits where its name was found, spelled
    /// exactly as it was on the way in. Failing that, the folder is gone as
    /// far as browsing can tell, and the selection is still the row it was
    /// entered from, which [`Browse::leave`] set. A folder renamed under any
    /// other spelling, case included, is a different locator and takes the
    /// fallback like any other disappearance.
    ///
    /// Then clamps, because a folder can be listed again after the card
    /// changed underneath, and a selection past the end would open whatever is
    /// last or nothing at all.
    pub fn set_count(&mut self, count: u16) {
        self.returning = None;
        self.count = count;
        if self.selection >= count {
            self.selection = count.saturating_sub(1);
        }
    }

    /// Offer a row while listing, so a folder that was left is found again by
    /// name rather than by the number it used to have.
    ///
    /// A row number survives only while the folder around it does. A book
    /// deleted from a computer above the folder shifts everything up, and
    /// returning to the old number then lands on a neighbour, which is the
    /// place-losing this exists to prevent. Names move with their rows.
    ///
    /// Costs nothing when no return is pending, which is every listing except
    /// the one after going up.
    pub fn note_row(&mut self, index: u16, name: &str) {
        let Some(returning) = &self.returning else {
            return;
        };
        if returning.as_str() == name {
            self.selection = index;
            self.returning = None;
        }
        // Exactly that name, or the fallback row `leave` set. A locator
        // names the entry it was obtained from, so a folder whose visible
        // name changed, case included, is a different locator now; sameness
        // across renames belongs to library identity, not to browsing.
    }

    /// Move the selection, stopping at either end rather than wrapping.
    pub fn move_by(&mut self, delta: i16) {
        if self.count == 0 {
            self.selection = 0;
            return;
        }
        let last = self.count - 1;
        let moved = i32::from(self.selection) + i32::from(delta);
        self.selection = moved.clamp(0, i32::from(last)) as u16;
    }

    /// Choose the selected row, which the caller has named.
    pub fn choose(&mut self, name: &str, kind: Row) -> Chosen {
        let next = match self.path.child(name) {
            Ok(next) => next,
            Err(error) => return Chosen::Refused(error),
        };
        match kind {
            Row::Book => Chosen::Open(next),
            Row::Folder => {
                // Pushed before the move, so leaving comes back to this row.
                // A full trail cannot happen while the path bounds agree, and
                // dropping the entry rather than refusing the move keeps
                // navigation working: the cost is landing at the top of the
                // parent, not being stuck.
                let _ = self.trail.push(self.selection);
                self.path = next;
                self.selection = 0;
                self.count = 0;
                Chosen::Entered
            }
        }
    }

    /// Go up one folder, aiming to land back on it in the parent's listing.
    ///
    /// The folder being left is remembered by name, and the row it was entered
    /// from is kept as a fallback for when that name is gone. The caller
    /// offers each row to [`Browse::note_row`] as it lists, and
    /// [`Browse::set_count`] settles it.
    ///
    /// `false` at the root, where there is nowhere to go and the caller
    /// decides what a back press means.
    pub fn leave(&mut self) -> bool {
        let Some(parent) = self.path.parent() else {
            return false;
        };
        let row = self.trail.pop().unwrap_or(0);
        let mut name = heapless::String::new();
        if let Some(leaving) = self.path.file_name() {
            let _ = name.push_str(leaving);
        }
        self.returning = Some(name);
        self.path = parent;
        // The fallback, in place before the listing starts: if the name is
        // gone, this is where the reader lands.
        self.selection = row;
        self.count = 0;
        true
    }

    /// Go back to the library root, forgetting the trail.
    pub fn reset(&mut self) {
        *self = Self::root();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// List a folder the way a caller does: every row offered, then the count.
    fn listed(browse: &mut Browse, rows: &[&str]) {
        listed_as(browse, rows);
    }

    /// A listing whose rows are short-only entries, showing their aliases.
    fn listed_short(browse: &mut Browse, rows: &[&str]) {
        listed_as(browse, rows);
    }

    fn listed_as(browse: &mut Browse, rows: &[&str]) {
        for (index, name) in rows.iter().enumerate() {
            browse.note_row(index as u16, name);
        }
        browse.set_count(rows.len() as u16);
    }

    /// A listing whose names do not matter to the test.
    fn listed_count(browse: &mut Browse, count: u16) {
        let rows: heapless::Vec<heapless::String<8>, 16> = (0..count)
            .map(|i| {
                let mut name = heapless::String::new();
                let _ = name.push('r');
                let _ = name.push((b'0' + (i % 10) as u8) as char);
                name
            })
            .collect();
        let refs: heapless::Vec<&str, 16> = rows.iter().map(|r| r.as_str()).collect();
        listed(browse, &refs);
    }

    #[test]
    fn browsing_starts_at_the_root() {
        let browse = Browse::root();
        assert!(browse.is_root());
        assert_eq!(browse.path().as_str(), "");
        assert_eq!(browse.selection(), 0);
    }

    #[test]
    fn entering_a_folder_moves_and_starts_at_the_top() {
        let mut browse = Browse::root();
        listed_count(&mut browse, 5);
        browse.move_by(3);

        assert_eq!(browse.choose("Fiction", Row::Folder), Chosen::Entered);
        assert_eq!(browse.path().as_str(), "Fiction");
        assert_eq!(browse.selection(), 0);
        assert_eq!(browse.count(), 0, "and waits to be told what is there");
    }

    const SHELF: [&str; 3] = ["Alice.epub", "History", "Rome.epub"];

    #[test]
    fn leaving_returns_to_the_folder_it_came_from() {
        let mut browse = Browse::root();
        listed(&mut browse, &SHELF);
        browse.move_by(1);
        browse.choose("History", Row::Folder);
        listed_count(&mut browse, 4);
        browse.move_by(2);

        assert!(browse.leave());
        listed(&mut browse, &SHELF);
        assert_eq!(browse.path().as_str(), "");
        assert_eq!(
            browse.selection(),
            1,
            "back where the reader was, not at the top of a long list",
        );
    }

    #[test]
    fn leaving_finds_the_folder_again_after_the_parent_shifted() {
        let mut browse = Browse::root();
        listed(&mut browse, &SHELF);
        browse.move_by(1);
        browse.choose("History", Row::Folder);
        listed_count(&mut browse, 2);

        // A computer deleted the book above it while the reader was inside.
        assert!(browse.leave());
        listed(&mut browse, &["History", "Rome.epub"]);

        assert_eq!(
            browse.selection(),
            0,
            "the row it was entered from now holds a neighbour, and the name \
             is what still points at the folder",
        );
    }

    #[test]
    fn leaving_falls_back_to_the_row_when_the_folder_is_gone() {
        let mut browse = Browse::root();
        listed(&mut browse, &SHELF);
        browse.move_by(1);
        browse.choose("History", Row::Folder);
        listed_count(&mut browse, 2);

        // The folder itself was removed while the reader was inside it.
        assert!(browse.leave());
        listed(&mut browse, &["Alice.epub", "Rome.epub"]);

        assert_eq!(
            browse.selection(),
            1,
            "no name to find, so the row it was entered from is what is left",
        );
    }

    #[test]
    fn a_case_only_rename_is_a_different_folder_now() {
        let mut browse = Browse::root();
        listed(&mut browse, &SHELF);
        browse.move_by(1);
        browse.choose("History", Row::Folder);
        listed_count(&mut browse, 2);

        // A computer renamed the folder while the reader was inside. The
        // visible name changed, so the locator changed; sameness across
        // renames is library identity's job, not the browse list's. The
        // return lands on the row the folder was entered from.
        assert!(browse.leave());
        listed(&mut browse, &["HISTORY", "Rome.epub"]);

        assert_eq!(browse.selection(), 1);
    }

    #[test]
    fn an_exact_spelling_further_down_beats_a_case_variant() {
        let mut browse = Browse::root();
        listed(&mut browse, &SHELF);
        browse.move_by(1);
        browse.choose("History", Row::Folder);
        listed_count(&mut browse, 2);

        assert!(browse.leave());
        listed(&mut browse, &["HISTORY", "History", "Rome.epub"]);

        assert_eq!(
            browse.selection(),
            1,
            "the folder that was entered is the one spelled that way",
        );
    }

    #[test]
    fn two_case_variants_name_no_folder_in_particular() {
        let mut browse = Browse::root();
        listed(&mut browse, &SHELF);
        browse.move_by(1);
        browse.choose("History", Row::Folder);
        listed_count(&mut browse, 2);

        // Written elsewhere: the spelling that was entered is gone and two
        // others answer to it. The resolver declines that, and so does going
        // up.
        assert!(browse.leave());
        listed(&mut browse, &["HISTORY", "history", "Rome.epub"]);

        assert_eq!(
            browse.selection(),
            1,
            "the row it was entered from, rather than a folder picked by \
             directory order",
        );
    }

    #[test]
    fn a_short_only_folder_is_not_renamed_by_a_non_ascii_case_change() {
        let mut browse = Browse::root();
        // Short-only rows show their 8.3 aliases, which the driver matched by
        // uppercasing ASCII, so these two are separate folders to it.
        listed_short(&mut browse, &["Alice.epub", "\u{dc}BER"]);
        browse.move_by(1);
        browse.choose("\u{dc}BER", Row::Folder);
        listed_count(&mut browse, 1);

        // Written elsewhere: the folder is gone and one spelled with the
        // lowercase letter stands at a different row.
        assert!(browse.leave());
        listed_short(&mut browse, &["\u{fc}ber", "Alice.epub"]);

        assert_eq!(
            browse.selection(),
            1,
            "the row it was entered from, not a folder the resolver would \
             refuse to open under that name",
        );
    }

    #[test]
    fn a_short_only_folder_recased_is_a_different_locator_too() {
        let mut browse = Browse::root();
        listed_short(&mut browse, &["Alice.epub", "HISTORY"]);
        browse.move_by(1);
        browse.choose("HISTORY", Row::Folder);
        listed_count(&mut browse, 1);

        // The visible name changed, so the locator changed; the fallback row
        // is the answer, in the alias form as in the long one.
        assert!(browse.leave());
        listed_short(&mut browse, &["history", "Alice.epub"]);

        assert_eq!(browse.selection(), 1);
    }

    #[test]
    fn a_folder_rewritten_with_a_long_name_is_a_different_locator() {
        let mut browse = Browse::root();
        listed_short(&mut browse, &["Alice.epub", "\u{dc}BER"]);
        browse.move_by(1);
        browse.choose("\u{dc}BER", Row::Folder);
        listed_count(&mut browse, 1);

        // Written elsewhere while the reader was inside: the entry now shows
        // a different name, so the folder that was left is not in this
        // listing, whatever entry the bytes ended up in. The fallback row is
        // the answer.
        assert!(browse.leave());
        listed(&mut browse, &["\u{fc}ber", "Alice.epub"]);

        assert_eq!(browse.selection(), 1);
    }

    #[test]
    fn a_folder_that_lost_its_long_name_answers_as_an_alias() {
        let mut browse = Browse::root();
        listed(&mut browse, &["Alice.epub", "\u{dc}ber"]);
        browse.move_by(1);
        browse.choose("\u{dc}ber", Row::Folder);
        listed_count(&mut browse, 1);

        // Now a short-only entry, which the driver matches by uppercasing
        // ASCII alone, so the lowercase spelling is a different folder.
        assert!(browse.leave());
        listed_short(&mut browse, &["\u{fc}BER", "Alice.epub"]);

        assert_eq!(
            browse.selection(),
            1,
            "the row it was entered from, not a folder the resolver would \
             refuse to open under that name",
        );
    }

    #[test]
    fn each_level_of_a_return_is_judged_by_its_own_rows() {
        let mut browse = Browse::root();
        // A long-name folder holding a short-only one.
        listed(&mut browse, &["Alice.epub", "Hist\u{f2}ry"]);
        browse.move_by(1);
        browse.choose("Hist\u{f2}ry", Row::Folder);
        listed_short(&mut browse, &["Bede.epub", "\u{dc}BER"]);
        browse.move_by(1);
        browse.choose("\u{dc}BER", Row::Folder);
        listed_count(&mut browse, 1);

        // Out of the short-only inner folder. The lowercase spelling is a
        // different name, so a different locator, and the selection stays on
        // the row it was entered from.
        assert!(browse.leave());
        listed_short(&mut browse, &["\u{fc}ber", "Bede.epub"]);
        assert_eq!(browse.selection(), 1);

        // Out of the long-name outer folder: the recased spelling is a
        // different locator by the same rule, form notwithstanding.
        assert!(browse.leave());
        listed(&mut browse, &["HIST\u{d2}RY", "Alice.epub"]);
        assert_eq!(browse.selection(), 1);
    }

    #[test]
    fn a_fallback_row_past_the_end_is_still_clamped() {
        let mut browse = Browse::root();
        listed(&mut browse, &SHELF);
        browse.move_by(2);
        browse.choose("Rome.epub", Row::Folder);
        listed_count(&mut browse, 1);

        assert!(browse.leave());
        listed(&mut browse, &["Alice.epub"]);

        assert_eq!(browse.selection(), 0);
    }

    #[test]
    fn leaving_the_root_says_so_rather_than_moving() {
        let mut browse = Browse::root();
        assert!(!browse.leave());
        assert!(browse.is_root());
    }

    #[test]
    fn choosing_a_book_hands_back_its_locator() {
        let mut browse = Browse::root();
        listed_count(&mut browse, 3);
        browse.choose("History", Row::Folder);
        listed_count(&mut browse, 2);

        let chosen = browse.choose("Rome.epub", Row::Book);
        assert_eq!(
            chosen,
            Chosen::Open(LibraryPath::parse("History/Rome.epub").expect("parse")),
        );
        assert_eq!(
            browse.path().as_str(),
            "History",
            "opening a book leaves the reader where they were",
        );
    }

    #[test]
    fn a_row_with_no_locator_is_refused_rather_than_followed() {
        let mut browse = Browse::root();
        listed_count(&mut browse, 1);
        for _ in 0..MAX_DEPTH {
            assert_eq!(browse.choose("Deeper", Row::Folder), Chosen::Entered);
            listed_count(&mut browse, 1);
        }

        assert_eq!(
            browse.choose("TooFar", Row::Folder),
            Chosen::Refused(PathError::TooDeep),
        );
        assert_eq!(browse.path().depth(), MAX_DEPTH, "and nothing moved",);
    }

    #[test]
    fn the_selection_stops_at_both_ends() {
        let mut browse = Browse::root();
        listed_count(&mut browse, 3);

        browse.move_by(-1);
        assert_eq!(browse.selection(), 0, "no wrapping off the top");
        browse.move_by(99);
        assert_eq!(browse.selection(), 2, "or off the bottom");
    }

    #[test]
    fn an_empty_folder_has_nothing_selected() {
        let mut browse = Browse::root();
        listed_count(&mut browse, 0);
        browse.move_by(4);
        assert_eq!(browse.selection(), 0);
    }

    #[test]
    fn a_shorter_listing_pulls_the_selection_back() {
        let mut browse = Browse::root();
        listed_count(&mut browse, 10);
        browse.move_by(9);

        // The card changed underneath and the folder is listed again.
        listed_count(&mut browse, 4);
        assert_eq!(
            browse.selection(),
            3,
            "a selection past the end would open whatever is last",
        );
    }

    #[test]
    fn resetting_forgets_where_it_had_been() {
        let mut browse = Browse::root();
        listed_count(&mut browse, 2);
        browse.choose("Fiction", Row::Folder);
        browse.reset();

        assert!(browse.is_root());
        assert_eq!(browse.selection(), 0);
        assert!(!browse.leave(), "and the trail went with it");
    }
}
