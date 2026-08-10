//! Book storage on the SD card, and the leftovers of an older way of doing it.
//!
//! The upload path itself lives in [`install`]: a book streams into scratch
//! space outside `/BOOKS` and is published by moving its directory entry, with
//! one journalled record covering the swap. Nothing here participates in that.
//!
//! What remains is the small shared vocabulary — removing a file without
//! leaking its cluster chain, comparing long names the way FAT does — and the
//! read side of the label sidecars.
//!
//! Those sidecars are history. Before uploads carried a VFAT long name, a book
//! on the card was an 8.3 alias and its real name lived in `XTEINK/LABELS`.
//! Books written that way are still on people's cards, so the catalog scan
//! still falls back to reading their labels, and deleting a book still clears
//! them. Nothing writes new ones.

#![no_std]
#![forbid(unsafe_code)]

pub mod install;

use embedded_sdmmc::{Directory, Mode, TimeSource};
use heapless::String;
use proto::cache::CACHE_ROOT_DIR;

/// Subdir under XTEINK holding one `<8.3-stem>.TXT` per book uploaded before
/// uploads carried a long name of their own, each with that book's real
/// filename. Read-only now: the catalog scan falls back to it for those older
/// books, and deleting a book clears whatever it left.
const LABELS_DIR: &str = "LABELS";

fn label_file_name(open_name: &str, out: &mut String<12>) {
    out.clear();
    let stem = open_name.split('.').next().unwrap_or(open_name);
    let _ = out.push_str(stem);
    let _ = out.push_str(".TXT");
}

/// The identity sidecar an older upload path wrote beside the label. Only
/// ever removed now.
fn identity_file_name(open_name: &str, out: &mut String<12>) {
    out.clear();
    let stem = open_name.split('.').next().unwrap_or(open_name);
    let _ = out.push_str(stem);
    let _ = out.push_str(".ID");
}

/// A directory handle keeps the volume manager's lifetime, not the borrow of
/// the parent it was opened through, so a file opened inside it can outlive
/// the handles walked to reach it.
fn open_or_make_dir<
    'a,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    parent: &Directory<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
) -> Result<Directory<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match parent.open_dir(name) {
        Ok(dir) => Ok(dir),
        Err(_) => {
            let _ = parent.make_dir_in_dir(name);
            parent.open_dir(name).map_err(|_| ())
        }
    }
}

/// Read an uploaded book's stashed label into `out`. Returns false (leaving
/// `out` untouched) when the book has no sidecar -- i.e. it wasn't uploaded, so
/// the caller falls back to the file-stem label.
pub fn read_upload_label<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
    out: &mut String<64>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(xteink) = root.open_dir(CACHE_ROOT_DIR) else {
        return false;
    };
    let Ok(labels) = xteink.open_dir(LABELS_DIR) else {
        return false;
    };
    let mut file_name = String::<12>::new();
    label_file_name(open_name, &mut file_name);
    let Ok(file) = labels.open_file_in_dir(file_name.as_str(), Mode::ReadOnly) else {
        return false;
    };
    let mut buf = [0u8; 64];
    let Ok(read) = file.read(&mut buf) else {
        return false;
    };
    let Ok(text) = core::str::from_utf8(&buf[..read]) else {
        return false;
    };
    if text.is_empty() {
        return false;
    }
    out.clear();
    let _ = out.push_str(text);
    true
}

/// The outcome of `remove_file_reclaiming_clusters`.
///
/// `Absent` means the file was already gone — the desired end state for
/// callers that only care that the name is free, but *not* proof that this
/// call deleted anything, which is why sidecar cleanup keys on `Removed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveStatus {
    Removed,
    Absent,
    Failed,
}

/// Remove a file without leaking its FAT cluster chain.
///
/// The pinned embedded-sdmmc delete only marks the directory entry deleted; it
/// does not release the file's clusters. `Mode::ReadWriteTruncate` calls
/// `truncate_cluster_chain`, which walks and frees the cluster chain and writes
/// the zeroed directory entry before returning (embedded-sdmmc d26892f,
/// `VolumeManager::open_file_in_dir`). A fault in that write-back leaves the
/// entry at its original length — the chain free is not visible until the entry
/// lands — so the file stays readable and identity-matched for the next re-upload
/// to retire.
///
/// Files only: opening a directory as a file fails, which would report the
/// delete as failed without attempting it. Directory entries hold no cluster
/// chain of their own to leak, so they stay on `delete_entry_in_dir`.
pub fn remove_file_reclaiming_clusters<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    directory: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
) -> RemoveStatus
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    {
        match directory.open_file_in_dir(name, Mode::ReadWriteTruncate) {
            Ok(file) => {
                if file.close().is_err() {
                    return RemoveStatus::Failed;
                }
            }
            Err(embedded_sdmmc::Error::NotFound) => return RemoveStatus::Absent,
            Err(_) => return RemoveStatus::Failed,
        }
    }
    match directory.delete_entry_in_dir(name) {
        Ok(()) => RemoveStatus::Removed,
        Err(embedded_sdmmc::Error::NotFound) => RemoveStatus::Absent,
        Err(_) => RemoveStatus::Failed,
    }
}

/// Remove `name` from the cache root, and report whether it is provably gone.
///
/// True means the card said so: removed, already absent, or no cache root to
/// hold it. False means the card would not answer, and the caller must treat
/// the file as still there.
///
/// The distinction matters for the catalog snapshot. A caller about to change
/// `/BOOKS` invalidates it first, because a clean install clears its journal
/// and a delete never writes one — so once the change is made, nothing on the
/// card would tell the next mount that the snapshot describes a shelf that
/// has moved.
pub fn clear_cache_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return true,
        Err(_) => return false,
    };
    remove_file_reclaiming_clusters(&cache_root, name) != RemoveStatus::Failed
}

pub fn delete_upload_sidecars<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(xteink) = root.open_dir(CACHE_ROOT_DIR) else {
        return;
    };
    let Ok(labels) = xteink.open_dir(LABELS_DIR) else {
        return;
    };
    let mut file_name = String::<12>::new();
    label_file_name(open_name, &mut file_name);
    let _ = remove_file_reclaiming_clusters(&labels, file_name.as_str());

    file_name.clear();
    identity_file_name(open_name, &mut file_name);
    let _ = remove_file_reclaiming_clusters(&labels, file_name.as_str());
}

/// The identity a pre-long-name upload recorded for the book at `open_name`.
///
/// `Ok(None)` is no usable sidecar — absent, or the wrong length, which no
/// retry will fix. [`install::InstallError::Card`] is a card that would not
/// answer, which must not be read as "this is not that book": that would
/// install a second copy beside the one already there.
pub fn read_upload_identity<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
) -> Result<Option<u64>, install::InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(install::InstallError::Card),
    };
    let labels = match xteink.open_dir(LABELS_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(install::InstallError::Card),
    };
    let mut file_name = String::<12>::new();
    identity_file_name(open_name, &mut file_name);
    let file = match labels.open_file_in_dir(file_name.as_str(), Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(install::InstallError::Card),
    };
    let mut buf = [0u8; 8];
    let parsed = proto::upload::parse_identity_read(file.length(), file.read(&mut buf), &buf);
    if file.close().is_err() {
        return Err(install::InstallError::Card);
    }
    parsed.map_err(|()| install::InstallError::Card)
}

/// Whether two VFAT long names denote the same file.
///
/// FAT ignores case over Unicode, so an ASCII-only comparison would let
/// `Märchen.epub` and `MÄRCHEN.epub` become two entries a computer sees as
/// one name twice. This is `core`'s simple case mapping, which matches how
/// the driver compares names when it refuses a duplicate. Simple mapping,
/// not full folding: the rare pairs that differ (ß/SS and friends) read as
/// distinct names, which is the safe direction — a missed match adds an
/// entry rather than deleting the wrong book.
fn same_long_name(left: &str, right: &str) -> bool {
    let mut left = left.chars().flat_map(char::to_lowercase);
    let mut right = right.chars().flat_map(char::to_lowercase);
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (a, b) if a != b => return false,
            _ => {}
        }
    }
}

/// Long-name scan buffer for finding the book that holds a name.
///
/// Sized against the upload name budget, not the FAT maximum: a name this
/// crate can produce is at most [`proto::upload::UPLOAD_FILENAME_BYTES`] (64) of UTF-8, so
/// 256 is generous, where a full 255-character FAT name would need roughly
/// three times it.
///
/// Undersizing is safe here because of how the pinned driver behaves: a long
/// name too large for the buffer comes back empty, so an oversized entry — a
/// hand-copied file with a very long name — is not recognised as the holder.
/// It cannot be a false negative, since a name that could equal ours is at
/// most 64 bytes and always decodes. Resize this if the comparison ever has
/// to handle names this crate did not write.
const LFN_SCAN_BYTES: usize = 256;
