//! Which app slot this ESP32-S3 firmware is executing from.
//!
//! Translates the mapped address of `_ESP_APP_DESC` (in DROM) back to a flash
//! byte offset via ESP32-S3's flash MMU table, avoiding any blind trust in
//! `otadata` when a fallback/forward boot occurred.

use proto::ota;

fn running_flash_offset() -> Option<u32> {
    let vaddr = (&raw const crate::_ESP_APP_DESC) as usize as u32;
    let entry_id = ota::s3_mmu_index(vaddr)?;
    let page_offset = esp_hal::flash::flash_mmu_entry_offset(entry_id)?;
    page_offset.checked_add(vaddr % ota::MMU_PAGE_SIZE)
}

pub fn running_slot(layout: &ota::OtaLayout) -> Option<u32> {
    let offset = running_flash_offset();
    let slot = offset.and_then(|off| ota::slot_containing(layout, off));
    match (offset, slot) {
        (Some(off), Some(s)) => {
            esp_println::println!("s3-mmu: executing at flash={:#x} (slot {})", off, s);
        }
        (Some(off), None) => {
            esp_println::println!("s3-mmu: flash={:#x} outside both app slots", off);
        }
        (None, _) => {
            esp_println::println!("s3-mmu: failed to resolve running flash offset");
        }
    }
    slot
}
