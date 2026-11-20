//! Boot-time firmware self-update from the SD card (`FWUPDX4P.BIN`).
//!
//! Follows CalendulaOS's slot-0 anchor / slot-1 update policy: updates always
//! target [`UPDATE_SLOT`] (slot 1), leaving [`ANCHOR_SLOT`] (slot 0) untouched.
//! On X4 Pro builds running in `app1` alongside CrossPoint in `app0`, anchor
//! validation refuses `BounceToAnchor` cleanly with `NoUsableAnchor` so slot 0
//! is never clobbered.

use embedded_sdmmc::{BlockDevice, File, Mode, TimeSource};
use esp_hal::flash::{Config as FlashConfig, Flash};
use esp_hal::Blocking;
use proto::ota::{
    self, ImageError, SelectEntry, UpdateAction, ANCHOR_SLOT, APP_DESC_PROJECT_NAME_LEN,
    APP_DESC_PROJECT_NAME_OFFSET, SELECT_ENTRY_LEN, UPDATE_SLOT,
};

use crate::sd_session::SdRoot;

const TRIGGER_FILE: &str = "FWUPDX4P.BIN";

const PARTITION_TABLE_OFFSET: u32 = 0x0000_8000;
const PARTITION_TABLE_LEN: usize = 0x1000;
const OTADATA_SECTOR_STRIDE: u32 = 0x0000_1000;
const OTA_COUNT: u32 = 2;

const SECTOR: usize = 4096;
const WRITE_CHUNK_BYTES: usize = 1024;
const WRITE_CHUNK_WORDS: usize = WRITE_CHUNK_BYTES / 4;

type S3Flash = Flash<'static, Blocking>;

#[allow(dead_code)]
#[derive(Debug)]
pub enum UpdateError {
    NoTrigger,
    ReadFile,
    Invalid(ImageError),
    ForeignImage,
    PartitionTable(ota::PartitionTableError),
    Flash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    Idle,
    Acted(UpdateAction),
}

impl UpdateOutcome {
    pub fn needs_reset(self) -> bool {
        !matches!(self, Self::Idle)
    }
}

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

struct FlashImage<'a> {
    flash: &'a mut S3Flash,
    offset: u32,
}

impl ota::ImageSource for FlashImage<'_> {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ()> {
        read_flash_bytes(self.flash, self.offset, buf)?;
        self.offset = self.offset.checked_add(buf.len() as u32).ok_or(())?;
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

fn read_flash_bytes(flash: &mut S3Flash, offset: u32, mut out: &mut [u8]) -> Result<(), ()> {
    let mut cursor = offset;
    while !out.is_empty() {
        let aligned_off = cursor & !3;
        let skip = (cursor - aligned_off) as usize;
        let mut words = [0u32; 128];
        let max_bytes = words.len() * 4 - skip;
        let take = out.len().min(max_bytes);
        let word_count = (skip + take).div_ceil(4);
        flash
            .read(aligned_off, &mut words[..word_count])
            .map_err(|_| ())?;
        for (i, dst) in out[..take].iter_mut().enumerate() {
            let abs = skip + i;
            let bytes = words[abs / 4].to_le_bytes();
            *dst = bytes[abs % 4];
        }
        out = &mut out[take..];
        cursor = cursor.checked_add(take as u32).ok_or(())?;
    }
    Ok(())
}

pub fn apply_pending_update(root: &SdRoot) -> UpdateOutcome {
    let staged = match try_apply(root) {
        Ok(staged) => staged,
        Err(UpdateError::NoTrigger) => return UpdateOutcome::Idle,
        Err(e) => {
            esp_println::println!("s3-ota: update failed: {:?}", e);
            if matches!(e, UpdateError::Invalid(_) | UpdateError::ForeignImage) {
                remove_trigger(root);
            }
            return UpdateOutcome::Idle;
        }
    };

    if staged.action.consumes_trigger() && !remove_trigger(root) {
        esp_println::println!(
            "s3-ota: WARNING trigger removal failed; aborting otadata switch to prevent loop"
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
            "s3-ota: update pending in slot {}; bouncing to slot {} anchor",
            UPDATE_SLOT,
            ANCHOR_SLOT
        );
    } else {
        esp_println::println!("s3-ota: update applied; resetting");
    }
    UpdateOutcome::Acted(staged.action)
}

fn remove_trigger(root: &SdRoot) -> bool {
    let shelf = match upload_store::library::open_library_root(root) {
        Ok(shelf) => shelf,
        Err(error) => {
            esp_println::println!(
                "s3-ota: shelf open failed before reclaim; not removing trigger ({:?})",
                error
            );
            return false;
        }
    };
    if let Err(error) = upload_store::reclaim::recover(root, shelf.as_ref()) {
        esp_println::println!(
            "s3-ota: reclaim unfinished; not removing trigger ({:?})",
            error
        );
        return false;
    }
    upload_store::reclaim::reclaim_entry(
        root,
        shelf.as_ref(),
        upload_store::reclaim::Place::Root,
        TRIGGER_FILE,
    )
    .is_ok()
}

fn select_slot(layout: &ota::OtaLayout, dest_slot: u32) -> bool {
    let mut flash = match flash_storage() {
        Some(f) => f,
        None => return false,
    };
    let (s0, s1) = match read_otadata(&mut flash, layout.otadata.offset) {
        Ok(s) => s,
        Err(e) => {
            esp_println::println!("s3-ota: read otadata err={:?}", e);
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
        esp_println::println!("s3-ota: write otadata err={:?}", e);
        return false;
    }
    esp_println::println!(
        "s3-ota: otadata sector {} -> seq {} (slot {})",
        switch.target_sector,
        switch.entry.ota_seq,
        dest_slot
    );
    true
}

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
    esp_println::println!("s3-ota: {} found ({} bytes)", TRIGGER_FILE, len);

    let mut flash = flash_storage().ok_or(UpdateError::Flash)?;
    let layout = read_ota_layout(&mut flash)?;
    let dest_partition = layout.slots[UPDATE_SLOT as usize];
    let (s0, s1) = read_otadata(&mut flash, layout.otadata.offset)?;

    let requested = ota::active_app_slot(&s0, &s1, OTA_COUNT).unwrap_or(ANCHOR_SLOT);
    let running = crate::mmu::running_slot(&layout);
    match running {
        Some(running) if running != requested => esp_println::println!(
            "s3-ota: otadata requests slot {} but slot {} is running; trusting MMU",
            requested,
            running
        ),
        None => esp_println::println!("s3-ota: running slot unknown; refusing to write"),
        Some(_) => {}
    }

    ota::validate_image_for_chip(
        &mut SdFile(&file),
        len,
        Some(dest_partition.size as usize),
        crate::board::CHIP_ID,
    )
    .map_err(|err| match err {
        ImageError::Read => UpdateError::ReadFile,
        other => UpdateError::Invalid(other),
    })?;

    let mut name = [0u8; APP_DESC_PROJECT_NAME_LEN];
    file.seek_from_start(APP_DESC_PROJECT_NAME_OFFSET)
        .map_err(|_| UpdateError::ReadFile)?;
    read_file_exact(&file, &mut name).map_err(|_| UpdateError::ReadFile)?;
    if !ota::staged_image_is_installable(&name, crate::board::BOARD_IDENTITY.as_bytes()) {
        esp_println::println!(
            "s3-ota: foreign image '{}' rejected by '{}'",
            core::str::from_utf8(ota::project_name(&name)).unwrap_or("<non-utf8>"),
            crate::board::BOARD_IDENTITY
        );
        return Err(UpdateError::ForeignImage);
    }
    file.seek_from_start(0).map_err(|_| UpdateError::ReadFile)?;

    let anchor_usable = anchor_holds_our_firmware(&mut flash, &layout);
    let action = ota::plan_update_action(running, requested, anchor_usable);
    match action {
        UpdateAction::WriteUpdateSlot => {
            esp_println::println!(
                "s3-ota: writing slot {} at {:#x} ({} bytes)",
                UPDATE_SLOT,
                dest_partition.offset,
                len
            );
            write_image(&mut flash, dest_partition.offset, &file, len)?;
        }
        UpdateAction::BounceToAnchor => {}
        UpdateAction::NoUsableAnchor | UpdateAction::RunningSlotUnknown => {
            esp_println::println!(
                "s3-ota: {:?} running={:?} requested={} anchor_usable={}",
                action,
                running,
                requested,
                anchor_usable
            );
        }
    }

    Ok(Staged { action, layout })
}

fn anchor_holds_our_firmware(flash: &mut S3Flash, layout: &ota::OtaLayout) -> bool {
    let anchor = layout.slots[ANCHOR_SLOT as usize];

    let mut magic = [0u8; 4];
    if read_flash_bytes(flash, anchor.offset, &mut magic).is_err() || magic[0] != ota::IMAGE_MAGIC {
        esp_println::println!("s3-ota: slot {} holds no valid image", ANCHOR_SLOT);
        return false;
    }

    let mut name = [0u8; APP_DESC_PROJECT_NAME_LEN];
    if let Err(()) = read_flash_bytes(
        flash,
        anchor.offset + APP_DESC_PROJECT_NAME_OFFSET,
        &mut name,
    ) {
        esp_println::println!("s3-ota: failed to read slot {} descriptor", ANCHOR_SLOT);
        return false;
    }
    if !ota::anchor_can_apply_update(&name, crate::board::BOARD_IDENTITY.as_bytes()) {
        esp_println::println!(
            "s3-ota: slot {} holds another firmware identity",
            ANCHOR_SLOT
        );
        return false;
    }

    let mut src = FlashImage {
        flash,
        offset: anchor.offset,
    };
    if let Err(e) =
        ota::validate_flash_image_for_chip(&mut src, anchor.size as usize, crate::board::CHIP_ID)
    {
        esp_println::println!("s3-ota: slot {} image invalid: {:?}", ANCHOR_SLOT, e);
        return false;
    }
    true
}

#[cfg(feature = "ota-selftest")]
pub fn run_selftest() -> bool {
    let mut flash = match flash_storage() {
        Some(f) => f,
        None => return false,
    };
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
        esp_println::println!("s3-selftest: already running slot {}; done", active);
        return false;
    }

    let anchor = layout.slots[ANCHOR_SLOT as usize];
    let update = layout.slots[UPDATE_SLOT as usize];
    let copy_len = anchor.size.min(update.size) & !(SECTOR as u32 - 1);
    if copy_len == 0 {
        return false;
    }
    let mut src_img = FlashImage {
        flash: &mut flash,
        offset: anchor.offset,
    };
    if ota::validate_flash_image_for_chip(&mut src_img, copy_len as usize, crate::board::CHIP_ID)
        .is_err()
    {
        return false;
    }
    if crate::mmu::running_slot(&layout) != Some(ANCHOR_SLOT) {
        return false;
    }

    let dst = update.offset;
    let src = anchor.offset;
    // SAFETY: Running from slot 0 (checked above); `dst..dst+copy_len` lies
    // wholly inside inactive slot 1 (`UPDATE_SLOT`).
    #[allow(unsafe_code)]
    let erased = unsafe { flash.erase(dst, dst + copy_len) };
    if erased.is_err() {
        return false;
    }

    let mut buf = [0u8; WRITE_CHUNK_BYTES];
    let mut words = [0xFFFF_FFFFu32; WRITE_CHUNK_WORDS];
    let mut off = 0u32;
    while off < copy_len {
        if read_flash_bytes(&mut flash, src + off, &mut buf).is_err() {
            return false;
        }
        bytes_to_words_padded(&buf, &mut words);
        // SAFETY: `dst + off` is inside inactive slot 1 (`UPDATE_SLOT`) and was erased above.
        #[allow(unsafe_code)]
        let wrote = unsafe { flash.write(dst + off, &words) };
        if wrote.is_err() {
            return false;
        }
        off += WRITE_CHUNK_BYTES as u32;
    }

    let (s0, s1) = match read_otadata(&mut flash, layout.otadata.offset) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let switch = ota::plan_switch(&s0, &s1, UPDATE_SLOT, OTA_COUNT);
    write_select_entry(
        &mut flash,
        layout.otadata.offset,
        switch.target_sector,
        &switch.entry,
    )
    .is_ok()
}

pub fn recover_to_slot0() -> bool {
    let mut flash = match flash_storage() {
        Some(f) => f,
        None => return false,
    };
    let layout = match read_ota_layout(&mut flash) {
        Ok(layout) => layout,
        Err(_) => return false,
    };
    let (s0, s1) = match read_otadata(&mut flash, layout.otadata.offset) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let active = ota::active_app_slot(&s0, &s1, OTA_COUNT);
    let mut head = [0u8; 4];
    let anchor_bootable = read_flash_bytes(
        &mut flash,
        layout.slots[ANCHOR_SLOT as usize].offset,
        &mut head,
    )
    .is_ok()
        && head[0] == ota::IMAGE_MAGIC;

    if !ota::plan_recovery_switch(active, anchor_bootable) {
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
        esp_println::println!("s3-recovery: otadata write failed");
        return false;
    }
    esp_println::println!(
        "s3-recovery: combo held; otadata sector {} -> seq {} (slot {})",
        switch.target_sector,
        switch.entry.ota_seq,
        ANCHOR_SLOT
    );
    true
}

pub fn mark_running_slot_valid() {
    let mut flash = match flash_storage() {
        Some(f) => f,
        None => return,
    };
    let layout = match read_ota_layout(&mut flash) {
        Ok(layout) => layout,
        Err(_) => return,
    };
    let (s0, s1) = match read_otadata(&mut flash, layout.otadata.offset) {
        Ok(v) => v,
        Err(_) => return,
    };

    let running = crate::mmu::running_slot(&layout);
    let requested = ota::active_app_slot(&s0, &s1, OTA_COUNT);
    esp_println::println!(
        "s3-ota: otadata requests slot {:?}, executing slot {:?}",
        requested,
        running
    );
    if !ota::may_mark_running_slot_valid(running, requested) {
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
        esp_println::println!("s3-ota: mark-valid failed");
        return;
    }
    esp_println::println!(
        "s3-ota: marked slot {} valid (seq {})",
        (valid.entry.ota_seq - 1) % OTA_COUNT,
        valid.entry.ota_seq
    );
}

#[allow(unsafe_code)]
fn flash_storage() -> Option<S3Flash> {
    // SAFETY: OTA update/recovery runs at boot while Core 0 owns flash and
    // before any other flash peripheral handle or radio activity exists.
    Flash::new(
        unsafe { esp_hal::peripherals::FLASH::steal() },
        FlashConfig::default(),
    )
    .ok()
}

fn read_ota_layout(flash: &mut S3Flash) -> Result<ota::OtaLayout, UpdateError> {
    let mut table = [0u8; PARTITION_TABLE_LEN];
    read_flash_bytes(flash, PARTITION_TABLE_OFFSET, &mut table).map_err(|()| UpdateError::Flash)?;
    ota::parse_ota_layout(&table, flash.capacity() as u32).map_err(UpdateError::PartitionTable)
}

fn read_otadata(
    flash: &mut S3Flash,
    otadata_offset: u32,
) -> Result<([u8; SELECT_ENTRY_LEN], [u8; SELECT_ENTRY_LEN]), UpdateError> {
    let mut s0 = [0u8; SELECT_ENTRY_LEN];
    let mut s1 = [0u8; SELECT_ENTRY_LEN];
    read_flash_bytes(flash, otadata_offset, &mut s0).map_err(|()| UpdateError::Flash)?;
    read_flash_bytes(flash, otadata_offset + OTADATA_SECTOR_STRIDE, &mut s1)
        .map_err(|()| UpdateError::Flash)?;
    Ok((s0, s1))
}

fn bytes_to_words_padded(bytes: &[u8], out: &mut [u32]) {
    out.fill(0xFFFF_FFFF);
    let mut i = 0;
    while i * 4 < bytes.len() && i < out.len() {
        let base = i * 4;
        let b0 = bytes[base];
        let b1 = bytes.get(base + 1).copied().unwrap_or(0xFF);
        let b2 = bytes.get(base + 2).copied().unwrap_or(0xFF);
        let b3 = bytes.get(base + 3).copied().unwrap_or(0xFF);
        out[i] = u32::from_le_bytes([b0, b1, b2, b3]);
        i += 1;
    }
}

fn write_image<D: BlockDevice, T: TimeSource, const MD: usize, const MF: usize, const MV: usize>(
    flash: &mut S3Flash,
    dest_offset: u32,
    file: &File<'_, D, T, MD, MF, MV>,
    len: usize,
) -> Result<(), UpdateError> {
    let erase_len = ((len as u32) + SECTOR as u32 - 1) & !(SECTOR as u32 - 1);
    // SAFETY: Caller only calls `write_image` when `mmu::running_slot` proved we
    // are NOT executing from `UPDATE_SLOT` (`dest_offset`..`dest_offset + erase_len`).
    #[allow(unsafe_code)]
    unsafe {
        flash
            .erase(dest_offset, dest_offset + erase_len)
            .map_err(|_| UpdateError::Flash)?;
    }

    let mut buf = [0u8; WRITE_CHUNK_BYTES];
    let mut words = [0xFFFF_FFFFu32; WRITE_CHUNK_WORDS];
    let mut written: u32 = 0;
    while (written as usize) < len {
        let want = core::cmp::min(WRITE_CHUNK_BYTES, len - written as usize);
        read_file_exact(file, &mut buf[..want]).map_err(|_| UpdateError::ReadFile)?;
        let word_count = want.div_ceil(4);
        bytes_to_words_padded(&buf[..want], &mut words[..word_count]);
        // SAFETY: `dest_offset + written` lies in inactive `UPDATE_SLOT`, erased above.
        #[allow(unsafe_code)]
        unsafe {
            flash
                .write(dest_offset + written, &words[..word_count])
                .map_err(|_| UpdateError::Flash)?;
        }
        written += want as u32;
    }
    Ok(())
}

fn write_select_entry(
    flash: &mut S3Flash,
    otadata_offset: u32,
    sector: usize,
    entry: &SelectEntry,
) -> Result<(), UpdateError> {
    let offset = otadata_offset + sector as u32 * OTADATA_SECTOR_STRIDE;
    let entry_bytes = entry.to_bytes();
    let mut words = [0xFFFF_FFFFu32; SELECT_ENTRY_LEN / 4];
    bytes_to_words_padded(&entry_bytes, &mut words);
    // SAFETY: `otadata` is a non-executable data partition; never mapped for
    // code fetch or immutable `.rodata` access.
    #[allow(unsafe_code)]
    unsafe {
        flash
            .erase(offset, offset + OTADATA_SECTOR_STRIDE)
            .map_err(|_| UpdateError::Flash)?;
        flash
            .write(offset, &words)
            .map_err(|_| UpdateError::Flash)?;
    }
    Ok(())
}
