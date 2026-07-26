//! Boot-time firmware self-update from the SD card.
//!
//! If `/FWUPDATE.BIN` is present at boot it is validated, written into the
//! update slot, selected by flipping `otadata`, deleted (so the next boot
//! doesn't re-apply it), and the device resets into the new firmware. This is
//! the recovery/update path that keeps flashing onto a locked unit from being a
//! one-way trip — the same scheme as the FreeInk SDK's `RecoveryBoot` and
//! CrossPoint's `FirmwareFlasher`/`OtaBootSwitch`, ported to Rust.
//!
//! # Slot policy: slot 0 is an anchor, not half of an A/B pair
//!
//! Updates always land in [`UPDATE_SLOT`] (slot 1). Slot 0 keeps whatever was
//! first installed there and is never written by this module, so the boot-time
//! hatch ([`recover_to_slot0`]) always has a known firmware to fall back to.
//! That is the FreeInk `RecoveryBoot` convention, where the recovery slot is
//! "deliberately never reflashed"; plain A/B alternation would eventually write
//! the update over the very image the hatch returns to.
//!
//! The cost is that an update staged while already running from slot 1 cannot
//! be written in place — that would erase the running firmware. Such a boot
//! instead points `otadata` back at the anchor and resets *without* consuming
//! the trigger file, so the anchor boot applies the update into slot 1 on the
//! next pass. One extra reboot, and slot 0 still never gets written.
//!
//! Only the update slot and the inactive `otadata` sector are written, so a
//! failure here never touches the running firmware: the bootloader keeps
//! selecting the current slot until a complete, valid image flips `otadata`.
//! Slot locations come from the partition table already installed on the
//! device; this is essential for locked X3 units that retain the stock layout.
//! The image format, partition parsing, seq CRC, slot-switch math, and the slot
//! policy above ([`ota::plan_update_action`], [`ota::plan_recovery_switch`])
//! live in [`proto::ota`] and are host-tested — including the reboot-crossing
//! hand-off, simulated there over real `otadata` sectors. This module is the
//! flash and SD I/O that answers those decisions and carries them out.
//!
//! The flash + `otadata` mechanism is hardware-validated (see `docs/FLASHING.md`);
//! a full end-to-end `FWUPDATE.BIN` run still awaits a card reader. Flash writes
//! freeze other tasks via a critical section; run this at boot while the radio
//! is idle.

use embedded_sdmmc::{BlockDevice, File, Mode, TimeSource};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_storage::FlashStorage;
use proto::ota::{
    self, ImageError, SelectEntry, UpdateAction, ANCHOR_SLOT, APP_DESC_PROJECT_NAME_LEN,
    APP_DESC_PROJECT_NAME_OFFSET, SELECT_ENTRY_LEN, UPDATE_SLOT,
};

use crate::sd_session::SdRoot;

/// One-shot trigger file at the card root. 8.3-safe so it opens without long
/// filename support, and distinct from the `update.bin` a user may keep on the
/// card as a permanent recovery image.
///
/// Device-specific so a card is safe to move between an X4 and an X3: each
/// build only picks up an image named for its own panel, so an X4 image
/// (`FWUPDATE.BIN`) is invisible to an X3 and vice versa. Flashing the wrong
/// build wouldn't necessarily brick, but would drive the wrong panel and
/// battery gauge — a black screen, not a recoverable state.
#[cfg(not(feature = "device-x3"))]
const TRIGGER_FILE: &str = "FWUPDATE.BIN";
#[cfg(feature = "device-x3")]
const TRIGGER_FILE: &str = "FWUPDX3.BIN";

// The otadata/slot offsets are *not* assumed from `partitions.csv`: stock X3
// units may retain a different table, so they are discovered at runtime from
// the partition table the installed bootloader actually uses.
const PARTITION_TABLE_OFFSET: u32 = 0x0000_8000;
const PARTITION_TABLE_LEN: usize = 0x1000;
const OTADATA_SECTOR_STRIDE: u32 = 0x0000_1000; // one 4 KiB sector per entry
const OTA_COUNT: u32 = 2;

const SECTOR: usize = 4096;

// Variants (and their payloads) exist to be logged over serial on the failure
// path; dead-code analysis ignores the derived Debug use, hence the allow.
#[allow(dead_code)]
#[derive(Debug)]
pub enum UpdateError {
    /// No trigger file present — the normal case, not really an error.
    NoTrigger,
    ReadFile,
    Invalid(ImageError),
    /// Structurally sound, but not an image this firmware may install — the
    /// other board, or an updater generation that would overwrite the anchor.
    ForeignImage,
    PartitionTable(ota::PartitionTableError),
    Flash,
}

/// What the boot-time update check decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Nothing staged, or nothing could be done; carry on booting.
    Idle,
    /// The planned [`UpdateAction`] was carried out and `otadata` now selects a
    /// new slot; the caller should reset into it.
    Acted(UpdateAction),
}

impl UpdateOutcome {
    /// Whether the boot should end in a reset into the newly selected slot.
    pub fn needs_reset(self) -> bool {
        !matches!(self, Self::Idle)
    }
}

/// Adapts an open SD file to [`ota::ImageSource`] for the validation pass.
struct SdFile<'f, D: BlockDevice, T: TimeSource, const MD: usize, const MF: usize, const MV: usize>(
    &'f File<'f, D, T, MD, MF, MV>,
);

impl<D: BlockDevice, T: TimeSource, const MD: usize, const MF: usize, const MV: usize>
    ota::ImageSource for SdFile<'_, D, T, MD, MF, MV>
{
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ()> {
        read_file_exact(self.0, buf)
    }
}

/// Adapts a flash partition to [`ota::ImageSource`], reading strictly forward
/// from `offset`.
struct FlashImage<'a, 'f> {
    flash: &'a mut FlashStorage<'f>,
    offset: u32,
}

impl ota::ImageSource for FlashImage<'_, '_> {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ()> {
        self.flash.read(self.offset, buf).map_err(|_| ())?;
        // The walk is bounded by the partition size, which is far below u32::MAX.
        self.offset += buf.len() as u32;
        Ok(())
    }
}

fn read_file_exact<
    D: BlockDevice,
    T: TimeSource,
    const MD: usize,
    const MF: usize,
    const MV: usize,
>(
    file: &File<'_, D, T, MD, MF, MV>,
    buf: &mut [u8],
) -> Result<(), ()> {
    let mut done = 0;
    while done < buf.len() {
        match file.read(&mut buf[done..]) {
            Ok(0) => return Err(()),
            Ok(n) => done += n,
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

/// Check for a pending SD update and apply it. A non-[`UpdateOutcome::Idle`]
/// return means the caller should now `software_reset()` into the slot
/// `otadata` selects. On any failure the trigger file is removed so a corrupt
/// image can't wedge every boot, and the running firmware is left untouched.
pub fn apply_pending_update(root: &SdRoot) -> UpdateOutcome {
    let staged = match try_apply(root) {
        Ok(staged) => staged,
        Err(UpdateError::NoTrigger) => return UpdateOutcome::Idle,
        Err(e) => {
            esp_println::println!("ota: update failed: {:?}", e);
            // Still a one-shot: clear the trigger so a bad image or a refusal
            // can't re-run on every boot.
            remove_trigger(root);
            return UpdateOutcome::Idle;
        }
    };

    // `consumes_trigger` is the tested rule: everything but the bounce clears
    // it. The bounce is the exception because the anchor boot it hands off to
    // is the one that applies the image — removing it here would discard the
    // update instead. Removal runs here rather than inside `try_apply`, where
    // the trigger's own read handle is still open; reclaiming its clusters
    // needs the file closed.
    if staged.action.consumes_trigger() && !remove_trigger(root) {
        esp_println::println!(
            "ota: WARNING trigger removal failed; aborting otadata switch to prevent boot loop"
        );
        return UpdateOutcome::Idle;
    }

    let Some(dest) = staged.action.selects_slot() else {
        return UpdateOutcome::Idle;
    };
    if !select_slot(&staged.layout, dest) {
        return UpdateOutcome::Idle;
    }
    if staged.action == UpdateAction::BounceToAnchor {
        esp_println::println!(
            "ota: update pending while running from slot {}; bouncing to the slot {} anchor to apply it",
            UPDATE_SLOT,
            ANCHOR_SLOT
        );
    } else {
        esp_println::println!("ota: update applied; resetting");
    }
    UpdateOutcome::Acted(staged.action)
}

/// Delete the one-shot trigger, reclaiming its clusters. Returns whether the
/// card no longer holds it.
fn remove_trigger(root: &SdRoot) -> bool {
    upload_store::remove_file_reclaiming_clusters(root, TRIGGER_FILE)
        != upload_store::RemoveStatus::Failed
}

/// Point `otadata` at `dest_slot`. Returns whether the write landed.
fn select_slot(layout: &ota::OtaLayout, dest_slot: u32) -> bool {
    let mut flash = flash_storage();
    let (s0, s1) = match read_otadata(&mut flash, layout.otadata.offset) {
        Ok(s) => s,
        Err(e) => {
            esp_println::println!("ota: failed to read otadata for switch: {:?}", e);
            return false;
        }
    };
    let switch = ota::plan_switch(&s0, &s1, dest_slot, OTA_COUNT);
    if let Err(e) = write_select_entry(
        &mut flash,
        layout.otadata.offset,
        switch.target_sector,
        &switch.entry,
    ) {
        esp_println::println!("ota: failed to write otadata switch: {:?}", e);
        return false;
    }
    esp_println::println!(
        "ota: otadata sector {} -> seq {} (slot {})",
        switch.target_sector,
        switch.entry.ota_seq,
        dest_slot
    );
    true
}

/// A carried-out [`UpdateAction`] plus the layout its `otadata` write needs.
struct Staged {
    action: UpdateAction,
    layout: ota::OtaLayout,
}

fn try_apply(root: &SdRoot) -> Result<Staged, UpdateError> {
    let file = root
        .open_file_in_dir(TRIGGER_FILE, Mode::ReadOnly)
        .map_err(|e| match e {
            embedded_sdmmc::Error::NotFound => UpdateError::NoTrigger,
            _ => UpdateError::ReadFile,
        })?;
    let len = file.length() as usize;
    esp_println::println!("ota: {} found, {} bytes", TRIGGER_FILE, len);

    let mut flash = flash_storage();
    let layout = read_ota_layout(&mut flash)?;

    // The destination is always the update slot, never the anchor. Derive its
    // offset and size from the table the bootloader will actually use.
    let dest_partition = layout.slots[UPDATE_SLOT as usize];
    let (s0, s1) = read_otadata(&mut flash, layout.otadata.offset)?;

    // Ask the hardware which slot is executing before believing `otadata`,
    // which only records which slot was *requested*. An unresolved answer is
    // kept as `None` rather than folded back into the request: `otadata` is
    // wrong in exactly the case a write would erase the running firmware, so
    // `ota::plan_update_action` refuses on it instead of guessing.
    let requested = ota::active_app_slot(&s0, &s1, OTA_COUNT).unwrap_or(ANCHOR_SLOT);
    let running = crate::mmu::running_slot(&layout);
    match running {
        Some(running) if running != requested => esp_println::println!(
            "ota: otadata requests slot {} but slot {} is executing; trusting the MMU",
            requested,
            running
        ),
        // Deliberately *not* falling back to `requested`: see
        // `ota::plan_update_action`, which refuses rather than guess.
        None => esp_println::println!(
            "ota: cannot prove which slot is executing; refusing to write either"
        ),
        Some(_) => {}
    }

    // Pass 1: prove the whole image before touching flash — and before any
    // decision to reboot. Validating ahead of the bounce below is what keeps a
    // corrupt file from costing the user a trip through the anchor to discover
    // it was corrupt; the anchor re-validates when it does the write.
    ota::validate_image(&mut SdFile(&file), len, Some(dest_partition.size as usize))
        .map_err(UpdateError::Invalid)?;

    // Structurally sound is not the same as belonging on this device. The
    // descriptor identity is what carries the panel and the updater hand-off,
    // and `validate_image` cannot see either: an image for the other board
    // renamed to our trigger drives the wrong panel, and a pre-anchor build
    // alternates slots and overwrites slot 0 on its next update — destroying
    // the anchor this policy exists to keep. The rule is
    // `ota::staged_image_is_installable`; this is the read that answers it.
    let mut name = [0u8; APP_DESC_PROJECT_NAME_LEN];
    file.seek_from_start(APP_DESC_PROJECT_NAME_OFFSET)
        .map_err(|_| UpdateError::ReadFile)?;
    read_file_exact(&file, &mut name).map_err(|_| UpdateError::ReadFile)?;
    if !ota::staged_image_is_installable(&name, crate::PROJECT_NAME.as_bytes()) {
        esp_println::println!(
            "ota: staged image is '{}', which this firmware ({}) must not install",
            core::str::from_utf8(ota::project_name(&name)).unwrap_or("<non-utf8>"),
            crate::PROJECT_NAME
        );
        return Err(UpdateError::ForeignImage);
    }
    file.seek_from_start(0).map_err(|_| UpdateError::ReadFile)?;

    // Writing the slot we are executing from would erase the running firmware
    // mid-boot, so that case hands the job back to the anchor instead. The rule
    // is `ota::plan_update_action`; this is the I/O that answers it and carries
    // it out.
    //
    // Checked on every path, not just before a bounce: `active` is what
    // `otadata` *requests*, and the bootloader falls forward to another slot
    // when the requested one does not verify. The anchor's own validity is the
    // only evidence available here that `otadata` is telling the truth, so it
    // is worth the full read even when we are seemingly running from the
    // anchor and about to take the ordinary write path.
    let anchor_usable = anchor_holds_our_firmware(&mut flash, &layout);
    let action = ota::plan_update_action(running, requested, anchor_usable);
    match action {
        UpdateAction::WriteUpdateSlot => {
            esp_println::println!(
                // Reaching this arm means the MMU resolved the running slot and
                // it is not the update slot, so `running` is `Some(ANCHOR_SLOT)`.
                "ota: running slot {:?}, writing slot {} at {:#x} ({} bytes)",
                running,
                UPDATE_SLOT,
                dest_partition.offset,
                dest_partition.size
            );
            // Pass 2: erase + stream the image into the update slot.
            write_image(&mut flash, dest_partition.offset, &file, len)?;
        }
        UpdateAction::BounceToAnchor => {}
        // Both refusals cover more than one situation — an anchor that cannot
        // apply the update, or a bounce the bootloader already rejected; an
        // unbootable anchor, or an MMU that did not answer at all. Log the
        // inputs the decision was made from rather than a story about them.
        UpdateAction::NoUsableAnchor | UpdateAction::RunningSlotUnknown => {
            esp_println::println!(
                "ota: {:?} — running {:?}, otadata requests slot {}, anchor usable {}",
                action,
                running,
                requested,
                anchor_usable
            )
        }
    }

    Ok(Staged { action, layout })
}

/// Whether the anchor slot holds a complete, valid image of *our* firmware.
///
/// Three questions, cheapest first, and all three have to answer yes:
///
/// - **Magic.** Is there an image here at all?
/// - **Identity.** Would it consume the trigger file we may leave behind for
///   it? A mixed install can leave CrossPoint or the stock firmware in slot 0,
///   and neither knows what the trigger file is; nor is the product name enough
///   — see [`crate::PROJECT_NAME`] for why the board and updater generation are
///   part of the identity. The rule is [`ota::anchor_can_apply_update`].
/// - **Integrity.** Would the *bootloader* load it? A flash interrupted partway
///   through writing slot 0 leaves the magic and descriptor intact and the tail
///   missing, which passes both checks above. The answer decides more than
///   whether to bounce: see [`ota::plan_update_action`] on why a firmware that
///   cannot trust the anchor cannot trust `otadata` about which slot it is
///   itself running from.
fn anchor_holds_our_firmware(flash: &mut FlashStorage, layout: &ota::OtaLayout) -> bool {
    let anchor = layout.slots[ANCHOR_SLOT as usize];

    let mut magic = [0u8; 4];
    if flash.read(anchor.offset, &mut magic).is_err() || magic[0] != ota::IMAGE_MAGIC {
        esp_println::println!("ota: slot {} holds no valid image", ANCHOR_SLOT);
        return false;
    }

    let mut name = [0u8; APP_DESC_PROJECT_NAME_LEN];
    if let Err(e) = flash.read(anchor.offset + APP_DESC_PROJECT_NAME_OFFSET, &mut name) {
        esp_println::println!(
            "ota: failed to read slot {} descriptor: {:?}",
            ANCHOR_SLOT,
            e
        );
        return false;
    }
    if !ota::anchor_can_apply_update(&name, crate::PROJECT_NAME.as_bytes()) {
        esp_println::println!(
            "ota: slot {} holds firmware of another identity; it could not apply this update",
            ANCHOR_SLOT
        );
        return false;
    }

    let mut src = FlashImage {
        flash,
        offset: anchor.offset,
    };
    if let Err(e) = ota::validate_flash_image(&mut src, anchor.size as usize) {
        esp_println::println!("ota: slot {} image is not loadable: {:?}", ANCHOR_SLOT, e);
        return false;
    }
    true
}

/// On-device validation of the flash + otadata path when no SD card reader is
/// available to place `FWUPDATE.BIN`. On the first boot (running from slot 0)
/// it copies the running image into the inactive slot and switches otadata to
/// it, so the next boot runs from the other slot — exercising esp-storage
/// erase/write, the seq CRC, the otadata switch, and the bootloader honouring
/// it, all without an SD file. One-shot: once running from the far slot it
/// no-ops. Compiled only under the `ota-selftest` feature.
#[cfg(feature = "ota-selftest")]
pub fn run_selftest() -> bool {
    // 3 MiB comfortably covers the ~2.5 MiB app image; the copy is self-
    // delimiting (the bootloader reads the header/segments and ignores the
    // trailing bytes), so an over-copy is harmless.
    const COPY_LEN: u32 = 0x0030_0000;

    let mut flash = flash_storage();
    let layout = match read_ota_layout(&mut flash) {
        Ok(layout) => layout,
        Err(_) => return false,
    };
    let (s0, s1) = match read_otadata(&mut flash, layout.otadata.offset) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let active = ota::active_app_slot(&s0, &s1, OTA_COUNT).unwrap_or(ANCHOR_SLOT);
    if active != ANCHOR_SLOT {
        esp_println::println!("selftest: already running from slot {}; done", active);
        return false;
    }

    let anchor = layout.slots[ANCHOR_SLOT as usize];
    let update = layout.slots[UPDATE_SLOT as usize];
    if anchor.size < COPY_LEN || update.size < COPY_LEN {
        esp_println::println!("selftest: OTA partition too small");
        return false;
    }
    let src = anchor.offset;
    let dst = update.offset;
    esp_println::println!(
        "selftest: copy slot {} -> slot {} ({} bytes)",
        ANCHOR_SLOT,
        UPDATE_SLOT,
        COPY_LEN
    );
    if flash.erase(dst, dst + COPY_LEN).is_err() {
        esp_println::println!("selftest: erase failed");
        return false;
    }
    let mut buf = [0u8; SECTOR];
    let mut off = 0u32;
    while off < COPY_LEN {
        if flash.read(src + off, &mut buf).is_err() {
            esp_println::println!("selftest: read failed @{:#x}", off);
            return false;
        }
        if flash.write(dst + off, &buf).is_err() {
            esp_println::println!("selftest: write failed @{:#x}", off);
            return false;
        }
        off += SECTOR as u32;
    }

    let (s0, s1) = match read_otadata(&mut flash, layout.otadata.offset) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let switch = ota::plan_switch(&s0, &s1, UPDATE_SLOT, OTA_COUNT);
    if write_select_entry(
        &mut flash,
        layout.otadata.offset,
        switch.target_sector,
        &switch.entry,
    )
    .is_err()
    {
        esp_println::println!("selftest: otadata write failed");
        return false;
    }
    esp_println::println!(
        "selftest: otadata sector {} -> seq {} (slot {})",
        switch.target_sector,
        switch.entry.ota_seq,
        UPDATE_SLOT
    );
    true
}

/// Boot-time escape hatch (the FreeInk SDK `RecoveryBoot` pattern): when the
/// combo is held at reset and we are running from the update slot, repoint
/// `otadata` at the anchor and return `true` so the caller resets into it.
/// Because [`apply_pending_update`] never writes the anchor, the firmware first
/// installed there is still there — so this reliably backs out of an update
/// that boots but misbehaves.
///
/// No-op (returns `false`) when already effectively on the anchor, or when the
/// anchor doesn't hold a valid image (so the combo can't switch into an empty
/// slot). The stock bootloader can't read buttons, so this is the earliest
/// point a held combo can be honoured — it must run before the app takes over.
pub fn recover_to_slot0() -> bool {
    let mut flash = flash_storage();
    let layout = match read_ota_layout(&mut flash) {
        Ok(layout) => layout,
        Err(_) => return false,
    };
    let (s0, s1) = match read_otadata(&mut flash, layout.otadata.offset) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let active = ota::active_app_slot(&s0, &s1, OTA_COUNT);
    // Any bootable firmware will do in the anchor here — unlike the update
    // bounce, the point is to leave the misbehaving slot, not to hand off work.
    // Deliberately magic-only, so a foreign-but-working anchor is still an
    // escape. The cost is that a *corrupt* anchor passes and the bootloader
    // then falls forward to the update slot, leaving `otadata` naming a slot we
    // are not on — a wasted reboot rather than a hazard, because
    // `ota::plan_update_action` treats an unbootable anchor as proof of exactly
    // that and refuses to write.
    let mut head = [0u8; 4];
    let anchor_bootable = flash
        .read(layout.slots[ANCHOR_SLOT as usize].offset, &mut head)
        .is_ok()
        && head[0] == ota::IMAGE_MAGIC;

    if !ota::plan_recovery_switch(active, anchor_bootable) {
        if active == Some(UPDATE_SLOT) && !anchor_bootable {
            esp_println::println!(
                "recovery: slot {} has no valid image; ignoring combo",
                ANCHOR_SLOT
            );
        }
        return false;
    }
    let switch = ota::plan_switch(&s0, &s1, ANCHOR_SLOT, OTA_COUNT);
    if write_select_entry(
        &mut flash,
        layout.otadata.offset,
        switch.target_sector,
        &switch.entry,
    )
    .is_err()
    {
        esp_println::println!("recovery: otadata write failed");
        return false;
    }
    esp_println::println!(
        "recovery: combo held; otadata sector {} -> seq {} (slot {})",
        switch.target_sector,
        switch.entry.ota_seq,
        ANCHOR_SLOT
    );
    true
}

/// Acknowledge a freshly OTA-booted app before the next deep-sleep reset can
/// make rollback-enabled bootloaders return to the previous firmware.
pub fn mark_running_slot_valid() {
    let mut flash = flash_storage();
    let layout = match read_ota_layout(&mut flash) {
        Ok(layout) => layout,
        Err(_) => return,
    };
    let (s0, s1) = match read_otadata(&mut flash, layout.otadata.offset) {
        Ok(v) => v,
        Err(_) => return,
    };

    // Marking "the running slot" valid is only honest if we are running the
    // slot otadata names. After a fall-forward we are not, and confirming that
    // entry would bless an image that just failed to boot — cementing the state
    // instead of leaving it for `plan_update_action` to notice.
    let running = crate::mmu::running_slot(&layout);
    let requested = ota::active_app_slot(&s0, &s1, OTA_COUNT);
    if !ota::may_mark_running_slot_valid(running, requested) {
        esp_println::println!(
            "ota: cannot prove we run the slot otadata selects (running {:?}, requested {:?}); \
             not marking it valid",
            running,
            requested
        );
        return;
    }

    let Some(valid) = ota::plan_mark_app_valid(&s0, &s1) else {
        return;
    };
    if write_select_entry(
        &mut flash,
        layout.otadata.offset,
        valid.target_sector,
        &valid.entry,
    )
    .is_err()
    {
        esp_println::println!("ota: mark-valid failed");
        return;
    }
    esp_println::println!(
        "ota: marked slot {} valid (seq {})",
        (valid.entry.ota_seq - 1) % OTA_COUNT,
        valid.entry.ota_seq
    );
}

#[allow(unsafe_code)]
fn flash_storage() -> FlashStorage<'static> {
    // SAFETY: OTA update/recovery runs at boot before application tasks use
    // flash directly. This preserves the old `FlashStorage::new()` singleton
    // behavior under esp-storage's explicit peripheral ownership API.
    FlashStorage::new(unsafe { esp_hal::peripherals::FLASH::steal() })
}

fn read_ota_layout(flash: &mut FlashStorage) -> Result<ota::OtaLayout, UpdateError> {
    let mut table = [0u8; PARTITION_TABLE_LEN];
    flash
        .read(PARTITION_TABLE_OFFSET, &mut table)
        .map_err(|_| UpdateError::Flash)?;
    ota::parse_ota_layout(&table, flash.capacity() as u32).map_err(UpdateError::PartitionTable)
}

fn read_otadata(
    flash: &mut FlashStorage,
    otadata_offset: u32,
) -> Result<([u8; SELECT_ENTRY_LEN], [u8; SELECT_ENTRY_LEN]), UpdateError> {
    let mut s0 = [0u8; SELECT_ENTRY_LEN];
    let mut s1 = [0u8; SELECT_ENTRY_LEN];
    flash
        .read(otadata_offset, &mut s0)
        .map_err(|_| UpdateError::Flash)?;
    flash
        .read(otadata_offset + OTADATA_SECTOR_STRIDE, &mut s1)
        .map_err(|_| UpdateError::Flash)?;
    Ok((s0, s1))
}

fn write_image<D: BlockDevice, T: TimeSource, const MD: usize, const MF: usize, const MV: usize>(
    flash: &mut FlashStorage,
    dest_offset: u32,
    file: &File<'_, D, T, MD, MF, MV>,
    len: usize,
) -> Result<(), UpdateError> {
    // Erase only the sectors we will write, rounded up to the 4 KiB boundary.
    let erase_len = ((len as u32) + SECTOR as u32 - 1) & !(SECTOR as u32 - 1);
    flash
        .erase(dest_offset, dest_offset + erase_len)
        .map_err(|_| UpdateError::Flash)?;

    let mut buf = [0u8; SECTOR];
    let mut written: u32 = 0;
    while (written as usize) < len {
        let want = core::cmp::min(SECTOR, len - written as usize);
        read_file_exact(file, &mut buf[..want]).map_err(|_| UpdateError::ReadFile)?;
        // NorFlash writes must be a multiple of WRITE_SIZE (4); pad the final
        // partial word with 0xFF (the erased state), leaving flash unchanged
        // past the real image bytes.
        let wlen = (want + 3) & !3;
        for b in &mut buf[want..wlen] {
            *b = 0xFF;
        }
        flash
            .write(dest_offset + written, &buf[..wlen])
            .map_err(|_| UpdateError::Flash)?;
        written += want as u32;
    }
    Ok(())
}

fn write_select_entry(
    flash: &mut FlashStorage,
    otadata_offset: u32,
    sector: usize,
    entry: &SelectEntry,
) -> Result<(), UpdateError> {
    let offset = otadata_offset + sector as u32 * OTADATA_SECTOR_STRIDE;
    flash
        .erase(offset, offset + OTADATA_SECTOR_STRIDE)
        .map_err(|_| UpdateError::Flash)?;
    flash
        .write(offset, &entry.to_bytes())
        .map_err(|_| UpdateError::Flash)?;
    Ok(())
}
