//! Native 1-bit SDMMC storage session on ESP32-S3 (`SDHOST` slot 0).
//!
//! Pins: `CLK=GPIO41`, `CMD=GPIO42`, `D0=GPIO40`, active-low power enable `GPIO5`.
//! Unlike C3, the S3 display (`SPI2`) and SD card (`SDHOST`) sit on separate
//! hardware buses, so progress painting during long catalog scans never needs
//! to yield or reconfigure the SD clock.

use core::cell::RefCell;

use app_core::{DisplayCommand, PowerEvent, SleepTrigger};
use embassy_futures::select::{select, Either};
use embassy_time::{block_for, Duration, Instant, Timer};
use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Directory, VolumeIdx, VolumeManager,
};
use esp_hal::dma::aligned::{DmaAlignedMut, InternalMemory};
use esp_hal::gpio::Output;
use esp_hal::rng::Rng;
use esp_hal::sdmmc::{BlockingError, BusWidth, CommandFlags, ResponseLen, Slot};
use esp_hal::Blocking;
use fw_common::upload::{UploadBegin, UploadChunk};
use fw_common::{
    DISPLAY_COMMANDS, POWER_EVENTS, UPLOAD_BEGINS, UPLOAD_CHUNKS, UPLOAD_RESULTS, UPLOAD_RETURNS,
    UPLOAD_STOPPED, UPLOAD_STOP_REQUESTS,
};
use static_cell::ConstStaticCell;
use storage::card::StaticTime;
use storage::progress::{ProgressSink, Silent};

pub(crate) use storage::sd_stats;

/// 1-bit SDMMC data frequency (Default Speed SDR, up to 25 MHz per SD spec).
const SD_IDENT_HZ: u32 = 400_000;
const SD_DATA_HZ: u32 = 20_000_000;

/// Multi-block DMA bounce buffer size (8 * 512 = 4 KiB in internal DRAM).
const SD_BOUNCE_BLOCKS: usize = 8;
const SD_BOUNCE_BYTES: usize = SD_BOUNCE_BLOCKS * Block::LEN;

static SD_BOUNCE: ConstStaticCell<InternalMemory<[u8; SD_BOUNCE_BYTES]>> =
    ConstStaticCell::new(InternalMemory::new([0u8; SD_BOUNCE_BYTES]));

/// Block-level transaction counter wrapper matching `fw_c3::sd_session`.
pub(crate) struct CountingDevice<B>(B);

impl<B: BlockDevice> BlockDevice for CountingDevice<B> {
    type Error = B::Error;

    fn read(&self, blocks: &mut [Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        sd_stats::bump(&sd_stats::READ_CALLS, 1);
        sd_stats::bump(&sd_stats::READ_BLOCKS, blocks.len() as u32);
        self.0.read(blocks, start_block_idx)
    }

    fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        sd_stats::bump(&sd_stats::WRITE_CALLS, 1);
        sd_stats::bump(&sd_stats::WRITE_BLOCKS, blocks.len() as u32);
        self.0.write(blocks, start_block_idx)
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        self.0.num_blocks()
    }
}

pub(crate) type SdRoot<'a> = Directory<'a, SdCardDevice<'a>, StaticTime, 8, 8, 1>;
pub(crate) type SdCardDevice<'a> = CountingDevice<SdmmcCard<'a>>;
pub use storage::card::SessionError as SdSessionError;

pub(crate) trait SessionPainter {
    fn paint(&mut self, percent: u8);
}

#[derive(Clone, Copy, Debug)]
struct CardInfo {
    rca: u16,
    is_sdhc: bool,
    num_blocks: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SdmmcError {
    NotInitialized,
    Command(BlockingError),
    DmaBuf,
    OutOfRange,
    WriteTimeout,
}

impl core::fmt::Display for SdmmcError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotInitialized => f.write_str("sdmmc not initialized"),
            Self::Command(err) => write!(f, "sdmmc command error: {:?}", err),
            Self::DmaBuf => f.write_str("sdmmc dma buffer error"),
            Self::OutOfRange => f.write_str("sdmmc block out of range"),
            Self::WriteTimeout => f.write_str("sdmmc write busy timeout"),
        }
    }
}

impl core::error::Error for SdmmcError {}

pub struct SdmmcBus {
    slot: Slot<'static, 0, Blocking>,
    power: Output<'static>,
    bounce: &'static mut InternalMemory<[u8; SD_BOUNCE_BYTES]>,
    card: Option<CardInfo>,
}

impl SdmmcBus {
    pub fn new(slot: Slot<'static, 0, Blocking>, power: Output<'static>) -> Self {
        Self {
            slot,
            power,
            bounce: SD_BOUNCE.take(),
            card: None,
        }
    }

    pub fn power_pin_mut(&mut self) -> &mut Output<'static> {
        self.card = None;
        &mut self.power
    }

    fn forget_card(&mut self) {
        self.card = None;
    }

    fn power_cycle(&mut self) {
        self.card = None;
        self.power.set_high();
        block_for(Duration::from_millis(10));
        self.power.set_low();
        block_for(Duration::from_millis(25));
    }

    fn wait_flags() -> CommandFlags {
        CommandFlags {
            wait_complete: true,
            stop_abort: false,
            busy: false,
        }
    }

    fn init_card(&mut self) -> Result<CardInfo, SdSessionError> {
        self.power_cycle();
        if self
            .slot
            .set_bus_low_level(BusWidth::Bit1, SD_IDENT_HZ)
            .is_err()
        {
            self.power.set_high();
            return Err(SdSessionError::CardInit);
        }
        if self.slot.send_init_sequence().is_err() {
            self.power.set_high();
            return Err(SdSessionError::CardInit);
        }

        // CMD0: GO_IDLE_STATE
        if self
            .slot
            .command_blocking(0, 0, ResponseLen::None, false, CommandFlags::default())
            .is_err()
        {
            self.power.set_high();
            return Err(SdSessionError::CardInit);
        }
        block_for(Duration::from_millis(2));

        // CMD8: SEND_IF_COND (0x1AA = 2.7-3.6V + check pattern 0xAA)
        let sd_v2 = match self.slot.command_blocking(
            8,
            0x0000_01AA,
            ResponseLen::Short,
            true,
            Self::wait_flags(),
        ) {
            Ok(resp) => (resp[0] & 0x0FFF) == 0x01AA,
            Err(_) => false,
        };

        // ACMD41: SD_SEND_OP_COND
        let acmd41_arg = if sd_v2 { 0x4030_0000 } else { 0x0030_0000 };
        let deadline = Instant::now() + Duration::from_millis(1500);
        let is_sdhc = loop {
            if self
                .slot
                .command_blocking(55, 0, ResponseLen::Short, true, Self::wait_flags())
                .is_err()
            {
                if Instant::now() >= deadline {
                    self.power.set_high();
                    return Err(SdSessionError::CardInit);
                }
                block_for(Duration::from_millis(10));
                continue;
            }
            if let Ok(resp) = self.slot.command_blocking(
                41,
                acmd41_arg,
                ResponseLen::Short,
                false,
                Self::wait_flags(),
            ) {
                let ocr = resp[0];
                if (ocr & (1 << 31)) != 0 {
                    break sd_v2 && (ocr & (1 << 30)) != 0;
                }
            }
            if Instant::now() >= deadline {
                self.power.set_high();
                return Err(SdSessionError::CardInit);
            }
            block_for(Duration::from_millis(10));
        };

        // CMD2: ALL_SEND_CID
        if self
            .slot
            .command_blocking(2, 0, ResponseLen::Long, false, Self::wait_flags())
            .is_err()
        {
            self.power.set_high();
            return Err(SdSessionError::CardInit);
        }

        // CMD3: SEND_RELATIVE_ADDR
        let mut rca = 0u16;
        for _ in 0..4 {
            let Ok(resp) =
                self.slot
                    .command_blocking(3, 0, ResponseLen::Short, true, Self::wait_flags())
            else {
                self.power.set_high();
                return Err(SdSessionError::CardInit);
            };
            rca = (resp[0] >> 16) as u16;
            if rca != 0 {
                break;
            }
        }
        if rca == 0 {
            self.power.set_high();
            return Err(SdSessionError::CardInit);
        }

        // CMD9: SEND_CSD (addressed via RCA << 16 in standby state)
        let Ok(csd) = self.slot.command_blocking(
            9,
            u32::from(rca) << 16,
            ResponseLen::Long,
            false,
            Self::wait_flags(),
        ) else {
            self.power.set_high();
            return Err(SdSessionError::CardInit);
        };
        let Some(num_blocks) = parse_csd_blocks(csd) else {
            self.power.set_high();
            return Err(SdSessionError::CardInit);
        };

        // CMD7: SELECT_CARD -> transfer state
        if self
            .slot
            .command_blocking(
                7,
                u32::from(rca) << 16,
                ResponseLen::Short,
                true,
                CommandFlags {
                    wait_complete: true,
                    stop_abort: false,
                    busy: true,
                },
            )
            .is_err()
        {
            self.power.set_high();
            return Err(SdSessionError::CardInit);
        }

        // Force 1-bit bus width (ACMD6 arg 0) matching GPIO40 D0-only wiring.
        let _ = self.slot.command_blocking(
            55,
            u32::from(rca) << 16,
            ResponseLen::Short,
            true,
            Self::wait_flags(),
        );
        let _ = self
            .slot
            .command_blocking(6, 0, ResponseLen::Short, true, Self::wait_flags());

        if !is_sdhc
            && self
                .slot
                .command_blocking(
                    16,
                    Block::LEN_U32,
                    ResponseLen::Short,
                    true,
                    Self::wait_flags(),
                )
                .is_err()
        {
            self.power.set_high();
            return Err(SdSessionError::CardInit);
        }

        if self
            .slot
            .set_bus_low_level(BusWidth::Bit1, SD_DATA_HZ)
            .is_err()
        {
            self.power.set_high();
            return Err(SdSessionError::CardInit);
        }

        let info = CardInfo {
            rca,
            is_sdhc,
            num_blocks,
        };
        self.card = Some(info);
        esp_println::println!(
            "s3-sdmmc: card init ok sdhc={} blocks={} hz={}",
            is_sdhc,
            num_blocks,
            SD_DATA_HZ,
        );
        Ok(info)
    }

    fn ensure_card(&mut self) -> Result<CardInfo, SdSessionError> {
        if let Some(info) = self.card {
            let _ = self.slot.set_bus_low_level(BusWidth::Bit1, SD_DATA_HZ);
            Ok(info)
        } else {
            self.init_card()
        }
    }

    fn wait_transfer_ready(&mut self, rca: u16) -> Result<(), SdmmcError> {
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut last_r1: Option<u32> = None;
        loop {
            match self.slot.command_blocking(
                13,
                u32::from(rca) << 16,
                ResponseLen::Short,
                true,
                Self::wait_flags(),
            ) {
                Ok(resp) => {
                    let r1 = resp[0];
                    last_r1 = Some(r1);
                    let ready_for_data = (r1 & (1 << 8)) != 0;
                    let state = (r1 >> 9) & 0xF;
                    if ready_for_data && state == 4 {
                        return Ok(());
                    }
                }
                Err(err) => {
                    esp_println::println!("s3-sdmmc: cmd13 err={:?} last_r1={:?}", err, last_r1);
                    return Err(SdmmcError::Command(err));
                }
            }
            if Instant::now() >= deadline {
                esp_println::println!("s3-sdmmc: write timeout r1={:?}", last_r1);
                return Err(SdmmcError::WriteTimeout);
            }
            block_for(Duration::from_micros(200));
        }
    }

    fn read_blocks(&mut self, blocks: &mut [Block], start: BlockIdx) -> Result<(), SdmmcError> {
        let info = self.card.ok_or(SdmmcError::NotInitialized)?;
        let start_idx = start.0;
        let end_idx = start_idx
            .checked_add(blocks.len() as u32)
            .ok_or(SdmmcError::OutOfRange)?;
        if end_idx > info.num_blocks {
            return Err(SdmmcError::OutOfRange);
        }
        if blocks.is_empty() {
            return Ok(());
        }

        let mut cur_idx = start_idx;
        for chunk in blocks.chunks_mut(SD_BOUNCE_BLOCKS) {
            let count = chunk.len() as u32;
            let bytes = chunk.len() * Block::LEN;
            let arg = if info.is_sdhc {
                cur_idx
            } else {
                cur_idx
                    .checked_mul(Block::LEN_U32)
                    .ok_or(SdmmcError::OutOfRange)?
            };
            let cmd = if count == 1 { 17 } else { 18 };
            let dma_buf = DmaAlignedMut::new(&mut self.bounce.get_mut().into_inner()[..bytes])
                .map_err(|_| SdmmcError::DmaBuf)?;
            if let Err(err) =
                self.slot
                    .read_blocks_blocking(cmd, arg, dma_buf, Block::LEN as u16, count)
            {
                let _ = self.slot.command_blocking(
                    12,
                    0,
                    ResponseLen::Short,
                    false,
                    CommandFlags {
                        wait_complete: false,
                        stop_abort: true,
                        busy: false,
                    },
                );
                let retry_buf =
                    DmaAlignedMut::new(&mut self.bounce.get_mut().into_inner()[..bytes])
                        .map_err(|_| SdmmcError::DmaBuf)?;
                if let Err(retry_err) =
                    self.slot
                        .read_blocks_blocking(cmd, arg, retry_buf, Block::LEN as u16, count)
                {
                    esp_println::println!(
                        "s3-sdmmc: read cmd={} blk={} cnt={} err={:?} retry={:?}",
                        cmd,
                        cur_idx,
                        count,
                        err,
                        retry_err
                    );
                    self.card = None;
                    return Err(SdmmcError::Command(retry_err));
                }
            }
            let raw = self.bounce.get_mut().into_inner();
            for (i, dst) in chunk.iter_mut().enumerate() {
                let offset = i * Block::LEN;
                dst.contents
                    .copy_from_slice(&raw[offset..offset + Block::LEN]);
            }
            cur_idx += count;
        }
        Ok(())
    }

    fn write_blocks(&mut self, blocks: &[Block], start: BlockIdx) -> Result<(), SdmmcError> {
        let info = self.card.ok_or(SdmmcError::NotInitialized)?;
        let start_idx = start.0;
        let end_idx = start_idx
            .checked_add(blocks.len() as u32)
            .ok_or(SdmmcError::OutOfRange)?;
        if end_idx > info.num_blocks {
            return Err(SdmmcError::OutOfRange);
        }
        if blocks.is_empty() {
            return Ok(());
        }

        let mut cur_idx = start_idx;
        for chunk in blocks.chunks(SD_BOUNCE_BLOCKS) {
            let count = chunk.len() as u32;
            let bytes = chunk.len() * Block::LEN;
            {
                let raw = self.bounce.get_mut().into_inner();
                for (i, src) in chunk.iter().enumerate() {
                    let offset = i * Block::LEN;
                    raw[offset..offset + Block::LEN].copy_from_slice(&src.contents);
                }
            }
            let arg = if info.is_sdhc {
                cur_idx
            } else {
                cur_idx
                    .checked_mul(Block::LEN_U32)
                    .ok_or(SdmmcError::OutOfRange)?
            };
            let cmd = if count == 1 { 24 } else { 25 };
            let dma_buf = DmaAlignedMut::new(&mut self.bounce.get_mut().into_inner()[..bytes])
                .map_err(|_| SdmmcError::DmaBuf)?;
            if let Err(err) =
                self.slot
                    .write_blocks_blocking(cmd, arg, dma_buf, Block::LEN as u16, count)
            {
                esp_println::println!(
                    "s3-sdmmc: write cmd={} blk={} cnt={} err={:?}",
                    cmd,
                    cur_idx,
                    count,
                    err
                );
                self.card = None;
                return Err(SdmmcError::Command(err));
            }
            self.wait_transfer_ready(info.rca)?;
            cur_idx += count;
        }
        Ok(())
    }
}

fn parse_csd_blocks(words: [u32; 4]) -> Option<u32> {
    let csd_structure = (words[3] >> 30) & 0x3;
    match csd_structure {
        // CSD v2.0 (SDHC/SDXC)
        1 => {
            let c_size = ((words[2] & 0x3F) << 16) | ((words[1] >> 16) & 0xFFFF);
            c_size.checked_add(1)?.checked_mul(1024)
        }
        // CSD v1.0 (SDSC)
        0 => {
            let read_bl_len = (words[2] >> 16) & 0xF;
            let c_size = ((words[2] & 0x3FF) << 2) | ((words[1] >> 30) & 0x3);
            let c_size_mult = (words[1] >> 15) & 0x7;
            let shift = read_bl_len + c_size_mult + 2;
            if !(9..32).contains(&shift) {
                return None;
            }
            (c_size + 1).checked_shl(shift - 9)
        }
        _ => None,
    }
}

pub(crate) struct SdmmcCard<'a>(RefCell<&'a mut SdmmcBus>);

impl BlockDevice for SdmmcCard<'_> {
    type Error = SdmmcError;

    fn read(&self, blocks: &mut [Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        self.0.borrow_mut().read_blocks(blocks, start_block_idx)
    }

    fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        self.0.borrow_mut().write_blocks(blocks, start_block_idx)
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        let bus = self.0.borrow();
        let info = bus.card.ok_or(SdmmcError::NotInitialized)?;
        Ok(BlockCount(info.num_blocks))
    }
}

struct BusProgress<'p> {
    painter: &'p mut dyn SessionPainter,
}

impl ProgressSink for BusProgress<'_> {
    fn now_ms(&mut self) -> u64 {
        Instant::now().as_millis()
    }

    fn report(&mut self, percent: u8) {
        self.painter.paint(percent);
    }
}

pub(crate) struct FwCard<'a> {
    bus: &'a mut SdmmcBus,
    painter: Option<&'a mut dyn SessionPainter>,
}

pub(crate) fn card(bus: &mut SdmmcBus) -> FwCard<'_> {
    FwCard { bus, painter: None }
}

pub(crate) fn card_reporting<'a>(
    bus: &'a mut SdmmcBus,
    painter: &'a mut dyn SessionPainter,
) -> FwCard<'a> {
    FwCard {
        bus,
        painter: Some(painter),
    }
}

impl storage::card::Card for FwCard<'_> {
    type Device<'a>
        = SdCardDevice<'a>
    where
        Self: 'a;

    fn with_root<R>(
        &mut self,
        f: impl for<'a> FnOnce(&SdRoot<'a>) -> R,
    ) -> Result<R, SdSessionError> {
        with_root(self.bus, f)
    }

    fn with_root_reporting<R>(
        &mut self,
        f: impl for<'a> FnOnce(&SdRoot<'a>, &mut dyn ProgressSink) -> R,
    ) -> Result<R, SdSessionError> {
        with_session(self.bus, reborrow(&mut self.painter), f)
    }
}

fn reborrow<'s>(
    painter: &'s mut Option<&mut dyn SessionPainter>,
) -> Option<&'s mut dyn SessionPainter> {
    match painter {
        Some(p) => Some(&mut **p),
        None => None,
    }
}

pub(crate) fn with_root<R>(
    bus: &mut SdmmcBus,
    f: impl for<'a> FnOnce(&SdRoot<'a>) -> R,
) -> Result<R, SdSessionError> {
    with_session(bus, None, |root, _| f(root))
}

pub(crate) fn with_session<R, F>(
    bus: &mut SdmmcBus,
    mut painter: Option<&mut dyn SessionPainter>,
    f: F,
) -> Result<R, SdSessionError>
where
    F: for<'a> FnOnce(&SdRoot<'a>, &mut dyn ProgressSink) -> R,
{
    let mut pending = Some(f);
    let was_warm = bus.card.is_some();
    let mut result = run_sd_session(bus, reborrow(&mut painter), &mut pending);
    if was_warm && pending.is_some() && result.is_err() {
        esp_println::println!("s3-sdmmc: warm reuse failed, cold retry");
        bus.forget_card();
        result = run_sd_session(bus, painter, &mut pending);
    }
    result
}

fn run_sd_session<R, F>(
    bus: &mut SdmmcBus,
    painter: Option<&mut dyn SessionPainter>,
    f: &mut Option<F>,
) -> Result<R, SdSessionError>
where
    F: for<'a> FnOnce(&SdRoot<'a>, &mut dyn ProgressSink) -> R,
{
    bus.ensure_card()?;
    let card_dev = CountingDevice(SdmmcCard(RefCell::new(bus)));
    let volume_mgr: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(card_dev, StaticTime, 5000);
    let result = match volume_mgr.open_volume(VolumeIdx(0)) {
        Ok(volume) => {
            let raw_volume = volume.to_raw_volume();
            if let Ok(raw_root) = volume_mgr.open_root_dir(raw_volume) {
                let root = Directory::new(raw_root, &volume_mgr);
                let callback = f.take().expect("sd session callback present");
                let value = match painter {
                    Some(painter) => callback(&root, &mut BusProgress { painter }),
                    None => callback(&root, &mut Silent),
                };
                drop(root);
                let _ = volume_mgr.close_volume(raw_volume);
                Ok(value)
            } else {
                let _ = volume_mgr.close_volume(raw_volume);
                Err(SdSessionError::Root)
            }
        }
        Err(_) => Err(SdSessionError::Volume),
    };
    result
}

pub(crate) async fn upload_session(
    bus: &mut SdmmcBus,
    frontlight: &mut crate::frontlight::Frontlight,
) {
    fw_common::upload::set_session_state(fw_common::upload::SessionState::Active);
    fw_common::UPLOAD_SESSION_READY.signal(());
    fw_common::UPLOAD_INTERRUPTS.try_take();
    esp_println::println!("s3-upload: session enter");

    if bus.ensure_card().is_err() {
        esp_println::println!("s3-upload: card init failed");
        let exit = refuse_uploads_until_exit(frontlight).await;
        finish_upload_session(exit).await;
        return;
    }

    let card_dev = CountingDevice(SdmmcCard(RefCell::new(bus)));
    let volume_mgr: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(card_dev, StaticTime, 5000);
    let Ok(volume) = volume_mgr.open_volume(VolumeIdx(0)) else {
        esp_println::println!("s3-upload: volume open failed");
        let exit = refuse_uploads_until_exit(frontlight).await;
        drop(volume_mgr);
        finish_upload_session(exit).await;
        return;
    };
    let raw_volume = volume.to_raw_volume();
    let Ok(raw_root) = volume_mgr.open_root_dir(raw_volume) else {
        esp_println::println!("s3-upload: root open failed");
        let exit = refuse_uploads_until_exit(frontlight).await;
        let _ = volume_mgr.close_volume(raw_volume);
        drop(volume_mgr);
        finish_upload_session(exit).await;
        return;
    };
    let root = Directory::new(raw_root, &volume_mgr);
    let catalog_cleared = upload_store::clear_cache_file(&root, proto::cache::CATALOG_FILE);
    if !catalog_cleared {
        esp_println::println!("s3-upload: catalog snapshot could not be cleared");
    }

    let exit = {
        let opened = match upload_store::library::open_library_root(&root) {
            Ok(shelf) => Some(shelf),
            Err(err) => {
                esp_println::println!("s3-upload: shelf open err={:?}", err);
                None
            }
        };
        match opened {
            None => refuse_uploads_until_exit(frontlight).await,
            Some(existing) => match upload_store::reclaim::recover(&root, existing.as_ref()) {
                Err(err) => {
                    esp_println::println!("s3-upload: reclaim unfinished ({:?})", err);
                    refuse_uploads_until_exit(frontlight).await
                }
                Ok(_) => {
                    let books = match existing {
                        Some(books) => Some(books),
                        None => match root.make_dir_in_dir(upload_store::SHELF_DIR) {
                            Ok(()) => upload_store::library::open_library_root(&root)
                                .ok()
                                .flatten(),
                            Err(_) => None,
                        },
                    };
                    match books {
                        Some(books) => {
                            serve_uploads(&root, &books, catalog_cleared, frontlight).await
                        }
                        None => refuse_uploads_until_exit(frontlight).await,
                    }
                }
            },
        }
    };
    drop(root);
    let _ = volume_mgr.close_volume(raw_volume);
    drop(volume_mgr);
    finish_upload_session(exit).await;
}

async fn serve_uploads<
    D: BlockDevice,
    T: embedded_sdmmc::TimeSource,
    const MD: usize,
    const MF: usize,
    const MV: usize,
>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    books: &Directory<'_, D, T, MD, MF, MV>,
    catalog_cleared: bool,
    frontlight: &mut crate::frontlight::Frontlight,
) -> UploadSessionExit {
    if !catalog_cleared {
        esp_println::println!("s3-upload: catalog not cleared");
    } else if upload_store::reclaim::recover(root, Some(books)).is_err() {
        esp_println::println!("s3-upload: reclaim unfinished");
    } else {
        let outcome = upload_store::install::recover_installs(root, books);
        if outcome.complete {
            let _ =
                crate::hw_sha::with_sha256(|engine| upload_store::replace::recover(root, engine));
        }
    }

    loop {
        let begin = match select(
            UPLOAD_BEGINS.receive(),
            select(UPLOAD_STOP_REQUESTS.receive(), DISPLAY_COMMANDS.receive()),
        )
        .await
        {
            Either::First(begin) => begin,
            Either::Second(Either::First(())) => return UploadSessionExit::Wireless,
            Either::Second(Either::Second(DisplayCommand::Sleep {
                generation,
                trigger,
            })) => {
                if trigger == SleepTrigger::Idle {
                    if fw_common::sleep_is_cancelled(generation).is_some() {
                        continue;
                    }
                    if fw_common::upload::UPLOAD_IN_FLIGHT.load(portable_atomic::Ordering::SeqCst) {
                        let _ = POWER_EVENTS.try_send(PowerEvent::BrowserActivity);
                        if wait_for_sleep_cancellation(generation).await {
                            continue;
                        }
                    }
                }
                fw_common::upload::set_session_state(fw_common::upload::SessionState::Closing);
                fw_common::UPLOAD_SESSION_READY.signal(());
                return UploadSessionExit::Sleep {
                    generation,
                    trigger,
                };
            }
            Either::Second(Either::Second(DisplayCommand::SetFrontlight {
                brightness,
                warmth,
            })) => {
                frontlight.set(brightness, warmth);
                continue;
            }
            Either::Second(Either::Second(
                DisplayCommand::Render(_) | DisplayCommand::StageNext { .. },
            )) => continue,
        };

        let settled = catalog_cleared && storage_settled(root, books);
        let ok = if !settled {
            if !begin.delete && drain_until_end().await.is_err() {
                return UploadSessionExit::Wireless;
            }
            false
        } else if begin.delete {
            let place = if begin.in_books {
                upload_store::reclaim::Place::Books
            } else {
                upload_store::reclaim::Place::Root
            };
            let removed =
                upload_store::reclaim::reclaim_entry(root, Some(books), place, begin.name.as_str())
                    .is_ok();
            if removed && begin.in_books {
                upload_store::delete_upload_sidecars(root, begin.name.as_str());
            }
            removed
        } else {
            match write_one_book(root, books, &begin).await {
                UploadWrite::Finished(landed) => landed.is_some(),
                UploadWrite::Interrupted(exit) => return exit,
            }
        };
        UPLOAD_RESULTS.send(ok).await;
    }
}

fn storage_settled<
    D: BlockDevice,
    T: embedded_sdmmc::TimeSource,
    const MD: usize,
    const MF: usize,
    const MV: usize,
>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    books: &Directory<'_, D, T, MD, MF, MV>,
) -> bool {
    if upload_store::reclaim::recover(root, Some(books)).is_err() {
        return false;
    }
    if !journal_is_clear(root) {
        upload_store::install::recover_installs(root, books);
        if !journal_is_clear(root) {
            return false;
        }
    }
    matches!(
        crate::hw_sha::with_sha256(|engine| upload_store::replace::recover(root, engine)),
        Ok(upload_store::replace::Recovery::Nothing | upload_store::replace::Recovery::Settled(_))
    )
}

fn journal_is_clear<
    D: BlockDevice,
    T: embedded_sdmmc::TimeSource,
    const MD: usize,
    const MF: usize,
    const MV: usize,
>(
    root: &Directory<'_, D, T, MD, MF, MV>,
) -> bool {
    matches!(
        upload_store::install::read_intent(root),
        Ok(upload_store::install::IntentState::Absent
            | upload_store::install::IntentState::Truncated)
    )
}

async fn finish_upload_session(exit: UploadSessionExit) {
    fw_common::upload::set_session_state(fw_common::upload::SessionState::Inactive);
    fw_common::UPLOAD_SESSION_READY.signal(());
    match exit {
        UploadSessionExit::Sleep {
            generation,
            trigger,
        } => {
            if UPLOAD_STOP_REQUESTS.try_receive().is_ok() {
                UPLOAD_STOPPED.send(()).await;
            }
            fw_common::UPLOAD_INTERRUPTS.signal(());
            if trigger == SleepTrigger::Idle && fw_common::sleep_is_cancelled(generation).is_some()
            {
                return;
            }
            DISPLAY_COMMANDS
                .send(DisplayCommand::Sleep {
                    generation,
                    trigger,
                })
                .await;
        }
        UploadSessionExit::Wireless => UPLOAD_STOPPED.send(()).await,
    }
}

#[derive(Clone, Copy)]
enum UploadSessionExit {
    Sleep {
        generation: u32,
        trigger: SleepTrigger,
    },
    Wireless,
}

enum UploadWrite {
    Finished(Option<upload_store::install::Landed>),
    Interrupted(UploadSessionExit),
}

async fn write_one_book<
    D: BlockDevice,
    T: embedded_sdmmc::TimeSource,
    const MD: usize,
    const MF: usize,
    const MV: usize,
>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    books: &Directory<'_, D, T, MD, MF, MV>,
    begin: &UploadBegin,
) -> UploadWrite {
    let mut staged = match upload_store::install::StagedUpload::begin(
        root,
        books,
        begin.long_name.as_str(),
        begin.legacy.clone(),
    ) {
        Ok(staged) => staged,
        Err(err) => {
            esp_println::println!("s3-upload: stage err={:?}", err);
            return match drain_until_end().await {
                Ok(()) => UploadWrite::Finished(None),
                Err(exit) => UploadWrite::Interrupted(exit),
            };
        }
    };
    let mut failed = false;
    let mut aborted = false;
    loop {
        let chunk = match next_upload_chunk().await {
            Ok(chunk) => chunk,
            Err(exit) => {
                staged.abandon(root);
                return UploadWrite::Interrupted(exit);
            }
        };
        if !failed && !chunk.abort {
            if let Some(buffer) = &chunk.buffer {
                if staged
                    .write(&buffer[..chunk.len.min(buffer.len())])
                    .is_err()
                {
                    failed = true;
                }
            }
        }
        let last = chunk.last;
        aborted |= chunk.abort;
        recycle(chunk).await;
        if last || aborted {
            break;
        }
    }
    if failed || aborted {
        staged.abandon(root);
        return UploadWrite::Finished(None);
    }
    let rng = Rng::new();
    match staged.install(root, books, &mut || rng.random()) {
        Ok(landed) => UploadWrite::Finished(landed),
        Err(err) => {
            esp_println::println!("s3-upload: install '{}' err={:?}", begin.long_name, err);
            UploadWrite::Finished(None)
        }
    }
}

async fn drain_until_end() -> Result<(), UploadSessionExit> {
    loop {
        let chunk = next_upload_chunk().await?;
        let done = chunk.last || chunk.abort;
        recycle(chunk).await;
        if done {
            return Ok(());
        }
    }
}

async fn wait_for_sleep_cancellation(generation: u32) -> bool {
    if fw_common::sleep_is_cancelled(generation).is_some() {
        return true;
    }
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        match select(fw_common::DISPLAY_RESUME.wait(), Timer::at(deadline)).await {
            Either::First(()) => {
                if fw_common::sleep_is_cancelled(generation).is_some() {
                    return true;
                }
            }
            Either::Second(_) => return false,
        }
    }
}

async fn next_upload_chunk() -> Result<UploadChunk, UploadSessionExit> {
    loop {
        match select(
            UPLOAD_CHUNKS.receive(),
            select(UPLOAD_STOP_REQUESTS.receive(), DISPLAY_COMMANDS.receive()),
        )
        .await
        {
            Either::First(chunk) => return Ok(chunk),
            Either::Second(Either::First(())) => return Err(UploadSessionExit::Wireless),
            Either::Second(Either::Second(DisplayCommand::Sleep {
                generation,
                trigger,
            })) => {
                if trigger == SleepTrigger::Idle {
                    if fw_common::sleep_is_cancelled(generation).is_some() {
                        continue;
                    }
                    let _ = POWER_EVENTS.try_send(PowerEvent::BrowserActivity);
                    if wait_for_sleep_cancellation(generation).await {
                        continue;
                    }
                }
                fw_common::upload::set_session_state(fw_common::upload::SessionState::Closing);
                fw_common::UPLOAD_SESSION_READY.signal(());
                return Err(UploadSessionExit::Sleep {
                    generation,
                    trigger,
                });
            }
            Either::Second(Either::Second(
                DisplayCommand::Render(_)
                | DisplayCommand::StageNext { .. }
                | DisplayCommand::SetFrontlight { .. },
            )) => {}
        }
    }
}

async fn recycle(chunk: UploadChunk) {
    if let Some(buffer) = chunk.buffer {
        UPLOAD_RETURNS.send(buffer).await;
    }
}

async fn refuse_uploads_until_exit(
    frontlight: &mut crate::frontlight::Frontlight,
) -> UploadSessionExit {
    loop {
        match select(
            UPLOAD_BEGINS.receive(),
            select(UPLOAD_STOP_REQUESTS.receive(), DISPLAY_COMMANDS.receive()),
        )
        .await
        {
            Either::First(begin) => {
                if !begin.delete {
                    if let Err(exit) = drain_until_end().await {
                        return exit;
                    }
                }
                UPLOAD_RESULTS.send(false).await;
            }
            Either::Second(Either::First(())) => return UploadSessionExit::Wireless,
            Either::Second(Either::Second(DisplayCommand::Sleep {
                generation,
                trigger,
            })) => {
                if trigger == SleepTrigger::Idle
                    && fw_common::sleep_is_cancelled(generation).is_some()
                {
                    continue;
                }
                fw_common::upload::set_session_state(fw_common::upload::SessionState::Closing);
                fw_common::UPLOAD_SESSION_READY.signal(());
                return UploadSessionExit::Sleep {
                    generation,
                    trigger,
                };
            }
            Either::Second(Either::Second(DisplayCommand::SetFrontlight {
                brightness,
                warmth,
            })) => {
                frontlight.set(brightness, warmth);
            }
            Either::Second(Either::Second(
                DisplayCommand::Render(_) | DisplayCommand::StageNext { .. },
            )) => {}
        }
    }
}
