//! A book's images on the card: its `IMAGES.BIN` (see
//! `proto::image::index`), built at the first open from the ZIP's central
//! directory and probed then, and the lookup each image in a chapter makes
//! as the content stream is captured.
//!
//! The index sits beside the book index in the book's cache directory, so
//! it goes wherever that directory goes, and it does not depend on type
//! settings or orientation, so it survives every relayout. Every image is
//! probed when the index is built, while the inflater and the XHTML window
//! are idle: the capture of the content stream is single-pass, so a chapter
//! cannot be laid out again once an image's size is known, as upstream's
//! layout-time probe allows.

use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use proto::epub::{EpubZipOps, OwnedZipEntry, ZipError, ZipInflateScratch};
use proto::image::index::{
    find_image, open_image_index, read_image, scan_images, write_image_state, BuildStep,
    DecodeState, DecodeStatus, ImageEntry, ImageIndexBuilder, ImageIndexHeader, ImagePlace,
    ImageProbe, ImageProber, IndexError, ProbeStatus, IMAGE_ENTRIES_AT, IMAGE_INDEX_FILE,
};
use proto::image::pack::{pack_file, SPOOL_FILE};
use proto::image::paragraph::{ImageLookup, Lookup};
use proto::image::path::{path_hash, resolve_image_path};
use proto::image::token::ContentImage;
use reader_cache::files::{image_index_key, IndexFile};
use ui::reading::{fit_image, fit_inline_image, fits_inline, PageBox};

/// Bytes a probe reads at a time: a stored image's, or a deflated one's
/// compressed input.
const PROBE_INPUT_BYTES: usize = 512;
/// Most bytes a probe reads, or inflates, before it calls the image
/// damaged. A PNG's header is its first 33 bytes; a JPEG's frame header
/// can follow large APPn segments (an EXIF thumbnail can be 64 KB), which
/// a stored image seeks past but a deflated one inflates and drops.
pub const PROBE_INPUT_CAP: u32 = 256 * 1024;
/// The part of the XHTML window a deflated probe inflates into; index
/// entries are read into the rest.
const PROBE_OUTPUT_BYTES: usize = 512;

/// The book's open image index, for one build or decode session.
pub struct BookImages<'v, D, T, const ND: usize, const NF: usize, const NV: usize>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: File<'v, D, T, ND, NF, NV>,
    header: ImageIndexHeader,
}

/// Why probing stopped short. The probes before it are kept, and the
/// images after it are probed at the next open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeStop {
    /// A read of the book failed: nothing is recorded for the image, so it
    /// is tried again.
    Archive,
    /// The index could not be read or written.
    Index,
}

/// The card would not read or write the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardFault;

/// Gives an entry the inline mark (`ImageEntry::inline`), and forgets its
/// decode in each orientation whose size that changes: one whose inline and
/// block sizes are the same keeps it. True when one was forgotten.
pub fn mark_entry_inline(entry: &mut ImageEntry) -> bool {
    let mut forgot = false;
    for portrait in [false, true] {
        let block = decoded_size(entry, portrait);
        entry.inline = true;
        let changes = decoded_size(entry, portrait) != block;
        entry.inline = false;
        if changes && entry.decode(portrait).status == DecodeStatus::Decoded {
            entry.set_decode(portrait, DecodeState::default());
            entry.wanted = true;
            forgot = true;
        }
    }
    entry.inline = true;
    forgot
}

/// The size an image decodes at in an orientation: its inline size once
/// layout has marked it inline (`ImageEntry::inline`), else its block size.
/// Layout fitted it from the same source size and page box.
pub fn decoded_size(entry: &ImageEntry, portrait: bool) -> (u16, u16) {
    let page_box = PageBox::for_portrait(portrait);
    let (width, height) = (entry.width.into(), entry.height.into());
    if entry.inline {
        fit_inline_image(page_box, width, height)
    } else {
        fit_image(page_box, width, height)
    }
}

impl<'v, D, T, const ND: usize, const NF: usize, const NV: usize> BookImages<'v, D, T, ND, NF, NV>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    pub fn header(&self) -> ImageIndexHeader {
        self.header
    }

    /// The source identity the index is keyed for.
    pub fn source(&self) -> (u32, u32) {
        (self.header.key.source_hash, self.header.key.source_size)
    }

    /// The entry in `slot`, as an image token names it; `None` when it
    /// fails its checks, and the image is then treated as missing. Err only
    /// when the card fails.
    pub fn read(&self, slot: u16) -> Result<Option<ImageEntry>, CardFault> {
        match read_image(&mut IndexFile(&self.file), &self.header, slot) {
            Ok(found) => Ok(found),
            Err(IndexError::Io) => Err(CardFault),
            Err(_) => Ok(None),
        }
    }

    /// Stores an entry's state in one write.
    pub fn write_state(&self, slot: u16, entry: &ImageEntry) -> Result<(), CardFault> {
        write_image_state(&mut IndexFile(&self.file), &self.header, slot, entry)
            .map_err(|_| CardFault)
    }

    /// Marks the image in `slot` as shown inside a line at a size other
    /// than its block size (`ImageEntry::inline`), in one write, so it
    /// decodes at its inline size from now on. A decode it already has at
    /// its block size is forgotten in the same write, in each orientation
    /// where the sizes differ (its record becomes an orphan in the pack),
    /// and done again; true when one was. Err only when the card fails.
    #[inline(never)]
    pub fn mark_inline(&self, slot: u16) -> Result<bool, CardFault> {
        let io = &mut IndexFile(&self.file);
        let mut entry = match read_image(io, &self.header, slot) {
            Ok(Some(entry)) if !entry.inline => entry,
            Err(IndexError::Io) => return Err(CardFault),
            _ => return Ok(false),
        };
        let forgot = mark_entry_inline(&mut entry);
        write_image_state(io, &self.header, slot, &entry).map_err(|_| CardFault)?;
        Ok(forgot)
    }

    /// Bytes of the orientation's pack that decoded images' records take:
    /// what a pack holding only them would be. Reads the index a run of
    /// entries at a time through `buf`.
    pub fn live_pack_bytes(&self, portrait: bool, buf: &mut [u8]) -> Result<u32, CardFault> {
        let mut live = 0u32;
        self.scan(buf, |_, entry| {
            if entry.decode(portrait).status == DecodeStatus::Decoded {
                let (width, height) = decoded_size(&entry, portrait);
                let record = proto::image::pack::PackRecord::new(entry.path_hash, width, height);
                live = live.saturating_add(record.map_or(0, |record| record.record_len()));
            }
            true
        })?;
        Ok(live)
    }

    /// Marks every image decoded in the orientation as not decoded again,
    /// one write each: before its pack is deleted, so nothing is left
    /// pointing into it. Images that failed stay failed.
    pub fn forget_decodes(&self, portrait: bool, buf: &mut [u8]) -> Result<(), CardFault> {
        scan_images(
            &mut IndexFile(&self.file),
            &self.header,
            0..self.header.entry_count,
            buf,
            |io, slot, mut entry| {
                if entry.decode(portrait).status == DecodeStatus::Decoded {
                    entry.set_decode(portrait, DecodeState::default());
                    entry.wanted = true;
                    write_image_state(io, &self.header, slot, &entry)?;
                }
                Ok(true)
            },
        )
        .map_err(|_| CardFault)
    }

    /// Calls `visit` with every entry that passes its checks, reading a run
    /// of entries at a time through `buf`, until it returns false. Err only
    /// when the card fails.
    pub fn scan(
        &self,
        buf: &mut [u8],
        mut visit: impl FnMut(u16, ImageEntry) -> bool,
    ) -> Result<(), CardFault> {
        scan_images(
            &mut IndexFile(&self.file),
            &self.header,
            0..self.header.entry_count,
            buf,
            |_, slot, entry| Ok(visit(slot, entry)),
        )
        .map_err(|_| CardFault)
    }

    /// Whether any entry is still unprobed, reading the index through
    /// `buf`. A card that will not answer counts as having some.
    pub fn has_unprobed(&self, buf: &mut [u8]) -> bool {
        let mut unprobed = false;
        let scanned = self.scan(buf, |_, entry| {
            unprobed = entry.probe == ProbeStatus::Unprobed;
            !unprobed
        });
        unprobed || scanned.is_err()
    }

    /// Probes every entry not probed yet and stores each result in its
    /// entry as one write. A probe reads the image's local header by its
    /// stored offset, checks that it names the image, then reads on until
    /// it can judge it (`ImageProber`): a PNG's first 33 bytes, or a JPEG's
    /// segments up to its frame header. A stored image is read a block at
    /// a time and seeks past segments the probe does not read; a deflated
    /// one is inflated that far and the rest dropped. No more than
    /// [`PROBE_INPUT_CAP`] bytes are read, or inflated, either way. An image
    /// whose archive entry or header is damaged, or that has not said what
    /// it is by the cap, is marked failed and keeps its alt text. A failed
    /// read records nothing and stops the probing, so the image is probed
    /// again at the next open.
    ///
    /// `window` (the XHTML window) holds a deflated probe's output and runs
    /// of index entries, `compressed` the bytes read, and `inflate` the
    /// stream: all idle before a build's first spine. Returns how many it
    /// probed.
    #[inline(never)]
    pub fn probe_all<Z: EpubZipOps>(
        &self,
        zip: &mut Z,
        compressed: &mut [u8],
        window: &mut [u8],
        inflate: &mut ZipInflateScratch,
    ) -> Result<usize, ProbeStop> {
        let input_len = compressed.len().min(PROBE_INPUT_BYTES);
        let input = &mut compressed[..input_len];
        let Some((output, entries)) = window.split_at_mut_checked(PROBE_OUTPUT_BYTES) else {
            return Err(ProbeStop::Index);
        };
        let mut probed = 0;
        let mut stop = None;
        let mut io = IndexFile(&self.file);
        scan_images(
            &mut io,
            &self.header,
            0..self.header.entry_count,
            entries,
            |io, slot, mut entry| {
                if entry.probe != ProbeStatus::Unprobed {
                    return Ok(true);
                }
                let source = OwnedZipEntry {
                    compression_method: entry.method,
                    compressed_size: entry.compressed_size,
                    uncompressed_size: entry.uncompressed_size,
                    local_header_offset: entry.local_header_offset,
                };
                let path_hash = entry.path_hash;
                let mut prober = ImageProber::new();
                let read = zip.visit_entry_start(
                    source,
                    &mut |name| {
                        core::str::from_utf8(name)
                            .is_ok_and(|name| proto::image::path::path_hash(name) == path_hash)
                    },
                    input,
                    output,
                    inflate,
                    PROBE_INPUT_CAP,
                    &mut prober,
                );
                entry.set_probe(match read {
                    Ok(()) => prober.finish(),
                    Err(ZipError::Io) => {
                        stop = Some(ProbeStop::Archive);
                        return Ok(false);
                    }
                    // Damage the archive holds: the same next time, so it
                    // must not hold the book back.
                    Err(_) => ImageProbe {
                        status: ProbeStatus::Failed,
                        ..ImageProbe::default()
                    },
                });
                write_image_state(io, &self.header, slot, &entry)?;
                probed += 1;
                Ok(true)
            },
        )
        .map_err(|_| ProbeStop::Index)?;
        match stop {
            Some(stop) => Err(stop),
            None => Ok(probed),
        }
    }
}

/// The lookup a chapter's images make against the book's index, as the
/// content stream is captured: `src` resolved against the chapter's ZIP
/// path into `path` (the ZIP name scratch, idle while the chapter
/// streams), hashed, and found by binary search on the card. A probed
/// image the decoder takes comes back shown, with whether it may go inside
/// a line (its block size is within the inline bound in every orientation,
/// so each decodes it at one size); anything else is missing, and its alt
/// text stands in. Each image met keeps the earliest place in reading
/// order it was met at (`ImageEntry::seen_at`), one write the first time,
/// so the decode pass can go nearest the reader first.
pub struct ChapterImages<'a, 'v, D, T, const ND: usize, const NF: usize, const NV: usize>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    images: &'a BookImages<'v, D, T, ND, NF, NV>,
    chapter: &'a str,
    path: &'a mut [u8],
    spine: u16,
    met: u16,
    /// A read or write of the index failed: the chapter stopped on a card
    /// fault, not on its content.
    card_failed: bool,
    /// Marking an image inline forgot a decode at its block size.
    forgot_decode: bool,
}

impl<'a, 'v, D, T, const ND: usize, const NF: usize, const NV: usize>
    ChapterImages<'a, 'v, D, T, ND, NF, NV>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    pub fn new(
        images: &'a BookImages<'v, D, T, ND, NF, NV>,
        chapter: &'a str,
        path: &'a mut [u8],
        spine: u16,
    ) -> Self {
        Self {
            images,
            chapter,
            path,
            spine,
            met: 0,
            card_failed: false,
            forgot_decode: false,
        }
    }

    /// Whether marking an image inline forgot a decode, so what the store
    /// noted as settled may need decoding again.
    pub fn forgot_decode(&self) -> bool {
        self.forgot_decode
    }

    /// Whether a lookup stopped the chapter because the card failed.
    pub fn card_failed(&self) -> bool {
        self.card_failed
    }
}

impl<D, T, const ND: usize, const NF: usize, const NV: usize> ImageLookup
    for ChapterImages<'_, '_, D, T, ND, NF, NV>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    #[inline(never)]
    fn look_up(&mut self, src: &str) -> Lookup {
        // Every pass meets the spine's images in the same order, so this is
        // the image's place in reading order.
        let at = ImagePlace {
            spine: self.spine,
            order: self.met,
        };
        self.met = self.met.saturating_add(1);
        let Ok(name) = resolve_image_path(self.chapter, src, self.path) else {
            return Lookup::Missing;
        };
        let io = &mut IndexFile(&self.images.file);
        let (slot, mut entry) = match find_image(io, &self.images.header, path_hash(name)) {
            Ok(Some(found)) => found,
            Ok(None) => return Lookup::Missing,
            Err(IndexError::Io) => {
                self.card_failed = true;
                return Lookup::Stop;
            }
            Err(_) => return Lookup::Missing,
        };
        let seen_at = entry.seen_at.map_or(at, |known| known.min(at));
        if entry.seen_at != Some(seen_at) {
            entry.seen_at = Some(seen_at);
            if write_image_state(io, &self.images.header, slot, &entry).is_err() {
                self.card_failed = true;
                return Lookup::Stop;
            }
        }
        match entry.probe {
            ProbeStatus::Supported if entry.width > 0 && entry.height > 0 => {
                let (width, height) = (entry.width.into(), entry.height.into());
                Lookup::Shown {
                    image: ContentImage {
                        slot,
                        width: entry.width,
                        height: entry.height,
                    },
                    inline: fits_inline(width, height),
                }
            }
            _ => Lookup::Missing,
        }
    }

    /// Marks the entry inline when a line shows it at a size other than its
    /// block size, so every decode of it, the idle passes' too, takes the
    /// size the line draws.
    fn shown_inline(&mut self, image: ContentImage) {
        let (width, height) = (image.width.into(), image.height.into());
        let differs = [PageBox::LANDSCAPE, PageBox::PORTRAIT]
            .into_iter()
            .any(|page_box| {
                fit_inline_image(page_box, width, height) != fit_image(page_box, width, height)
            });
        if !differs {
            return;
        }
        match self.images.mark_inline(image.slot) {
            Ok(forgot) => self.forgot_decode |= forgot,
            Err(CardFault) => self.card_failed = true,
        }
    }
}

/// Whether the book has images the index holds, from its ZIP's central
/// directory. A directory damaged part way counts as none: lookups by name
/// still find the entries before the damage, so the book builds as it
/// always did, with its images as alt text. A failed read is a card fault,
/// which would otherwise leave a cache that keeps the alt text for good.
pub fn zip_has_images<Z: EpubZipOps>(zip: &mut Z, buf: &mut [u8]) -> Result<bool, CardFault> {
    match zip.has_image_entries(buf) {
        Ok(found) => Ok(found),
        Err(ZipError::Io) => Err(CardFault),
        Err(_) => Ok(false),
    }
}

/// Opens the book's image index in its cache directory `dir` to decode or
/// draw images, never building it: an index no build has written yet
/// holds no image a decode could take.
pub fn open_existing_book_images<'d, D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &'d Directory<'_, D, T, ND, NF, NV>,
    source: (u32, u32),
    mode: Mode,
) -> Option<BookImages<'d, D, T, ND, NF, NV>>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = dir.open_file_in_dir(IMAGE_INDEX_FILE, mode).ok()?;
    let header = open_image_index(&mut IndexFile(&file), &image_index_key(source)).ok()?;
    Some(BookImages { file, header })
}

/// The book's cache directory, which holds its image index and packs:
/// `/READER/CACHE2/<key>`.
pub fn book_cache_dir<'a, D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &'a Directory<'_, D, T, ND, NF, NV>,
    key: &str,
) -> Option<Directory<'a, D, T, ND, NF, NV>>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut dir = root.open_dir(proto::cache::CACHE_ROOT_DIR).ok()?;
    dir.change_dir(proto::cache::CACHE_V2_DIR).ok()?;
    dir.change_dir(key).ok()?;
    Some(dir)
}

/// Opens the book's image index in its cache directory `dir` for a build,
/// first building it from the ZIP's central directory when there is no
/// valid one (none yet, an interrupted build, or one for another copy of
/// the book). The build streams the directory through `walk`, a run of
/// entries per read, and keeps its records in `scratch`: the compressed
/// scratch and the XHTML window, idle before a build's first spine. Slots
/// follow from the directory alone, so a rebuilt index names every image
/// as before, but every image is undecoded again: `rebuilt` says so.
///
/// `Ok(None)` when the ZIP's directory is damaged or the card cannot hold
/// the index: the build then lays images out as alt text, as it always did.
/// `Err` when a read or write fails, for the build to stop and be retried.
#[inline(never)]
pub fn open_book_images<'d, Z, D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &'d Directory<'_, D, T, ND, NF, NV>,
    source: (u32, u32),
    zip: &mut Z,
    walk: &mut [u8],
    scratch: &mut [u8],
    rebuilt: &mut bool,
) -> Result<Option<BookImages<'d, D, T, ND, NF, NV>>, CardFault>
where
    Z: EpubZipOps,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let index_key = image_index_key(source);
    let device_fault = |error: &embedded_sdmmc::Error<D::Error>| {
        matches!(error, embedded_sdmmc::Error::DeviceError(_))
    };
    match dir.open_file_in_dir(IMAGE_INDEX_FILE, Mode::ReadWriteAppend) {
        Ok(file) => match open_image_index(&mut IndexFile(&file), &index_key) {
            Ok(header) => {
                slog!("images: index holds {} image(s)", header.entry_count);
                return Ok(Some(BookImages { file, header }));
            }
            // A file too short for its header and fan-out is a build cut
            // short, which reads past the end: rebuilt, not a card fault.
            Err(IndexError::Io) if file.length() >= IMAGE_ENTRIES_AT => return Err(CardFault),
            Err(_) => slog!("images: index unusable; rebuilding"),
        },
        Err(error) if device_fault(&error) => return Err(CardFault),
        Err(_) => {}
    }
    // Every record in the old packs would be an orphan: delete them, and any
    // spool a power cut left, once the old index is empty on the card. One
    // that failed to open on a passing fault may be sound, and a cut would
    // leave it naming decodes in a deleted pack.
    let file = match dir.open_file_in_dir(IMAGE_INDEX_FILE, Mode::ReadWriteCreateOrTruncate) {
        Ok(file) => file,
        Err(error) if device_fault(&error) => return Err(CardFault),
        Err(_) => return Ok(None),
    };
    file.flush().map_err(|_| CardFault)?;
    for name in [pack_file(false), pack_file(true), SPOOL_FILE] {
        let _ = upload_store::remove_file_reclaiming_clusters(dir, name);
    }
    let Ok(mut builder) = ImageIndexBuilder::new(scratch, index_key) else {
        return Ok(None);
    };
    // Each pass takes the buckets the last one left, so at most one pass
    // per bucket.
    let header = loop {
        match zip.for_each_entry(walk, &mut |entry| {
            builder.offer(entry);
            true
        }) {
            Ok(_) => {}
            Err(ZipError::Io) => return Err(CardFault),
            Err(_) => return Ok(None),
        }
        match builder.end_pass(&mut IndexFile(&file)) {
            Ok(BuildStep::Done(header)) => break header,
            Ok(_) => {}
            Err(IndexError::Io) => return Err(CardFault),
            Err(_) => return Ok(None),
        }
    };
    slog!(
        "images: index built with {} image(s){}",
        header.entry_count,
        if header.truncated { ", truncated" } else { "" }
    );
    *rebuilt = true;
    Ok(Some(BookImages { file, header }))
}
