//! ESP-IDF application-image integrity validation.
//!
//! Before a candidate firmware is written into the inactive OTA slot — over the
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
    /// The source reported a read error / short read.
    Read,
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

    let mut header = [0u8; HEADER_LEN];
    src.read_exact(&mut header).map_err(|_| ImageError::Read)?;
    if header[0] != IMAGE_MAGIC {
        return Err(ImageError::BadMagic);
    }
    let segment_count = header[1];
    // Byte 23 (`hash_appended`) flags a SHA-256 trailer over the whole image.
    let hash_appended = header[23] != 0;

    let mut sha = Sha256::new();
    sha.update(header);
    // The XOR checksum is seeded with 0xEF and covers segment *data* only.
    let mut checksum = CHECKSUM_SEED;
    let mut pos = HEADER_LEN;

    let mut buf = [0u8; STREAM_CHUNK];
    for _ in 0..segment_count {
        if pos + SEG_HEADER_LEN > image_len {
            return Err(ImageError::BadSegments);
        }
        let mut seg_header = [0u8; SEG_HEADER_LEN];
        src.read_exact(&mut seg_header).map_err(|_| ImageError::Read)?;
        sha.update(seg_header);
        pos += SEG_HEADER_LEN;

        let data_len =
            u32::from_le_bytes([seg_header[4], seg_header[5], seg_header[6], seg_header[7]]) as usize;
        if pos + data_len > image_len {
            return Err(ImageError::BadSegments);
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
    if expected_len != image_len {
        return Err(ImageError::BadSize);
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
    fn build_image(segment_lens: &[usize], hash_appended: bool) -> Vec<u8> {
        let mut img = Vec::new();
        let mut header = [0u8; HEADER_LEN];
        header[0] = IMAGE_MAGIC;
        header[1] = segment_lens.len() as u8;
        header[23] = if hash_appended { 1 } else { 0 };
        img.extend_from_slice(&header);

        let mut checksum = CHECKSUM_SEED;
        for (i, &len) in segment_lens.iter().enumerate() {
            let mut seg_header = [0u8; SEG_HEADER_LEN];
            // load address (arbitrary, not validated) + data length
            seg_header[0..4].copy_from_slice(&(0x3c00_0000u32 + i as u32).to_le_bytes());
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

    #[test]
    fn accepts_hash_appended_image() {
        let img = valid_image(true);
        let len = img.len();
        assert_eq!(validate_image(&mut cursor(img), len, Some(0x640000)), Ok(()));
    }

    #[test]
    fn accepts_image_without_hash_trailer() {
        let img = valid_image(false);
        let len = img.len();
        assert_eq!(validate_image(&mut cursor(img), len, None), Ok(()));
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
}
