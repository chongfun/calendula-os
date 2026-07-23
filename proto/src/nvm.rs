#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProgressRecord {
    pub book_id: u32,
    pub page: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppStateRecord {
    pub book_id: u32,
    pub chapter: u16,
    pub screen: u32,
    pub shell_orientation: u8,
    pub reading_orientation: u8,
    pub refresh_policy: u8,
    pub font_size: u8,
    pub line_spacing: u8,
    pub font_weight: u8,
    pub font_family: u8,
    pub front_buttons: u8,
    pub source_hash: u32,
    pub source_size: u32,
}

impl AppStateRecord {
    pub const ENCODED_LEN: usize = 36;
    const V3_ENCODED_LEN: usize = 32;
    const V1_ENCODED_LEN: usize = 24;
    const MAGIC: u32 = 0x5834_4F53;
    const VERSION: u8 = 4;
    const V3_VERSION: u8 = 3;
    const V2_VERSION: u8 = 2;
    const V1_VERSION: u8 = 1;
    /// FontSize::Medium / LineSpacing::Normal / FontWeight::Normal as u8 in
    /// app-core.
    const DEFAULT_FONT_SIZE: u8 = 1;
    const DEFAULT_LINE_SPACING: u8 = 1;
    const DEFAULT_FONT_WEIGHT: u8 = 0;
    const DEFAULT_FONT_FAMILY: u8 = 0;

    pub const fn new(book_id: u32) -> Self {
        Self {
            book_id,
            chapter: 0,
            screen: 0,
            shell_orientation: 3,
            reading_orientation: 0,
            refresh_policy: 1,
            font_size: Self::DEFAULT_FONT_SIZE,
            line_spacing: Self::DEFAULT_LINE_SPACING,
            font_weight: Self::DEFAULT_FONT_WEIGHT,
            font_family: Self::DEFAULT_FONT_FAMILY,
            front_buttons: 0,
            source_hash: 0,
            source_size: 0,
        }
    }

    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        write_u32(&mut out, 0, Self::MAGIC);
        out[4] = Self::VERSION;
        out[5] = self.shell_orientation;
        out[6] = self.reading_orientation;
        out[7] = self.refresh_policy;
        write_u32(&mut out, 8, self.book_id);
        write_u16(&mut out, 12, self.chapter);
        write_u32(&mut out, 14, self.screen);
        write_u32(&mut out, 18, self.source_hash);
        write_u32(&mut out, 22, self.source_size);
        out[26] = self.font_size;
        out[27] = self.line_spacing;
        // V4 adds the type weight at byte 28; the checksum span covers the
        // reserved tail. The font family later took reserved byte 29 and the
        // front-button layout byte 30: records written before either carry
        // zero there, which is the respective default (Literata, pages
        // right), so no version bump was needed. Byte 31 stays reserved zero.
        out[28] = self.font_weight;
        out[29] = self.font_family;
        out[30] = self.front_buttons;
        let checksum = checksum(&out[..32]);
        write_u32(&mut out, 32, checksum);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::V1_ENCODED_LEN {
            return None;
        }
        if read_u32(bytes, 0) != Self::MAGIC {
            return None;
        }
        match bytes[4] {
            Self::VERSION => {
                if bytes.len() < Self::ENCODED_LEN {
                    return None;
                }
                let expected = read_u32(bytes, 32);
                if checksum(&bytes[..32]) != expected {
                    return None;
                }
                Some(Self {
                    book_id: read_u32(bytes, 8),
                    chapter: read_u16(bytes, 12),
                    screen: read_u32(bytes, 14),
                    shell_orientation: bytes[5],
                    reading_orientation: bytes[6],
                    refresh_policy: bytes[7],
                    font_size: bytes[26],
                    line_spacing: bytes[27],
                    font_weight: bytes[28],
                    font_family: bytes[29],
                    front_buttons: bytes[30],
                    source_hash: read_u32(bytes, 18),
                    source_size: read_u32(bytes, 22),
                })
            }
            Self::V3_VERSION | Self::V2_VERSION => {
                if bytes.len() < Self::V3_ENCODED_LEN {
                    return None;
                }
                let expected = read_u32(bytes, 28);
                if checksum(&bytes[..28]) != expected {
                    return None;
                }
                let (font_size, line_spacing) = if bytes[4] == Self::V3_VERSION {
                    (bytes[26], bytes[27])
                } else {
                    (Self::DEFAULT_FONT_SIZE, Self::DEFAULT_LINE_SPACING)
                };
                Some(Self {
                    book_id: read_u32(bytes, 8),
                    chapter: read_u16(bytes, 12),
                    screen: read_u32(bytes, 14),
                    shell_orientation: bytes[5],
                    reading_orientation: bytes[6],
                    refresh_policy: bytes[7],
                    font_size,
                    line_spacing,
                    font_weight: Self::DEFAULT_FONT_WEIGHT,
                    font_family: Self::DEFAULT_FONT_FAMILY,
                    front_buttons: 0,
                    source_hash: read_u32(bytes, 18),
                    source_size: read_u32(bytes, 22),
                })
            }
            Self::V1_VERSION => {
                let expected = read_u32(bytes, 20);
                if checksum(&bytes[..20]) != expected {
                    return None;
                }
                Some(Self {
                    book_id: read_u32(bytes, 8),
                    chapter: read_u16(bytes, 12),
                    screen: read_u32(bytes, 14),
                    shell_orientation: bytes[5],
                    reading_orientation: bytes[6],
                    refresh_policy: bytes[7],
                    font_size: Self::DEFAULT_FONT_SIZE,
                    line_spacing: Self::DEFAULT_LINE_SPACING,
                    font_weight: Self::DEFAULT_FONT_WEIGHT,
                    font_family: Self::DEFAULT_FONT_FAMILY,
                    front_buttons: 0,
                    source_hash: 0,
                    source_size: 0,
                })
            }
            _ => None,
        }
    }
}

pub trait ProgressStore {
    type Error;

    fn load(&mut self) -> Result<Option<ProgressRecord>, Self::Error>;
    fn store(&mut self, record: ProgressRecord) -> Result<(), Self::Error>;
}

pub trait AppStateStore {
    type Error;

    fn load_app_state(&mut self) -> Result<Option<AppStateRecord>, Self::Error>;
    fn store_app_state(&mut self, record: AppStateRecord) -> Result<(), Self::Error>;
}

/// Station credentials at `/XTEINK/WIFI.BIN`, written by the onboarding
/// portal and read back ahead of every sync session. Same envelope as
/// `AppStateRecord`: magic, version, payload, checksum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WifiCredentialsRecord {
    pub ssid: [u8; 32],
    pub ssid_len: u8,
    pub password: [u8; 64],
    pub password_len: u8,
}

impl WifiCredentialsRecord {
    pub const ENCODED_LEN: usize = 4 + 1 + 1 + 1 + 32 + 64 + 4;
    const MAGIC: u32 = 0x5834_5746; // "X4WF"
    const VERSION: u8 = 1;

    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        write_u32(&mut out, 0, Self::MAGIC);
        out[4] = Self::VERSION;
        out[5] = self.ssid_len.min(32);
        out[6] = self.password_len.min(64);
        out[7..39].copy_from_slice(&self.ssid);
        out[39..103].copy_from_slice(&self.password);
        let checksum = checksum(&out[..103]);
        write_u32(&mut out, 103, checksum);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::ENCODED_LEN
            || read_u32(bytes, 0) != Self::MAGIC
            || bytes[4] != Self::VERSION
            || read_u32(bytes, 103) != checksum(&bytes[..103])
        {
            return None;
        }
        let mut record = Self {
            ssid: [0; 32],
            ssid_len: bytes[5].min(32),
            password: [0; 64],
            password_len: bytes[6].min(64),
        };
        record.ssid.copy_from_slice(&bytes[7..39]);
        record.password.copy_from_slice(&bytes[39..103]);
        if record.ssid_len == 0 {
            return None;
        }
        Some(record)
    }
}

/// Per-book reading position, stored as POS.BIN beside that book's cache.
///
/// The authoritative record of where the reader is in a book: the global
/// [`AppStateRecord`] carries a copy, but only as a mirror for readers that
/// expect to find position there.
///
/// `salt` is mixed into the checksum by the caller rather than fixed here. The
/// stored screen is a page index under one panel's pagination, so a card moved
/// between panels of different sizes must fail validation and resume at the
/// book's start instead of a page that does not exist. The geometry that
/// decides the salt lives above this crate, and a salt of zero leaves the
/// checksum byte-identical to an unsalted one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PositionRecord {
    pub chapter: u16,
    pub screen: u32,
}

impl PositionRecord {
    pub const ENCODED_LEN: usize = 15;
    const MAGIC: &'static [u8; 4] = b"X4PS";
    const VERSION: u8 = 1;
    /// The checksum spans everything before it.
    const CHECKSUM_AT: usize = 11;

    pub fn encode(self, salt: u32) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[..4].copy_from_slice(Self::MAGIC);
        out[4] = Self::VERSION;
        out[5..7].copy_from_slice(&self.chapter.to_le_bytes());
        out[7..11].copy_from_slice(&self.screen.to_le_bytes());
        let sum = Self::checksum(&out[..Self::CHECKSUM_AT], salt);
        out[Self::CHECKSUM_AT..].copy_from_slice(&sum.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8], salt: u32) -> Option<Self> {
        if bytes.len() < Self::ENCODED_LEN
            || &bytes[..4] != Self::MAGIC
            || bytes[4] != Self::VERSION
        {
            return None;
        }
        let sum = Self::checksum(&bytes[..Self::CHECKSUM_AT], salt);
        if bytes[Self::CHECKSUM_AT..Self::ENCODED_LEN] != sum.to_le_bytes() {
            return None;
        }
        Some(Self {
            chapter: u16::from_le_bytes([bytes[5], bytes[6]]),
            screen: u32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]),
        })
    }

    /// Byte sum shifted by the panel salt. Deliberately not the FNV hash the
    /// other records use: this envelope is shared with MarigoldOS and has to
    /// stay byte-identical.
    fn checksum(bytes: &[u8], salt: u32) -> u32 {
        bytes
            .iter()
            .map(|byte| *byte as u32)
            .sum::<u32>()
            .wrapping_add(salt)
    }
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut hash = 0x811C_9DC5u32;
    for byte in bytes {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn write_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset] = value as u8;
    out[offset + 1] = (value >> 8) as u8;
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset] = value as u8;
    out[offset + 1] = (value >> 8) as u8;
    out[offset + 2] = (value >> 16) as u8;
    out[offset + 3] = (value >> 24) as u8;
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    bytes[offset] as u16 | ((bytes[offset + 1] as u16) << 8)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    bytes[offset] as u32
        | ((bytes[offset + 1] as u32) << 8)
        | ((bytes[offset + 2] as u32) << 16)
        | ((bytes[offset + 3] as u32) << 24)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> AppStateRecord {
        AppStateRecord {
            book_id: 7,
            chapter: 3,
            screen: 41,
            shell_orientation: 2,
            reading_orientation: 1,
            refresh_policy: 2,
            font_size: 2,
            line_spacing: 0,
            font_weight: 1,
            font_family: 1,
            front_buttons: 1,
            source_hash: 0xDEAD_BEEF,
            source_size: 123_456,
        }
    }

    /// The exact bytes `record()` encodes to, computed independently of this
    /// implementation.
    ///
    /// The other tests here prove old records still decode; this one proves the
    /// layout itself has not moved. Both files this crate describes are shared
    /// byte-for-byte with MarigoldOS so cards carry reading state between the
    /// two firmwares, and every compatibility test in this module would still
    /// pass if the whole envelope shifted underneath them in lockstep.
    const STATE_GOLDEN: [u8; AppStateRecord::ENCODED_LEN] = [
        0x53, 0x4f, 0x34, 0x58, 0x04, 0x02, 0x01, 0x02, 0x07, 0x00, 0x00, 0x00, 0x03, 0x00, 0x29,
        0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x40, 0xe2, 0x01, 0x00, 0x02, 0x00, 0x01, 0x01,
        0x01, 0x00, 0xa7, 0x76, 0x1e, 0x60,
    ];

    #[test]
    fn app_state_encodes_to_the_agreed_bytes() {
        assert_eq!(record().encode(), STATE_GOLDEN);
        assert_eq!(AppStateRecord::decode(&STATE_GOLDEN), Some(record()));
    }

    /// `PositionRecord { chapter: 3, screen: 41 }` at an unsalted checksum.
    const POSITION_GOLDEN: [u8; PositionRecord::ENCODED_LEN] = [
        0x58, 0x34, 0x50, 0x53, 0x01, 0x03, 0x00, 0x29, 0x00, 0x00, 0x00, 0x5c, 0x01, 0x00, 0x00,
    ];

    fn position() -> PositionRecord {
        PositionRecord {
            chapter: 3,
            screen: 41,
        }
    }

    #[test]
    fn position_encodes_to_the_agreed_bytes() {
        assert_eq!(position().encode(0), POSITION_GOLDEN);
        assert_eq!(
            PositionRecord::decode(&POSITION_GOLDEN, 0),
            Some(position())
        );
    }

    #[test]
    fn position_round_trips_under_any_salt() {
        for salt in [0, 1, 0x0100_0193, u32::MAX] {
            let encoded = position().encode(salt);
            assert_eq!(
                PositionRecord::decode(&encoded, salt),
                Some(position()),
                "salt {salt:#x} must round trip"
            );
        }
    }

    #[test]
    fn a_position_from_another_panel_is_refused() {
        // The stored screen is a page index under one pagination. Reading it
        // back under a different geometry has to fail rather than resume at a
        // page that does not exist in this one.
        let written_on_another_panel = position().encode(0x0011_0022);
        assert_eq!(PositionRecord::decode(&written_on_another_panel, 0), None);
    }

    #[test]
    fn a_corrupt_position_is_refused() {
        for byte in [0, 4, 5, 11] {
            let mut encoded = position().encode(0);
            encoded[byte] ^= 0xFF;
            assert_eq!(
                PositionRecord::decode(&encoded, 0),
                None,
                "a flipped byte {byte} must not decode"
            );
        }
        assert_eq!(
            PositionRecord::decode(&position().encode(0)[..14], 0),
            None,
            "a truncated record must not decode"
        );
    }

    #[test]
    fn app_state_round_trips_with_type_settings() {
        let encoded = record().encode();
        assert_eq!(AppStateRecord::decode(&encoded), Some(record()));
    }

    #[test]
    fn v3_records_decode_with_default_weight() {
        // A V3 record keeps its size/spacing but predates the weight byte, so
        // it must decode as the default weight. Rebuild the record as a 32-byte
        // V3 image: version 3 with the checksum over the first 28 bytes.
        let mut encoded = record().encode();
        encoded[4] = AppStateRecord::V3_VERSION;
        let checksum = checksum(&encoded[..28]);
        write_u32(&mut encoded, 28, checksum);

        let decoded =
            AppStateRecord::decode(&encoded[..AppStateRecord::V3_ENCODED_LEN]).expect("v3 decodes");
        assert_eq!(decoded.font_size, 2);
        assert_eq!(decoded.line_spacing, 0);
        assert_eq!(decoded.font_weight, AppStateRecord::DEFAULT_FONT_WEIGHT);
        assert_eq!(decoded.book_id, 7);
    }

    #[test]
    fn v2_records_decode_with_default_type_settings() {
        // A V2 record zeroes the type bytes; size, spacing, and weight all
        // fall back to defaults. The checksum spans the first 28 bytes.
        let mut encoded = record().encode();
        encoded[4] = AppStateRecord::V2_VERSION;
        encoded[26] = 0;
        encoded[27] = 0;
        let checksum = checksum(&encoded[..28]);
        write_u32(&mut encoded, 28, checksum);

        let decoded =
            AppStateRecord::decode(&encoded[..AppStateRecord::V3_ENCODED_LEN]).expect("v2 decodes");
        assert_eq!(decoded.font_size, AppStateRecord::DEFAULT_FONT_SIZE);
        assert_eq!(decoded.line_spacing, AppStateRecord::DEFAULT_LINE_SPACING);
        assert_eq!(decoded.font_weight, AppStateRecord::DEFAULT_FONT_WEIGHT);
        assert_eq!(decoded.book_id, 7);
        assert_eq!(decoded.source_hash, 0xDEAD_BEEF);
    }

    #[test]
    fn pre_family_v4_records_decode_as_literata() {
        // V4 records written before the Font setting carry the reserved zero
        // at byte 29; that must decode as the default (Literata) family.
        let mut encoded = record().encode();
        encoded[29] = 0;
        let checksum = checksum(&encoded[..32]);
        write_u32(&mut encoded, 32, checksum);

        let decoded = AppStateRecord::decode(&encoded).expect("pre-family v4 decodes");
        assert_eq!(decoded.font_family, AppStateRecord::DEFAULT_FONT_FAMILY);
        assert_eq!(decoded.font_weight, 1);
    }

    #[test]
    fn pre_front_buttons_v4_records_decode_as_pages_right() {
        // V4 records written before the Front buttons setting carry the
        // reserved zero at byte 30; that must decode as the default
        // (pages right) layout.
        let mut encoded = record().encode();
        encoded[30] = 0;
        let checksum = checksum(&encoded[..32]);
        write_u32(&mut encoded, 32, checksum);

        let decoded = AppStateRecord::decode(&encoded).expect("pre-front-buttons v4 decodes");
        assert_eq!(decoded.front_buttons, 0);
        assert_eq!(decoded.font_family, 1);
    }

    #[test]
    fn corrupt_checksum_is_rejected() {
        let mut encoded = record().encode();
        encoded[26] ^= 0xFF;
        assert_eq!(AppStateRecord::decode(&encoded), None);
    }
}
