//! Which app slot this firmware is *executing* from.
//!
//! `otadata` records the slot the bootloader was asked to boot, not the one it
//! booted: when the selected image fails verification ESP-IDF falls forward to
//! another app partition and leaves `otadata` alone. Asking the hardware
//! instead removes the guesswork — translate a mapped code address back to a
//! flash offset through the flash MMU, which is what ESP-IDF's
//! `spi_flash_cache2phys()` does and what FreeInk's `RecoveryBoot` gets from
//! `esp_ota_get_running_partition()`. esp-hal exposes no equivalent.
//!
//! The arithmetic, and every constant describing what the table *contains*,
//! live in [`proto::ota`], asserted there against values captured from an X3
//! running a known slot. What stays here is where the table *is* and the
//! volatile read of it — the two things a host test cannot exercise.

use proto::ota;

/// MMU table base — ESP-IDF's `DR_REG_MMU_TABLE` for the ESP32-C3.
const MMU_TABLE: u32 = 0x600C_5000;

#[allow(unsafe_code)]
fn table_entry(index: u32) -> Option<u32> {
    if index >= ota::MMU_ENTRY_COUNT {
        return None;
    }
    // SAFETY: the MMU table is memory-mapped, word-aligned, and `index` is
    // bounded above. Reading it cannot disturb the mapping.
    Some(unsafe { core::ptr::read_volatile((MMU_TABLE + index * 4) as *const u32) })
}

/// The flash offset this function's own code was mapped from.
fn running_flash_offset() -> Option<u32> {
    let vaddr = running_flash_offset as *const () as u32;
    let entry = table_entry(ota::mmu_index(vaddr))?;
    ota::mmu_flash_offset(vaddr, entry)
}

/// The app slot this firmware is running from, or `None` if the mapping does
/// not resolve into one. `None` is not a licence to fall back to `otadata`:
/// `otadata` is wrong exactly when the bootloader fell forward, which is the
/// case a write would erase the running firmware in. Callers must refuse — see
/// [`ota::plan_update_action`] and [`ota::may_mark_running_slot_valid`], which
/// both fail closed on it.
pub fn running_slot(layout: &ota::OtaLayout) -> Option<u32> {
    let slot = ota::slot_containing(layout, running_flash_offset()?);
    match slot {
        Some(s) => esp_println::println!("mmu: executing from slot {}", s),
        None => esp_println::println!("mmu: execution address is outside both app slots"),
    }
    slot
}
