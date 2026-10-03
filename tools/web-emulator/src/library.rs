//! The demo shelf as the Library lists it: a small folder tree, each folder
//! listed the way the device lists a directory on the card. Books come
//! first and then folders, each A to Z.

use core::cmp::Ordering;

use crate::books::SHELF;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// A book, by its shelf index (the catalog index a pick answers with).
    Book(u16),
    /// A folder inside the one listed, by its name.
    Folder(&'static str),
}

impl Row {
    pub fn name(self) -> &'static str {
        match self {
            Row::Book(index) => SHELF[usize::from(index)].title,
            Row::Folder(name) => name,
        }
    }
}

/// The rows of the folder at `path`, which is `/`-separated from the root
/// and empty for the root itself.
pub fn listing(path: &str) -> Vec<Row> {
    let mut books: Vec<u16> = (0..SHELF.len() as u16)
        .filter(|&index| SHELF[usize::from(index)].folder == path)
        .collect();
    books.sort_by(|&a, &b| cmp_names(SHELF[usize::from(a)].title, SHELF[usize::from(b)].title));

    let mut folders: Vec<&'static str> = SHELF
        .iter()
        .filter_map(|book| child_folder(path, book.folder))
        .collect();
    folders.sort_by(|a, b| cmp_names(a, b));
    folders.dedup();

    books
        .into_iter()
        .map(Row::Book)
        .chain(folders.into_iter().map(Row::Folder))
        .collect()
}

/// How many folders deep `path` is: 0 at the root.
pub fn depth(path: &str) -> u8 {
    if path.is_empty() {
        0
    } else {
        path.split('/').count().min(usize::from(u8::MAX)) as u8
    }
}

/// The path of folder `name` inside `path`.
pub fn child_path(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}/{name}")
    }
}

/// `path` split into its parent and its own name; the root has neither.
pub fn split_last(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
}

/// The first folder below `path` on the way to `folder`, if `folder` lies
/// inside `path` at all.
fn child_folder(path: &str, folder: &'static str) -> Option<&'static str> {
    let below = if path.is_empty() {
        folder
    } else {
        folder.strip_prefix(path)?.strip_prefix('/')?
    };
    below.split('/').next().filter(|name| !name.is_empty())
}

/// The device's order for two names in one directory: ASCII case folded
/// first, then the exact bytes, so names that differ only in case still have
/// a fixed order. Mirrors `upload_store::library::cmp_child_names`.
fn cmp_names(a: &str, b: &str) -> Ordering {
    fn folded(name: &str) -> impl Iterator<Item = u8> + '_ {
        name.bytes().map(|byte| byte.to_ascii_lowercase())
    }
    folded(a).cmp(folded(b)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(path: &str) -> Vec<&'static str> {
        listing(path).into_iter().map(Row::name).collect()
    }

    #[test]
    fn folders_list_books_then_folders_a_to_z() {
        assert_eq!(
            names(""),
            [
                "A Christmas Carol",
                "Aesop's Fables",
                "Alice's Adventures in Wonderland",
                "The Gods of Pegana",
                "Science Fiction",
            ]
        );
        assert_eq!(
            names("Science Fiction"),
            ["A Princess of Mars", "Last and First Men", "H. G. Wells"]
        );
        assert_eq!(
            names("Science Fiction/H. G. Wells"),
            ["The Time Machine", "The War of the Worlds"]
        );
    }

    #[test]
    fn every_book_is_listed_once() {
        let mut found = Vec::new();
        let mut pending = vec![String::new()];
        while let Some(path) = pending.pop() {
            for row in listing(&path) {
                match row {
                    Row::Book(index) => found.push(index),
                    Row::Folder(name) => pending.push(child_path(&path, name)),
                }
            }
        }
        found.sort_unstable();
        assert_eq!(found, (0..SHELF.len() as u16).collect::<Vec<_>>());
    }

    #[test]
    fn names_sort_case_folded_then_exact() {
        assert_eq!(cmp_names("apple", "Banana"), Ordering::Less);
        assert_eq!(cmp_names("Apple", "apple"), Ordering::Less);
        assert_eq!(cmp_names("A Princess", "Aesop"), Ordering::Less);
    }

    #[test]
    fn paths_split_and_join() {
        assert_eq!(depth(""), 0);
        assert_eq!(depth("Science Fiction/H. G. Wells"), 2);
        assert_eq!(child_path("", "Science Fiction"), "Science Fiction");
        assert_eq!(
            split_last("Science Fiction/H. G. Wells"),
            ("Science Fiction", "H. G. Wells")
        );
        assert_eq!(split_last("Science Fiction"), ("", "Science Fiction"));
    }
}
