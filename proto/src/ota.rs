//! ESP-IDF application-image integrity validation.
//!
//! Before a candidate firmware is written into the update slot — over the
//! SD card or the air — it must be checked as thoroughly as the bootloader
//! would check it, because the in-app update path writes the slot raw and flips
//! `otadata` directly (bypassing the ROM's `esp_image_verify`, which rejects our
//! wide-eFuse-range image). A truncated or corrupt `.bin` that reached `otadata`
//! would brick the device on next boot, so it is rejected here first.
//!
//! This mirrors the ESP-IDF image format and the FreeInk SDK / CrossPoint
//! `FirmwareFlasher::validateImageFile`: image magic, a walk of the segment
//! table, the trailing XOR checksum byte, and the appended SHA-256 (when the
//! header flags it). It streams the image in fixed chunks so it needs no heap
//! and only a few hundred bytes of stack — the whole image never sits in RAM.

use crc::{Algorithm, Crc};
use sha2::{Digest, Sha256};

/// First byte of every ESP-IDF application image.
pub const IMAGE_MAGIC: u8 = 0xE9;

const HEADER_LEN: usize = 24;
const SEG_HEADER_LEN: usize = 8;
const CHECKSUM_SEED: u8 = 0xEF;
const SHA_TRAILER_LEN: usize = 32;
const MIN_IMAGE_LEN: usize = 64 * 1024;
const STREAM_CHUNK: usize = 512;

/// Source of image bytes, read strictly forward. `read_exact` must fill the
/// whole buffer from the current offset or report an error (a short read at EOF
/// is an error — the validator already knows the expected length).
pub trait ImageSource {
    /// The error is deliberately unit: the validator maps any read failure
    /// to `ImageError::Read`, so a richer type would only be discarded.
    #[allow(clippy::result_unit_err)]
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageError {
    /// Smaller than any real firmware — almost certainly truncated.
    TooSmall,
    /// Larger than the destination OTA partition.
    TooLarge,
    /// First byte is not `0xE9`.
    BadMagic,
    /// Segment table is malformed or a segment runs past end-of-file.
    BadSegments,
    /// XOR checksum byte does not match the image body.
    BadChecksum,
    /// Appended SHA-256 does not match the computed hash.
    BadSha,
    /// Body + padding (+ SHA) length does not equal the file length.
    BadSize,
    /// A slot image carries no SHA-256 trailer, so nothing covers its segment
    /// headers. Only rejected for resident images — see [`validate_flash_image`].
    NoHashTrailer,
    /// Built for a different chip than this firmware runs on.
    WrongChip,
    /// A segment is laid out in a way the bootloader would refuse: a length it
    /// cannot load, or a flash-mapped segment whose address and file offset
    /// disagree about where in the MMU page it sits.
    BadSegmentLayout,
    /// The source reported a read error / short read.
    Read,
}

/// `chip_id` for the ESP32-C3, the only silicon either board uses. The
/// bootloader refuses an image stamped for another chip, so an anchor claiming
/// one would not boot however intact it is.
pub const EXPECTED_CHIP_ID: u16 = 5;

/// The two windows the ESP32-C3 maps flash into: data (DROM) and instruction
/// (IROM). Segments loaded elsewhere are copied to RAM instead of mapped —
/// including the zero-address padding segments esptool inserts — and the
/// mapping rule below does not apply to them.
fn is_flash_mapped(load_addr: u32) -> bool {
    (0x3C00_0000..0x3C80_0000).contains(&load_addr)
        || (0x4200_0000..0x4280_0000).contains(&load_addr)
}

/// One application partition discovered in the ESP-IDF partition table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppPartition {
    pub offset: u32,
    pub size: u32,
}

/// Flash locations needed by the two-slot OTA updater.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtaLayout {
    pub otadata: AppPartition,
    pub slots: [AppPartition; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionTableError {
    TooShort,
    BadEntry,
    DuplicateOtaData,
    DuplicateOtaSlot(u8),
    /// The table defines `ota_2` or beyond; the two-slot updater's sequence
    /// math would disagree with the bootloader's modulo-N slot selection.
    UnsupportedOtaSlot(u8),
    MissingOtaData,
    MissingOtaSlot(u8),
    InvalidBounds,
    Overlap,
}

const PARTITION_ENTRY_LEN: usize = 32;
const PARTITION_MAGIC: u16 = 0x50AA;
const PARTITION_MD5_MAGIC: u16 = 0xEBEB;
const PARTITION_TYPE_APP: u8 = 0x00;
const PARTITION_TYPE_DATA: u8 = 0x01;
const PARTITION_SUBTYPE_DATA_OTA: u8 = 0x00;
const PARTITION_SUBTYPE_APP_OTA_0: u8 = 0x10;
// ESP-IDF defines app subtypes ota_0 (0x10) through ota_15 (0x1F).
const PARTITION_SUBTYPE_APP_OTA_15: u8 = 0x1F;

/// Discover the actual OTA locations from an ESP-IDF partition table.
///
/// Locked X3 units may retain the stock table (`ota_1` at `0x780000`) while
/// CrossPoint/Marigold/Calendula installations use `ota_1` at `0x650000`. The
/// updater must follow the table the bootloader will use rather than assuming
/// either layout. Only the `otadata`, `ota_0`, and `ota_1` entries are
/// retained.
pub fn parse_ota_layout(table: &[u8], flash_size: u32) -> Result<OtaLayout, PartitionTableError> {
    if table.len() < PARTITION_ENTRY_LEN {
        return Err(PartitionTableError::TooShort);
    }

    let mut otadata = None;
    let mut slots = [None, None];

    for raw in table.chunks_exact(PARTITION_ENTRY_LEN) {
        let magic = u16::from_le_bytes([raw[0], raw[1]]);
        if magic == u16::MAX || magic == PARTITION_MD5_MAGIC {
            break;
        }
        if magic != PARTITION_MAGIC {
            return Err(PartitionTableError::BadEntry);
        }

        let partition = AppPartition {
            offset: u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
            size: u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
        };

        match (raw[2], raw[3]) {
            (PARTITION_TYPE_DATA, PARTITION_SUBTYPE_DATA_OTA) => {
                validate_partition(partition, flash_size)?;
                if otadata.replace(partition).is_some() {
                    return Err(PartitionTableError::DuplicateOtaData);
                }
            }
            (PARTITION_TYPE_APP, subtype)
                if (PARTITION_SUBTYPE_APP_OTA_0..=PARTITION_SUBTYPE_APP_OTA_0 + 1)
                    .contains(&subtype) =>
            {
                validate_partition(partition, flash_size)?;
                let slot = usize::from(subtype - PARTITION_SUBTYPE_APP_OTA_0);
                if slots[slot].replace(partition).is_some() {
                    return Err(PartitionTableError::DuplicateOtaSlot(slot as u8));
                }
            }
            // Fail closed on ota_2..=ota_15: the bootloader selects the active
            // slot as seq % N over *all* OTA partitions, so treating a valid
            // N>2 table as two-slot could erase the running partition.
            (PARTITION_TYPE_APP, subtype)
                if (PARTITION_SUBTYPE_APP_OTA_0..=PARTITION_SUBTYPE_APP_OTA_15)
                    .contains(&subtype) =>
            {
                return Err(PartitionTableError::UnsupportedOtaSlot(
                    subtype - PARTITION_SUBTYPE_APP_OTA_0,
                ));
            }
            _ => {}
        }
    }

    let otadata = otadata.ok_or(PartitionTableError::MissingOtaData)?;
    let slot0 = slots[0].ok_or(PartitionTableError::MissingOtaSlot(0))?;
    let slot1 = slots[1].ok_or(PartitionTableError::MissingOtaSlot(1))?;
    if otadata.size < 0x2000 {
        return Err(PartitionTableError::InvalidBounds);
    }
    if partitions_overlap(otadata, slot0)
        || partitions_overlap(otadata, slot1)
        || partitions_overlap(slot0, slot1)
    {
        return Err(PartitionTableError::Overlap);
    }

    Ok(OtaLayout {
        otadata,
        slots: [slot0, slot1],
    })
}

fn validate_partition(partition: AppPartition, flash_size: u32) -> Result<(), PartitionTableError> {
    let Some(end) = partition.offset.checked_add(partition.size) else {
        return Err(PartitionTableError::InvalidBounds);
    };
    if partition.size == 0
        || partition.offset & 0xFFF != 0
        || partition.size & 0xFFF != 0
        || end > flash_size
    {
        return Err(PartitionTableError::InvalidBounds);
    }
    Ok(())
}

fn partitions_overlap(a: AppPartition, b: AppPartition) -> bool {
    a.offset < b.offset + b.size && b.offset < a.offset + a.size
}

/// Validate a candidate ESP-IDF image end to end.
///
/// `image_len` is the exact byte length of the source. `partition_len`, when
/// given, bounds the image to the destination OTA partition size. On `Ok(())`
/// the entire source has been consumed and the image is safe to flash.
pub fn validate_image<S: ImageSource>(
    src: &mut S,
    image_len: usize,
    partition_len: Option<usize>,
) -> Result<(), ImageError> {
    if image_len < MIN_IMAGE_LEN {
        return Err(ImageError::TooSmall);
    }
    if let Some(limit) = partition_len {
        if image_len > limit {
            return Err(ImageError::TooLarge);
        }
    }
    walk_image(src, image_len, Some(image_len), Strictness::Staged)
}

/// How hard an image has to work to be believed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Strictness {
    /// A staged file. It is about to be written to the update slot, and the
    /// bootloader gets the final say afterwards with the anchor still intact
    /// behind it — so a structural check is enough here, and a SHA trailer is
    /// not demanded. What the image *is* is settled separately, by
    /// [`staged_image_is_installable`].
    Staged,
    /// An image already resident in a slot, being read as evidence that the
    /// bootloader *would* boot it. Both answers cost something here — a wrong
    /// yes strands the update in a bounce the bootloader refuses, a wrong no
    /// refuses an update that was fine — so every condition we can cheaply
    /// reproduce is applied.
    Resident,
}

/// Validate an image already resident in a flash partition, to the same
/// standard as a staged one — segment walk, XOR checksum, appended SHA-256.
///
/// Unlike a file, a partition has no length: past the image lies whatever was
/// there before. The ESP-IDF image is self-delimiting, so the walk measures it
/// and `partition_len` only bounds how far that walk may run.
///
/// This is what makes a slot's contents *evidence about the bootloader* — it
/// loads a slot only if the image verifies — which is what
/// [`plan_update_action`] needs of the anchor before handing it the update. A
/// magic byte alone proves nothing: a flash interrupted partway through writing
/// slot 0 leaves the first bytes intact and the tail missing.
///
/// # Why the SHA-256 trailer is mandatory here
///
/// The XOR checksum covers segment *data* only, so on its own it says nothing
/// about the segment headers — a corrupted `load_addr` passes it untouched, and
/// the bootloader then refuses the image we just called bootable. The SHA
/// trailer covers every byte, headers included, which is what lets this
/// function stand in for the bootloader's own verdict. An image without one
/// cannot make that promise and is rejected, however intact it looks. Every
/// image this project builds appends one.
///
/// The remaining conditions checked here — chip id, segment layout — are the
/// ones a *deliberately* rewritten image could still satisfy the hash with.
/// This is not a complete reimplementation of `esp_image_format.c`, and it
/// cannot be: see [`plan_update_action`] for what an optimistic answer costs.
pub fn validate_flash_image<S: ImageSource>(
    src: &mut S,
    partition_len: usize,
) -> Result<(), ImageError> {
    walk_image(src, partition_len, None, Strictness::Resident)
}

/// The shared segment walk. `limit` bounds how far the walk may read;
/// `exact_len`, when given, is a known source length the measured image must
/// match exactly (a file), rather than merely fit within (a partition).
fn walk_image<S: ImageSource>(
    src: &mut S,
    limit: usize,
    exact_len: Option<usize>,
    strictness: Strictness,
) -> Result<(), ImageError> {
    let mut header = [0u8; HEADER_LEN];
    src.read_exact(&mut header).map_err(|_| ImageError::Read)?;
    if header[0] != IMAGE_MAGIC {
        return Err(ImageError::BadMagic);
    }
    let segment_count = header[1];
    // Byte 23 (`hash_appended`) flags a SHA-256 trailer over the whole image.
    let hash_appended = header[23] != 0;

    // Bytes 12..14 are `chip_id`. Checked for staged images too: one built for
    // another chip cannot run here whichever slot it is sitting in, and finding
    // that out before the write is strictly better than after.
    if u16::from_le_bytes([header[12], header[13]]) != EXPECTED_CHIP_ID {
        return Err(ImageError::WrongChip);
    }
    // The trailer is only demanded of a resident image, whose bytes are being
    // read as evidence about the bootloader. A staged one is about to be
    // written to the update slot with the bootloader still to pass judgement.
    if strictness == Strictness::Resident && !hash_appended {
        return Err(ImageError::NoHashTrailer);
    }

    let mut sha = Sha256::new();
    sha.update(header);
    // The XOR checksum is seeded with 0xEF and covers segment *data* only.
    let mut checksum = CHECKSUM_SEED;
    let mut pos = HEADER_LEN;

    let mut buf = [0u8; STREAM_CHUNK];
    for _ in 0..segment_count {
        // `pos` advances by header-controlled lengths, and `usize` is 32 bits on
        // the device: a corrupt `data_len` can carry these sums past the end of
        // the address space, where a release build wraps silently and would slip
        // past the bound. Overflow is itself proof the segments are nonsense.
        if pos
            .checked_add(SEG_HEADER_LEN)
            .is_none_or(|end| end > limit)
        {
            return Err(ImageError::BadSegments);
        }
        let mut seg_header = [0u8; SEG_HEADER_LEN];
        src.read_exact(&mut seg_header)
            .map_err(|_| ImageError::Read)?;
        sha.update(seg_header);
        pos += SEG_HEADER_LEN;

        let data_len =
            u32::from_le_bytes([seg_header[4], seg_header[5], seg_header[6], seg_header[7]])
                as usize;
        if pos.checked_add(data_len).is_none_or(|end| end > limit) {
            return Err(ImageError::BadSegments);
        }

        if strictness == Strictness::Resident {
            let load_addr =
                u32::from_le_bytes([seg_header[0], seg_header[1], seg_header[2], seg_header[3]]);
            // The loader moves whole words, so a ragged segment is not loadable.
            if !data_len.is_multiple_of(4) {
                return Err(ImageError::BadSegmentLayout);
            }
            // A flash-mapped segment is not copied: the MMU points a 64 KiB page
            // at it, so its address and its offset in the image must agree on
            // where inside that page the segment begins. A corrupted `load_addr`
            // — the one field the XOR checksum never covers — breaks this.
            if is_flash_mapped(load_addr)
                && (pos as u32) & (MMU_PAGE_SIZE - 1) != load_addr & (MMU_PAGE_SIZE - 1)
            {
                return Err(ImageError::BadSegmentLayout);
            }
        }

        let mut remaining = data_len;
        while remaining > 0 {
            let want = remaining.min(STREAM_CHUNK);
            let chunk = &mut buf[..want];
            src.read_exact(chunk).map_err(|_| ImageError::Read)?;
            sha.update(&chunk[..]);
            for &b in chunk.iter() {
                checksum ^= b;
            }
            remaining -= want;
        }
        pos += data_len;
    }

    // The image is padded up to the next 16-byte boundary; the stored checksum
    // byte sits at that boundary minus one. `pad_len` is always in 1..=16.
    let pad_end = (pos + 16) & !15usize;
    let expected_len = pad_end + if hash_appended { SHA_TRAILER_LEN } else { 0 };
    match exact_len {
        // A file: the image must account for every byte of it.
        Some(len) if expected_len != len => return Err(ImageError::BadSize),
        // A partition: the image must fit, and still be firmware-sized.
        None if expected_len > limit => return Err(ImageError::TooLarge),
        None if expected_len < MIN_IMAGE_LEN => return Err(ImageError::TooSmall),
        _ => {}
    }
    let pad_len = pad_end - pos;
    if pad_len == 0 || pad_len > 16 {
        return Err(ImageError::BadSize);
    }
    let mut pad = [0u8; 16];
    src.read_exact(&mut pad[..pad_len])
        .map_err(|_| ImageError::Read)?;
    sha.update(&pad[..pad_len]);

    let stored_checksum = pad[pad_len - 1];
    if checksum != stored_checksum {
        return Err(ImageError::BadChecksum);
    }

    if hash_appended {
        let mut trailer = [0u8; SHA_TRAILER_LEN];
        src.read_exact(&mut trailer).map_err(|_| ImageError::Read)?;
        let computed = sha.finalize();
        if computed.as_slice() != trailer {
            return Err(ImageError::BadSha);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// otadata / OTA slot selection
//
// The stock bootloader chooses the app partition from `otadata`: two 32-byte
// "select entries", one per flash sector. The entry with the highest *valid*
// `ota_seq` wins, and `(ota_seq - 1) % ota_partition_count` is the app slot to
// boot. An in-app update writes the freshly-flashed slot's entry into the
// *other* otadata sector with a higher seq, so the next boot selects it —
// without the ROM's `esp_image_verify` (which rejects our wide-eFuse image).
//
// This mirrors `esp-bootloader-esp-idf`'s `Ota` and the FreeInk SDK's
// `RecoveryBoot`/`OtaBootSwitch`. Keeping a host-testable copy here lets the
// seq/CRC/slot math be verified without hardware.
// ---------------------------------------------------------------------------

/// Length of one otadata select entry, and of each otadata flash sector's used
/// prefix. 32 bytes is also flash-encryption friendly.
pub const SELECT_ENTRY_LEN: usize = 32;

/// A never-written otadata seq (erased flash).
pub const UNINITIALIZED_SEQ: u32 = 0xFFFF_FFFF;

// esp_ota_img_states_t values we care about.
/// Freshly written, not yet marked valid. What we write on a new flash — this
/// is the state the FreeInk SDK / CrossPoint switch uses and it boots on the X4.
pub const OTA_IMG_NEW: u32 = 0x0;
/// Bootloader rollback-enabled state: the new app has booted once and must
/// mark itself valid before the next reset, or the bootloader may roll back.
pub const OTA_IMG_PENDING_VERIFY: u32 = 0x1;
pub const OTA_IMG_VALID: u32 = 0x2;
pub const OTA_IMG_INVALID: u32 = 0x3;
pub const OTA_IMG_ABORTED: u32 = 0x4;

// esp-bootloader-esp-idf's otadata CRC: reflected CRC-32, poly 0x04C11DB7,
// init 0, xorout 0xFFFFFFFF, over the little-endian `ota_seq` bytes. Identical
// to the ROM's `crc32_le(u32::MAX, ..)`. Verified: seq 1 -> 0x4743989A, which
// matches a real on-device otadata dump.
const OTADATA_CRC: Algorithm<u32> = Algorithm {
    width: 32,
    poly: 0x04c1_1db7,
    init: 0,
    refin: true,
    refout: true,
    xorout: 0xffff_ffff,
    check: 0,
    residue: 0,
};

/// CRC of a 4-byte little-endian `ota_seq`, as the bootloader stores and checks.
pub fn seq_crc(ota_seq: u32) -> u32 {
    Crc::<u32>::new(&OTADATA_CRC).checksum(&ota_seq.to_le_bytes())
}

/// The fields of an otadata select entry we act on. `seq_label` (20 bytes,
/// unused by the bootloader) is written as 0xFF and otherwise ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectEntry {
    pub ota_seq: u32,
    pub ota_state: u32,
    pub crc: u32,
}

impl SelectEntry {
    /// A fresh entry with a correct CRC.
    pub fn new(ota_seq: u32, ota_state: u32) -> Self {
        Self {
            ota_seq,
            ota_state,
            crc: seq_crc(ota_seq),
        }
    }

    pub fn from_bytes(b: &[u8; SELECT_ENTRY_LEN]) -> Self {
        Self {
            ota_seq: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            ota_state: u32::from_le_bytes([b[24], b[25], b[26], b[27]]),
            crc: u32::from_le_bytes([b[28], b[29], b[30], b[31]]),
        }
    }

    pub fn to_bytes(&self) -> [u8; SELECT_ENTRY_LEN] {
        let mut b = [0u8; SELECT_ENTRY_LEN];
        b[0..4].copy_from_slice(&self.ota_seq.to_le_bytes());
        b[4..24].copy_from_slice(&[0xFF; 20]); // seq_label, unused
        b[24..28].copy_from_slice(&self.ota_state.to_le_bytes());
        b[28..32].copy_from_slice(&self.crc.to_le_bytes());
        b
    }

    /// A bootable entry: initialised, CRC intact, and not marked bad — exactly
    /// the bootloader's own validity test.
    pub fn is_valid(&self) -> bool {
        self.ota_seq != UNINITIALIZED_SEQ
            && self.crc == seq_crc(self.ota_seq)
            && self.ota_state != OTA_IMG_INVALID
            && self.ota_state != OTA_IMG_ABORTED
    }
}

/// The app OTA slot the bootloader is currently selecting, derived from the two
/// otadata sectors. `None` means otadata is uninitialised (erased), in which
/// case the bootloader falls back to the first app partition — treat it as slot
/// 0. This is the slot `otadata` *requests*; the slot actually executing comes
/// from the running-slot lookup, and [`plan_update_action`] compares the two.
pub fn active_app_slot(
    sector0: &[u8; SELECT_ENTRY_LEN],
    sector1: &[u8; SELECT_ENTRY_LEN],
    ota_count: u32,
) -> Option<u32> {
    let e0 = SelectEntry::from_bytes(sector0);
    let e1 = SelectEntry::from_bytes(sector1);
    let s0 = e0.is_valid().then_some(e0.ota_seq);
    let s1 = e1.is_valid().then_some(e1.ota_seq);
    let active_seq = match (s0, s1) {
        (None, None) => return None,
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (Some(a), Some(b)) => a.max(b),
    };
    Some((active_seq - 1) % ota_count.max(1))
}

/// The active otadata entry and its sector, using the same "highest valid seq"
/// rule as the bootloader. `None` means both entries are erased or bad.
pub fn active_select_entry(
    sector0: &[u8; SELECT_ENTRY_LEN],
    sector1: &[u8; SELECT_ENTRY_LEN],
) -> Option<(usize, SelectEntry)> {
    let e0 = SelectEntry::from_bytes(sector0);
    let e1 = SelectEntry::from_bytes(sector1);
    let s0 = e0.is_valid().then_some(e0.ota_seq);
    let s1 = e1.is_valid().then_some(e1.ota_seq);
    match (s0, s1) {
        (None, None) => None,
        (Some(_), None) => Some((0, e0)),
        (None, Some(_)) => Some((1, e1)),
        (Some(a), Some(b)) if a >= b => Some((0, e0)),
        (Some(_), Some(_)) => Some((1, e1)),
    }
}

/// If the selected app is in a rollback trial state, return the otadata write
/// needed to acknowledge it as booted successfully.
pub fn plan_mark_app_valid(
    sector0: &[u8; SELECT_ENTRY_LEN],
    sector1: &[u8; SELECT_ENTRY_LEN],
) -> Option<OtaSwitch> {
    let (target_sector, active) = active_select_entry(sector0, sector1)?;
    if active.ota_state != OTA_IMG_NEW && active.ota_state != OTA_IMG_PENDING_VERIFY {
        return None;
    }
    Some(OtaSwitch {
        target_sector,
        entry: SelectEntry::new(active.ota_seq, OTA_IMG_VALID),
    })
}

/// The single otadata write that makes `dest_slot` the next boot target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OtaSwitch {
    /// Which otadata sector (0 or 1) to erase and overwrite.
    pub target_sector: usize,
    /// The 32-byte entry to write there.
    pub entry: SelectEntry,
}

/// Plan the otadata write that boots `dest_slot` (0-based app OTA index) next.
///
/// `sector0`/`sector1` are the two raw otadata entries as read from flash, and
/// `ota_count` is the number of OTA app partitions (2 for our layout). Mirrors
/// `OtaBootSwitch::switchTo`: find the active entry (highest valid seq), pick
/// the smallest higher seq that maps to `dest_slot`, and write it into the
/// *other* sector so the bootloader sees a newer, valid entry there.
pub fn plan_switch(
    sector0: &[u8; SELECT_ENTRY_LEN],
    sector1: &[u8; SELECT_ENTRY_LEN],
    dest_slot: u32,
    ota_count: u32,
) -> OtaSwitch {
    let e0 = SelectEntry::from_bytes(sector0);
    let e1 = SelectEntry::from_bytes(sector1);
    let s0 = e0.is_valid().then_some(e0.ota_seq);
    let s1 = e1.is_valid().then_some(e1.ota_seq);

    let (active_sector, active_seq) = match (s0, s1) {
        (None, None) => (None, 0),
        (Some(a), None) => (Some(0usize), a),
        (None, Some(b)) => (Some(1usize), b),
        (Some(a), Some(b)) if a >= b => (Some(0usize), a),
        (Some(_), Some(b)) => (Some(1usize), b),
    };

    let ota_count = ota_count.max(1);
    let mut new_seq = active_seq + 1;
    while (new_seq - 1) % ota_count != dest_slot % ota_count {
        new_seq += 1;
    }

    let target_sector = match active_sector {
        Some(0) => 1,
        Some(_) => 0,
        None => 0,
    };

    OtaSwitch {
        target_sector,
        entry: SelectEntry::new(new_seq, OTA_IMG_NEW),
    }
}

// ---------------------------------------------------------------------------
// Slot policy
//
// Slot 0 is a recovery anchor, not half of an A/B pair: in-app updates always
// land in `UPDATE_SLOT`, and nothing writes `ANCHOR_SLOT`, so the boot-time
// escape hatch always has a known firmware to fall back to. This is the FreeInk
// `RecoveryBoot` convention, where the recovery slot is deliberately never
// reflashed. The decisions live here, apart from the flash and SD I/O that
// carries them out, so the reboot-crossing behaviour is host-testable.
// ---------------------------------------------------------------------------

/// The recovery anchor: whatever firmware was first installed at `0x10000`.
/// Never written by an in-app update.
pub const ANCHOR_SLOT: u32 = 0;
/// Where every in-app update lands.
pub const UPDATE_SLOT: u32 = 1;

/// Offset of the app descriptor's `project_name` within an app image: a 24-byte
/// image header plus an 8-byte segment header puts `esp_app_desc_t` at `0x20`,
/// and `project_name` sits 48 bytes into it, past `magic_word`,
/// `secure_version`, `reserv1[2]`, and `version[32]`.
pub const APP_DESC_PROJECT_NAME_OFFSET: u32 = 0x20 + 48;
/// Length of the descriptor's fixed-width `project_name` field.
pub const APP_DESC_PROJECT_NAME_LEN: usize = 32;

/// The firmware identity each device build stamps into its app descriptor, and
/// compares an anchor against before bouncing an update into it.
///
/// Format: `CalendulaOS <board> u<updater-generation> (MarigoldOS)`. Both
/// builds are the same product at the same version, so the product name alone
/// cannot answer "could this anchor apply my update?" — the board decides which
/// trigger filename and which panel the image is for, and the generation digit
/// is bumped whenever the trigger filename or the update hand-off changes.
/// `fw` selects one of these by feature; they live here so the strings the
/// firmware stamps and the strings the tests check are the same constants.
pub const IDENTITY_X4: &str = "CalendulaOS X4 u1 (MarigoldOS)";
/// See [`IDENTITY_X4`].
pub const IDENTITY_X3: &str = "CalendulaOS X3 u1 (MarigoldOS)";

/// The NUL-terminated name inside a fixed-width descriptor `project_name`
/// field. An unterminated field is taken whole, matching how the bootloader
/// treats a name that exactly fills the space.
pub fn project_name(field: &[u8; APP_DESC_PROJECT_NAME_LEN]) -> &[u8] {
    let end = field
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(APP_DESC_PROJECT_NAME_LEN);
    &field[..end]
}

/// Whether the anchor slot's firmware could itself apply the update the running
/// firmware found — the precondition for [`UpdateAction::BounceToAnchor`].
///
/// `anchor_field` is the raw descriptor field read from the anchor slot, and
/// `running_identity` the running build's own. The test is exact equality, not
/// a product-name prefix: the identity carries the board and the updater
/// generation as well as the product, and a difference in *any* of them means
/// the anchor would not consume this trigger file. An anchor for the other
/// board would boot firmware for the wrong panel and never look for this
/// board's trigger filename; an anchor from an older updater generation may not
/// recognise the trigger at all. Both must refuse rather than bounce.
///
/// Deliberately stricter than the recovery hatch's own check
/// ([`plan_recovery_switch`], which only asks for a bootable image). The hatch
/// is an explicit user action whose whole purpose can be falling back to a
/// foreign firmware — CrossPoint or the stock app — parked in the anchor. A
/// bounce is automatic and unrequested, so it has to be sure the anchor will
/// finish the job.
/// The fixed parts of an identity: `CalendulaOS <board> u<gen> (MarigoldOS)`.
const IDENTITY_PREFIX: &[u8] = b"CalendulaOS ";
const IDENTITY_SUFFIX: &[u8] = b" (MarigoldOS)";

/// Split an identity into its board and updater generation, or `None` if it is
/// not one of ours at all.
pub fn parse_identity(name: &[u8]) -> Option<(&[u8], u32)> {
    let rest = name.strip_prefix(IDENTITY_PREFIX)?;
    let rest = rest.strip_suffix(IDENTITY_SUFFIX)?;
    let sep = rest.iter().rposition(|&b| b == b' ')?;
    let (board, generation) = rest.split_at(sep);
    let digits = generation.strip_prefix(b" u")?;
    if board.is_empty() || digits.is_empty() {
        return None;
    }
    let mut n: u32 = 0;
    for &d in digits {
        n = n
            .checked_mul(10)?
            .checked_add(d.checked_sub(b'0').filter(|v| *v < 10)? as u32)?;
    }
    Some((board, n))
}

/// Whether a *staged* image may be installed by this firmware.
///
/// Structural validity is not enough. The descriptor identity is what encodes
/// the panel and the updater hand-off, so an image that merely parses as an
/// ESP32-C3 binary can still be the wrong thing entirely: a build for the other
/// board drives the wrong panel, a pre-anchor build alternates slots and
/// overwrites slot 0 on its next update, and an image with a different or
/// non-canonical identity cannot be serviced by our immutable slot-0 anchor on
/// subsequent updates.
///
/// For the current protocol, an in-app update must match the running firmware's
/// descriptor identity exactly (`project_name(candidate) == running_identity`),
/// ensuring the permanent slot-0 anchor remains capable of servicing future
/// updates. Moving to a new updater generation requires replacing or
/// re-establishing the slot-0 anchor via the computer/OEM installation path.
pub fn staged_image_is_installable(
    candidate: &[u8; APP_DESC_PROJECT_NAME_LEN],
    running_identity: &[u8],
) -> bool {
    project_name(candidate) == running_identity
}

pub fn anchor_can_apply_update(
    anchor_field: &[u8; APP_DESC_PROJECT_NAME_LEN],
    running_identity: &[u8],
) -> bool {
    project_name(anchor_field) == running_identity
}

/// What a boot that found a pending, already-validated update image should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateAction {
    /// Write the image into [`UPDATE_SLOT`] and select it.
    WriteUpdateSlot,
    /// We are running from [`UPDATE_SLOT`], so the image cannot be written
    /// without erasing the running firmware. Select [`ANCHOR_SLOT`] and reset,
    /// leaving the trigger for the anchor boot to consume.
    BounceToAnchor,
    /// Running from [`UPDATE_SLOT`], and no bounce is left to try: either the
    /// anchor could not apply the update — bouncing would move the user off
    /// their firmware and strand it — or `otadata` already selects the anchor,
    /// meaning the bootloader refused a bounce we already made.
    NoUsableAnchor,
    /// Which slot is executing was not established: either the running-slot
    /// lookup did not resolve, or it reports the anchor while the anchor fails
    /// validation, which cannot both be true. The slot a write would erase may
    /// be the one currently executing, so nothing is written.
    RunningSlotUnknown,
}

impl UpdateAction {
    /// Whether this action consumes the one-shot trigger file.
    ///
    /// Only the bounce keeps it: the anchor boot it hands off to is the one
    /// that applies the image. Everything else — a completed write, or a
    /// refusal — must clear the trigger so it cannot re-run or wedge a boot.
    pub fn consumes_trigger(self) -> bool {
        !matches!(self, Self::BounceToAnchor)
    }

    /// The slot `otadata` should select afterwards, if any.
    pub fn selects_slot(self) -> Option<u32> {
        match self {
            Self::WriteUpdateSlot => Some(UPDATE_SLOT),
            Self::BounceToAnchor => Some(ANCHOR_SLOT),
            Self::NoUsableAnchor | Self::RunningSlotUnknown => None,
        }
    }
}

/// Decide what to do with a pending update.
///
/// `running_slot` is the slot the hardware says is executing — `None` when that
/// cannot be resolved — `requested_slot` the slot **`otadata` selects**, and
/// `anchor_usable` whether [`ANCHOR_SLOT`] holds a complete, valid image of our
/// own firmware identity, one that would both boot and itself apply the trigger
/// file.
///
/// # `otadata` is a request, not a report
///
/// The bootloader loads the slot `otadata` selects *only if that image
/// verifies*; when it does not, ESP-IDF falls forward to another app partition
/// and boots it **without** rewriting `otadata`. So a firmware that read
/// `otadata` to learn where it was running could conclude it was on the anchor,
/// judge the update slot idle, and erase the very image it was executing —
/// destroying the last bootable copy on the device. That is why `running_slot`
/// comes from the flash MMU instead, and why `None` is refused rather than
/// filled in from `requested_slot`: the case that makes `otadata` wrong is
/// exactly the case a write would be fatal in.
///
/// # What `anchor_usable` decides, and what it cannot
///
/// With the running slot established, the anchor's validity is no longer
/// evidence about *where we are*; it answers whether a bounce could finish the
/// job. It is checked on every path anyway, because both remaining decisions
/// need it: whether to hand off, and whether handing off would strand the
/// update.
///
/// It is an optimistic answer. "Would the bootloader boot this?" is decided by
/// [`validate_flash_image`], which is not `esp_image_format.c` and does not
/// reproduce every condition it applies, so an anchor can satisfy every check
/// here — including the SHA-256 over every byte — and still be refused. That is
/// handled after the fact rather than predicted: `otadata` selecting the anchor
/// while the update slot is executing is a bounce the bootloader already
/// refused, and is answered with [`NoUsableAnchor`] instead of another attempt.
///
/// [`NoUsableAnchor`]: UpdateAction::NoUsableAnchor
pub fn plan_update_action(
    running_slot: Option<u32>,
    requested_slot: u32,
    anchor_usable: bool,
) -> UpdateAction {
    // Fail closed, as [`may_mark_running_slot_valid`] does. Without proof of
    // which slot is executing there is no safe write: `otadata` alone cannot
    // supply it, since the case that makes it wrong — the bootloader falling
    // forward — is exactly the case a write would erase the running firmware
    // in. Inferring from the anchor's validity is not enough either, because
    // an anchor can pass our checks and still be refused by the bootloader.
    let Some(running_slot) = running_slot else {
        return UpdateAction::RunningSlotUnknown;
    };
    // A bounce that did not take. We pointed `otadata` at the anchor and reset;
    // the bootloader handed back the update slot, so the anchor satisfied our
    // checks and failed its own. `validate_flash_image` is not
    // `esp_image_format.c` and never will be, so this is reachable — and
    // bouncing again would only repeat the reset, forever. Refuse instead, and
    // let the trigger go: the way out is a computer or the OEM updater writing
    // slot 0, exactly as for any other unusable anchor.
    if requested_slot == ANCHOR_SLOT && running_slot == UPDATE_SLOT {
        return UpdateAction::NoUsableAnchor;
    }
    if running_slot == UPDATE_SLOT {
        return if anchor_usable {
            UpdateAction::BounceToAnchor
        } else {
            UpdateAction::NoUsableAnchor
        };
    }
    if anchor_usable {
        UpdateAction::WriteUpdateSlot
    } else {
        UpdateAction::RunningSlotUnknown
    }
}

/// Whether a mark-valid may proceed: only when the running slot is known *and*
/// is the one `otadata` selects.
///
/// Fails closed. `plan_mark_app_valid` blesses the entry `otadata` names, so
/// running it without proof we are executing that slot risks confirming an
/// image the bootloader just rejected — the precise case the running-slot
/// lookup exists to catch, and precisely when that lookup returning `None`
/// would otherwise wave it through.
pub fn may_mark_running_slot_valid(running: Option<u32>, requested: Option<u32>) -> bool {
    matches!((running, requested), (Some(r), Some(q)) if r == q)
}

// ---------------------------------------------------------------------------
// Which slot is *executing*
//
// `otadata` records the slot the bootloader was asked to boot. To learn the one
// it actually booted, translate a mapped code address back to a flash offset
// through the ESP32-C3's flash MMU — what ESP-IDF's `spi_flash_cache2phys()`
// does, and what FreeInk's RecoveryBoot gets from `esp_ota_get_running_
// partition()`. esp-hal exposes no equivalent, so the arithmetic lives here
// (host-testable) and `fw::mmu` supplies the one volatile register read.
//
// Every constant below was confirmed on an X3: see the tests, which assert the
// exact vaddr/entry pairs observed there against the offsets the ESP-IDF
// bootloader itself reported loading from.

/// Bytes of flash one MMU entry maps.
pub const MMU_PAGE_SIZE: u32 = 0x1_0000;
/// Entries in the C3's table. IBUS and DBUS share it, so the linker keeps their
/// virtual addresses from colliding within the 8 MiB window it covers.
pub const MMU_ENTRY_COUNT: u32 = 128;
/// Offset within that window; `MMU_ENTRY_COUNT * MMU_PAGE_SIZE - 1`.
const MMU_VADDR_MASK: u32 = 0x7F_FFFF;
/// Set on an entry that maps nothing. Observed as exactly `0x100` on hardware.
const MMU_INVALID_BIT: u32 = 0x100;
/// Selects the physical page number from a valid entry.
const MMU_PAGE_MASK: u32 = 0xFF;

/// The MMU table index that maps `vaddr`.
pub fn mmu_index(vaddr: u32) -> u32 {
    (vaddr & MMU_VADDR_MASK) / MMU_PAGE_SIZE
}

/// Resolve `vaddr` to a flash offset given the MMU table `entry` that maps it.
/// `None` when the entry maps nothing.
pub fn mmu_flash_offset(vaddr: u32, entry: u32) -> Option<u32> {
    if entry & MMU_INVALID_BIT != 0 {
        return None;
    }
    Some((entry & MMU_PAGE_MASK) * MMU_PAGE_SIZE + (vaddr % MMU_PAGE_SIZE))
}

/// The app slot holding `offset`, if it falls in one.
pub fn slot_containing(layout: &OtaLayout, offset: u32) -> Option<u32> {
    layout
        .slots
        .iter()
        .position(|p| offset >= p.offset && offset - p.offset < p.size)
        .map(|i| i as u32)
}

/// Whether a held recovery combo should repoint `otadata` at the anchor.
///
/// `active_slot` is [`active_app_slot`]'s reading — `None` (erased otadata)
/// means the bootloader already defaults to the anchor, so there is nothing to
/// undo. `anchor_bootable` only asks for a plausible image: the point is to
/// leave a misbehaving slot, not to hand off work, so any firmware will do.
pub fn plan_recovery_switch(active_slot: Option<u32>, anchor_bootable: bool) -> bool {
    active_slot == Some(UPDATE_SLOT) && anchor_bootable
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// Cursor over an owned byte buffer implementing `ImageSource`.
    struct Cursor {
        bytes: Vec<u8>,
        pos: usize,
    }
    impl ImageSource for Cursor {
        fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ()> {
            let end = self.pos + buf.len();
            if end > self.bytes.len() {
                return Err(());
            }
            buf.copy_from_slice(&self.bytes[self.pos..end]);
            self.pos = end;
            Ok(())
        }
    }

    /// Build a structurally valid ESP-IDF image with the given segment data
    /// lengths, correct XOR checksum, and (optionally) a SHA-256 trailer — the
    /// same construction the bootloader validates against.
    /// Segment load addresses mirroring a real build: the first segment is
    /// flash-mapped into DROM at the offset its data actually sits at (the
    /// congruence the bootloader requires), and the rest are the zero-address
    /// padding segments esptool emits, which are copied rather than mapped.
    fn segment_load_addr(index: usize, data_offset: usize) -> u32 {
        if index == 0 {
            0x3C00_0000 + data_offset as u32
        } else {
            0
        }
    }

    fn build_image(segment_lens: &[usize], hash_appended: bool) -> Vec<u8> {
        let mut img = Vec::new();
        let mut header = [0u8; HEADER_LEN];
        header[0] = IMAGE_MAGIC;
        header[1] = segment_lens.len() as u8;
        header[12..14].copy_from_slice(&EXPECTED_CHIP_ID.to_le_bytes());
        header[23] = if hash_appended { 1 } else { 0 };
        img.extend_from_slice(&header);

        let mut checksum = CHECKSUM_SEED;
        for (i, &len) in segment_lens.iter().enumerate() {
            let mut seg_header = [0u8; SEG_HEADER_LEN];
            let load_addr = segment_load_addr(i, img.len() + SEG_HEADER_LEN);
            seg_header[0..4].copy_from_slice(&load_addr.to_le_bytes());
            seg_header[4..8].copy_from_slice(&(len as u32).to_le_bytes());
            img.extend_from_slice(&seg_header);
            for j in 0..len {
                let b = (i as u8).wrapping_mul(31).wrapping_add(j as u8);
                checksum ^= b;
                img.push(b);
            }
        }

        // Pad to the next 16-byte boundary; the last pad byte is the checksum.
        let pad_end = (img.len() + 16) & !15usize;
        while img.len() < pad_end - 1 {
            img.push(0);
        }
        img.push(checksum);

        if hash_appended {
            let mut sha = Sha256::new();
            sha.update(&img);
            img.extend_from_slice(&sha.finalize());
        }
        img
    }

    fn cursor(bytes: Vec<u8>) -> Cursor {
        Cursor { bytes, pos: 0 }
    }

    // A minimum-size image needs >= 64 KiB; use one fat segment plus a few small
    // ones so the segment walk and the streaming chunk boundary both get hit.
    fn valid_image(hash_appended: bool) -> Vec<u8> {
        build_image(&[70_000, 8, 513, 1], hash_appended)
    }

    fn partition_entry(kind: u8, subtype: u8, offset: u32, size: u32) -> [u8; 32] {
        let mut entry = [0u8; 32];
        entry[0..2].copy_from_slice(&PARTITION_MAGIC.to_le_bytes());
        entry[2] = kind;
        entry[3] = subtype;
        entry[4..8].copy_from_slice(&offset.to_le_bytes());
        entry[8..12].copy_from_slice(&size.to_le_bytes());
        entry
    }

    fn partition_table(slot1_offset: u32, slot_size: u32) -> Vec<u8> {
        let mut table = Vec::new();
        table.extend_from_slice(&partition_entry(
            PARTITION_TYPE_DATA,
            PARTITION_SUBTYPE_DATA_OTA,
            0xE000,
            0x2000,
        ));
        table.extend_from_slice(&partition_entry(
            PARTITION_TYPE_APP,
            PARTITION_SUBTYPE_APP_OTA_0,
            0x10000,
            slot_size,
        ));
        table.extend_from_slice(&partition_entry(
            PARTITION_TYPE_APP,
            PARTITION_SUBTYPE_APP_OTA_0 + 1,
            slot1_offset,
            slot_size,
        ));
        table.extend_from_slice(&[0xFF; 32]);
        table
    }

    #[test]
    fn discovers_crosspoint_ota_layout() {
        let table = partition_table(0x650000, 0x640000);
        let layout = parse_ota_layout(&table, 0x1000000).unwrap();
        assert_eq!(layout.otadata.offset, 0xE000);
        assert_eq!(layout.slots[0].offset, 0x10000);
        assert_eq!(layout.slots[1].offset, 0x650000);
        assert_eq!(layout.slots[1].size, 0x640000);
    }

    #[test]
    fn discovers_stock_x3_ota_layout() {
        let table = partition_table(0x780000, 0x770000);
        let layout = parse_ota_layout(&table, 0x1000000).unwrap();
        assert_eq!(layout.slots[0].offset, 0x10000);
        assert_eq!(layout.slots[0].size, 0x770000);
        assert_eq!(layout.slots[1].offset, 0x780000);
        assert_eq!(layout.slots[1].size, 0x770000);
    }

    #[test]
    fn stock_x3_update_targets_the_slot_the_bootloader_will_select() {
        let table = partition_table(0x780000, 0x770000);
        let layout = parse_ota_layout(&table, 0x1000000).unwrap();
        let active_slot0 = SelectEntry::new(1, OTA_IMG_VALID).to_bytes();
        let erased = [0xFF; SELECT_ENTRY_LEN];
        let active = active_app_slot(&active_slot0, &erased, 2).unwrap();
        let destination = (active + 1) % 2;
        let switch = plan_switch(&active_slot0, &erased, destination, 2);

        assert_eq!(destination, 1);
        assert_eq!(layout.slots[destination as usize].offset, 0x780000);
        assert_eq!((switch.entry.ota_seq - 1) % 2, destination);
    }

    #[test]
    fn rejects_tables_with_more_than_two_ota_slots() {
        let mut table = partition_table(0x650000, 0x640000);
        let terminator = table.split_off(table.len() - 32);
        table.extend_from_slice(&partition_entry(
            PARTITION_TYPE_APP,
            PARTITION_SUBTYPE_APP_OTA_0 + 2,
            0xC90000,
            0x100000,
        ));
        table.extend_from_slice(&terminator);
        assert_eq!(
            parse_ota_layout(&table, 0x1000000),
            Err(PartitionTableError::UnsupportedOtaSlot(2))
        );
    }

    #[test]
    fn rejects_incomplete_or_out_of_flash_ota_layouts() {
        let mut missing_slot = partition_table(0x780000, 0x770000);
        missing_slot.drain(64..96);
        assert_eq!(
            parse_ota_layout(&missing_slot, 0x1000000),
            Err(PartitionTableError::MissingOtaSlot(1))
        );

        let outside_flash = partition_table(0xF00000, 0x200000);
        assert_eq!(
            parse_ota_layout(&outside_flash, 0x1000000),
            Err(PartitionTableError::InvalidBounds)
        );
    }

    #[test]
    fn accepts_hash_appended_image() {
        let img = valid_image(true);
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, Some(0x640000)),
            Ok(())
        );
    }

    #[test]
    fn accepts_image_without_hash_trailer() {
        let img = valid_image(false);
        let len = img.len();
        assert_eq!(validate_image(&mut cursor(img), len, None), Ok(()));
    }

    // --- Images resident in a flash partition -------------------------------
    //
    // A partition has no length: past the image lies whatever was there before.
    // `validate_flash_image` measures the image from its own structure, so it
    // has to accept the trailing junk a file-based check would call BadSize.

    /// Slot contents: the image, then whatever the partition already held.
    fn in_partition(img: Vec<u8>, partition_len: usize) -> Cursor {
        let mut bytes = img;
        bytes.resize(partition_len, 0xA5);
        cursor(bytes)
    }

    const SLOT_LEN: usize = 0x64_0000;

    /// A slot-resident image, shaped like one the build actually produces:
    /// every segment a whole number of words, and the mapped segment congruent
    /// with its offset. `valid_image` deliberately is not — its ragged 513- and
    /// 1-byte segments exist to poke the streaming chunk boundary, and the
    /// bootloader would not load them.
    fn resident_image(hash_appended: bool) -> Vec<u8> {
        build_image(&[70_000, 8, 512, 4], hash_appended)
    }

    #[test]
    fn accepts_a_flash_image_followed_by_partition_junk() {
        let img = resident_image(true);
        assert_eq!(
            validate_flash_image(&mut in_partition(img, SLOT_LEN), SLOT_LEN),
            Ok(())
        );
    }

    /// The case that motivates checking the anchor at all: a write to slot 0
    /// interrupted partway leaves the header and descriptor intact and the tail
    /// wrong, which a magic-and-identity check waves through.
    #[test]
    fn rejects_a_flash_image_whose_body_is_corrupt() {
        let mut img = resident_image(true);
        let len = img.len();
        img[len / 2] ^= 0xFF;
        assert_eq!(
            validate_flash_image(&mut in_partition(img, SLOT_LEN), SLOT_LEN),
            Err(ImageError::BadChecksum)
        );
    }

    /// Corruption the XOR checksum cannot see — two body bytes flipped by the
    /// same mask cancel out in the XOR — still fails on the SHA-256 trailer.
    #[test]
    fn rejects_flash_corruption_the_checksum_cannot_see() {
        let mut img = resident_image(true);
        // Both offsets are segment data, which is what the checksum covers.
        img[HEADER_LEN + SEG_HEADER_LEN] ^= 0xFF;
        img[HEADER_LEN + SEG_HEADER_LEN + 1] ^= 0xFF;
        assert_eq!(
            validate_flash_image(&mut in_partition(img, SLOT_LEN), SLOT_LEN),
            Err(ImageError::BadSha)
        );
    }

    #[test]
    fn rejects_a_flash_image_with_no_magic() {
        let mut img = resident_image(true);
        img[0] = 0x00;
        assert_eq!(
            validate_flash_image(&mut in_partition(img, SLOT_LEN), SLOT_LEN),
            Err(ImageError::BadMagic)
        );
    }

    /// An erased slot reads as all-ones, not as a tiny valid image.
    #[test]
    fn rejects_an_erased_slot() {
        let mut src = cursor(std::vec![0xFF; SLOT_LEN]);
        assert_eq!(
            validate_flash_image(&mut src, SLOT_LEN),
            Err(ImageError::BadMagic)
        );
    }

    /// The segment walk must stay inside the partition even when the segment
    /// headers claim otherwise — the bound is the partition, not a file length.
    #[test]
    fn rejects_a_flash_image_claiming_more_than_the_partition_holds() {
        let img = resident_image(true);
        assert_eq!(
            validate_flash_image(&mut in_partition(img, SLOT_LEN), 0x1_0000),
            Err(ImageError::BadSegments)
        );
    }

    /// Overwrite segment 0's `load_addr` in place, leaving the segment data —
    /// and so the XOR checksum — untouched. This is the field the checksum
    /// never covers.
    fn set_first_load_addr(img: &mut [u8], load_addr: u32) {
        img[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&load_addr.to_le_bytes());
    }

    fn reseal_sha(img: &mut [u8]) {
        let body = img.len() - SHA_TRAILER_LEN;
        let mut sha = Sha256::new();
        sha.update(&img[..body]);
        let digest = sha.finalize();
        img[body..].copy_from_slice(&digest);
    }

    /// The finding's case: same identity, intact segment data, a correct XOR
    /// checksum, no SHA trailer — and a `load_addr` the bootloader would refuse
    /// to map. Nothing in the image covers that field, so believing this anchor
    /// is exactly how the updater comes to erase the slot it is running from.
    #[test]
    fn rejects_an_unhashed_flash_image_with_a_corrupt_load_address() {
        let mut img = resident_image(false);
        set_first_load_addr(&mut img, 0x3C00_1234);

        // The corruption is genuinely invisible to the file-level checks: as a
        // staged image, with its exact length known, this still validates.
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img.clone()), len, None),
            Ok(()),
            "the XOR checksum cannot see a segment header"
        );

        assert_eq!(
            validate_flash_image(&mut in_partition(img, SLOT_LEN), SLOT_LEN),
            Err(ImageError::NoHashTrailer)
        );
    }

    /// And with the hash resealed over the bad address — the case a trailer
    /// alone would wave through — the layout check is what refuses it.
    #[test]
    fn rejects_a_flash_image_whose_mapped_segment_cannot_be_mapped() {
        let mut img = resident_image(true);
        set_first_load_addr(&mut img, 0x3C00_1234);
        reseal_sha(&mut img);
        assert_eq!(
            validate_flash_image(&mut in_partition(img, SLOT_LEN), SLOT_LEN),
            Err(ImageError::BadSegmentLayout)
        );
    }

    /// A segment that is not flash-mapped is copied to RAM, so the congruence
    /// rule does not apply — real images carry zero-address padding segments
    /// that would fail it. Rejecting those would refuse every genuine anchor.
    #[test]
    fn accepts_a_flash_image_with_unmapped_padding_segments() {
        let img = resident_image(true);
        assert!(img.len() > MIN_IMAGE_LEN);
        assert_eq!(
            validate_flash_image(&mut in_partition(img, SLOT_LEN), SLOT_LEN),
            Ok(())
        );
    }

    #[test]
    fn rejects_a_flash_image_without_a_hash_trailer() {
        let img = resident_image(false);
        assert_eq!(
            validate_flash_image(&mut in_partition(img, SLOT_LEN), SLOT_LEN),
            Err(ImageError::NoHashTrailer)
        );
    }

    #[test]
    fn rejects_a_flash_image_built_for_another_chip() {
        let mut img = resident_image(true);
        img[12..14].copy_from_slice(&(EXPECTED_CHIP_ID + 1).to_le_bytes());
        reseal_sha(&mut img);
        assert_eq!(
            validate_flash_image(&mut in_partition(img, SLOT_LEN), SLOT_LEN),
            Err(ImageError::WrongChip)
        );
    }

    #[test]
    fn rejects_a_flash_image_with_a_ragged_segment() {
        // 513 bytes of data in the second segment is not a whole number of
        // words, so the loader could not move it.
        let img = build_image(&[70_000, 513], true);
        assert_eq!(
            validate_flash_image(&mut in_partition(img, SLOT_LEN), SLOT_LEN),
            Err(ImageError::BadSegmentLayout)
        );
    }

    /// A `data_len` near `u32::MAX` makes `pos + data_len` wrap on the device's
    /// 32-bit `usize`, where a release build does not trap. Unchecked, the sum
    /// lands back below `limit` and the walk sails past its own bound.
    #[test]
    fn rejects_a_segment_length_that_would_overflow_the_walk() {
        let mut img = resident_image(true);
        // Segment 0's length field, immediately after the image header.
        img[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            validate_flash_image(&mut in_partition(img, SLOT_LEN), SLOT_LEN),
            Err(ImageError::BadSegments)
        );
    }

    #[test]
    fn rejects_a_flash_image_too_small_to_be_firmware() {
        let img = build_image(&[64], true);
        assert_eq!(
            validate_flash_image(&mut in_partition(img, SLOT_LEN), SLOT_LEN),
            Err(ImageError::TooSmall)
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut img = valid_image(true);
        img[0] = 0x00;
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::BadMagic)
        );
    }

    #[test]
    fn rejects_too_small() {
        let img = build_image(&[16], true);
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::TooSmall)
        );
    }

    #[test]
    fn rejects_image_larger_than_partition() {
        let img = valid_image(true);
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, Some(len - 1)),
            Err(ImageError::TooLarge)
        );
    }

    #[test]
    fn rejects_corrupt_body_via_checksum() {
        let mut img = valid_image(false);
        // Flip a byte inside the first segment's data (just past the 24-byte
        // header + 8-byte segment header). Without a SHA trailer the XOR
        // checksum is the gate that catches it.
        img[HEADER_LEN + SEG_HEADER_LEN + 3] ^= 0xFF;
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::BadChecksum)
        );
    }

    #[test]
    fn rejects_corrupt_body_via_sha_when_checksum_still_matches() {
        // Flip two bytes whose XOR cancels in the checksum but not in SHA-256,
        // proving the SHA trailer catches damage the byte-XOR misses.
        let mut img = valid_image(true);
        let a = HEADER_LEN + SEG_HEADER_LEN + 1;
        let b = HEADER_LEN + SEG_HEADER_LEN + 2;
        img[a] ^= 0x5A;
        img[b] ^= 0x5A;
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::BadSha)
        );
    }

    #[test]
    fn rejects_length_mismatch() {
        // A trailing byte the segment table + padding don't account for: the
        // structural length no longer equals the declared length.
        let mut img = valid_image(false);
        img.push(0);
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::BadSize)
        );
    }

    #[test]
    fn rejects_short_source() {
        // The declared length is structurally consistent, but the source runs
        // out before delivering it (e.g. a half-written SD file).
        let mut img = valid_image(true);
        let len = img.len();
        img.truncate(len - 10);
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::Read)
        );
    }

    #[test]
    fn rejects_segment_running_past_eof() {
        let mut img = valid_image(false);
        // Inflate the first segment's declared data length so it overruns EOF.
        let huge = 0x00FF_FFFFu32.to_le_bytes();
        img[HEADER_LEN + 4..HEADER_LEN + 8].copy_from_slice(&huge);
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::BadSegments)
        );
    }

    // --- otadata -----------------------------------------------------------

    #[test]
    fn seq_crc_matches_rom_and_device() {
        // seq 1 -> 0x4743989A is a real on-device otadata CRC and the value the
        // authoritative esp-bootloader-esp-idf algorithm produces. The others
        // are independently computed from the same CRC parameters.
        assert_eq!(seq_crc(1), 0x4743_989A);
        assert_eq!(seq_crc(2), 0x55F6_3774);
        assert_eq!(seq_crc(3), 0xED4A_5011);
    }

    #[test]
    fn select_entry_round_trips_with_valid_crc() {
        let e = SelectEntry::new(5, OTA_IMG_NEW);
        let bytes = e.to_bytes();
        let back = SelectEntry::from_bytes(&bytes);
        assert_eq!(back, e);
        assert!(back.is_valid());
        // seq_label region is 0xFF, CRC sits in the last 4 bytes.
        assert_eq!(&bytes[4..24], &[0xFF; 20]);
        assert_eq!(
            u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]),
            seq_crc(5)
        );
    }

    #[test]
    fn uninitialised_and_corrupt_entries_are_invalid() {
        let erased = [0xFFu8; SELECT_ENTRY_LEN];
        assert!(!SelectEntry::from_bytes(&erased).is_valid());

        let mut bad = SelectEntry::new(7, OTA_IMG_NEW).to_bytes();
        bad[28] ^= 0xFF; // corrupt the stored CRC
        assert!(!SelectEntry::from_bytes(&bad).is_valid());

        let aborted = SelectEntry::new(7, OTA_IMG_ABORTED);
        assert!(!aborted.is_valid());
    }

    #[test]
    fn switch_from_erased_otadata_targets_requested_slot() {
        let erased = [0xFFu8; SELECT_ENTRY_LEN];
        // First boot from erased otadata into slot 0.
        let sw0 = plan_switch(&erased, &erased, 0, 2);
        assert_eq!(sw0.target_sector, 0);
        assert_eq!(sw0.entry.ota_seq, 1); // (1-1)%2 == 0
        assert!(sw0.entry.is_valid());

        // ...or into slot 1.
        let sw1 = plan_switch(&erased, &erased, 1, 2);
        assert_eq!(sw1.entry.ota_seq, 2); // (2-1)%2 == 1
        assert_eq!((sw1.entry.ota_seq - 1) % 2, 1);
    }

    #[test]
    fn switch_writes_other_sector_with_higher_seq() {
        // sector0 active at seq=3 (slot (3-1)%2 == 0). Switch to slot 1.
        let active = SelectEntry::new(3, OTA_IMG_NEW).to_bytes();
        let erased = [0xFFu8; SELECT_ENTRY_LEN];
        let sw = plan_switch(&active, &erased, 1, 2);

        assert_eq!(sw.target_sector, 1, "must write the inactive sector");
        assert!(sw.entry.ota_seq > 3, "new seq must exceed the active seq");
        assert_eq!((sw.entry.ota_seq - 1) % 2, 1, "must map to slot 1");
        assert!(sw.entry.is_valid());
    }

    #[test]
    fn active_slot_tracks_highest_valid_seq() {
        let erased = [0xFFu8; SELECT_ENTRY_LEN];
        assert_eq!(active_app_slot(&erased, &erased, 2), None);

        // seq 3 -> slot (3-1)%2 == 0
        let s3 = SelectEntry::new(3, OTA_IMG_NEW).to_bytes();
        assert_eq!(active_app_slot(&s3, &erased, 2), Some(0));

        // higher seq 4 in the other sector -> slot (4-1)%2 == 1 wins
        let s4 = SelectEntry::new(4, OTA_IMG_NEW).to_bytes();
        assert_eq!(active_app_slot(&s3, &s4, 2), Some(1));

        // an aborted higher seq is ignored -> falls back to seq 3
        let s9_aborted = SelectEntry::new(9, OTA_IMG_ABORTED).to_bytes();
        assert_eq!(active_app_slot(&s3, &s9_aborted, 2), Some(0));
    }

    #[test]
    fn mark_app_valid_only_rewrites_trial_state() {
        let old = SelectEntry::new(3, OTA_IMG_VALID).to_bytes();
        let new = SelectEntry::new(4, OTA_IMG_PENDING_VERIFY).to_bytes();
        let sw = plan_mark_app_valid(&old, &new).expect("pending app should be acknowledged");
        assert_eq!(sw.target_sector, 1);
        assert_eq!(sw.entry.ota_seq, 4);
        assert_eq!(sw.entry.ota_state, OTA_IMG_VALID);
        assert_eq!(sw.entry.crc, seq_crc(4));

        let valid = sw.entry.to_bytes();
        assert_eq!(plan_mark_app_valid(&old, &valid), None);
    }

    #[test]
    fn mark_app_valid_accepts_new_state() {
        let erased = [0xFFu8; SELECT_ENTRY_LEN];
        let new = SelectEntry::new(2, OTA_IMG_NEW).to_bytes();
        let sw = plan_mark_app_valid(&erased, &new).expect("new app should be acknowledged");
        assert_eq!(sw.target_sector, 1);
        assert_eq!(sw.entry.ota_seq, 2);
        assert_eq!(sw.entry.ota_state, OTA_IMG_VALID);
    }

    #[test]
    fn switch_ignores_invalidated_higher_seq() {
        // sector1 has a higher seq but is ABORTED, so sector0 (seq 3) is active.
        let active = SelectEntry::new(3, OTA_IMG_NEW).to_bytes();
        let aborted = SelectEntry::new(9, OTA_IMG_ABORTED).to_bytes();
        let sw = plan_switch(&active, &aborted, 0, 2);
        // Active is sector0, so we write sector1, seq just above 3 mapping slot 0.
        assert_eq!(sw.target_sector, 1);
        assert_eq!(sw.entry.ota_seq, 5); // 4->(3)%2=1 no; 5->(4)%2=0 yes
        assert_eq!((sw.entry.ota_seq - 1) % 2, 0);
    }

    // --- Slot policy -------------------------------------------------------

    #[test]
    fn project_name_stops_at_the_terminator() {
        let mut field = [0u8; APP_DESC_PROJECT_NAME_LEN];
        field[..5].copy_from_slice(b"hello");
        assert_eq!(project_name(&field), b"hello");
    }

    #[test]
    fn project_name_takes_an_unterminated_field_whole() {
        let field = [b'x'; APP_DESC_PROJECT_NAME_LEN];
        assert_eq!(project_name(&field).len(), APP_DESC_PROJECT_NAME_LEN);
    }

    #[test]
    fn project_name_of_an_erased_field_is_empty() {
        assert_eq!(project_name(&[0u8; APP_DESC_PROJECT_NAME_LEN]), b"");
    }

    /// A descriptor field as it sits in flash: the identity, zero-padded.
    fn descriptor_field(identity: &[u8]) -> [u8; APP_DESC_PROJECT_NAME_LEN] {
        let mut field = [0u8; APP_DESC_PROJECT_NAME_LEN];
        field[..identity.len()].copy_from_slice(identity);
        field
    }

    // The identities the two device builds stamp — the same constants `fw`
    // puts in the descriptor, so these tests cannot drift from the firmware.
    const X4: &[u8] = IDENTITY_X4.as_bytes();
    const X3: &[u8] = IDENTITY_X3.as_bytes();

    #[test]
    fn an_anchor_of_the_same_identity_can_apply_the_update() {
        assert!(anchor_can_apply_update(&descriptor_field(X4), X4));
        assert!(anchor_can_apply_update(&descriptor_field(X3), X3));
    }

    /// The gap this identity closes: both boards ship the same product under
    /// the same version, but take different trigger filenames and drive
    /// different panels, so neither may bounce into the other.
    #[test]
    fn an_anchor_for_the_other_board_cannot_apply_the_update() {
        assert!(!anchor_can_apply_update(&descriptor_field(X3), X4));
        assert!(!anchor_can_apply_update(&descriptor_field(X4), X3));
    }

    /// Same product, same board, older updater: it may not know this trigger.
    #[test]
    fn an_anchor_of_an_older_updater_generation_cannot_apply_the_update() {
        let older = b"CalendulaOS X4 u0 (MarigoldOS)";
        assert!(!anchor_can_apply_update(&descriptor_field(older), X4));
    }

    /// The pre-identity builds, whose descriptor carried only the product name.
    #[test]
    fn an_anchor_predating_the_board_identity_cannot_apply_the_update() {
        let legacy = b"CalendulaOS (MarigoldOS)";
        assert!(!anchor_can_apply_update(&descriptor_field(legacy), X4));
    }

    #[test]
    fn a_foreign_or_erased_anchor_cannot_apply_the_update() {
        assert!(!anchor_can_apply_update(&descriptor_field(b"CrossInk"), X4));
        assert!(!anchor_can_apply_update(
            &[0u8; APP_DESC_PROJECT_NAME_LEN],
            X4
        ));
        assert!(!anchor_can_apply_update(
            &[0xFFu8; APP_DESC_PROJECT_NAME_LEN],
            X4
        ));
    }

    /// A prefix match would have accepted the other board; equality must not.
    #[test]
    fn a_prefix_of_the_identity_is_not_a_match() {
        assert!(!anchor_can_apply_update(
            &descriptor_field(b"CalendulaOS X4"),
            X4
        ));
    }

    /// Every identity has to survive the round trip through the fixed-width
    /// field the bootloader actually stores.
    #[test]
    fn the_identities_fit_the_descriptor_field() {
        for identity in [X4, X3] {
            assert!(identity.len() <= APP_DESC_PROJECT_NAME_LEN);
            assert_eq!(project_name(&descriptor_field(identity)), identity);
        }
    }

    #[test]
    fn an_update_from_the_anchor_writes_the_update_slot() {
        assert_eq!(
            plan_update_action(Some(ANCHOR_SLOT), ANCHOR_SLOT, true),
            UpdateAction::WriteUpdateSlot
        );
        // The anchor's contents are *not* irrelevant here, even though nothing
        // needs handing off: a bad anchor means `otadata` is not describing the
        // slot we are running from. See
        // `an_unbootable_anchor_means_otadata_is_not_where_we_are_running`.
    }

    #[test]
    fn an_update_from_the_update_slot_bounces_through_a_usable_anchor() {
        assert_eq!(
            plan_update_action(Some(UPDATE_SLOT), UPDATE_SLOT, true),
            UpdateAction::BounceToAnchor
        );
    }

    #[test]
    fn a_foreign_anchor_is_refused_rather_than_bounced_into() {
        assert_eq!(
            plan_update_action(Some(UPDATE_SLOT), UPDATE_SLOT, false),
            UpdateAction::NoUsableAnchor
        );
    }

    #[test]
    fn only_the_bounce_preserves_the_trigger() {
        assert!(UpdateAction::WriteUpdateSlot.consumes_trigger());
        assert!(UpdateAction::NoUsableAnchor.consumes_trigger());
        assert!(UpdateAction::RunningSlotUnknown.consumes_trigger());
        assert!(!UpdateAction::BounceToAnchor.consumes_trigger());
    }

    /// The bootloader boots the slot `otadata` names only if that image
    /// verifies, and falls forward without rewriting `otadata` when it does
    /// not. So `otadata` naming an unbootable anchor is proof we are running
    /// somewhere else — and the only place left is the slot a write erases.
    #[test]
    fn an_unbootable_anchor_means_otadata_is_not_where_we_are_running() {
        assert_eq!(
            plan_update_action(Some(ANCHOR_SLOT), ANCHOR_SLOT, false),
            UpdateAction::RunningSlotUnknown
        );
        assert_eq!(
            plan_update_action(Some(ANCHOR_SLOT), ANCHOR_SLOT, false).selects_slot(),
            None,
            "a write here would erase the running firmware"
        );
    }

    /// `selects_slot` is a *boot* target, not a write target — the bounce
    /// deliberately selects the anchor. That the anchor is never written is a
    /// property of the whole lifecycle, proved by
    /// [`many_updates_in_a_row_never_write_the_anchor`].
    #[test]
    fn each_action_selects_the_slot_it_names() {
        assert_eq!(
            UpdateAction::WriteUpdateSlot.selects_slot(),
            Some(UPDATE_SLOT)
        );
        assert_eq!(
            UpdateAction::BounceToAnchor.selects_slot(),
            Some(ANCHOR_SLOT)
        );
        assert_eq!(UpdateAction::NoUsableAnchor.selects_slot(), None);
        assert_eq!(UpdateAction::RunningSlotUnknown.selects_slot(), None);
    }

    // --- MMU translation, against values captured from an X3 ----------------
    //
    // The device was running the probe build from slot 1 (0x650000). Its
    // ESP-IDF bootloader reported, and the probe read back:
    //
    //   segment 0: paddr=0x650020 vaddr=0x3c000020   (DROM)
    //   segment 4: paddr=0x930020 vaddr=0x422e0020   (IROM)
    //   boot: Loaded app from partition at offset 0x650000
    //
    //   rodata vaddr 0x3c00aab9 -> table[0]  = 0x65
    //   code   vaddr 0x423175e0 -> table[49] = 0x96
    //
    // These are the numbers, not a model of them.

    #[test]
    fn the_mmu_index_matches_the_hardware_capture() {
        assert_eq!(mmu_index(0x3C00_AAB9), 0);
        assert_eq!(mmu_index(0x4231_75E0), 49);
        // The IROM segment base the bootloader mapped, at table[46] = 0x93.
        assert_eq!(mmu_index(0x422E_0020), 46);
    }

    #[test]
    fn the_mmu_resolves_the_addresses_the_bootloader_mapped() {
        // rodata: slot 1 + 0xaab9, inside the DROM segment at paddr 0x650020.
        assert_eq!(mmu_flash_offset(0x3C00_AAB9, 0x65), Some(0x65_AAB9));
        // code: offset 0x375c0 into the IROM segment at paddr 0x930020.
        assert_eq!(mmu_flash_offset(0x4231_75E0, 0x96), Some(0x96_75E0));
        assert_eq!(0x93_0020 + 0x3_75C0, 0x96_75E0);
    }

    #[test]
    fn an_unmapped_entry_resolves_to_nothing() {
        // Every entry past the image read exactly this on hardware.
        assert_eq!(mmu_flash_offset(0x4231_75E0, 0x100), None);
    }

    /// The whole point: the addresses above identify slot 1, which is what the
    /// bootloader said it loaded — not slot 0, which `otadata` could name.
    #[test]
    fn the_captured_addresses_identify_the_slot_the_bootloader_loaded() {
        let layout = OtaLayout {
            otadata: AppPartition {
                offset: 0xE000,
                size: 0x2000,
            },
            slots: [
                AppPartition {
                    offset: 0x1_0000,
                    size: 0x64_0000,
                },
                AppPartition {
                    offset: 0x65_0000,
                    size: 0x64_0000,
                },
            ],
        };
        for vaddr_entry in [(0x3C00_AAB9u32, 0x65u32), (0x4231_75E0, 0x96)] {
            let off = mmu_flash_offset(vaddr_entry.0, vaddr_entry.1).unwrap();
            assert_eq!(slot_containing(&layout, off), Some(UPDATE_SLOT));
        }
        // Sanity: the anchor's own first page still resolves to the anchor.
        assert_eq!(slot_containing(&layout, 0x1_0000), Some(ANCHOR_SLOT));
        assert_eq!(slot_containing(&layout, 0x0_F000), None);
    }

    #[test]
    fn recovery_acts_only_from_the_update_slot_with_a_bootable_anchor() {
        assert!(plan_recovery_switch(Some(UPDATE_SLOT), true));
        assert!(!plan_recovery_switch(Some(UPDATE_SLOT), false));
        assert!(!plan_recovery_switch(Some(ANCHOR_SLOT), true));
        // Erased otadata already defaults to the anchor: nothing to undo.
        assert!(!plan_recovery_switch(None, true));
    }

    // --- The staged-update lifecycle, across reboots ------------------------

    /// Enough of a device to run the update decision over real reboots: two
    /// otadata sectors the planners actually read and write, the bytes in each
    /// app slot, and the one-shot trigger file on the card.
    struct Device {
        sectors: [[u8; SELECT_ENTRY_LEN]; 2],
        slots: [&'static str; 2],
        trigger: Option<&'static str>,
        /// What *our* validator makes of the anchor.
        anchor_usable: bool,
        /// What the *bootloader* makes of it. Normally the same, but our
        /// validator is not `esp_image_format.c`, so an anchor can pass ours
        /// and fail its — the case that used to bounce forever.
        anchor_boots: bool,
    }

    /// What one boot did, for asserting on the sequence.
    #[derive(Debug, PartialEq, Eq)]
    enum Boot {
        /// No trigger present; the reader would start.
        Ran(u32),
        Acted(UpdateAction),
    }

    impl Device {
        fn new(active_slot: u32, slots: [&'static str; 2], anchor_usable: bool) -> Self {
            // Seed otadata so the bootloader selects `active_slot`.
            let seq = active_slot + 1;
            let mut dev = Self {
                sectors: [[0xFF; SELECT_ENTRY_LEN]; 2],
                slots,
                trigger: None,
                anchor_usable,
                anchor_boots: anchor_usable,
            };
            dev.sectors[0] = SelectEntry::new(seq, OTA_IMG_VALID).to_bytes();
            assert_eq!(dev.active(), active_slot, "seeding picked the wrong slot");
            dev
        }

        /// The slot `otadata` asks for, or `None` when both sectors are erased.
        fn requested(&self) -> Option<u32> {
            active_app_slot(&self.sectors[0], &self.sectors[1], 2)
        }

        /// `ota_state` of the entry the bootloader would use.
        fn active_state(&self) -> u32 {
            active_select_entry(&self.sectors[0], &self.sectors[1])
                .expect("otadata should be initialised")
                .1
                .ota_state
        }

        /// The boot-time mark-valid step, in `fw::main`'s position: before the
        /// update check, and only with proof we run the slot otadata names.
        fn mark_valid_step(&mut self) {
            if !may_mark_running_slot_valid(Some(self.running_slot()), self.requested()) {
                return;
            }
            if let Some(sw) = plan_mark_app_valid(&self.sectors[0], &self.sectors[1]) {
                self.sectors[sw.target_sector] = sw.entry.to_bytes();
            }
        }

        /// The slot `otadata` *asks* for.
        fn active(&self) -> u32 {
            active_app_slot(&self.sectors[0], &self.sectors[1], 2).unwrap_or(ANCHOR_SLOT)
        }

        /// The slot the bootloader actually loads — which is not always the one
        /// `otadata` asks for. ESP-IDF verifies the selected image and, when it
        /// fails, falls forward to another app partition and boots that one
        /// *without* rewriting `otadata`. The firmware then wakes up somewhere
        /// other than where `otadata` says it is.
        fn running_slot(&self) -> u32 {
            match self.active() {
                ANCHOR_SLOT if !self.anchor_boots => UPDATE_SLOT,
                slot => slot,
            }
        }

        /// One power-on, in `fw::ota_update::apply_pending_update`'s order:
        /// decide, write the image, consume the trigger, switch.
        ///
        /// The firmware aborts the switch if trigger removal fails, so that a
        /// stale trigger cannot re-run on every boot. This model always
        /// succeeds at removal: that branch is an SD-write failure with no
        /// bearing on any rule in this module, and simulating it here would
        /// only be re-asserting `fw` code from `proto`.
        fn boot(&mut self) -> Boot {
            self.mark_valid_step();
            let active = self.active();
            let running = self.running_slot();
            let Some(image) = self.trigger else {
                return Boot::Ran(running);
            };

            let action = plan_update_action(Some(running), active, self.anchor_usable);
            if action == UpdateAction::WriteUpdateSlot {
                // The invariant the whole slot policy exists to protect. An
                // erase of the executing slot destroys the running firmware
                // mid-write, and with a bad anchor it is the last image left.
                assert_ne!(
                    running, UPDATE_SLOT,
                    "about to erase the slot this firmware is executing from"
                );
                self.slots[UPDATE_SLOT as usize] = image;
            }
            if action.consumes_trigger() {
                self.trigger = None;
            }
            if let Some(dest) = action.selects_slot() {
                let sw = plan_switch(&self.sectors[0], &self.sectors[1], dest, 2);
                self.sectors[sw.target_sector] = sw.entry.to_bytes();
                assert_eq!(self.active(), dest, "the switch did not take effect");
            }
            Boot::Acted(action)
        }
    }

    /// The bootloader fall-forward, end to end.
    ///
    /// Slot 0 keeps its magic and identity but its image body is corrupt — an
    /// interrupted flash. A bounce puts `otadata` on slot 0; the bootloader
    /// rejects it, boots slot 1 anyway, and leaves `otadata` alone. The next
    /// boot therefore reads "active = slot 0" while executing slot 1, and the
    /// naive reading of that is "slot 1 is idle, erase it" — erasing the only
    /// bootable image on the device.
    #[test]
    fn a_corrupt_anchor_never_costs_us_the_running_firmware() {
        let mut dev = Device::new(UPDATE_SLOT, ["corrupt-anchor", "good-fw"], false);
        dev.trigger = Some("new-fw");

        // Running from slot 1, so a write is impossible and a bounce is the
        // only way to land the update — but not into an anchor that won't boot.
        assert_eq!(dev.boot(), Boot::Acted(UpdateAction::NoUsableAnchor));
        assert_eq!(dev.trigger, None, "a refusal is still one-shot");
        assert_eq!(dev.active(), UPDATE_SLOT, "otadata must not have moved");
        assert_eq!(dev.slots, ["corrupt-anchor", "good-fw"]);

        // And if something else strands otadata on the dead anchor — the
        // recovery hatch, a half-finished bounce from an older build — the
        // next boot with a trigger must still not write the slot it is on.
        dev.sectors[1] = plan_switch(&dev.sectors[0], &dev.sectors[1], ANCHOR_SLOT, 2)
            .entry
            .to_bytes();
        assert_eq!(dev.active(), ANCHOR_SLOT, "otadata now lies");
        assert_eq!(dev.running_slot(), UPDATE_SLOT, "but we run from slot 1");

        dev.trigger = Some("new-fw");
        // `NoUsableAnchor`, not `RunningSlotUnknown`: the MMU told us which slot
        // is running, so nothing is unknown here — `otadata` names an anchor the
        // bootloader would not boot. Either way nothing is written.
        assert_eq!(dev.boot(), Boot::Acted(UpdateAction::NoUsableAnchor));
        assert_eq!(
            dev.slots[UPDATE_SLOT as usize], "good-fw",
            "the running firmware must survive"
        );
        assert_eq!(dev.trigger, None);
    }

    /// An anchor our validator approves but the bootloader refuses. The bounce
    /// selects slot 0, the bootloader hands back slot 1, and `otadata` still
    /// says slot 0 — so the next boot sees the same inputs that produced the
    /// bounce. Without the failed-bounce check it bounces again, and the device
    /// resets forever.
    #[test]
    fn a_bounce_the_bootloader_refuses_is_not_retried_forever() {
        let mut dev = Device::new(UPDATE_SLOT, ["anchor-fw", "good-fw"], true);
        dev.anchor_boots = false; // passes our checks, fails the bootloader's
        dev.trigger = Some("new-fw");

        // Boot 1: the anchor looks fine from here, so hand off to it.
        assert_eq!(dev.boot(), Boot::Acted(UpdateAction::BounceToAnchor));
        assert_eq!(dev.active(), ANCHOR_SLOT);
        assert_eq!(dev.trigger, Some("new-fw"), "the bounce keeps the trigger");

        // Boot 2: the bootloader rejected the anchor and fell back to slot 1,
        // leaving otadata pointing at slot 0.
        assert_eq!(dev.running_slot(), UPDATE_SLOT);
        assert_eq!(dev.boot(), Boot::Acted(UpdateAction::NoUsableAnchor));
        assert_eq!(dev.trigger, None, "the retry loop ends by consuming it");
        assert_eq!(dev.slots, ["anchor-fw", "good-fw"], "nothing was written");

        // Boot 3 and onwards: no trigger, so no reset. It runs from slot 1 —
        // where the bootloader put it — not the slot otadata still names.
        assert_eq!(dev.boot(), Boot::Ran(UPDATE_SLOT));
    }

    /// The other half of a failed bounce. `plan_switch` writes `OTA_IMG_NEW`,
    /// so the entry naming the anchor is unconfirmed when the bootloader
    /// refuses it and hands back the update slot. The next boot's mark-valid
    /// step would then stamp `VALID` on the slot that just failed to boot —
    /// recording that a dead image had run successfully. It must not.
    #[test]
    fn a_failed_bounce_never_blesses_the_slot_that_would_not_boot() {
        let mut dev = Device::new(UPDATE_SLOT, ["anchor-fw", "good-fw"], true);
        dev.anchor_boots = false;
        dev.trigger = Some("new-fw");

        assert_eq!(dev.boot(), Boot::Acted(UpdateAction::BounceToAnchor));
        assert_eq!(dev.active(), ANCHOR_SLOT);
        assert_eq!(
            dev.active_state(),
            OTA_IMG_NEW,
            "the bounce leaves the anchor's entry unconfirmed"
        );

        // The bootloader refuses the anchor; we wake up on the update slot with
        // otadata still naming slot 0, and its entry still unconfirmed.
        assert_eq!(dev.running_slot(), UPDATE_SLOT);
        assert_eq!(dev.boot(), Boot::Acted(UpdateAction::NoUsableAnchor));
        assert_eq!(
            dev.active_state(),
            OTA_IMG_NEW,
            "an image the bootloader rejected must never be marked valid"
        );

        // Nor on any later boot, with the trigger gone.
        dev.boot();
        assert_eq!(dev.active_state(), OTA_IMG_NEW);
    }

    /// The ordinary case still confirms: a normal update lands, and the boot
    /// after it marks the slot it is genuinely running valid.
    #[test]
    fn a_landed_update_is_marked_valid_on_the_next_boot() {
        let mut dev = Device::new(ANCHOR_SLOT, ["anchor-fw", "old-fw"], true);
        dev.trigger = Some("new-fw");

        assert_eq!(dev.boot(), Boot::Acted(UpdateAction::WriteUpdateSlot));
        assert_eq!(dev.active(), UPDATE_SLOT);
        assert_eq!(dev.active_state(), OTA_IMG_NEW, "not yet confirmed");

        assert_eq!(dev.boot(), Boot::Ran(UPDATE_SLOT));
        assert_eq!(
            dev.active_state(),
            OTA_IMG_VALID,
            "the slot we are actually running gets confirmed"
        );
    }

    #[test]
    fn a_failed_bounce_never_reports_a_bounce() {
        // The finding's exact shape: anchor approved, otadata asks for slot 0,
        // the MMU says slot 1 is executing.
        let action = plan_update_action(Some(UPDATE_SLOT), ANCHOR_SLOT, true);
        assert_ne!(action, UpdateAction::BounceToAnchor);
        assert_eq!(action, UpdateAction::NoUsableAnchor);
        assert!(action.consumes_trigger());
        assert_eq!(action.selects_slot(), None);
    }

    // --- Which staged images may be installed --------------------------------

    #[test]
    fn the_shipped_identities_parse() {
        assert_eq!(parse_identity(X4), Some((&b"X4"[..], 1)));
        assert_eq!(parse_identity(X3), Some((&b"X3"[..], 1)));
    }

    #[test]
    fn a_current_image_for_this_board_is_installable() {
        assert!(staged_image_is_installable(&descriptor_field(X4), X4));
        assert!(staged_image_is_installable(&descriptor_field(X3), X3));
    }

    #[test]
    fn an_image_for_the_other_board_is_not_installable() {
        assert!(!staged_image_is_installable(&descriptor_field(X3), X4));
        assert!(!staged_image_is_installable(&descriptor_field(X4), X3));
    }

    /// The failure that motivates this: a pre-anchor build still alternates
    /// slots, so installing one destroys slot 0 on its next update.
    #[test]
    fn an_image_that_would_overwrite_the_anchor_is_not_installable() {
        assert!(!staged_image_is_installable(
            &descriptor_field(b"CalendulaOS X4 u0 (MarigoldOS)"),
            X4
        ));
        assert!(!staged_image_is_installable(
            &descriptor_field(b"CalendulaOS (MarigoldOS)"),
            X4
        ));
    }

    #[test]
    fn a_foreign_image_is_not_installable() {
        for name in [
            &b"CrossPoint"[..],
            b"esp-idf",
            b"",
            b"CalendulaOS X4 u (MarigoldOS)",
            b"CalendulaOS X4 uX (MarigoldOS)",
            b"CalendulaOS X4 u1",
            b"X4 u1 (MarigoldOS)",
        ] {
            assert!(
                !staged_image_is_installable(&descriptor_field(name), X4),
                "{:?} must not be installable",
                core::str::from_utf8(name)
            );
        }
    }

    /// An image with a different updater generation must be rejected because an
    /// immutable slot-0 anchor of generation u1 cannot service future updates
    /// for a u2 image.
    #[test]
    fn a_different_updater_generation_is_not_installable() {
        assert!(!staged_image_is_installable(
            &descriptor_field(b"CalendulaOS X4 u2 (MarigoldOS)"),
            X4
        ));
        assert!(!staged_image_is_installable(
            &descriptor_field(b"CalendulaOS X4 u17 (MarigoldOS)"),
            X4
        ));
    }

    /// Regression test for [P1]: An identity alias like `u01` with leading zeros
    /// parses to generation 1 but does not byte-match the canonical `u1` identity.
    /// It must be rejected by `staged_image_is_installable` to prevent a one-way
    /// upgrade where slot 1 running `u01` fails anchor validation on future updates.
    #[test]
    fn u01_generation_alias_is_rejected_and_canonical_u1_is_accepted() {
        let alias_u01 = descriptor_field(b"CalendulaOS X4 u01 (MarigoldOS)");
        let canonical_u1 = descriptor_field(b"CalendulaOS X4 u1 (MarigoldOS)");
        let running_identity = X4; // "CalendulaOS X4 u1 (MarigoldOS)"

        assert!(
            !staged_image_is_installable(&alias_u01, running_identity),
            "u01 alias must be rejected"
        );
        assert!(
            staged_image_is_installable(&canonical_u1, running_identity),
            "canonical u1 candidate must be accepted"
        );
    }

    /// Lifecycle test: A u1 anchor attempting to install a u2 candidate must
    /// be rejected by `staged_image_is_installable`, preventing a one-way
    /// upgrade that leaves slot 1 unable to use the u1 anchor on future updates.
    #[test]
    fn u1_anchor_rejects_u2_installation_to_prevent_one_way_upgrade_deadlock() {
        let u1_anchor_identity = descriptor_field(b"CalendulaOS X4 u1 (MarigoldOS)");
        let u2_candidate_identity = descriptor_field(b"CalendulaOS X4 u2 (MarigoldOS)");

        // 1. Starts with a u1 anchor (running u1).
        let running_identity = X4; // "CalendulaOS X4 u1 (MarigoldOS)"

        // 2. Attempt to install u2: must be rejected.
        assert!(
            !staged_image_is_installable(&u2_candidate_identity, running_identity),
            "u1 firmware must reject u2 staged image to avoid losing anchor compatibility"
        );

        // Prove that same-generation (u1) updates succeed end-to-end:
        let u1_candidate_identity = descriptor_field(b"CalendulaOS X4 u1 (MarigoldOS)");
        assert!(staged_image_is_installable(
            &u1_candidate_identity,
            running_identity
        ));

        let mut dev = Device::new(ANCHOR_SLOT, ["u1-anchor", "old-fw"], true);
        dev.trigger = Some("u1-image");
        assert_eq!(dev.boot(), Boot::Acted(UpdateAction::WriteUpdateSlot));

        // 3. Boots the resulting slot-1 image (running u1).
        assert_eq!(dev.boot(), Boot::Ran(UPDATE_SLOT));

        // 4. Stages another update while running from slot 1.
        dev.trigger = Some("u1-image-2");
        let anchor_usable = anchor_can_apply_update(&u1_anchor_identity, running_identity);
        assert!(
            anchor_usable,
            "u1 anchor must be usable by u1 running image"
        );
        assert_eq!(dev.boot(), Boot::Acted(UpdateAction::BounceToAnchor));
    }

    #[test]
    fn a_staged_image_for_another_chip_is_rejected() {
        let mut img = resident_image(true);
        img[12..14].copy_from_slice(&(EXPECTED_CHIP_ID + 1).to_le_bytes());
        let len = img.len();
        assert_eq!(
            validate_image(&mut cursor(img), len, None),
            Err(ImageError::WrongChip)
        );
    }

    /// An unresolved MMU lookup must not fall back to `otadata`. `otadata` is
    /// wrong precisely when the bootloader fell forward, and that is the case
    /// where a write erases the running firmware — so with no proof, nothing is
    /// written and nothing is selected, whichever slot `otadata` names and
    /// however good the anchor looks.
    #[test]
    fn an_unprovable_running_slot_writes_nothing() {
        for requested in [ANCHOR_SLOT, UPDATE_SLOT] {
            for anchor_usable in [true, false] {
                let action = plan_update_action(None, requested, anchor_usable);
                assert_eq!(
                    action,
                    UpdateAction::RunningSlotUnknown,
                    "requested={requested} anchor_usable={anchor_usable}"
                );
                assert_ne!(action, UpdateAction::WriteUpdateSlot);
                assert_eq!(action.selects_slot(), None, "otadata must not move");
                assert!(action.consumes_trigger(), "and it must not re-run");
            }
        }
    }

    #[test]
    fn marking_valid_needs_proof_of_the_running_slot() {
        assert!(may_mark_running_slot_valid(
            Some(UPDATE_SLOT),
            Some(UPDATE_SLOT)
        ));
        assert!(may_mark_running_slot_valid(
            Some(ANCHOR_SLOT),
            Some(ANCHOR_SLOT)
        ));
        // Disagreement: otadata names a slot we are not executing.
        assert!(!may_mark_running_slot_valid(
            Some(UPDATE_SLOT),
            Some(ANCHOR_SLOT)
        ));
        // Fail closed. An unreadable MMU is exactly when otadata is least
        // trustworthy, so it must not be the case that waves the marking
        // through.
        assert!(!may_mark_running_slot_valid(None, Some(ANCHOR_SLOT)));
        assert!(!may_mark_running_slot_valid(Some(UPDATE_SLOT), None));
        assert!(!may_mark_running_slot_valid(None, None));
    }

    #[test]
    fn an_update_staged_from_the_anchor_lands_in_one_reboot() {
        let mut dev = Device::new(ANCHOR_SLOT, ["anchor-fw", "old-fw"], true);
        dev.trigger = Some("new-fw");

        assert_eq!(dev.boot(), Boot::Acted(UpdateAction::WriteUpdateSlot));
        assert_eq!(dev.active(), UPDATE_SLOT);
        assert_eq!(dev.slots[UPDATE_SLOT as usize], "new-fw");
        assert_eq!(dev.slots[ANCHOR_SLOT as usize], "anchor-fw");
        assert_eq!(dev.trigger, None, "the trigger must be one-shot");

        // The next boot is an ordinary one.
        assert_eq!(dev.boot(), Boot::Ran(UPDATE_SLOT));
    }

    /// The hand-off this whole policy rests on: staged while running from the
    /// update slot, the update still lands, and slot 0 is never written.
    #[test]
    fn an_update_staged_from_the_update_slot_bounces_and_then_lands() {
        let mut dev = Device::new(UPDATE_SLOT, ["anchor-fw", "old-fw"], true);
        dev.trigger = Some("new-fw");

        // Boot 1: cannot write the slot we run from, so hand off to the anchor.
        assert_eq!(dev.boot(), Boot::Acted(UpdateAction::BounceToAnchor));
        assert_eq!(dev.active(), ANCHOR_SLOT);
        assert_eq!(
            dev.trigger,
            Some("new-fw"),
            "the bounce must leave the trigger for the anchor boot"
        );
        assert_eq!(
            dev.slots[UPDATE_SLOT as usize], "old-fw",
            "nothing written yet"
        );

        // Boot 2: the anchor applies it.
        assert_eq!(dev.boot(), Boot::Acted(UpdateAction::WriteUpdateSlot));
        assert_eq!(dev.active(), UPDATE_SLOT);
        assert_eq!(dev.slots[UPDATE_SLOT as usize], "new-fw");
        assert_eq!(dev.trigger, None);

        // Boot 3: settled.
        assert_eq!(dev.boot(), Boot::Ran(UPDATE_SLOT));
        assert_eq!(dev.slots[ANCHOR_SLOT as usize], "anchor-fw");
    }

    #[test]
    fn a_foreign_anchor_refuses_instead_of_stranding_the_update() {
        let mut dev = Device::new(UPDATE_SLOT, ["crosspoint", "old-fw"], false);
        dev.trigger = Some("new-fw");

        assert_eq!(dev.boot(), Boot::Acted(UpdateAction::NoUsableAnchor));
        // Still on our own firmware, and the trigger is cleared so it cannot
        // re-run the refusal on every future boot.
        assert_eq!(dev.active(), UPDATE_SLOT);
        assert_eq!(dev.trigger, None);
        assert_eq!(dev.slots[ANCHOR_SLOT as usize], "crosspoint");
        assert_eq!(dev.boot(), Boot::Ran(UPDATE_SLOT));
    }

    /// The bounce must not be able to loop: whatever slot a boot starts from,
    /// the sequence reaches a settled state and bounces at most once.
    #[test]
    fn the_handoff_always_terminates() {
        for start in [ANCHOR_SLOT, UPDATE_SLOT] {
            for anchor_usable in [true, false] {
                let mut dev = Device::new(start, ["anchor-fw", "old-fw"], anchor_usable);
                dev.trigger = Some("new-fw");

                let mut bounces = 0;
                let mut settled = false;
                for _ in 0..8 {
                    match dev.boot() {
                        Boot::Acted(UpdateAction::BounceToAnchor) => bounces += 1,
                        Boot::Ran(_) => {
                            settled = true;
                            break;
                        }
                        Boot::Acted(_) => {}
                    }
                }
                assert!(
                    settled,
                    "start={start} anchor_usable={anchor_usable} never settled"
                );
                assert!(
                    bounces <= 1,
                    "start={start} anchor_usable={anchor_usable} bounced {bounces} times"
                );
                // The invariant the whole policy exists to protect.
                assert_eq!(
                    dev.slots[ANCHOR_SLOT as usize], "anchor-fw",
                    "the anchor was overwritten"
                );
            }
        }
    }

    /// Repeated updates keep alternating through the anchor without ever
    /// writing it — the case plain A/B alternation would have clobbered.
    #[test]
    fn many_updates_in_a_row_never_write_the_anchor() {
        let mut dev = Device::new(ANCHOR_SLOT, ["anchor-fw", "factory"], true);
        for image in ["fw-1", "fw-2", "fw-3", "fw-4"] {
            dev.trigger = Some(image);
            // Each update needs at most a bounce plus a write.
            for _ in 0..3 {
                if dev.trigger.is_none() {
                    break;
                }
                dev.boot();
            }
            assert_eq!(dev.slots[UPDATE_SLOT as usize], image);
            assert_eq!(dev.slots[ANCHOR_SLOT as usize], "anchor-fw");
        }
    }

    /// After a bounce the hatch is still armed against the firmware the update
    /// produced: the anchor is intact, so a held combo can back it out.
    #[test]
    fn the_hatch_still_works_on_the_firmware_an_update_installed() {
        let mut dev = Device::new(UPDATE_SLOT, ["anchor-fw", "old-fw"], true);
        dev.trigger = Some("bad-fw");
        dev.boot(); // bounce
        dev.boot(); // write + select

        assert_eq!(dev.active(), UPDATE_SLOT);
        assert!(plan_recovery_switch(Some(dev.active()), true));

        // The combo's switch lands on the untouched anchor.
        let sw = plan_switch(&dev.sectors[0], &dev.sectors[1], ANCHOR_SLOT, 2);
        dev.sectors[sw.target_sector] = sw.entry.to_bytes();
        assert_eq!(dev.active(), ANCHOR_SLOT);
        assert_eq!(dev.slots[ANCHOR_SLOT as usize], "anchor-fw");
    }
}
