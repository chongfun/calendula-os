use crate::layout;
use crate::store::{
    ReaderStore, EMPTY_BOOK_SECTION_RECORD, MAX_BOOK_SECTIONS, MAX_SD_TOC_ITEMS,
    MAX_SD_TOC_TEXT_BYTES,
};
use display::font::FontStyle;
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use heapless::String;
use proto::cache::{
    book_index_file_name, decode_block, decode_book_v2_header, decode_book_v2_section,
    decode_cover_header, decode_layout_config_registry, decode_page, decode_section_v2_header,
    decode_toc, decode_toc_chapter, decode_toc_file_header, encode_block, encode_book_v2_header,
    encode_book_v2_section, encode_content_header, encode_content_record_header,
    encode_layout_config_registry, encode_page, encode_section_v2_header, encode_toc,
    encode_toc_file_header, is_legacy_section_file_name, layout_cache_key,
    layout_key_of_book_index_file_name, layout_key_of_section_file_name, section_file_name,
    section_file_name_parts, BookV2Header, BookV2SectionRecord, ContentHeader, ContentRecordHeader,
    LayoutConfigRegistry, SectionV2Header, TocFileHeader, BLOCK_RECORD_BYTES,
    BOOK_INDEX_FILE_BYTES, BOOK_V2_HEADER_BYTES, BOOK_V2_SECTION_RECORD_BYTES, CACHE_BOOK_FILE,
    CACHE_CONFIG_BYTES, CACHE_CONFIG_FILE, CACHE_CONTENT_FILE, CACHE_COVER_FILE, CACHE_ROOT_DIR,
    CACHE_SECTIONS_DIR, CACHE_SECTION_FILE_BYTES, CACHE_STATE_FILE, CACHE_TOC_FILE, CACHE_V2_DIR,
    CONTENT_HEADER_BYTES, CONTENT_RECORD_HEADER_BYTES, COVER_HEADER_BYTES, PAGE_RECORD_BYTES,
    SECTION_V2_HEADER_BYTES, TOC_CHAPTER_RECORD_BYTES, TOC_FILE_HEADER_BYTES, TOC_RECORD_BYTES,
};
use proto::font_pack::{
    decode_font_pack_name, FontPackFaceRecord, FontPackHeader, FONT_PACK_DIR,
    FONT_PACK_FACE_RECORD_BYTES, FONT_PACK_FILE, FONT_PACK_HEADER_BYTES,
};

use proto::durable::{
    decode_durable_record, encode_durable_record, generation_is_newer, DURABLE_MAX_BYTES,
    DURABLE_OVERHEAD,
};

/// Read and validate one durable generation file; `payload` receives the
/// record body and the valid record's generation is returned. Any missing,
/// short, oversized, or corrupt file reads as `None`.
fn read_generation_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    directory: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
    magic: [u8; 4],
    payload: &mut [u8],
) -> Option<u32>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let total = payload.len().checked_add(DURABLE_OVERHEAD)?;
    if total > DURABLE_MAX_BYTES {
        return None;
    }
    let file = directory.open_file_in_dir(name, Mode::ReadOnly).ok()?;
    if file.length() as usize != total {
        return None;
    }
    let mut bytes = [0u8; DURABLE_MAX_BYTES];
    read_exact_file(&file, &mut bytes[..total]).ok()?;
    decode_durable_record(magic, &bytes[..total], payload)
}

/// Read the newest valid generation out of an A/B file pair into `payload`.
/// False means neither side holds a valid record.
fn read_two_generation<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    directory: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    names: [&str; 2],
    magic: [u8; 4],
    payload: &mut [u8],
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut other = [0u8; DURABLE_MAX_BYTES];
    let a = read_generation_file(directory, names[0], magic, payload);
    let b = read_generation_file(directory, names[1], magic, &mut other[..payload.len()]);
    match (a, b) {
        (None, None) => false,
        (Some(_), None) => true,
        (None, Some(_)) => {
            payload.copy_from_slice(&other[..payload.len()]);
            true
        }
        (Some(a), Some(b)) if generation_is_newer(b, a) => {
            payload.copy_from_slice(&other[..payload.len()]);
            true
        }
        (Some(_), Some(_)) => true,
    }
}

/// Write `payload` as the next generation of an A/B file pair, overwriting
/// the *older* side so the newest survivor is never the one mid-write, then
/// prove the write by re-reading it through the validating read path.
fn write_two_generation<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    directory: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    names: [&str; 2],
    magic: [u8; 4],
    payload: &[u8],
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut scratch = [0u8; DURABLE_MAX_BYTES];
    let a = read_generation_file(directory, names[0], magic, &mut scratch[..payload.len()]);
    let b = read_generation_file(directory, names[1], magic, &mut scratch[..payload.len()]);
    let (target, generation) = match (a, b) {
        (Some(a), Some(b)) if generation_is_newer(b, a) => (0, b.wrapping_add(1)),
        (Some(a), Some(_)) => (1, a.wrapping_add(1)),
        (Some(a), None) => (1, a.wrapping_add(1)),
        (None, Some(b)) => (0, b.wrapping_add(1)),
        (None, None) => (0, 1),
    };
    let mut record = [0u8; DURABLE_MAX_BYTES];
    let total = encode_durable_record(magic, generation, payload, &mut record)?;
    {
        let file = directory
            .open_file_in_dir(names[target], Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| ())?;
        file.write(&record[..total]).map_err(|_| ())?;
    }
    let mut verify = [0u8; DURABLE_MAX_BYTES];
    let verified = read_generation_file(
        directory,
        names[target],
        magic,
        &mut verify[..payload.len()],
    );
    if verified == Some(generation) && &verify[..payload.len()] == payload {
        Ok(())
    } else {
        Err(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomFontManifest {
    pub name: heapless::String<{ crate::store::MAX_CUSTOM_FONT_NAME }>,
    pub identity: u64,
    pub faces: [FontPackFaceRecord; crate::store::MAX_CUSTOM_FONT_FACES],
    pub face_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheLoadResult {
    Hit { pages: usize, repaginated: bool },
    Miss,
    Invalid,
    TooShort { pages: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BookIndexLoadResult {
    Hit {
        /// The index was published mid-build and a walk meant to come back for
        /// the rest. Whether that walk still exists is the caller's to know;
        /// see `app_core::storage_loop::partial_index_is_usable`.
        unfinished: bool,
    },
    Miss,
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverLoadResult {
    Hit,
    Miss,
    Invalid,
}

#[allow(clippy::result_unit_err)] // Nothing to report but failure: the card gives no distinguishable reason and every caller only branches on success.
pub fn ensure_v2_cache_dirs<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    let cache = open_or_make_dir(&xteink, CACHE_V2_DIR)?;
    let book = open_or_make_dir(&cache, key)?;
    let _ = open_or_make_dir(&book, CACHE_SECTIONS_DIR)?;
    Ok(())
}

const POSITION_FILE: &str = "POS.BIN";
const POSITION_GENERATIONS: [&str; 2] = ["POSA.BIN", "POSB.BIN"];
/// MarigoldOS v0.4.x durable-position magic; keep byte-identical so cards
/// carry reading positions between the two firmwares.
const POSITION_DURABLE_MAGIC: [u8; 4] = *b"MGPS";
const POSITION_BYTES: usize = proto::nvm::PositionRecord::ENCODED_LEN;

/// Panel-geometry salt mixed into the position checksum. The stored screen
/// is a page-within-chapter index under this panel's pagination, so a
/// position written on a differently sized panel (an SD card moved between
/// an X4 and an X3) is meaningless. Zero on the X4 keeps every existing
/// POS.BIN validating byte-for-byte; the X3's non-zero salt makes an
/// X4-written record fail the checksum, so the reader resumes at book start
/// rather than a stale page. The chapter would survive, but there is no
/// separate progress source to reconcile it against, so full reset is the
/// honest fallback.
const POSITION_GEOMETRY_SALT: u32 = (display::WIDTH as u32 ^ display::HEIGHT as u32) ^ (800 ^ 480);

// The salt must vanish on the X4 or an upgrade would reject every existing
// POS.BIN; guard the backward-compat guarantee at compile time.
#[cfg(not(feature = "device-x3"))]
const _: () = assert!(POSITION_GEOMETRY_SALT == 0);

fn encode_position(chapter: u16, screen: u32) -> [u8; POSITION_BYTES] {
    proto::nvm::PositionRecord { chapter, screen }.encode(POSITION_GEOMETRY_SALT)
}

fn decode_position(bytes: &[u8]) -> Option<(u16, u32)> {
    proto::nvm::PositionRecord::decode(bytes, POSITION_GEOMETRY_SALT)
        .map(|record| (record.chapter, record.screen))
}

/// Per-book reading position beside the book's cache records, so
/// switching books no longer abandons the previous one's place.
#[allow(clippy::result_unit_err)] // Nothing to report but failure: the card gives no distinguishable reason and every caller only branches on success.
pub fn write_position_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    chapter: u16,
    screen: u32,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    let cache = open_or_make_dir(&xteink, CACHE_V2_DIR)?;
    let book = open_or_make_dir(&cache, key)?;
    write_two_generation(
        &book,
        POSITION_GENERATIONS,
        POSITION_DURABLE_MAGIC,
        &encode_position(chapter, screen),
    )
}

pub fn read_position_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> Option<(u16, u32)>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
    let book = cache.open_dir(key).ok()?;
    let mut bytes = [0u8; POSITION_BYTES];
    if read_two_generation(
        &book,
        POSITION_GENERATIONS,
        POSITION_DURABLE_MAGIC,
        &mut bytes,
    ) {
        return decode_position(&bytes);
    }
    // Legacy single-file fallback, kept readable so an upgrade resumes at
    // the pre-durable position; the next write lands on the A/B pair.
    let file = book.open_file_in_dir(POSITION_FILE, Mode::ReadOnly).ok()?;
    let len = file.read(&mut bytes).ok()?;
    decode_position(&bytes[..len])
}

#[allow(clippy::result_unit_err)] // Nothing to report but failure: the card gives no distinguishable reason and every caller only branches on success.
pub fn write_state_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    record: proto::nvm::AppStateRecord,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    write_two_generation(
        &xteink,
        STATE_GENERATIONS,
        STATE_DURABLE_MAGIC,
        &record.encode(),
    )
}

const STATE_GENERATIONS: [&str; 2] = ["STATEA.BIN", "STATEB.BIN"];
/// MarigoldOS v0.4.x durable-state magic; byte-identical for card interchange.
const STATE_DURABLE_MAGIC: [u8; 4] = *b"MGST";

/// Read the newest valid STATEA/STATEB generation, falling back to the
/// legacy `/XTEINK/STATE.BIN`. Returns None when every copy is absent,
/// short, or fails its checksum.
pub fn read_state_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<proto::nvm::AppStateRecord>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let mut bytes = [0u8; proto::nvm::AppStateRecord::ENCODED_LEN];
    if read_two_generation(&xteink, STATE_GENERATIONS, STATE_DURABLE_MAGIC, &mut bytes) {
        return proto::nvm::AppStateRecord::decode(&bytes);
    }
    let file = xteink
        .open_file_in_dir(CACHE_STATE_FILE, Mode::ReadOnly)
        .ok()?;
    // One read suffices for a 32-byte record; shorter V1/V2 files decode
    // from their actual length.
    let len = file.read(&mut bytes).ok()?;
    proto::nvm::AppStateRecord::decode(&bytes[..len])
}

pub fn read_custom_font_manifest<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<CustomFontManifest>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let fonts = xteink.open_dir(FONT_PACK_DIR).ok()?;
    let file = fonts
        .open_file_in_dir(FONT_PACK_FILE, Mode::ReadOnly)
        .ok()?;
    let mut header_bytes = [0u8; FONT_PACK_HEADER_BYTES];
    if file.read(&mut header_bytes).ok()? != FONT_PACK_HEADER_BYTES {
        return None;
    }
    let header = FontPackHeader::decode(&header_bytes).ok()?;
    if file.length() != header.total_len {
        return None;
    }
    let face_count = usize::from(header.face_count).min(crate::store::MAX_CUSTOM_FONT_FACES);
    let mut faces = [FontPackFaceRecord::EMPTY; crate::store::MAX_CUSTOM_FONT_FACES];
    file.seek_from_start(header.face_table_offset).ok()?;
    let mut face_bytes = [0u8; FONT_PACK_FACE_RECORD_BYTES];
    for face in faces.iter_mut().take(face_count) {
        if file.read(&mut face_bytes).ok()? != FONT_PACK_FACE_RECORD_BYTES {
            return None;
        }
        *face = FontPackFaceRecord::decode(&face_bytes).ok()?;
    }
    file.seek_from_start(header.name_offset).ok()?;
    let mut name_bytes = [0u8; proto::font_pack::FONT_PACK_MAX_NAME_BYTES];
    let name_len = header.name_len as usize;
    if file.read(&mut name_bytes[..name_len]).ok()? != name_len {
        return None;
    }
    let name = decode_font_pack_name(header, &name_bytes[..name_len]).ok()?;
    Some(CustomFontManifest {
        name,
        identity: header.identity,
        faces,
        face_count,
    })
}

const WIFI_FILE: &str = "WIFI.BIN";
const WIFI_GENERATIONS: [&str; 2] = ["WIFIA.BIN", "WIFIB.BIN"];
/// The AP hint's own generation pair, separate from the credentials' —
/// see [`proto::nvm::WifiApHintRecord`] for why the two are not one record.
const WIFI_HINT_GENERATIONS: [&str; 2] = ["WIFHA.BIN", "WIFHB.BIN"];
/// MarigoldOS v0.4.x durable-credentials magic; byte-identical for card
/// interchange.
const WIFI_DURABLE_MAGIC: [u8; 4] = *b"MGWF";
const WIFI_HINT_DURABLE_MAGIC: [u8; 4] = *b"CAWH";

/// Write the onboarding portal's credentials to alternating WIFIA/WIFIB.
#[allow(clippy::result_unit_err)] // Nothing to report but failure: the card gives no distinguishable reason and every caller only branches on success.
pub fn write_wifi_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    record: proto::nvm::WifiCredentialsRecord,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    write_two_generation(
        &xteink,
        WIFI_GENERATIONS,
        WIFI_DURABLE_MAGIC,
        &record.encode(),
    )
}

/// Delete every stored credential copy (legacy WIFI.BIN and both
/// generations); missing files count as success.
pub fn delete_wifi_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(xteink) = root.open_dir(CACHE_ROOT_DIR) else {
        return true;
    };
    let mut ok = true;
    for name in [
        WIFI_FILE,
        WIFI_GENERATIONS[0],
        WIFI_GENERATIONS[1],
        // Forgetting a network forgets which AP served it. Leaving the hint
        // would steer the next join for a *different* network toward it —
        // the SSID hash refuses that, but a hint for a network the user
        // deliberately dropped has no reason to survive either.
        WIFI_HINT_GENERATIONS[0],
        WIFI_HINT_GENERATIONS[1],
    ] {
        ok &= upload_store::remove_file_reclaiming_clusters(&xteink, name)
            != upload_store::RemoveStatus::Failed;
    }
    ok
}

/// Persist which AP the station last associated through, so the next session
/// can join it directly instead of sweeping every channel.
///
/// Failure is ignored by callers by design: the hint is an accelerator, and a
/// session that cannot write one simply scans next time.
#[allow(clippy::result_unit_err)] // Same as the credentials writer above: the card gives no distinguishable reason, and the only caller branches on success.
pub fn write_wifi_hint_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    record: proto::nvm::WifiApHintRecord,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    write_two_generation(
        &xteink,
        WIFI_HINT_GENERATIONS,
        WIFI_HINT_DURABLE_MAGIC,
        &record.encode(),
    )
}

/// Read the stored AP hint. `None` for missing, torn, or nonsensical —
/// every one of which just means the next join scans.
pub fn read_wifi_hint_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<proto::nvm::WifiApHintRecord>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let mut bytes = [0u8; proto::nvm::WifiApHintRecord::ENCODED_LEN];
    read_two_generation(
        &xteink,
        WIFI_HINT_GENERATIONS,
        WIFI_HINT_DURABLE_MAGIC,
        &mut bytes,
    )
    .then(|| proto::nvm::WifiApHintRecord::decode(&bytes))
    .flatten()
}

/// Read the newest WIFIA/WIFIB generation, falling back to legacy WIFI.BIN;
/// None when every copy is missing, short, or corrupt.
pub fn read_wifi_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<proto::nvm::WifiCredentialsRecord>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let mut bytes = [0u8; proto::nvm::WifiCredentialsRecord::ENCODED_LEN];
    if read_two_generation(&xteink, WIFI_GENERATIONS, WIFI_DURABLE_MAGIC, &mut bytes) {
        return proto::nvm::WifiCredentialsRecord::decode(&bytes);
    }
    let file = xteink.open_file_in_dir(WIFI_FILE, Mode::ReadOnly).ok()?;
    let len = file.read(&mut bytes).ok()?;
    proto::nvm::WifiCredentialsRecord::decode(&bytes[..len])
}

pub fn load_v2_cover_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    library: &mut ReaderStore,
) -> CoverLoadResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_cover_file(root, key, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; COVER_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return CoverLoadResult::Invalid;
        }
        let Ok(header) = decode_cover_header(&header_bytes) else {
            return CoverLoadResult::Invalid;
        };
        // Read straight into the store's cover buffer: a stack copy here is
        // an ~8 KB frame on a path that already runs near the stack floor.
        if read_exact_file(file, library.cover_bits_mut()).is_err() {
            library.clear_cover();
            return CoverLoadResult::Invalid;
        }
        library.finish_cover_load(header.width, header.height);
        CoverLoadResult::Hit
    })
    .unwrap_or(CoverLoadResult::Miss)
}

/// Read just the book's total page count from the V2 index header,
/// without loading any section records. Used at boot restore so the Home
/// progress bar has a denominator before the book is opened. Returns 0 if the
/// index is missing, stale, or for another book.
///
/// Takes the most recently used config's index, which is the one the reader
/// will open under while the settings stand. Like the check below, it does
/// not insist on the current config: a count from another one is a better
/// denominator than none, and always was — the pre-per-config version read
/// whatever config the single index had been left in.
pub fn read_v2_book_total_pages<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    library: &ReaderStore,
) -> u32
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_any_v2_book_file(root, key, false, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        read_exact_file(file, &mut header_bytes).ok()?;
        let header = decode_book_v2_header(&header_bytes).ok()?;
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.custom_font_identity != library.custom_font_identity()
            || header.total_pages == 0
        {
            return None;
        }
        Some(header.total_pages)
    })
    .unwrap_or(0)
}

/// Bounds the TOC and label counts a BOOK.BIN header may claim before any
/// body loader trusts them.
fn v2_toc_label_bounds_ok(header: &BookV2Header) -> bool {
    header.toc_count as usize <= MAX_SD_TOC_ITEMS
        && header.toc_text_bytes as usize <= MAX_SD_TOC_TEXT_BYTES
        && header.title_text_bytes as usize <= 64
        && header.author_text_bytes as usize <= 64
}

/// Read the TOC records and text at the file's current position (just past
/// the section records) into the library. The one decoder of BOOK.BIN's
/// TOC body — `load_v2_book_index` and `load_v2_book_labels_and_toc` both
/// call it. On any failure the library's TOC is left cleared and false is
/// returned.
fn read_v2_toc_into_library<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    header: &BookV2Header,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    library.clear_toc();
    if !read_records_batched(
        file,
        TOC_RECORD_BYTES,
        header.toc_count as usize,
        |index, bytes| {
            let Ok(record) = decode_toc(bytes) else {
                return false;
            };
            if !toc_record_fits_text(record, header.toc_text_bytes) {
                return false;
            }
            library.toc[index] = record;
            library.toc_page[index] = 0;
            true
        },
    ) {
        library.clear_toc();
        return false;
    }
    if header.toc_text_bytes > 0 {
        let text_len = header.toc_text_bytes as usize;
        if read_exact_file(file, &mut library.toc_text[..text_len]).is_err() {
            library.clear_toc();
            return false;
        }
        library.toc_text_len = text_len;
        library.toc_count = header.toc_count as usize;
    }
    true
}

/// Read the title/author labels at the file's current position (just past
/// the TOC text) and publish them to the library — the shared tail of both
/// BOOK.BIN body loaders. A book with neither label leaves the store's
/// labels untouched.
fn read_v2_labels_into_library<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    header: &BookV2Header,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut title = [0u8; 64];
    let mut author = [0u8; 64];
    let mut title_str = "";
    let mut author_str = "";
    if header.title_text_bytes > 0 {
        let title_len = header.title_text_bytes as usize;
        if read_exact_file(file, &mut title[..title_len]).is_err() {
            return false;
        }
        let Ok(parsed_title) = core::str::from_utf8(&title[..title_len]) else {
            return false;
        };
        title_str = parsed_title;
    }
    if header.author_text_bytes > 0 {
        let author_len = header.author_text_bytes as usize;
        if read_exact_file(file, &mut author[..author_len]).is_err() {
            return false;
        }
        let Ok(parsed_author) = core::str::from_utf8(&author[..author_len]) else {
            return false;
        };
        author_str = parsed_author;
    }
    if header.title_text_bytes > 0 || header.author_text_bytes > 0 {
        library.set_book_labels(title_str, author_str);
    }
    true
}

pub fn load_v2_book_index<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
) -> BookIndexLoadResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_book_file(root, key, layout_key_for(library), Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return BookIndexLoadResult::Invalid;
        }
        let Ok(header) = decode_book_v2_header(&header_bytes) else {
            return BookIndexLoadResult::Invalid;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.font_config
                != layout::reader_layout_config(library.type_settings(), library.portrait())
            || header.custom_font_identity != library.custom_font_identity()
            || header.section_count as usize > MAX_BOOK_SECTIONS
            || !v2_toc_label_bounds_ok(&header)
            || header.total_pages == 0
        {
            return BookIndexLoadResult::Invalid;
        }
        let mut sections = [EMPTY_BOOK_SECTION_RECORD; MAX_BOOK_SECTIONS];
        if !read_records_batched(
            file,
            BOOK_V2_SECTION_RECORD_BYTES,
            header.section_count as usize,
            |index, bytes| {
                let Ok(record) = decode_book_v2_section(bytes) else {
                    return false;
                };
                if record.page_count == 0 {
                    return false;
                }
                sections[index] = record;
                true
            },
        ) {
            return BookIndexLoadResult::Invalid;
        }
        if !read_v2_toc_into_library(file, &header, library) {
            return BookIndexLoadResult::Invalid;
        }
        if !read_v2_labels_into_library(file, &header, library) {
            return BookIndexLoadResult::Invalid;
        }
        library.set_book_index(
            header.total_pages,
            header.partial,
            &sections[..header.section_count as usize],
        );
        BookIndexLoadResult::Hit {
            unfinished: header.resume_spine != 0,
        }
    })
    .unwrap_or(BookIndexLoadResult::Miss)
}

/// Read just the stored EPUB title from a book's v2 cache index, skipping the
/// section records and the rest of the body. The Library list uses this to
/// label books whose on-disk name can't carry a real title (8.3 upload names)
/// with the title learned the last time the book was opened. Returns false
/// (leaving `out` untouched) when there is no cache for the book, the cached
/// identity doesn't match, or the cache holds no title.
///
/// The title is settings-independent, so any of the book's per-config
/// indexes answers; this takes the most recently used one the registry
/// names. A book whose registry is missing falls through to the label
/// fallbacks rather than paying a directory listing per row — this runs once
/// per book at catalog scan.
pub fn read_cached_book_title<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    out: &mut String<64>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_any_v2_book_file(root, key, false, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        read_exact_file(file, &mut header_bytes).ok()?;
        let header = decode_book_v2_header(&header_bytes).ok()?;
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || !v2_toc_label_bounds_ok(&header)
            || header.title_text_bytes == 0
            || header.title_text_bytes as usize > 64
        {
            return None;
        }
        // The title text sits after the header, the section records, and the
        // TOC block (records + text) -- the same body order write_v2_book_index
        // lays down and load_v2_book_index reads through.
        let title_offset = BOOK_V2_HEADER_BYTES as u32
            + header.section_count as u32 * BOOK_V2_SECTION_RECORD_BYTES as u32
            + header.toc_count as u32 * TOC_RECORD_BYTES as u32
            + header.toc_text_bytes;
        file.seek_from_start(title_offset).ok()?;
        let title_len = header.title_text_bytes as usize;
        let mut title = [0u8; 64];
        read_exact_file(file, &mut title[..title_len]).ok()?;
        let title_str = core::str::from_utf8(&title[..title_len]).ok()?;
        out.clear();
        let _ = out.push_str(title_str);
        Some(true)
    })
    .unwrap_or(false)
}

/// What a cache's BOOK.BIN had to say about itself.
///
/// The three cases are not interchangeable, because a cache key is only 28
/// bits of hash and collisions are an accepted possibility. `Absent` says the
/// cache has no index — nothing usable is there, whoever it belongs to.
/// `Unreadable` says there *is* an index and we could not tell whose it is,
/// which is exactly when a caller about to delete has to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheHeader {
    Present(BookV2Header),
    /// No BOOK.BIN at all.
    Absent,
    /// BOOK.BIN is there and says nothing usable: truncated, corrupt, or the
    /// read failed.
    Unreadable,
}

/// Read a book cache's v2 header, for its stored source identity and section
/// count. Used by the orphan sweep to decide whether a cache still belongs to
/// a book on the card, and by the clear to prove a key names the book it was
/// asked about.
///
/// Source identity is settings-independent, so any of the book's per-config
/// indexes answers. The registry names them; a cache whose registry was
/// lost is then searched by listing, because this is the reader whose `Absent`
/// gets a cache deleted and it must not report one for indexes that are
/// sitting right there.
pub fn read_cache_header<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> CacheHeader
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // Opened step by step rather than through `with_v2_book_file`, which
    // folds every failure into one `None`. A directory that is not there and
    // a directory that would not open are the same value to it, and the
    // difference is the whole point here.
    macro_rules! open {
        ($opened:expr) => {
            match $opened {
                Ok(handle) => handle,
                Err(embedded_sdmmc::Error::NotFound) => return CacheHeader::Absent,
                Err(_) => return CacheHeader::Unreadable,
            }
        };
    }
    let xteink = open!(root.open_dir(CACHE_ROOT_DIR));
    let cache = open!(xteink.open_dir(CACHE_V2_DIR));
    let book_dir = open!(cache.open_dir(key));
    let stored = read_layout_registry_in(&book_dir);
    let registry = stored.unwrap_or_default();

    let mut candidate_identity: Option<(u32, u32)> = None;
    let mut first_full_header: Option<BookV2Header> = None;
    let mut unreadable = false;
    let mut mismatch = false;

    let mut update_identity = |h: BookV2Header| {
        if let Some((chash, csize)) = candidate_identity {
            if chash != h.source_hash || csize != h.source_size {
                mismatch = true;
            }
        } else {
            candidate_identity = Some((h.source_hash, h.source_size));
        }
        if first_full_header.is_none() {
            first_full_header = Some(h);
        }
    };

    let mut name = String::<BOOK_INDEX_FILE_BYTES>::new();
    for layout_key in registry.keys() {
        book_index_file_name(*layout_key, &mut name);
        match read_book_index_header(&book_dir, name.as_str()) {
            Some(Some(header)) => update_identity(header),
            Some(None) => unreadable = true,
            None => {}
        }
    }

    let Some(unlisted_names) = book_index_names_unlisted_by(&book_dir, &registry) else {
        return CacheHeader::Unreadable;
    };
    for unlisted in unlisted_names {
        match read_book_index_header(&book_dir, unlisted.as_str()) {
            Some(Some(header)) => update_identity(header),
            Some(None) => unreadable = true,
            None => {}
        }
    }

    match read_book_index_header(&book_dir, CACHE_BOOK_FILE) {
        Some(Some(header)) => update_identity(header),
        Some(None) => unreadable = true,
        None => {}
    }

    match book_dir.open_file_in_dir(CACHE_TOC_FILE, Mode::ReadOnly) {
        Ok(file) => {
            let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
            if read_exact_file(&file, &mut header_bytes).is_ok() {
                if let Ok(toc_header) = decode_toc_file_header(&header_bytes) {
                    if let Some((chash, csize)) = candidate_identity {
                        if chash != toc_header.source_hash || csize != toc_header.source_size {
                            mismatch = true;
                        }
                    } else {
                        candidate_identity = Some((toc_header.source_hash, toc_header.source_size));
                    }
                } else {
                    unreadable = true;
                }
            } else {
                unreadable = true;
            }
        }
        Err(embedded_sdmmc::Error::NotFound) => {}
        Err(_) => unreadable = true,
    }

    if unreadable || mismatch || (stored.is_none() && registry_present(&book_dir)) {
        return CacheHeader::Unreadable;
    }

    if let Some(header) = first_full_header {
        CacheHeader::Present(header)
    } else if candidate_identity.is_some() {
        CacheHeader::Unreadable
    } else {
        CacheHeader::Absent
    }
}

/// `None` when the named index is not there, `Some(None)` when it is there
/// and unreadable, `Some(Some(header))` when it decodes.
fn read_book_index_header<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
) -> Option<Option<BookV2Header>>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match book.open_file_in_dir(name, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return None,
        // A card that would not open the file is not proof the index is
        // missing, and only "missing" licenses a delete — so it reads as
        // present-and-unreadable, the distinction this whole function exists
        // to make.
        Err(_) => return Some(None),
    };
    let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
    if read_exact_file(&file, &mut header_bytes).is_err() {
        return Some(None);
    }
    Some(decode_book_v2_header(&header_bytes).ok())
}

/// Whether the registry file is there at all, for the one caller that has to
/// tell "no cache" from "a cache we cannot interpret". Anything but an
/// outright `NotFound` counts as present, for the same reason as above.
fn registry_present<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    !matches!(
        book.open_file_in_dir(CACHE_CONFIG_FILE, Mode::ReadOnly),
        Err(embedded_sdmmc::Error::NotFound)
    )
}

/// Section files deleted per directory-listing pass. embedded-sdmmc will not
/// let a file be opened while an iteration holds the volume lock, so names are
/// collected first and deleted once the walk has returned, and the loop
/// re-lists until the directory comes back empty.
///
/// The batch is what keeps that staging off the stack budget: 16 short names
/// is 392 B, against the 7,680 B a whole `MAX_BOOK_SECTIONS` book would need,
/// and it costs 20 extra listings only for a book at that cap — an ordinary
/// book empties in two or three passes. The deepest caller is the publish
/// failure path inside the EPUB-open chain, which is why this is sized rather
/// than left to the book.
const SECTION_SWEEP_BATCH: usize = 16;

/// A FAT short name at its widest: `NNNNNNNN.EXT`.
const SHORT_NAME_BYTES: usize = 12;

/// Delete one book cache completely: every section file, BOOK/TOC/COVER/CONT,
/// then the emptied `SECTIONS/` and `<key>/` directories themselves. Returns
/// whether the cache is actually gone.
///
/// Nothing here trusts the cache's own header for what to delete. It used to
/// remove sections by generated name over `0..section_count`, which left a
/// header that undercounted (a torn write, a truncated build) with orphan
/// section files that no later pass could name — the sweep reads
/// `section_count` from the same BOOK.BIN this deletes, so once the header was
/// gone the leftovers were unnameable and `SECTIONS/` stayed non-empty
/// forever. `SECTIONS/` is enumerated instead, so what is on the card decides
/// what gets deleted.
///
/// Per-config index files (`BK*.BIN`) and section files are removed first, and
/// `CFG.BIN` goes last once all artifacts are cleared. Keeping `CFG.BIN` until
/// the end ensures an interrupted clear preserves the registry so surviving
/// per-config files remain accounted for on subsequent opens.
///
/// The global reading position in XTEINK/STATE.BIN is never touched.
pub fn empty_cache_dir<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // A directory that isn't there holds no cache; anything else going wrong
    // on the way in means we cannot say the cache is gone.
    let xteink = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return true,
        Err(_) => return false,
    };
    let cache = match xteink.open_dir(CACHE_V2_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return true,
        Err(_) => return false,
    };
    let position_kept;
    let mut cleared = true;
    {
        let book = match cache.open_dir(key) {
            Ok(dir) => dir,
            Err(embedded_sdmmc::Error::NotFound) => return true,
            Err(_) => return false,
        };
        for name in [
            CACHE_BOOK_FILE,
            CACHE_TOC_FILE,
            CACHE_COVER_FILE,
            CACHE_CONTENT_FILE,
        ] {
            if upload_store::remove_file_reclaiming_clusters(&book, name)
                == upload_store::RemoveStatus::Failed
            {
                cleared = false;
            }
        }
        // The per-config indexes, by listing rather than by name: the
        // registry that names them stays until they are gone, so an
        // interrupted clear leaves surviving indexes accounted for. A key
        // past the listing budget survives to fail `book_dir_is_reclaimed`
        // below, which is what stops this from reporting a clear it did not
        // finish.
        match book_index_names_unlisted_by(&book, &LayoutConfigRegistry::new()) {
            Some(names) => {
                for name in &names {
                    if upload_store::remove_file_reclaiming_clusters(&book, name.as_str())
                        == upload_store::RemoveStatus::Failed
                    {
                        cleared = false;
                    }
                }
            }
            // A listing that would not run leaves indexes this pass never got
            // to name. `book_dir_is_reclaimed` below would refuse the clear
            // anyway; failing here says so without depending on that.
            None => cleared = false,
        }
        match book.open_dir(CACHE_SECTIONS_DIR) {
            Ok(sections) => {
                if !empty_sections_dir(&sections) {
                    cleared = false;
                }
            }
            Err(embedded_sdmmc::Error::NotFound) => {}
            Err(_) => cleared = false,
        }
        if cleared {
            // The SECTIONS handle has dropped; the empty directory can go now
            // (a directory entry has no chain to reclaim). A refusal here
            // leaves an empty directory, not cache data, so it does not make
            // the clear a failure.
            let _ = book.delete_file_in_dir(CACHE_SECTIONS_DIR);
            if upload_store::remove_file_reclaiming_clusters(&book, CACHE_CONFIG_FILE)
                == upload_store::RemoveStatus::Failed
            {
                cleared = false;
            }
        }
        // Everything above worked from a list of names. This is the part that
        // does not: it asks the directory what is actually left, which is the
        // only way to catch what the names never covered — a file from an
        // older layout, a `SECTIONS/` that refused to go, anything a future
        // format adds without teaching this function about it.
        cleared = cleared && book_dir_is_reclaimed(&book);
        // Everything above is re-derivable from the EPUB; the position is not.
        // POS*.BIN is the authoritative record of where the reader is in this
        // book, so it is never swept, and the directory holding it has to stay
        // as well. A book moved off the card and back keys to the same name and
        // size, so its place is still waiting when it returns.
        position_kept = has_position_file(&book);
    }
    if cleared && !position_kept {
        // Likewise the book handle: closed by the scope above, deletable here
        // (a directory entry has no chain to reclaim).
        let _ = cache.delete_file_in_dir(key);
    }
    cleared
}

/// Delete the section files a freshly published index no longer names.
///
/// Section files are keyed by section *ordinal* — a dense `0..count` counter
/// the walk assigns as it flushes, distinct from the spine index the record
/// also carries — so a rebuild producing fewer sections than the one before
/// it strands `S<cfg><count>..` on the card. Nothing references them:
/// `load_v2_section_by_global_page` indexes off the config's book index. They
/// are unreachable but not free, and they survive until the whole cache
/// directory is emptied.
///
/// Scoped to one layout config. The ordinal ranges of two resident configs
/// overlap by construction — each numbers its own sections from zero — so a
/// prune that went by ordinal alone would delete the *other* config's
/// pagination past this one's count, which is a working cached copy of the book
/// and the whole point of keeping more than one.
///
/// No shipping setting reaches that state today. Sections split on content
/// volume — `flush_if_full` breaks on block and text capacity long before the
/// page-count bound — so re-flowing the same content produces the same count.
/// Measured on the X3 2026-08-04: one book at 1252, 763 and 708 pages across a
/// type-size change and an orientation flip held 79 sections throughout. What
/// *would* shrink the count is a change to the capacity constants themselves,
/// which re-derives fewer sections over the same content and strands the old
/// tail. This exists for that case; it is insurance, not a present-day leak.
///
/// **Only ever call this with a final section count.** A suspended walk is
/// coming back to write more sections, and pruning against its provisional
/// count would delete the ones it is about to need.
///
/// Best effort by design: orphans are dead weight, not corruption, so a
/// failure here must not turn a successful publish into a failed one. It is
/// safe to run only *after* the new index is on the card — everything it
/// deletes is already unreachable from the index a reader could be holding.
///
/// Returns how many files it removed.
fn prune_orphan_sections_in<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sections: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    layout_key: u8,
    keep_count: u16,
) -> usize
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    use core::fmt::Write;
    let mut removed = 0usize;
    // Same shape as `empty_sections_dir`: collect a bounded batch by listing,
    // delete it, and list again, because deleting while iterating is not
    // something the directory walk promises. The spare pass proves the tail
    // is gone rather than merely out of budget.
    let max_passes = MAX_BOOK_SECTIONS.div_ceil(SECTION_SWEEP_BATCH) + 1;
    for _ in 0..max_passes {
        let mut names: heapless::Vec<String<SHORT_NAME_BYTES>, SECTION_SWEEP_BATCH> =
            heapless::Vec::new();
        if sections
            .iterate_dir(|entry| {
                if entry.attributes.is_directory() || names.is_full() {
                    return;
                }
                let mut name = String::<SHORT_NAME_BYTES>::new();
                if write!(name, "{}", entry.name).is_err() {
                    return;
                }
                // Parsed rather than trusted, and matched on the config as well
                // as the ordinal: `SECTIONS/` is on removable media, so a name
                // that is not one of ours is left alone, and one that is
                // another config's is not this prune's to take.
                match section_file_name_parts(name.as_str()) {
                    Some((key, ordinal)) if key == layout_key && ordinal >= keep_count => {
                        let _ = names.push(name);
                    }
                    _ => {}
                }
            })
            .is_err()
        {
            return removed;
        }
        if names.is_empty() {
            return removed;
        }
        // Attempt every name in the batch, including the ones after a failure.
        // `remove_file_reclaiming_clusters` opens, truncates, closes and then
        // deletes, and a fault in any of those fails that one file without
        // saying anything about the next — the card model these paths are
        // tested against injects exactly that, a single refused write followed
        // by writes that succeed. Abandoning the batch on the first failure
        // would leave the rest stranded until another completed rebuild, and
        // for the capacity-constant change this exists for there may never be
        // one.
        let mut progressed = false;
        for name in &names {
            if upload_store::remove_file_reclaiming_clusters(sections, name.as_str())
                != upload_store::RemoveStatus::Failed
            {
                removed += 1;
                progressed = true;
            }
        }
        if !progressed {
            // A pass that took nothing is where best-effort stops. The listing
            // is deterministic, so the next pass would collect the same names
            // and refuse them again; this is the bound on a card that really
            // has stopped accepting deletes, while a pass that took anything
            // still earns the failures beside it one more attempt.
            return removed;
        }
    }
    removed
}

/// [`prune_orphan_sections_in`], opening the book's `SECTIONS/` directory and
/// taking the layout config from the store whose pagination was just published
/// — the same derivation every other per-config reader and writer uses, so no
/// caller can prune one config's range out of another's files.
pub fn prune_orphan_sections<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    library: &ReaderStore,
    keep_count: u16,
) -> usize
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let layout_key = layout_key_for(library);
    with_v2_sections_dir(root, key, |sections| match sections {
        Some(sections) => prune_orphan_sections_in(sections, layout_key, keep_count),
        None => 0,
    })
}

/// Delete every file in an opened `SECTIONS/` directory, by listing rather
/// than by generated name. Returns false if any delete failed or the directory
/// still had entries after the pass budget — either way the caller must not
/// report the cache as cleared.
fn empty_sections_dir<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sections: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    use core::fmt::Write;
    // A full book is MAX_BOOK_SECTIONS files; the spare pass is what proves
    // the directory came back empty rather than merely running out of budget.
    let max_passes = MAX_BOOK_SECTIONS.div_ceil(SECTION_SWEEP_BATCH) + 1;
    for _ in 0..max_passes {
        let mut names: heapless::Vec<String<SHORT_NAME_BYTES>, SECTION_SWEEP_BATCH> =
            heapless::Vec::new();
        // Something this pass could not take. It keeps the walk going (there
        // may still be files worth deleting) but a pass that ends with the
        // directory non-empty must not read as emptied.
        let mut blocked = false;
        if sections
            .iterate_dir(|entry| {
                let mut name = String::<SHORT_NAME_BYTES>::new();
                if write!(name, "{}", entry.name).is_err() {
                    // A name this pass cannot reproduce is a name it cannot
                    // delete; say so rather than silently leaving it.
                    blocked = true;
                    return;
                }
                if name.as_str() == "." || name.as_str() == ".." {
                    return;
                }
                if entry.attributes.is_directory() {
                    // Nothing here writes directories. Whatever it is, this
                    // pass cannot delete it, and skipping it silently was how
                    // a `SECTIONS/` holding only a subdirectory reported
                    // itself emptied.
                    blocked = true;
                    return;
                }
                if names.push(name).is_err() {
                    blocked = true;
                }
            })
            .is_err()
        {
            return false;
        }
        if names.is_empty() {
            return !blocked;
        }
        for name in &names {
            if upload_store::remove_file_reclaiming_clusters(sections, name.as_str())
                == upload_store::RemoveStatus::Failed
            {
                return false;
            }
        }
    }
    false
}

/// The files a cleared cache is allowed to keep: the reading position, in
/// either the durable A/B pair or the legacy single file.
fn is_kept_after_clear(name: &str) -> bool {
    POSITION_GENERATIONS
        .iter()
        .chain(core::iter::once(&POSITION_FILE))
        .any(|kept| name.eq_ignore_ascii_case(kept))
}

/// Whether the book's cache directory holds nothing but its reading position.
///
/// This is what success is reported on. The deletes above name what they
/// expect to find; this asks what is really there, so a clear cannot claim to
/// have reclaimed a directory it left occupied.
fn book_dir_is_reclaimed<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    use core::fmt::Write;
    let mut reclaimed = true;
    let walked = book.iterate_dir(|entry| {
        let mut name = String::<SHORT_NAME_BYTES>::new();
        if write!(name, "{}", entry.name).is_err() {
            reclaimed = false;
            return;
        }
        if name.as_str() == "." || name.as_str() == ".." {
            return;
        }
        // A surviving `SECTIONS/` lands here too: it is not a position file,
        // so it fails the check like any other leftover.
        if !is_kept_after_clear(name.as_str()) {
            reclaimed = false;
        }
    });
    walked.is_ok() && reclaimed
}

/// Whether a book's cache directory still holds a reading position, in either
/// the durable A/B pair or the legacy single file.
fn has_position_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    POSITION_GENERATIONS
        .iter()
        .chain(core::iter::once(&POSITION_FILE))
        .any(|name| book.open_file_in_dir(*name, Mode::ReadOnly).is_ok())
}

#[expect(clippy::too_many_arguments)] // The index's own field set: identity, shape, and the resume cursor, all caller-owned
pub fn write_v2_book_index<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    total_pages: u32,
    sections: &[BookV2SectionRecord],
    library: &ReaderStore,
    partial: bool,
    // The spine item a suspended progressive build will walk next, or `0`
    // when nothing is still building this index. See
    // [`proto::cache::BookV2Header::resume_spine`] — it is what stops an
    // abandoned build's index from capping the book forever.
    resume_spine: u16,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if total_pages == 0 || sections.is_empty() || sections.len() > MAX_BOOK_SECTIONS {
        return false;
    }
    if ensure_v2_cache_dirs(root, key).is_err() {
        return false;
    }
    let layout_key = layout_key_for(library);
    with_v2_book_file(
        root,
        key,
        layout_key,
        Mode::ReadWriteCreateOrTruncate,
        |file| {
            let toc_count = library
                .toc_count
                .min(MAX_SD_TOC_ITEMS)
                .min(u16::MAX as usize);
            let title_text_bytes = library.title.len().min(64) as u32;
            let author_text_bytes = library.author.len().min(64) as u32;
            let header = BookV2Header {
                source_hash: source_identity.0,
                source_size: source_identity.1,
                total_pages,
                section_count: sections.len().min(u16::MAX as usize) as u16,
                spine_count: sections
                    .iter()
                    .map(|section| section.spine as usize + 1)
                    .max()
                    .unwrap_or(0)
                    .min(u16::MAX as usize) as u16,
                toc_count: toc_count as u16,
                toc_text_bytes: library
                    .toc_text_len
                    .min(MAX_SD_TOC_TEXT_BYTES)
                    .min(u32::MAX as usize) as u32,
                title_text_bytes,
                author_text_bytes,
                viewport_width: 800,
                viewport_height: 480,
                font_config: layout::reader_layout_config(
                    library.type_settings(),
                    library.portrait(),
                ),
                custom_font_identity: library.custom_font_identity(),
                partial,
                resume_spine,
            };
            let mut bytes = [0u8; BOOK_V2_HEADER_BYTES];
            if encode_book_v2_header(header, &mut bytes).is_err() {
                return false;
            }
            let mut stage = WriteStage::new(file);
            if stage.push(&bytes).is_err() {
                return false;
            }
            let mut record_bytes = [0u8; BOOK_V2_SECTION_RECORD_BYTES];
            for section in sections {
                if encode_book_v2_section(*section, &mut record_bytes).is_err()
                    || stage.push(&record_bytes).is_err()
                {
                    return false;
                }
            }
            let mut toc_bytes = [0u8; TOC_RECORD_BYTES];
            for record in library.toc.iter().take(toc_count).copied() {
                if encode_toc(record, &mut toc_bytes).is_err() || stage.push(&toc_bytes).is_err() {
                    return false;
                }
            }
            if stage.flush().is_err() {
                return false;
            }
            if header.toc_text_bytes > 0
                && file
                    .write(&library.toc_text[..header.toc_text_bytes as usize])
                    .is_err()
            {
                return false;
            }
            if header.title_text_bytes > 0
                && file
                    .write(&library.title.as_bytes()[..header.title_text_bytes as usize])
                    .is_err()
            {
                return false;
            }
            if header.author_text_bytes > 0
                && file
                    .write(&library.author.as_bytes()[..header.author_text_bytes as usize])
                    .is_err()
            {
                return false;
            }
            true
        },
    )
    .unwrap_or(false)
}

fn toc_record_fits_text(record: proto::cache::TocRecord, text_bytes: u32) -> bool {
    range_fits(record.title_offset, record.title_len, text_bytes)
        && range_fits(record.href_offset, record.href_len, text_bytes)
        && range_fits(record.anchor_offset, record.anchor_len, text_bytes)
}

fn range_fits(offset: u32, len: u16, text_bytes: u32) -> bool {
    offset
        .checked_add(len as u32)
        .map(|end| end <= text_bytes)
        .unwrap_or(false)
}

pub fn load_v2_section_by_global_page<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    global_page: u32,
    library: &mut ReaderStore,
) -> CacheLoadResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some(section) = library.section_for_global_page(global_page) else {
        return CacheLoadResult::Miss;
    };
    let result = load_v2_section_cache(
        root,
        key,
        source_identity,
        section.section,
        section.spine,
        section.page_count as usize,
        library,
    );
    if let CacheLoadResult::Hit { pages, repaginated } = result {
        library.set_current_section_range(section.start_page, pages);
        if repaginated {
            let _ = write_v2_section_cache(root, key, source_identity, section.section, library);
        }
    }
    result
}

fn open_or_make_dir<
    'a,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    parent: &'a Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
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

pub(crate) fn load_v2_section_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    section: u16,
    expected_spine: u16,
    target_pages: usize,
    library: &mut ReaderStore,
) -> CacheLoadResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let expected_config = layout::reader_layout_config(library.type_settings(), library.portrait());
    with_v2_section_file(
        root,
        key,
        layout_cache_key(expected_config),
        section,
        Mode::ReadOnly,
        |file| {
            let mut header_bytes = [0u8; SECTION_V2_HEADER_BYTES];
            if read_exact_file(file, &mut header_bytes).is_err() {
                return CacheLoadResult::Invalid;
            }
            let Ok(header) = decode_section_v2_header(&header_bytes) else {
                return CacheLoadResult::Invalid;
            };
            if header.source_hash != source_identity.0
                || header.source_size != source_identity.1
                || header.spine != expected_spine
            {
                return CacheLoadResult::Invalid;
            }
            if header.custom_font_identity != library.custom_font_identity() {
                return CacheLoadResult::Invalid;
            }
            // Cached blocks are pre-wrapped lines: they survive a spacing
            // change (heights re-walk below) but not a size change, which
            // alters every wrap point and needs the full EPUB rebuild.
            //
            // The file name already carries the wrap-relevant bits, so what is
            // left for this check to catch is a wrap-version or panel-salt bump
            // — the axes deliberately kept out of the name, so a bump retires
            // every config's files in place rather than stranding them.
            if header.font_config & !0b11 != expected_config & !0b11 {
                return CacheLoadResult::Invalid;
            }
            let layout_matches = header.font_config == expected_config;
            if !load_v2_section_body(file, header, library) {
                return CacheLoadResult::Invalid;
            }
            if !layout_matches {
                layout::rebuild_page_index(library);
            }
            let pages = library.page_count;
            if pages < target_pages {
                CacheLoadResult::TooShort { pages }
            } else {
                CacheLoadResult::Hit {
                    pages,
                    repaginated: !layout_matches,
                }
            }
        },
    )
    .unwrap_or(CacheLoadResult::Miss)
}

pub(crate) fn write_v2_section_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    section: u16,
    library: &ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if ensure_v2_cache_dirs(root, key).is_err() {
        cache_log!("cache: v2 ensure dirs failed key={}", key);
        return false;
    }
    with_v2_section_file(
        root,
        key,
        layout_key_for(library),
        section,
        Mode::ReadWriteCreateOrTruncate,
        |file| write_v2_section_body(file, source_identity, library.cached_spine, library),
    )
    .unwrap_or_else(|| {
        cache_log!(
            "cache: v2 open section failed key={} section={}",
            key,
            section
        );
        false
    })
}

/// Open the book's SECTIONS directory once and run `f` with it, so a whole
/// build writes tens of section files without re-walking the four-level
/// cache chain per section. Directory creation failure passes `None`: the
/// build still runs, every section write reports failure, and the book is
/// marked partial — the same degraded path as before.
pub fn with_v2_sections_dir<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    f: impl for<'a> FnOnce(Option<&Directory<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>) -> R,
) -> R
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // One handle walks the chain via change_dir, so the whole build holds a
    // single directory slot instead of the four-level ladder. The caller is
    // responsible for `ensure_v2_cache_dirs` when the tree might not exist
    // yet (the full build runs it once up front); a missing tree lands in
    // the `f(None)` fallback like any other open failure.
    let Some(mut dir) = open_v2_book_dir(root, key) else {
        return f(None);
    };
    if dir.change_dir(CACHE_SECTIONS_DIR).is_err() {
        return f(None);
    }
    f(Some(&dir))
}

/// Write one section file into an already-open SECTIONS directory — the
/// per-section body of `write_v2_section_cache` without the per-call
/// directory walk.
pub fn write_v2_section_cache_in<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sections: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_identity: (u32, u32),
    section: u16,
    library: &ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut name = String::<CACHE_SECTION_FILE_BYTES>::new();
    section_file_name(layout_key_for(library), section, &mut name);
    match sections.open_file_in_dir(name.as_str(), Mode::ReadWriteCreateOrTruncate) {
        Ok(file) => write_v2_section_body(&file, source_identity, library.cached_spine, library),
        Err(_) => {
            cache_log!("cache: v2 open section failed section={}", section);
            false
        }
    }
}

/// Open the book's cache directory (`XTEINK/CACHE2/<key>`) with one handle
/// walked via `change_dir` — the single owner of that path walk. Opening a
/// directory another walk also passes through is fine: this embedded-sdmmc
/// rev allows duplicate directory opens (directories hold no cached
/// state); only deleting an open directory errors.
pub fn open_v2_book_dir<
    'v,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &'v Directory<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> Option<Directory<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut dir = root.open_dir(CACHE_ROOT_DIR).ok()?;
    dir.change_dir(CACHE_V2_DIR).ok()?;
    dir.change_dir(key).ok()?;
    Some(dir)
}

/// The layout config `library` is currently reading under, as the key its
/// cache files are named for. Every per-config reader and writer derives it
/// here rather than taking it as an argument: the store already carries the
/// settings and the page box, and a caller that could pass a different key
/// than the header check compares against would be able to write a file
/// under one config's name holding another's pagination.
fn layout_key_for(library: &ReaderStore) -> u8 {
    layout_cache_key(layout::reader_layout_config(
        library.type_settings(),
        library.portrait(),
    ))
}

/// How many registry-unlisted index files one directory listing will
/// collect. Two configs are resident by design, so a third is already a
/// registry that lost track; past four, whatever wrote them was not this.
const UNLISTED_INDEX_BUDGET: usize = 4;

/// Read a book's resident-config registry from an already-open book
/// directory. `None` covers both "no registry" and "a registry that says
/// nothing usable" — the caller's response to either is to write the config
/// it is opening, so they need not be told apart.
fn read_layout_registry_in<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<LayoutConfigRegistry>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = book
        .open_file_in_dir(CACHE_CONFIG_FILE, Mode::ReadOnly)
        .ok()?;
    let mut bytes = [0u8; CACHE_CONFIG_BYTES];
    read_exact_file(&file, &mut bytes).ok()?;
    decode_layout_config_registry(&bytes).ok()
}

fn write_layout_registry_in<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    registry: &LayoutConfigRegistry,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut bytes = [0u8; CACHE_CONFIG_BYTES];
    if encode_layout_config_registry(registry, &mut bytes).is_err() {
        return false;
    }
    match book.open_file_in_dir(CACHE_CONFIG_FILE, Mode::ReadWriteCreateOrTruncate) {
        Ok(file) => file.write(&bytes).is_ok(),
        Err(_) => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutConfigAdoption {
    /// The registry already listed this config, so a cache built under it
    /// should be waiting — which is what makes a flip back to a
    /// previously-read type size or orientation instant instead of a full
    /// re-wrap. Whether every file really landed is the load's to find out;
    /// this is what the registry claims, and it is reported, not relied on.
    pub resident: bool,
    /// A least-recently-used config whose files this adoption deleted to
    /// stay inside [`proto::cache::CACHE_CONFIG_SLOTS`]. Set only once every
    /// one of them is gone; a partial delete reports `eviction_failed`.
    pub evicted: Option<u8>,
    /// The least-recently-used config this open had to evict and could not:
    /// one of its files refused to delete. Nothing was promoted — the
    /// registry still names that config, so it stays counted and the next
    /// open under this one retries the same deletes.
    pub eviction_failed: Option<u8>,
    /// Pre-per-config artifacts (an unkeyed `BOOK.BIN`, unkeyed section
    /// files) were found and deleted. One-time, on the first open after the
    /// firmware that wrote them.
    pub purged_legacy: bool,
    /// The registry write did not land, so `CFG.BIN` no longer describes which
    /// configs are on the card. Reported so the log names the card that is
    /// failing small writes.
    pub registry_write_failed: bool,
    /// Directory access or creation failed on the way in.
    pub dirs_failed: bool,
}

impl LayoutConfigAdoption {
    pub fn succeeded(&self) -> bool {
        !self.dirs_failed && self.eviction_failed.is_none() && !self.registry_write_failed
    }
}

/// Claim the layout config `library` is reading under as this book's most
/// recently used, and hold the card to at most
/// [`proto::cache::CACHE_CONFIG_SLOTS`] paginated copies of the book.
///
/// Runs once per book open, before anything tries to load or build, because
/// it is what both of those need to be true: the registry has to name this
/// config before an index written under it can be found again, and the
/// least recently used config's files have to be gone before a third one's
/// arrive. Adopting before the load rather than after means an open that
/// then fails has evicted for nothing — one rebuild's worth of cost, traded
/// for never having a build write files no reader can find.
///
/// An eviction that cannot delete every one of that config's files abandons
/// the adoption rather than completing it: the registry is left naming the
/// config whose files are still there. See the failure path below for why
/// that ordering is the one that keeps the bound honest.
pub fn adopt_layout_config<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    library: &ReaderStore,
) -> LayoutConfigAdoption
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let layout_key = layout_key_for(library);
    let mut adoption = LayoutConfigAdoption {
        resident: false,
        evicted: None,
        eviction_failed: None,
        purged_legacy: false,
        registry_write_failed: false,
        dirs_failed: false,
    };
    // The tree has to exist before the registry can be written into it, and
    // this is the earliest point in an open that knows the book's key. A
    // failure here fails the build too, so there is nothing else to do.
    if ensure_v2_cache_dirs(root, key).is_err() {
        cache_log!("cache: adopt config ensure dirs failed key={}", key);
        adoption.dirs_failed = true;
        return adoption;
    }
    let Some(book) = open_v2_book_dir(root, key) else {
        adoption.dirs_failed = true;
        return adoption;
    };
    match book_dir_ownership(&book, source_identity) {
        BookDirOwnership::Match => {
            adoption.purged_legacy = purge_legacy_artifacts_in(&book);
        }
        BookDirOwnership::Mismatch => {
            cache_log!("cache: adopt config identity mismatch key={}", key);
            if !purge_colliding_cache_artifacts_in(&book) {
                adoption.dirs_failed = true;
                return adoption;
            }
            adoption.purged_legacy = true;
        }
        BookDirOwnership::Unreadable => {
            cache_log!("cache: adopt config identity unreadable key={}", key);
            adoption.dirs_failed = true;
            return adoption;
        }
    }
    // A registry file that is there and will not decode gets rebuilt from the
    // index files that really are on the card, rather than started over from
    // empty. Starting empty is what loses the bound: the configs already there
    // stop being counted, so no eviction ever takes them, and every later open
    // is free to add another full section set on top. One refused `CFG.BIN`
    // write is enough to reach that state — it truncates on open — and the cost
    // compounds, because each open that then fails to write leaves one more set
    // uncounted. Repairing on read is what makes that self-correcting.
    //
    // Seeding cannot know which config was most recently used, so the order is
    // whatever the listing gave. The config being adopted is promoted to the
    // front below either way, and the worst a wrong order does is evict the
    // less useful of two survivors.
    let mut registry = match read_layout_registry_in(&book) {
        Some(registry) => registry,
        None => match layout_registry_from_index_files(&book) {
            Some(registry) if !registry.is_empty() || registry_present(&book) => registry,
            Some(_) => LayoutConfigRegistry::new(),
            None => {
                adoption.registry_write_failed = true;
                return adoption;
            }
        },
    };
    adoption.resident = registry.contains(layout_key);
    let evicted = registry.promote(layout_key);
    if let Some(evicted) = evicted {
        // Delete first, then record the shorter registry: a failure between
        // the two leaves files the registry no longer names, which the next
        // clear sweeps by listing. Recording first would risk the opposite —
        // a registry claiming a config whose files are half gone.
        if !delete_layout_config_artifacts_in(&book, evicted) {
            // The delete refused, so the promoted registry does not describe
            // the card and must not be written. Leaving the stored one alone
            // is what keeps the two-config bound honest: it still names the
            // config whose files are still there, so eviction keeps counting
            // them and the next open under this config retries the same
            // deletes. Writing the promotion here would unregister an intact
            // cache instead — no later eviction would look at it again, and a
            // card that has just refused to give space back would be asked
            // for a third set on top of it.
            cache_log!(
                "cache: config evict failed key={} cfg={} kept={}",
                key,
                layout_key,
                evicted
            );
            adoption.eviction_failed = Some(evicted);
            return adoption;
        }
        adoption.evicted = Some(evicted);
    }
    if !write_layout_registry_in(&book, &registry) {
        cache_log!(
            "cache: config registry write failed key={} cfg={}",
            key,
            layout_key
        );
        adoption.registry_write_failed = true;
    }
    adoption
}

/// Rebuild a registry from the per-config index files a book directory actually
/// holds.
///
/// Ordering is lost — a listing cannot say which config was read last — so this
/// only restores the *count*, which is the part eviction needs to hold the
/// two-config bound.
///
/// When more than two configurations exist, any excess key evicted during
/// promotion is explicitly deleted from the card before returning. If directory
/// enumeration or an excess file deletion fails, `None` is returned to prevent
/// an incomplete or corrupt registry from being committed.
fn layout_registry_from_index_files<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<LayoutConfigRegistry>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut registry = LayoutConfigRegistry::new();
    for _ in 0..64 {
        let names = book_index_names_unlisted_by(book, &registry)?;
        if names.is_empty() {
            return Some(registry);
        }
        for name in &names {
            if let Some(layout_key) = layout_key_of_book_index_file_name(name.as_str()) {
                if let Some(evicted) = registry.promote(layout_key) {
                    if !delete_layout_config_artifacts_in(book, evicted) {
                        cache_log!(
                            "cache: reconstruct registry failed to delete excess key {}",
                            evicted
                        );
                        return None;
                    }
                }
            }
        }
    }
    None
}

/// Delete one layout config's cache files: its section files and its index.
/// The other configs' files, and everything settings-independent (TOC.BIN,
/// COVER.BIN, CONT.BIN, the position), stay.
///
/// Section files go first and the index last. Keeping the index until all of
/// its section files are gone ensures that an interrupted deletion leaves the
/// index on disk as the durable marker for reconstruction on the next open.
fn delete_layout_config_artifacts_in<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    layout_key: u8,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let sections_ok = match book.open_dir(CACHE_SECTIONS_DIR) {
        Ok(sections) => sweep_section_files(&sections, |name| {
            layout_key_of_section_file_name(name) == Some(layout_key)
        }),
        // No `SECTIONS/` is the end state this wants, reached without it.
        Err(embedded_sdmmc::Error::NotFound) => true,
        // A directory that would not open may still hold the config's files.
        Err(_) => false,
    };
    if !sections_ok {
        return false;
    }
    let mut name = String::<BOOK_INDEX_FILE_BYTES>::new();
    book_index_file_name(layout_key, &mut name);
    upload_store::remove_file_reclaiming_clusters(book, name.as_str())
        != upload_store::RemoveStatus::Failed
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BookDirOwnership {
    Match,
    Mismatch,
    Unreadable,
}

/// Check whether existing headers in a book cache directory match the expected
/// source identity. Returns `Mismatch` if any readable header identifies a
/// different source book (a 28-bit key collision), `Unreadable` if any file or
/// directory listing could not be safely inspected, or `Match` if all readable
/// headers match (or none exist).
fn book_dir_ownership<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_identity: (u32, u32),
) -> BookDirOwnership
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    use core::fmt::Write;

    let mut saw_mismatch = false;
    let mut saw_unreadable = false;
    let mut index_names = heapless::Vec::<String<SHORT_NAME_BYTES>, 16>::new();
    let mut has_toc = false;
    let mut has_legacy_book = false;

    let err = book.iterate_dir(|entry| {
        if entry.attributes.is_directory() {
            return;
        }
        let mut name = String::<SHORT_NAME_BYTES>::new();
        if write!(name, "{}", entry.name).is_err() {
            saw_unreadable = true;
            return;
        }
        if name.as_str() == CACHE_TOC_FILE {
            has_toc = true;
        } else if name.as_str() == CACHE_BOOK_FILE {
            has_legacy_book = true;
        } else if layout_key_of_book_index_file_name(name.as_str()).is_some()
            && index_names.push(name).is_err()
        {
            saw_unreadable = true;
        }
    });

    if err.is_err() {
        saw_unreadable = true;
    }

    if has_toc {
        match book.open_file_in_dir(CACHE_TOC_FILE, Mode::ReadOnly) {
            Ok(file) => {
                let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
                if read_exact_file(&file, &mut header_bytes).is_ok() {
                    if let Ok(header) = decode_toc_file_header(&header_bytes) {
                        if header.source_hash != source_identity.0
                            || header.source_size != source_identity.1
                        {
                            saw_mismatch = true;
                        }
                    } else {
                        saw_unreadable = true;
                    }
                } else {
                    saw_unreadable = true;
                }
            }
            Err(_) => saw_unreadable = true,
        }
    }

    if has_legacy_book {
        match read_book_index_header(book, CACHE_BOOK_FILE) {
            Some(Some(header)) => {
                if header.source_hash != source_identity.0
                    || header.source_size != source_identity.1
                {
                    saw_mismatch = true;
                }
            }
            Some(None) => saw_unreadable = true,
            None => {}
        }
    }

    for name in &index_names {
        match read_book_index_header(book, name.as_str()) {
            Some(Some(header)) => {
                if header.source_hash != source_identity.0
                    || header.source_size != source_identity.1
                {
                    saw_mismatch = true;
                }
            }
            Some(None) => saw_unreadable = true,
            None => {}
        }
    }

    if saw_mismatch {
        BookDirOwnership::Mismatch
    } else if saw_unreadable {
        BookDirOwnership::Unreadable
    } else {
        BookDirOwnership::Match
    }
}

/// Remove all cache files (sections, TOC/COVER/CONT, indexes, CFG.BIN) in a
/// directory that was occupied by a colliding book identity.
fn purge_colliding_cache_artifacts_in<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // Section files go first. If section deletion fails or is interrupted, the
    // old owner's index files or TOC.BIN remain on disk as durable identity
    // markers so subsequent opens continue to see a Mismatch and retry.
    let sections_ok = match book.open_dir(CACHE_SECTIONS_DIR) {
        Ok(sections) => empty_sections_dir(&sections),
        Err(embedded_sdmmc::Error::NotFound) => true,
        Err(_) => false,
    };
    if !sections_ok {
        return false;
    }
    // Non-marker content files (COVER.BIN, CONT.BIN, POS.BIN, POSA/B.BIN) go next.
    // If any fail to delete, stop before removing identity markers (BK*.BIN,
    // BOOK.BIN, TOC.BIN) or CFG.BIN so durable identity markers remain intact
    // on disk for retry.
    let mut non_markers_ok = true;
    for name in [
        CACHE_COVER_FILE,
        CACHE_CONTENT_FILE,
        POSITION_FILE,
        POSITION_GENERATIONS[0],
        POSITION_GENERATIONS[1],
    ] {
        if upload_store::remove_file_reclaiming_clusters(book, name)
            == upload_store::RemoveStatus::Failed
        {
            non_markers_ok = false;
        }
    }
    if !non_markers_ok {
        return false;
    }
    // Repeatedly sweep per-config index files until a pass proves no unlisted
    // index files remain (draining all batches for >4 index files).
    let mut indexes_ok = true;
    for _ in 0..64 {
        match book_index_names_unlisted_by(book, &LayoutConfigRegistry::new()) {
            Some(names) if names.is_empty() => break,
            Some(names) => {
                for name in &names {
                    if upload_store::remove_file_reclaiming_clusters(book, name.as_str())
                        == upload_store::RemoveStatus::Failed
                    {
                        indexes_ok = false;
                    }
                }
            }
            None => {
                indexes_ok = false;
                break;
            }
        }
    }
    if !indexes_ok {
        return false;
    }
    // Identity markers (BOOK.BIN, TOC.BIN) and the registry (CFG.BIN) go last.
    let mut final_ok = true;
    for name in [CACHE_BOOK_FILE, CACHE_TOC_FILE, CACHE_CONFIG_FILE] {
        if upload_store::remove_file_reclaiming_clusters(book, name)
            == upload_store::RemoveStatus::Failed
        {
            final_ok = false;
        }
    }
    final_ok
}

/// Delete the artifacts of the single-config scheme this replaced: the
/// unkeyed `BOOK.BIN` and the unkeyed `S<spine>.BIN` section files.
///
/// Gated on `BOOK.BIN` being there, which is the marker: nothing writes it
/// anymore, so its presence is the only reason to pay for a sections listing.
/// The marker is therefore deleted only once the sections sweep reports itself
/// finished — it is the whole retry mechanism, and a purge that took it first
/// would leave anything it could not delete with nothing to bring a later open
/// back for it.
///
/// Returns whether there was anything to purge — not whether all of it went.
/// Unlike eviction, no caller needs the stronger answer: no registry ever named
/// these files, so what refuses to go is retried by the next open, which still
/// finds `BOOK.BIN`, and swept by the next clear if that one runs first.
fn purge_legacy_artifacts_in<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if book
        .open_file_in_dir(CACHE_BOOK_FILE, Mode::ReadOnly)
        .is_err()
    {
        return false;
    }
    // Sections first, the marker last. `BOOK.BIN` is the only thing that brings
    // a later open back here, so taking it before the sweep has finished is
    // what would strand whatever the sweep could not: nothing reads the unkeyed
    // names, no registry counts them, and no later pass would look again.
    // Leaving the marker costs one failed open per later open and buys the
    // retry. Nothing reads `BOOK.BIN` itself anymore, so a marker outliving its
    // sections misleads no reader.
    let swept = match book.open_dir(CACHE_SECTIONS_DIR) {
        Ok(sections) => sweep_section_files(&sections, is_legacy_section_file_name),
        // No `SECTIONS/` is the end state the sweep wants, reached without it.
        Err(embedded_sdmmc::Error::NotFound) => true,
        Err(_) => false,
    };
    if swept {
        let _ = upload_store::remove_file_reclaiming_clusters(book, CACHE_BOOK_FILE);
    }
    true
}

/// Delete every file in an open `SECTIONS/` whose name `wanted` accepts, in
/// the same bounded batches as [`empty_sections_dir`]: names are collected
/// per pass and deleted with the listing closed, because deleting while
/// iterating is what the directory walk cannot survive.
///
/// Returns whether a listing came back with nothing left for `wanted` — the
/// only evidence that the sweep is finished. A refused delete, a listing that
/// would not run, a name that could not be read back to test, or a pass
/// budget spent before the directory came back clean all read as false; the
/// caller must not report the files gone on any of them.
fn sweep_section_files<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sections: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    wanted: impl Fn(&str) -> bool,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    use core::fmt::Write;
    let max_passes = MAX_BOOK_SECTIONS.div_ceil(SECTION_SWEEP_BATCH) + 1;
    for _ in 0..max_passes {
        let mut names: heapless::Vec<String<SHORT_NAME_BYTES>, SECTION_SWEEP_BATCH> =
            heapless::Vec::new();
        // Something this pass could not put to `wanted`. A full batch is not
        // that — the next pass re-lists what it left — but a name that would
        // not read back is: it may be one of the files being swept, and a
        // sweep cannot claim what it never got to test.
        let mut blocked = false;
        if sections
            .iterate_dir(|entry| {
                if names.is_full() || entry.attributes.is_directory() {
                    return;
                }
                let mut name = String::<SHORT_NAME_BYTES>::new();
                if write!(name, "{}", entry.name).is_err() {
                    blocked = true;
                    return;
                }
                if wanted(name.as_str()) && names.push(name).is_err() {
                    blocked = true;
                }
            })
            .is_err()
        {
            return false;
        }
        if names.is_empty() {
            return !blocked;
        }
        for name in &names {
            if upload_store::remove_file_reclaiming_clusters(sections, name.as_str())
                == upload_store::RemoveStatus::Failed
            {
                // A refusal will repeat on the next pass and spend the whole
                // budget re-finding the same file. Leaving it costs SD space
                // until the next clear, which lists rather than names.
                return false;
            }
        }
    }
    false
}

/// Open the book's `CONT.BIN` (settings-independent content cache) and run
/// `f` with it.
pub fn with_v2_content_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let dir = open_v2_book_dir(root, key)?;
    let file = dir.open_file_in_dir(CACHE_CONTENT_FILE, mode).ok()?;
    Some(f(&file))
}

/// Delete the book's `CONT.BIN`. Failures are ignored — a stale or corrupt
/// content cache is only ever an accelerator, and the next full build
/// recreates it.
pub fn delete_v2_content_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some(dir) = open_v2_book_dir(root, key) else {
        return;
    };
    let _ = upload_store::remove_file_reclaiming_clusters(&dir, CACHE_CONTENT_FILE);
}

/// Captures the build's `push_block` stream into `<key>/CONT.BIN` so a later
/// type-settings change replays it instead of re-reading and re-parsing the
/// EPUB. Failure is one-way and silent: the capture disables itself, and
/// `finish` deletes the partial file — CONT.BIN is purely an accelerator, so
/// the build itself never fails on its account.
pub struct ContentCapture<
    'd,
    's,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// `None` once disabled (setup or write failure).
    file: Option<File<'d, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
    stage: &'s mut [u8],
    len: usize,
    source_identity: (u32, u32),
    spine_count: u16,
}

impl<'d, 's, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    ContentCapture<'d, 's, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// Create `CONT.BIN` in the book's cache dir (from `open_v2_content_dir`)
    /// and write its header with `complete = false`; the flag flips in
    /// `finish` only after the whole spine walk captured. Any failure — or
    /// `None` for the dir — returns a disabled capture.
    pub fn begin(
        dir: Option<&'d Directory<'d, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
        source_identity: (u32, u32),
        stage: &'s mut [u8],
    ) -> Self {
        let mut capture = Self {
            file: None,
            stage,
            len: 0,
            source_identity,
            spine_count: 0,
        };
        let Some(dir) = dir else {
            return capture;
        };
        let Ok(file) = dir.open_file_in_dir(CACHE_CONTENT_FILE, Mode::ReadWriteCreateOrTruncate)
        else {
            return capture;
        };
        let mut header = [0u8; CONTENT_HEADER_BYTES];
        let encoded = encode_content_header(
            ContentHeader {
                source_hash: source_identity.0,
                source_size: source_identity.1,
                complete: false,
                spine_count: 0,
                content_len: 0,
            },
            &mut header,
        );
        if encoded.is_ok() && file.write(&header).is_ok() {
            capture.file = Some(file);
        }
        capture
    }

    /// Re-open the `CONT.BIN` an earlier step of the same progressive build
    /// left behind, positioned to append. `spine_count` is what that step
    /// reported through [`Self::suspend`]; carrying it is what lets the final
    /// step write an accurate header.
    //
    /// The header is deliberately not touched here. It has said
    /// `complete = false` since [`Self::begin`] and stays that way until the
    /// walk actually ends, so a build abandoned between steps — by sleep, by
    /// the sync loan — leaves a file replay will refuse rather than a
    /// truncated stream it would trust.
    pub fn resume(
        dir: Option<&'d Directory<'d, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
        source_identity: (u32, u32),
        stage: &'s mut [u8],
        spine_count: u16,
    ) -> Self {
        let mut capture = Self {
            file: None,
            stage,
            len: 0,
            source_identity,
            spine_count,
        };
        let Some(dir) = dir else {
            return capture;
        };
        if let Ok(file) = dir.open_file_in_dir(CACHE_CONTENT_FILE, Mode::ReadWriteAppend) {
            capture.file = Some(file);
        }
        capture
    }

    /// Flush what is staged and hand the file back to the next step of the
    /// same build, reporting whether the capture is still healthy and how many
    /// spine groups it has recorded.
    //
    /// A `false` here is not a build failure — CONT.BIN is only ever an
    /// accelerator — but it is one-way: the next step starts
    /// [`disabled`](Self::disabled) rather than appending to a file with a
    /// hole in it, and the incomplete header keeps replay away from it.
    pub fn suspend(mut self) -> (bool, u16) {
        if let Some(file) = self.file.as_ref() {
            if staged_flush(file, self.stage, &mut self.len).is_err() {
                self.file = None;
            }
        }
        let healthy = self.file.is_some();
        drop(self.file.take());
        (healthy, self.spine_count)
    }

    /// A capture that records nothing, for a continuation whose earlier step
    /// already gave up on CONT.BIN. The build carries on unaffected; only the
    /// next settings change pays a full rebuild instead of a replay.
    pub fn disabled(stage: &'s mut [u8], source_identity: (u32, u32)) -> Self {
        Self {
            file: None,
            stage,
            len: 0,
            source_identity,
            spine_count: 0,
        }
    }

    /// Record one `push_block` call. The text follows the fixed record
    /// header; see `proto::cache::ContentRecordHeader`.
    pub fn push_block_record(
        &mut self,
        spine_index: u16,
        text: &str,
        role: proto::text::TextRole,
        style: proto::text::FontStyle,
        align: proto::text::TextAlign,
        paragraph_end: bool,
    ) {
        if self.file.is_none() {
            return;
        }
        let Ok(text_len) = u16::try_from(text.len()) else {
            self.file = None;
            return;
        };
        if text.len() > crate::READER_XHTML_SCRATCH {
            self.file = None;
            return;
        }
        let mut header = [0u8; CONTENT_RECORD_HEADER_BYTES];
        if encode_content_record_header(
            ContentRecordHeader {
                spine_index,
                text_len,
                role,
                style,
                align,
                paragraph_end,
                spine_end: false,
            },
            &mut header,
        )
        .is_err()
        {
            self.file = None;
            return;
        }

        self.stage_push(&header);
        self.stage_push(text.as_bytes());
    }

    /// Record the end of one spine item, so replay knows where to finish
    /// the current section run.
    pub fn spine_end(&mut self, spine_index: u16) {
        if self.file.is_none() {
            return;
        }
        self.spine_count = self.spine_count.saturating_add(1);
        let mut header = [0u8; CONTENT_RECORD_HEADER_BYTES];
        if encode_content_record_header(
            ContentRecordHeader {
                spine_index,
                text_len: 0,
                role: proto::text::TextRole::Body,
                style: proto::text::FontStyle::Regular,
                align: proto::text::TextAlign::Left,
                paragraph_end: false,
                spine_end: true,
            },
            &mut header,
        )
        .is_err()
        {
            self.file = None;
            return;
        }
        self.stage_push(&header);
    }

    /// Batch small record writes through the caller-owned staging buffer
    /// via the shared `staged_write`; any write failure disables the
    /// capture.
    fn stage_push(&mut self, bytes: &[u8]) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        if staged_write(file, self.stage, &mut self.len, bytes).is_err() {
            self.file = None;
        }
    }

    /// Flush and mark the capture complete (`keep = true`, the whole spine
    /// walk captured cleanly), or delete the partial file through the same
    /// directory handle the capture was opened from. Returns whether a
    /// complete CONT.BIN was kept.
    pub fn finish(
        mut self,
        dir: Option<&Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
        keep: bool,
    ) -> bool {
        let mut kept = false;
        if keep {
            if let Some(file) = self.file.as_ref() {
                if staged_flush(file, self.stage, &mut self.len).is_err() {
                    self.file = None;
                }
            }
            if let Some(file) = self.file.as_ref() {
                let file_len = file.length();
                let mut header = [0u8; CONTENT_HEADER_BYTES];
                kept = encode_content_header(
                    ContentHeader {
                        source_hash: self.source_identity.0,
                        source_size: self.source_identity.1,
                        complete: true,
                        spine_count: self.spine_count,
                        content_len: file_len,
                    },
                    &mut header,
                )
                .is_ok()
                    && file.seek_from_start(0).is_ok()
                    && file.write(&header).is_ok();
            }
        }
        drop(self.file.take());
        if !kept {
            if let Some(dir) = dir {
                let _ = upload_store::remove_file_reclaiming_clusters(dir, CACHE_CONTENT_FILE);
            }
        }
        kept
    }
}

/// Load only the labels (title/author) and the resident TOC copy from a
/// book's v2 index, accepting any layout config or custom-font identity:
/// the content replay path runs precisely when the index is layout-invalid,
/// but its TOC and labels are settings-independent and must survive into
/// the rewritten index. Deliberately does not touch the section index.
///
/// "Any config" is now literal: the index this wants belongs to whichever
/// config the book was last read under, not the one being built, so it works
/// through the registry's slots in order. Slot 0 is the config being adopted
/// and its index is exactly the one that just missed, so in practice the
/// answer comes from the slot behind it.
pub fn load_v2_book_labels_and_toc<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_any_v2_book_file(root, key, true, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        read_exact_file(file, &mut header_bytes).ok()?;
        let header = decode_book_v2_header(&header_bytes).ok()?;
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.section_count as usize > MAX_BOOK_SECTIONS
            || !v2_toc_label_bounds_ok(&header)
        {
            return None;
        }
        let toc_offset =
            BOOK_V2_HEADER_BYTES + header.section_count as usize * BOOK_V2_SECTION_RECORD_BYTES;
        file.seek_from_start(toc_offset as u32).ok()?;
        (read_v2_toc_into_library(file, &header, library)
            && read_v2_labels_into_library(file, &header, library))
        .then_some(true)
    })
    .unwrap_or(false)
}

fn with_v2_section_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    layout_key: u8,
    spine: u16,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
    let book_dir = cache.open_dir(key).ok()?;
    let sections = book_dir.open_dir(CACHE_SECTIONS_DIR).ok()?;
    let mut name = String::<CACHE_SECTION_FILE_BYTES>::new();
    section_file_name(layout_key, spine, &mut name);
    let file = sections.open_file_in_dir(name.as_str(), mode).ok()?;
    Some(f(&file))
}

fn with_v2_book_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    layout_key: u8,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
    let book_dir = cache.open_dir(key).ok()?;
    let mut name = String::<BOOK_INDEX_FILE_BYTES>::new();
    book_index_file_name(layout_key, &mut name);
    let file = book_dir.open_file_in_dir(name.as_str(), mode).ok()?;
    Some(f(&file))
}

/// Run `f` against whichever of the book's per-config indexes is there,
/// for the readers after facts that do not depend on the layout config: the
/// source identity, the title, the TOC. They used to open one fixed
/// `BOOK.BIN`; now the index is per config, so "the book's index" means
/// asking the registry which configs are resident and taking the first that
/// opens. `f` returning `None` means "this index did not answer" and moves
/// on to the next.
///
/// `scan_if_unlisted` covers the registry being lost while index files
/// survive: a bounded directory listing finds them by name. Callers on a
/// scan-time path leave it off, because for them a miss costs only a
/// fallback label, not a cache.
fn with_any_v2_book_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    scan_if_unlisted: bool,
    mut f: impl for<'a> FnMut(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> Option<R>,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let book_dir = open_v2_book_dir(root, key)?;
    let mut name = String::<BOOK_INDEX_FILE_BYTES>::new();
    let registry = read_layout_registry_in(&book_dir).unwrap_or_default();
    for layout_key in registry.keys() {
        book_index_file_name(*layout_key, &mut name);
        if let Ok(file) = book_dir.open_file_in_dir(name.as_str(), Mode::ReadOnly) {
            if let Some(answer) = f(&file) {
                return Some(answer);
            }
        }
    }
    if !scan_if_unlisted {
        return None;
    }
    // A listing that would not run reads as nothing left to try. Every caller
    // here is after a config-independent fact and treats `None` as a miss it
    // has a fallback for — unlike `read_cache_header`, whose own loop must tell
    // a failed listing from an empty directory because a delete hangs on it.
    for unlisted in book_index_names_unlisted_by(&book_dir, &registry).unwrap_or_default() {
        if let Ok(file) = book_dir.open_file_in_dir(unlisted.as_str(), Mode::ReadOnly) {
            if let Some(answer) = f(&file) {
                return Some(answer);
            }
        }
    }
    None
}

/// The per-config index files present in a book's cache directory that the
/// registry does not name — what is left to try when the registry was lost
/// or never written. Bounded by [`CACHE_CONFIG_SLOTS`]-plus-slack rather
/// than by the 64 keys a name could carry: past that, whatever wrote them
/// was not this firmware.
fn book_index_names_unlisted_by<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    registry: &LayoutConfigRegistry,
) -> Option<heapless::Vec<String<BOOK_INDEX_FILE_BYTES>, UNLISTED_INDEX_BUDGET>>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    use core::fmt::Write;
    let mut found = heapless::Vec::new();
    // `None` on a listing that would not run, never an empty result. The caller
    // reads "nothing here" as licence to delete the directory, and a card that
    // refused to enumerate has not said that.
    if book
        .iterate_dir(|entry| {
            if found.is_full() || entry.attributes.is_directory() {
                return;
            }
            let mut name = String::<BOOK_INDEX_FILE_BYTES>::new();
            if write!(name, "{}", entry.name).is_err() {
                return;
            }
            let Some(layout_key) = layout_key_of_book_index_file_name(name.as_str()) else {
                return;
            };
            if !registry.contains(layout_key) {
                let _ = found.push(name);
            }
        })
        .is_err()
    {
        return None;
    }
    Some(found)
}

fn with_v2_toc_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
    let book_dir = cache.open_dir(key).ok()?;
    let file = book_dir.open_file_in_dir(CACHE_TOC_FILE, mode).ok()?;
    Some(f(&file))
}

/// Load the on-disk chapter list (TOC.BIN) into the store's text buffer for
/// the Chapters overview. Reuses the section text buffer -- the reading
/// section is reloaded on exit -- so no resident RAM is spent on the list.
pub fn load_v2_toc_into_text<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
    window_start: usize,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_toc_file(root, key, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            cache_log!("toc window: header read failed");
            return false;
        }
        let Ok(header) = decode_toc_file_header(&header_bytes) else {
            cache_log!("toc window: header decode failed");
            return false;
        };
        if header.source_hash != source_identity.0 || header.source_size != source_identity.1 {
            cache_log!("toc window: identity mismatch");
            return false;
        }
        let total = header.chapter_count as usize;
        let start = window_start.min(total.saturating_sub(1));
        let len = (total - start).min(crate::store::TOC_WINDOW_CAPACITY);
        let offset = TOC_FILE_HEADER_BYTES + start * TOC_CHAPTER_RECORD_BYTES;
        if file.seek_from_start(offset as u32).is_err() {
            cache_log!("toc window: seek failed");
            return false;
        }
        let bytes = len.saturating_mul(TOC_CHAPTER_RECORD_BYTES);
        let Some(buf) = library.cached_text_mut(bytes) else {
            return false;
        };
        if read_exact_file(file, buf).is_err() {
            cache_log!("toc window: body read failed");
            return false;
        }
        library.set_toc_window(start, len, total);
        true
    })
    .unwrap_or(false)
}

/// Fill the resident per-section `chapter_start` marks from TOC.BIN, so the
/// firmware can resolve the current chapter for any reading page across the
/// whole book -- past the 128-entry resident/event caps and past chapter 255
/// (the map is bounded by the section count, not the chapter count). The
/// book index must already be loaded so spines resolve to sections.
pub fn load_v2_toc_chapter_map<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_toc_file(root, key, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return false;
        }
        let Ok(header) = decode_toc_file_header(&header_bytes) else {
            return false;
        };
        if header.source_hash != source_identity.0 || header.source_size != source_identity.1 {
            return false;
        }
        let section_count = library.book_section_count.min(MAX_BOOK_SECTIONS);
        library.chapter_start.fill(0);
        library.chapter_start_ready = false;
        if !read_records_batched(
            file,
            TOC_CHAPTER_RECORD_BYTES,
            header.chapter_count as usize,
            |index, bytes| {
                let spine = i16::from_le_bytes([bytes[0], bytes[1]]);
                proto::cache::mark_chapter_start(
                    &mut library.chapter_start[..section_count],
                    &library.book_sections[..section_count],
                    index as u16,
                    spine,
                );
                true
            },
        ) {
            return false;
        }
        library.chapter_start_ready = true;
        true
    })
    .unwrap_or(false)
}

/// Read one chapter's title straight from its TOC.BIN record (a single seek
/// and 48-byte read) into the resident current-chapter slot, so the Home and
/// sleep colophons can name a chapter the 128-entry resident list omits.
pub fn read_v2_toc_chapter_title<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    chapter: u16,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_toc_file(root, key, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return false;
        }
        let Ok(header) = decode_toc_file_header(&header_bytes) else {
            return false;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || chapter >= header.chapter_count
        {
            return false;
        }
        let offset = (TOC_FILE_HEADER_BYTES + chapter as usize * TOC_CHAPTER_RECORD_BYTES) as u32;
        if file.seek_from_start(offset).is_err() {
            return false;
        }
        let mut record = [0u8; TOC_CHAPTER_RECORD_BYTES];
        if read_exact_file(file, &mut record).is_err() {
            return false;
        }
        let Ok(parsed) = decode_toc_chapter(&record) else {
            return false;
        };
        library.set_current_chapter(chapter, parsed.title_str(), source_identity);
        true
    })
    .unwrap_or(false)
}

/// Write the full chapter list to TOC.BIN: a header plus `chapter_count`
/// pre-encoded `TOC_CHAPTER_RECORD_BYTES` records (the caller assembles them
/// in a scratch buffer during the TOC parse). Keeping the list on the card
/// lets a long book's TOC stay out of the tight reader RAM.
pub fn write_v2_toc_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    chapter_count: usize,
    records: &[u8],
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if ensure_v2_cache_dirs(root, key).is_err() {
        return false;
    }
    with_v2_toc_file(root, key, Mode::ReadWriteCreateOrTruncate, |file| {
        let header = TocFileHeader {
            source_hash: source_identity.0,
            source_size: source_identity.1,
            chapter_count: chapter_count.min(u16::MAX as usize) as u16,
        };
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        if encode_toc_file_header(header, &mut header_bytes).is_err()
            || file.write(&header_bytes).is_err()
        {
            return false;
        }
        !records.is_empty() && file.write(records).is_ok() || records.is_empty()
    })
    .unwrap_or(false)
}

fn with_v2_cover_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
    let book_dir = cache.open_dir(key).ok()?;
    let file = book_dir.open_file_in_dir(CACHE_COVER_FILE, mode).ok()?;
    Some(f(&file))
}

fn load_v2_section_body<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    header: SectionV2Header,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let page_count = header.page_count as usize;
    let block_count = header.block_count as usize;
    let text_bytes = header.text_bytes as usize;
    if !library.can_hold_section(page_count, block_count, text_bytes) {
        return false;
    }
    library.clear_lines();
    if !read_records_batched(file, PAGE_RECORD_BYTES, page_count, |index, bytes| {
        let Ok(page) = decode_page(bytes) else {
            return false;
        };
        library.set_cached_page(index, page, header.spine)
    }) {
        return false;
    }
    if !read_records_batched(file, BLOCK_RECORD_BYTES, block_count, |index, bytes| {
        let Ok(block) = decode_block(bytes) else {
            return false;
        };
        library.set_cached_block(
            index,
            block,
            display_style_for_proto_style(block.style),
            header.spine,
        )
    }) {
        return false;
    }
    if !read_records_batched(file, 1, block_count, |index, bytes| {
        library.set_cached_paragraph_end(index, bytes[0] & 0b01 != 0)
            && library.set_cached_paragraph_start(index, bytes[0] & 0b10 != 0)
    }) {
        return false;
    }
    let Some(text) = library.cached_text_mut(text_bytes) else {
        return false;
    };
    if read_exact_file(file, text).is_err() {
        return false;
    }
    library.finish_cached_section(
        header.spine,
        page_count,
        block_count,
        text_bytes,
        header.partial,
    );
    true
}

fn write_v2_section_body<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_identity: (u32, u32),
    spine: u16,
    library: &ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let header = SectionV2Header {
        source_hash: source_identity.0,
        source_size: source_identity.1,
        spine,
        page_count: library.page_count.min(u16::MAX as usize) as u16,
        block_count: library.block_count.min(u16::MAX as usize) as u16,
        text_bytes: library.text_len.min(u32::MAX as usize) as u32,
        viewport_width: 800,
        viewport_height: 480,
        font_config: layout::reader_layout_config(library.type_settings(), library.portrait()),
        custom_font_identity: library.custom_font_identity(),
        bytes_consumed: 0,
        total_bytes: 0,
        partial: library.section_partial,
    };
    let mut bytes = [0u8; SECTION_V2_HEADER_BYTES];
    if encode_section_v2_header(header, &mut bytes).is_err() || file.write(&bytes).is_err() {
        cache_log!("cache: v2 write header failed");
        return false;
    }
    write_section_records(file, library)
}

fn write_section_records<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    library: &ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut record = [0u8; 16];
    let mut stage = WriteStage::new(file);
    for page in library.pages.iter().take(library.page_count) {
        if encode_page(*page, &mut record[..PAGE_RECORD_BYTES]).is_err()
            || stage.push(&record[..PAGE_RECORD_BYTES]).is_err()
        {
            cache_log!("cache: write page record failed");
            return false;
        }
    }
    for block in library.blocks.iter().take(library.block_count) {
        if encode_block(*block, &mut record[..BLOCK_RECORD_BYTES]).is_err()
            || stage.push(&record[..BLOCK_RECORD_BYTES]).is_err()
        {
            cache_log!("cache: write block record failed");
            return false;
        }
    }
    // One flag byte per block: bit 0 marks a paragraph end, bit 1 a
    // paragraph start (the indented opening line).
    for index in 0..library.block_count {
        let end = library.block_paragraph_end[index];
        let start = library.block_paragraph_start[index];
        let flag = (end as u8) | ((start as u8) << 1);
        if stage.push(&[flag]).is_err() {
            cache_log!("cache: write paragraph flag failed");
            return false;
        }
    }
    if stage.flush().is_err() {
        cache_log!("cache: write staged records failed");
        return false;
    }
    if file.write(&library.text[..library.text_len]).is_err() {
        cache_log!("cache: write text failed");
        return false;
    }
    true
}

/// Staging size for batched record reads. Kept small: this sits on the
/// stack inside the EPUB open path, in the same tight budget region.
const RECORD_STAGE_BYTES: usize = 256;

/// Append `bytes` to `file` through a caller-owned staging buffer: flush
/// the stage when the bytes don't fit the remaining capacity, and bypass
/// it entirely for writes at least as large as the whole buffer. The one
/// implementation of the batching arithmetic — `WriteStage` and
/// `ContentCapture` both delegate here.
fn staged_write<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    buf: &mut [u8],
    len: &mut usize,
    bytes: &[u8],
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if bytes.len() > buf.len() - *len {
        staged_flush(file, buf, len)?;
    }
    if bytes.len() >= buf.len() {
        return file.write(bytes).map_err(|_| ());
    }
    buf[*len..*len + bytes.len()].copy_from_slice(bytes);
    *len += bytes.len();
    Ok(())
}

/// Write out whatever `staged_write` has accumulated. `len` resets even on
/// failure so a disabled writer can't replay stale bytes.
fn staged_flush<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    buf: &[u8],
    len: &mut usize,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if *len == 0 {
        return Ok(());
    }
    let result = file.write(&buf[..*len]).map_err(|_| ());
    *len = 0;
    result
}

/// Batch small writes through one staging buffer — the write-side twin of
/// `read_records_batched`. The FAT layer pays the same per-call overhead on
/// writes (block lookup plus a read-modify-write of the current sector), so
/// 1-16 byte record writes dominate section write time without it.
struct WriteStage<
    'f,
    'v,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: &'f File<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    buf: [u8; RECORD_STAGE_BYTES],
    len: usize,
}

impl<'f, 'v, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    WriteStage<'f, 'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn new(file: &'f File<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> Self {
        Self {
            file,
            buf: [0u8; RECORD_STAGE_BYTES],
            len: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), ()> {
        staged_write(self.file, &mut self.buf, &mut self.len, bytes)
    }

    fn flush(&mut self) -> Result<(), ()> {
        staged_flush(self.file, &self.buf, &mut self.len)
    }
}

/// Read `count` fixed-size records through one staging buffer instead of
/// one embedded-sdmmc read call per record; the FAT layer pays per-call
/// overhead, so 4-16 byte reads dominate section and index load time.
fn read_records_batched<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    record_len: usize,
    count: usize,
    mut apply: impl FnMut(usize, &[u8]) -> bool,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if record_len == 0 || record_len > RECORD_STAGE_BYTES {
        return false;
    }
    let mut stage = [0u8; RECORD_STAGE_BYTES];
    let per_batch = (RECORD_STAGE_BYTES / record_len) * record_len;
    let mut index = 0usize;
    while index < count {
        let take = ((count - index) * record_len).min(per_batch);
        if read_exact_file(file, &mut stage[..take]).is_err() {
            return false;
        }
        for chunk in stage[..take].chunks_exact(record_len) {
            if !apply(index, chunk) {
                return false;
            }
            index += 1;
        }
    }
    true
}

#[allow(clippy::result_unit_err)] // Nothing to report but failure: the card gives no distinguishable reason and every caller only branches on success.
pub fn read_exact_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    mut out: &mut [u8],
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    while !out.is_empty() {
        let read = file.read(out).map_err(|_| ())?;
        if read == 0 {
            return Err(());
        }
        let tmp = out;
        out = &mut tmp[read..];
    }
    Ok(())
}

fn display_style_for_proto_style(style: proto::text::FontStyle) -> FontStyle {
    match style {
        proto::text::FontStyle::BoldItalic => FontStyle::BoldItalic,
        proto::text::FontStyle::Bold => FontStyle::Bold,
        proto::text::FontStyle::Italic => FontStyle::Italic,
        proto::text::FontStyle::Regular => FontStyle::Regular,
    }
}
