//! Shared ESP-IDF application descriptor encoding. Board identity stays platform-owned.

#[repr(C)]
pub struct EspAppDesc {
    pub magic_word: u32,
    pub secure_version: u32,
    pub reserv1: [u32; 2],
    pub version: [u8; 32],
    pub project_name: [u8; 32],
    pub time: [u8; 16],
    pub date: [u8; 16],
    pub idf_ver: [u8; 32],
    pub app_elf_sha256: [u8; 32],
    pub min_efuse_blk_rev_full: u16,
    pub max_efuse_blk_rev_full: u16,
    pub mmu_page_size: u8,
    pub spi_flash_mode: u8,
    pub reserv3: [u8; 2],
    pub reserv2: [u32; 18],
}

// Zero-pad a string into a fixed descriptor field. The fields are always
// [u8; 32] in the image, so filling them costs no bytes; overlong input
// fails the const evaluation instead of truncating silently.
pub const fn desc_field<const N: usize>(s: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        out[i] = bytes[i];
        i += 1;
    }
    out
}

pub const fn new(version: &str, project_name: &str) -> EspAppDesc {
    EspAppDesc {
        magic_word: 0xABCD5432,
        secure_version: 0,
        reserv1: [0; 2],
        version: desc_field(version),
        project_name: desc_field(project_name),
        time: *b"00:00:00\0\0\0\0\0\0\0\0",
        date: *b"2026-05-20\0\0\0\0\0\0",
        idf_ver: [0; 32],
        app_elf_sha256: [0; 32],
        min_efuse_blk_rev_full: 0,
        max_efuse_blk_rev_full: 65535,
        mmu_page_size: 16,
        spi_flash_mode: 2,
        reserv3: [0; 2],
        reserv2: [0; 18],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esp_app_desc_layout() {
        assert_eq!(core::mem::size_of::<EspAppDesc>(), 256);
        assert_eq!(core::mem::align_of::<EspAppDesc>(), 4);
    }

    #[test]
    fn new_sets_expected_fields_for_x4_and_x3() {
        for identity in [proto::ota::IDENTITY_X4, proto::ota::IDENTITY_X3] {
            let desc = new("0.9.0", identity);
            assert_eq!(desc.magic_word, 0xABCD5432);
            assert_eq!(desc.mmu_page_size, 16);
            assert_eq!(desc.spi_flash_mode, 2);
            assert_eq!(desc.min_efuse_blk_rev_full, 0);
            assert_eq!(desc.max_efuse_blk_rev_full, 65535);
            assert_eq!(desc.date, *b"2026-05-20\0\0\0\0\0\0");
            assert_eq!(desc.time, *b"00:00:00\0\0\0\0\0\0\0\0");
            assert_eq!(
                proto::ota::project_name(&desc.project_name),
                identity.as_bytes()
            );
            assert!(desc.version.starts_with(b"0.9.0\0"));
            assert!(desc.version[6..].iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn desc_field_nul_pads_and_preserves_exact_width() {
        let short = desc_field::<32>("abc");
        assert_eq!(&short[..3], b"abc");
        assert!(short[3..].iter().all(|&b| b == 0));

        let empty = desc_field::<32>("");
        assert_eq!(empty, [0u8; 32]);

        let exact_str = "0123456789abcdef0123456789abcdef";
        assert_eq!(exact_str.len(), 32);
        let exact = desc_field::<32>(exact_str);
        assert_eq!(&exact, exact_str.as_bytes());
    }
}
