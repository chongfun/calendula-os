//! Low-priority, single-EPUB metadata discovery. Nothing here builds a
//! reading cache, modifies reading positions, or holds an SD session across
//! background slices.
use crate::book_build::{ReaderCacheScratch, SdFileReadAt};
use crate::card::Card;
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use heapless::String;
use proto::epub::{EpubError, ZipError, ZipStream};
use proto::library_path::LibraryPath;

/// A single row is one background operation. Only transient I/O gets retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Skipped,
    Updated,
    Retry,
}

/// Called only when there is no progressive build, page-image decode, or
/// pending position resolution. The caller retires title-index and browse
/// proofs before the first catalog write, including writes that report error.
#[inline(never)]
pub fn probe_row(
    card: &mut impl Card,
    row: usize,
    scratch: &mut ReaderCacheScratch<'_>,
    before_write: impl FnOnce(),
) -> ProbeOutcome {
    card.with_root(|root| probe_with_root(root, row, scratch, before_write))
        .unwrap_or(ProbeOutcome::Retry)
}

#[inline(never)]
fn probe_with_root<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    row: usize,
    scratch: &mut ReaderCacheScratch<'_>,
    before_write: impl FnOnce(),
) -> ProbeOutcome
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some(record) = crate::library_sd::read_catalog_record_at(root, row) else {
        return ProbeOutcome::Retry;
    };
    if !record.title_source.needs_probe() || record.book_id.is_none() {
        return ProbeOutcome::Skipped;
    }
    let Some(at) = record.root else {
        return ProbeOutcome::Skipped;
    };
    let Ok(locator) = LibraryPath::parse(record.path.as_str()) else {
        return ProbeOutcome::Skipped;
    };
    if locator.as_str() != record.path.as_str() {
        return ProbeOutcome::Skipped;
    }
    let opened = upload_store::library::with_book_at(root, at, &locator, |dir, alias| {
        let file = match dir.open_file_in_dir(alias, Mode::ReadOnly) {
            Ok(file) => file,
            // The book may have been removed since the catalog was scanned.
            Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
            // Do not turn transient card failures into permanent row skips.
            Err(_) => return Err(ProbeOutcome::Retry),
        };
        // A replaced file is not the copy this catalog row described.
        // Never mark it NoMetadataTitle, even when its ZIP is invalid.
        if file.length() != record.byte_size {
            return Ok(None);
        }
        Ok(Some(read_metadata(file, scratch)))
    });
    let mut metadata = match opened {
        Ok(Some(Ok(Some(Ok(metadata))))) => metadata,
        // A replaced/deleted file is not evidence that a stale row may be
        // marked complete. The next catalog reconciliation will fix it.
        Ok(None) | Ok(Some(Ok(None))) => return ProbeOutcome::Skipped,
        Ok(Some(Err(outcome))) => return outcome,
        Ok(Some(Ok(Some(Err(EpubError::Zip(ZipError::Io)))))) | Err(_) => {
            return ProbeOutcome::Retry;
        }
        // Malformed / unsupported books are settled for this catalog only.
        Ok(Some(Ok(Some(Err(_))))) => proto::epub::LibraryMetadata::default(),
    };
    // The locator has a full filename even when the display label is only
    // 64 bytes. An explicit filename marker is a lower-priority fallback.
    if metadata.volume.is_empty() {
        if let Some(volume) = volume_from_locator(record.path.as_str()) {
            metadata.volume = volume;
        }
    }
    // A foreground open may have supplied its real title since this probe
    // began. The commit re-reads and verifies BookId + physical identity and
    // refuses to replace any source that no longer needs probing.
    let result = crate::library_sd::update_catalog_probe_metadata(
        root,
        row,
        &record,
        &metadata,
        before_write,
    );
    match result {
        crate::library_sd::CatalogTitleUpdate::Changed { .. } => ProbeOutcome::Updated,
        crate::library_sd::CatalogTitleUpdate::Unchanged => ProbeOutcome::Skipped,
        crate::library_sd::CatalogTitleUpdate::Failed => ProbeOutcome::Retry,
    }
}

/// Prefer a complete locator filename to the truncated display_name; EPUB
/// suffix matching is case-insensitive, including mixed-case extensions.
fn volume_from_locator(locator: &str) -> Option<String<{ proto::catalog::CATALOG_VOLUME_BYTES }>> {
    let filename = locator.rsplit('/').next()?;
    let stem = [".epub", ".epu"].iter().find_map(|extension| {
        let end = filename.len().checked_sub(extension.len())?;
        filename
            .get(end..)
            .filter(|suffix| suffix.eq_ignore_ascii_case(extension))
            .map(|_| &filename[..end])
    })?;
    proto::epub::explicit_volume(stem)
}

/// The same ZIP/container/OPF streaming utilities as book opens, with a much
/// smaller read budget and a 256 KiB decompressed OPF ceiling. The EPUB's
/// chapters, images and nav are never visited.
#[inline(never)]
fn read_metadata<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    scratch: &mut ReaderCacheScratch<'_>,
) -> Result<proto::epub::LibraryMetadata, EpubError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut zip =
        ZipStream::new(SdFileReadAt::for_metadata(file), scratch.tail).map_err(EpubError::from)?;
    let mut opf_path = String::<256>::new();
    proto::epub::load_container_xml_and_find_opf_path(
        &mut zip,
        scratch.header,
        scratch.name,
        scratch.compressed,
        scratch.container,
        scratch.zip_inflate,
        &mut opf_path,
    )?;
    let entry = zip
        .find_entry(opf_path.as_str(), scratch.header, scratch.name)
        .map_err(EpubError::from)?;
    if entry.uncompressed_size > 256 * 1024 {
        return Err(EpubError::MetadataTooLarge);
    }
    proto::epub::parse_opf_library_metadata_streamed(|feed| {
        zip.read_entry_to_sink(
            entry,
            scratch.compressed,
            scratch.xhtml,
            scratch.zip_inflate,
            &mut |chunk: &[u8]| feed(chunk).map_err(|_| ZipError::Inflate),
        )
        .map_err(EpubError::from)
    })
}

/// Probe completion lives in the catalog's source byte, not in RAM.
/// A v13 rescan marks cached-title rows BookCache and probes them as well,
/// so books opened before the upgrade also acquire series/volume fields.

#[derive(Clone, Copy, Debug)]
pub struct MetadataDiscovery {
    pub epoch: u32,
    pub next_row: usize,
    /// Failures opening or validating the catalog itself. These control
    /// retry backoff, but must never consume an individual row's budget.
    pub catalog_failed_reads: u8,
    /// Consecutive read/probe failures for the current catalog row only.
    pub failed_reads: u8,
    /// The first write must retire the old TIX4 proof and browse addresses.
    pub retired: bool,
    pub titles_found: u32,
    pub rows_skipped: u32,
}

impl MetadataDiscovery {
    pub fn new(epoch: u32) -> Self {
        Self {
            epoch,
            next_row: 0,
            catalog_failed_reads: 0,
            failed_reads: 0,
            retired: false,
            titles_found: 0,
            rows_skipped: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::volume_from_locator;

    #[test]
    fn volume_fallback_uses_the_full_locator_and_mixed_case_extension() {
        assert_eq!(
            volume_from_locator("Light Novels/An extremely long title that passes the catalog label limit (Vol. 14).ePuB")
                .unwrap()
                .as_str(),
            "14"
        );
        assert_eq!(
            volume_from_locator("Library/Series Volume 3.ePu")
                .unwrap()
                .as_str(),
            "3"
        );
        assert!(volume_from_locator("Library/86.epub").is_none());
        assert!(volume_from_locator("Library/Another 2026.epub").is_none());
    }
}
