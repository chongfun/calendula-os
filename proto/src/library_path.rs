//! Canonical locators for books and folders inside the library.
//!
//! The card's own hierarchy is the library, so a locator is a path. It is
//! stored root-relative, meaning relative to the library root rather than the
//! card root, because the root is a product decision that may move and a
//! locator that hard-coded it would have to be rewritten if it did.
//!
//! Structurally normalized on the way in, so a stored locator cannot mean
//! something else later:
//!
//! - `/` separates components, with no trailing or repeated ones; `parse`
//!   accepts one leading separator and normalizes it away, so a stored
//!   locator carries none;
//! - `.` and `..` are refused rather than resolved, since a stored `..` is a
//!   locator that can climb out of the library;
//! - characters FAT reserves, and control characters, are refused.
//!
//! Spelling is preserved exactly and equality is exact UTF-8 equality, so a
//! locator names exactly the directory entry it was obtained from. FAT's
//! case-insensitive equivalence is deliberately not reproduced: a locator is
//! where a copy was observed rather than an identity, and sameness across
//! renames belongs to library identity.
//!
//! The bounds exist because a locator ends up in fixed-size records. A path
//! deeper or longer than these is refused rather than truncated, since a
//! truncated locator names a different file.

use heapless::String;

/// Separator between components, on the card and in a serialized locator.
pub const SEPARATOR: char = '/';
/// Deepest a book may sit below the library root.
pub const MAX_DEPTH: usize = 8;
/// Longest a serialized locator may be.
pub const MAX_PATH_BYTES: usize = 256;
/// Longest one component may be. Shorter than a full VFAT name, which can
/// reach 255 UTF-16 units, so a locator stays inside its record.
pub const MAX_COMPONENT_BYTES: usize = 128;

/// Which directory a locator is relative to.
///
/// A locator is library-root-relative, and that is the whole point of it: the
/// library root can move without rewriting one. Loose EPUBs copied to the
/// card root predate the library root and stay readable, so they get a root
/// of their own rather than a locator that starts by naming the library
/// directory. Spelling the library root into the path would give the type two
/// coordinate systems, and would spend one of `MAX_DEPTH` levels saying which.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BookRoot {
    /// The browsable hierarchy the reader organizes.
    #[default]
    Library,
    /// The card root, where books copied before the library root existed sit.
    /// Nothing is written there, and nothing nests there.
    CardRoot,
}

/// Characters FAT reserves. `/` is the separator and is split on before a
/// component is checked, so seeing one inside a component means it was
/// escaped or doubled.
const RESERVED: [char; 9] = ['"', '*', '/', ':', '<', '>', '?', '\\', '|'];

/// Why a path could not be a locator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathError {
    /// A repeated or trailing separator, which leaves an otherwise-empty
    /// component. One leading separator is accepted and normalized away.
    EmptyComponent,
    /// `.` or `..`, which a stored locator may not contain.
    RelativeComponent,
    /// More components than [`MAX_DEPTH`].
    TooDeep,
    /// Longer than [`MAX_PATH_BYTES`] once normalized.
    TooLong,
    /// A component longer than [`MAX_COMPONENT_BYTES`].
    ComponentTooLong,
    /// A character FAT reserves, or a control character.
    IllegalCharacter,
}

/// Where a book or folder sits, relative to the library root.
///
/// The empty path is the root itself.
///
/// Equality is exact, deliberately, and so is resolution: the resolver
/// compares the displayed long name, or the rendered alias when there is no
/// long name, exactly as enumeration produced it, and then opens the entry
/// through its transient 8.3 alias. Reproducing the filesystem's forgiving
/// name equivalence in a durable locator imported the driver's exact
/// Unicode case semantics into this crate, and every divergence between the
/// copy and the driver was a wrong book; exactness deletes the whole
/// question. Two entries whose names differ only by case are two locators,
/// as they are two directory entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibraryPath {
    text: String<MAX_PATH_BYTES>,
}

impl LibraryPath {
    /// The library root.
    pub const fn root() -> Self {
        Self {
            text: String::new(),
        }
    }

    /// Parse and normalize, accepting an optional leading separator so a
    /// caller can hand over what a user typed or what a record stored.
    pub fn parse(text: &str) -> Result<Self, PathError> {
        let trimmed = text.strip_prefix(SEPARATOR).unwrap_or(text);
        let mut path = Self::root();
        if trimmed.is_empty() {
            return Ok(path);
        }
        for component in trimmed.split(SEPARATOR) {
            path.push(component)?;
        }
        Ok(path)
    }

    /// This path with one more component below it.
    pub fn child(&self, component: &str) -> Result<Self, PathError> {
        let mut next = self.clone();
        next.push(component)?;
        Ok(next)
    }

    /// The path one level up, or `None` at the root.
    pub fn parent(&self) -> Option<Self> {
        let at = self.text.rfind(SEPARATOR);
        match at {
            Some(at) => Some(Self {
                text: String::try_from(&self.text[..at]).ok()?,
            }),
            None if self.text.is_empty() => None,
            None => Some(Self::root()),
        }
    }

    /// The last component, or `None` at the root.
    pub fn file_name(&self) -> Option<&str> {
        if self.text.is_empty() {
            return None;
        }
        match self.text.rfind(SEPARATOR) {
            Some(at) => Some(&self.text[at + 1..]),
            None => Some(self.text.as_str()),
        }
    }

    /// Components from the root down.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.text
            .split(SEPARATOR)
            .filter(|component| !component.is_empty())
    }

    pub fn depth(&self) -> usize {
        self.components().count()
    }

    pub fn is_root(&self) -> bool {
        self.text.is_empty()
    }

    /// The normalized locator, without a leading separator.
    pub fn as_str(&self) -> &str {
        self.text.as_str()
    }

    fn push(&mut self, component: &str) -> Result<(), PathError> {
        if component.is_empty() {
            return Err(PathError::EmptyComponent);
        }
        if component == "." || component == ".." {
            return Err(PathError::RelativeComponent);
        }
        if component.len() > MAX_COMPONENT_BYTES {
            return Err(PathError::ComponentTooLong);
        }
        if component
            .chars()
            .any(|c| c.is_control() || RESERVED.contains(&c))
        {
            return Err(PathError::IllegalCharacter);
        }
        if self.depth() + 1 > MAX_DEPTH {
            return Err(PathError::TooDeep);
        }
        if !self.text.is_empty() {
            self.text.push(SEPARATOR).map_err(|_| PathError::TooLong)?;
        }
        self.text
            .push_str(component)
            .map_err(|_| PathError::TooLong)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::{format, string::String as StdString, vec::Vec};

    use super::*;

    #[test]
    fn a_locator_is_stored_root_relative() {
        let path = LibraryPath::parse("/History/Rome/SPQR.epub").expect("parse");
        assert_eq!(path.as_str(), "History/Rome/SPQR.epub");
        assert_eq!(
            path,
            LibraryPath::parse("History/Rome/SPQR.epub").expect("parse"),
            "the leading separator is accepted and normalized away",
        );
    }

    #[test]
    fn the_empty_path_is_the_root() {
        for text in ["", "/"] {
            let path = LibraryPath::parse(text).expect("parse");
            assert!(path.is_root());
            assert_eq!(path.depth(), 0);
            assert_eq!(path.file_name(), None);
            assert_eq!(path.parent(), None);
        }
    }

    #[test]
    fn components_walk_from_the_root_down() {
        let path = LibraryPath::parse("Fiction/Dune.epub").expect("parse");
        assert_eq!(
            path.components().collect::<Vec<_>>(),
            ["Fiction", "Dune.epub"]
        );
        assert_eq!(path.file_name(), Some("Dune.epub"));
        assert_eq!(path.depth(), 2);

        let parent = path.parent().expect("has a parent");
        assert_eq!(parent.as_str(), "Fiction");
        assert_eq!(parent.parent().expect("root").as_str(), "");
    }

    #[test]
    fn a_child_extends_a_path() {
        let folder = LibraryPath::parse("History").expect("parse");
        let book = folder.child("Rome.epub").expect("child");
        assert_eq!(book.as_str(), "History/Rome.epub");
        assert_eq!(folder.as_str(), "History", "the parent is left alone");
    }

    #[test]
    fn a_locator_compares_exactly_and_keeps_its_spelling() {
        let one = LibraryPath::parse("Fiction/Dune.epub").expect("parse");
        let other = LibraryPath::parse("FICTION/dune.EPUB").expect("parse");
        assert_ne!(
            one, other,
            "a path cannot tell which components are long entries and which \
             are short ones, and the driver compares those differently, so it \
             declines to guess",
        );
        assert_eq!(
            one.as_str(),
            "Fiction/Dune.epub",
            "the case it was given is kept",
        );
        // Resolution is exact too; see the walk's own tests.
    }

    #[test]
    fn a_locator_is_not_equal_to_one_that_expands_to_it() {
        let dotted = LibraryPath::parse("\u{130}.epub").expect("parse");
        let expanded = LibraryPath::parse("i\u{307}.epub").expect("parse");
        assert_ne!(dotted, expanded);
    }

    #[test]
    fn relative_components_are_refused() {
        for text in ["../Dune.epub", "Fiction/../Dune.epub", "./Dune.epub"] {
            assert_eq!(
                LibraryPath::parse(text),
                Err(PathError::RelativeComponent),
                "{text} would let a stored locator climb out of the library",
            );
        }
    }

    #[test]
    fn empty_components_are_refused() {
        for text in ["Fiction//Dune.epub", "Fiction/", "//"] {
            assert_eq!(
                LibraryPath::parse(text),
                Err(PathError::EmptyComponent),
                "{text}"
            );
        }
    }

    #[test]
    fn reserved_and_control_characters_are_refused() {
        for text in [
            "Fic:tion/Dune.epub",
            "Dune?.epub",
            "back\\slash",
            "bell\u{7}",
        ] {
            assert_eq!(
                LibraryPath::parse(text),
                Err(PathError::IllegalCharacter),
                "{text}",
            );
        }
    }

    #[test]
    fn the_depth_bound_is_enforced() {
        let deep = "a/".repeat(MAX_DEPTH) + "Dune.epub";
        assert_eq!(LibraryPath::parse(&deep), Err(PathError::TooDeep));

        let at_limit = "a/".repeat(MAX_DEPTH - 1) + "Dune.epub";
        let path = LibraryPath::parse(&at_limit).expect("the limit itself fits");
        assert_eq!(path.depth(), MAX_DEPTH);
    }

    #[test]
    fn the_length_bounds_are_enforced() {
        let long_component = "x".repeat(MAX_COMPONENT_BYTES + 1);
        assert_eq!(
            LibraryPath::parse(&long_component),
            Err(PathError::ComponentTooLong),
        );

        // Two components inside the component bound, over the path bound.
        let long_path: StdString = format!(
            "{}/{}",
            "x".repeat(MAX_COMPONENT_BYTES),
            "y".repeat(MAX_COMPONENT_BYTES)
        );
        assert_eq!(LibraryPath::parse(&long_path), Err(PathError::TooLong));
    }

    #[test]
    fn a_refused_path_leaves_nothing_behind() {
        let folder = LibraryPath::parse("Fiction").expect("parse");
        assert!(folder.child("..").is_err());
        assert_eq!(folder.as_str(), "Fiction");
    }
}
