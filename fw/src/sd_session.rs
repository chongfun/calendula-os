use crate::display_flush::Epd;
use crate::source_owner::{
    refusal_for_begin, refusal_for_delete, refusal_for_publish, refusal_for_recovery,
    refusal_for_upload_error, Refusal, SourceCaps, SourceCommit, SourceEvent, SourceOp,
    SourceOwnerState, SourceUploadOp,
};

use crate::upload::{UploadBegin, UploadChunk, UploadName};
use crate::{
    DISPLAY_COMMANDS, SOURCE_EVENTS, SOURCE_OPS, UPLOAD_BEGINS, UPLOAD_CHUNKS, UPLOAD_RESULTS,
    UPLOAD_RETURNS, UPLOAD_STOPPED, UPLOAD_STOP_REQUESTS,
};
use app_core::DisplayCommand;
use core::sync::atomic::{AtomicU8, Ordering};
use embassy_futures::select::{select, Either};
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
use embedded_hal::spi::{Operation, SpiBus as BlockingSpiBus, SpiDevice};
use embedded_sdmmc::sdcard::CardType;
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use embedded_sdmmc::{Directory, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use esp_hal::gpio::Output;
use esp_hal::spi::master::{Config as SpiConfig, SpiDmaBus};
use esp_hal::time::Rate;
use esp_hal::Async;
use source_store::list::listed_book_at;
use source_store::ops::{
    delete_book, ensure_epoch_headroom, load_catalog, DeleteOutcome, DeleteRequest,
    IdempotencyStore,
};
use source_store::receipts::MAX_RECEIPTS_PER_EPOCH;
use source_store::recover::{recover_book, RecoveryOutcome, RecoveryRequest};
use source_store::select::MAX_SOURCE_SLOTS;
use source_store::upload::{
    abort_upload, begin_upload, upload_chunk as source_upload_chunk, FreshIdentity,
    UploadBeginOutcome, UploadResult,
};

/// SD SPI-mode identification must run at 100-400 kHz; data transfer is
/// specced to 25 MHz. The shared bus otherwise runs at the active panel's
/// clock (historically the X4's 40 MHz — out of SD spec entirely, and what
/// the read-retry machinery in the EPUB path was quietly absorbing; both
/// panels now run their rated 20 MHz).
const SD_IDENT_FREQ_KHZ: u32 = 400;
const SD_DATA_FREQ_MHZ: u32 = 25;
/// Restore frequency after SD access: the active panel's SPI clock. This
/// MUST stay per-panel even though both currently rate 20 MHz — the
/// UC8253 (X3) can't decode above ~20 MHz, so restoring an out-of-spec
/// X4-style clock leaves the panel deaf to every subsequent command
/// (init included, since the boot catalog read precedes it).
const DISPLAY_FREQ_HZ: u32 = display::epd::SPI_HZ;

/// Block-level SD transaction counters for `bench:` telemetry. Single-writer
/// (all SD traffic runs on the storage/display task), so plain load+store is
/// enough on this RV32IMC core — no RMW atomics needed. Read via `snapshot`
/// deltas around a workload; never reset, so concurrent snapshots stay
/// comparable.
pub(crate) mod sd_stats {
    use core::sync::atomic::{AtomicU32, Ordering};

    pub(crate) static READ_CALLS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static READ_BLOCKS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static WRITE_CALLS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static WRITE_BLOCKS: AtomicU32 = AtomicU32::new(0);

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct Snapshot {
        pub(crate) read_calls: u32,
        pub(crate) read_blocks: u32,
        pub(crate) write_calls: u32,
        pub(crate) write_blocks: u32,
    }

    pub(crate) fn snapshot() -> Snapshot {
        Snapshot {
            read_calls: READ_CALLS.load(Ordering::Relaxed),
            read_blocks: READ_BLOCKS.load(Ordering::Relaxed),
            write_calls: WRITE_CALLS.load(Ordering::Relaxed),
            write_blocks: WRITE_BLOCKS.load(Ordering::Relaxed),
        }
    }

    impl Snapshot {
        pub(crate) fn since(self, start: Snapshot) -> Snapshot {
            Snapshot {
                read_calls: self.read_calls.wrapping_sub(start.read_calls),
                read_blocks: self.read_blocks.wrapping_sub(start.read_blocks),
                write_calls: self.write_calls.wrapping_sub(start.write_calls),
                write_blocks: self.write_blocks.wrapping_sub(start.write_blocks),
            }
        }
    }

    pub(crate) fn bump(counter: &AtomicU32, amount: u32) {
        let value = counter.load(Ordering::Relaxed).wrapping_add(amount);
        counter.store(value, Ordering::Relaxed);
    }
}

/// Counts physical block transactions on their way to the SD card, so bench
/// telemetry can report exact CMD17/CMD24-level traffic per workload.
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

pub(crate) struct StaticTime;

impl TimeSource for StaticTime {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 56,
            zero_indexed_month: 4,
            zero_indexed_day: 19,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SdDelay;

impl DelayNs for SdDelay {
    fn delay_ns(&mut self, ns: u32) {
        sd_spi_pace(ns.saturating_div(100).max(1));
    }
}

pub(crate) struct SdSpiDevice<'a, SPI, CS> {
    pub(crate) spi: &'a mut SPI,
    pub(crate) cs: &'a mut CS,
    pub(crate) delay: SdDelay,
}

/// Also sizes the shared bus's RX DMA buffer in main.rs: SD traffic is the
/// only read path on SPI2 (the EPD is write-only), and every SD operation
/// bounces through one of these chunks. Sized to one SD block so a 512-B
/// data read/write is a single DMA transaction instead of eight; the
/// extra ~448 B of DRAM comes out of the stack headroom build.rs asserts.
pub(crate) const SD_SPI_CHUNK_BYTES: usize = 512;

#[repr(align(4))]
struct AlignedSdChunk([u8; SD_SPI_CHUNK_BYTES]);

/// The one bounce chunk all SD SPI operations share. Static rather than a
/// local so the 512-B block never lands on the reader's deep-call stack
/// (the 27 KB link-time budget in build.rs is nearly spent); as .bss it is
/// instead counted against the stack-headroom ASSERT. Sound for the same
/// reason `sd_stats` uses plain load/store: every SD transaction runs on
/// the storage/display task, and the borrows below never overlap.
struct SdBounce {
    chunk: core::cell::UnsafeCell<AlignedSdChunk>,
    /// A shared static cannot rule out overlapping borrows at compile
    /// time, so this flag turns any overlap (including reentrancy from
    /// the callback) into a panic instead of aliased `&mut`s.
    busy: portable_atomic::AtomicBool,
}
// Safety: only the single SD-owning task touches the chunk (see above),
// and `with_sd_bounce` panics on overlapping access.
#[allow(unsafe_code)]
unsafe impl Sync for SdBounce {}
static SD_BOUNCE: SdBounce = SdBounce {
    chunk: core::cell::UnsafeCell::new(AlignedSdChunk([0xFF; SD_SPI_CHUNK_BYTES])),
    busy: portable_atomic::AtomicBool::new(false),
};

/// Runs `f` with exclusive access to the shared bounce chunk, refilled
/// with the 0xFF idle pattern SD cards expect on MOSI during reads. The
/// closure signature keeps the borrow from escaping.
#[allow(unsafe_code)]
fn with_sd_bounce<R>(f: impl FnOnce(&mut AlignedSdChunk) -> R) -> R {
    use portable_atomic::Ordering;
    if SD_BOUNCE.busy.swap(true, Ordering::Acquire) {
        panic!("sd bounce chunk borrowed twice");
    }
    // Safety: the busy flag above makes this the only live borrow, and
    // it cannot outlive `f`.
    let chunk = unsafe { &mut *SD_BOUNCE.chunk.get() };
    chunk.0.fill(0xFF);
    let result = f(chunk);
    SD_BOUNCE.busy.store(false, Ordering::Release);
    result
}

fn sd_spi_pace(iterations: u32) {
    for _ in 0..iterations {
        core::hint::spin_loop();
    }
}

impl<SPI, CS> embedded_hal::spi::ErrorType for SdSpiDevice<'_, SPI, CS>
where
    SPI: embedded_hal::spi::ErrorType,
{
    type Error = SPI::Error;
}

impl<SPI, CS> SpiDevice for SdSpiDevice<'_, SPI, CS>
where
    SPI: BlockingSpiBus<u8>,
    CS: OutputPin,
{
    fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Self::Error> {
        let _ = self.cs.set_low();
        let mut result = Ok(());

        for operation in operations {
            result = match operation {
                Operation::Read(buffer) => self.read_with_sd_clocks(buffer),
                Operation::Write(buffer) => self.write_chunked(buffer),
                Operation::Transfer(read, write) => self.transfer_chunked(read, write),
                Operation::TransferInPlace(buffer) => self.transfer_in_place_chunked(buffer),
                Operation::DelayNs(ns) => {
                    self.delay.delay_ns(*ns);
                    Ok(())
                }
            };

            if result.is_err() {
                break;
            }
        }

        let _ = self.spi.flush();
        let _ = self.cs.set_high();
        result
    }
}

impl<SPI, CS> SdSpiDevice<'_, SPI, CS>
where
    SPI: BlockingSpiBus<u8>,
{
    fn read_with_sd_clocks(&mut self, buffer: &mut [u8]) -> Result<(), SPI::Error> {
        for chunk in buffer.chunks_mut(SD_SPI_CHUNK_BYTES) {
            with_sd_bounce(|bounce| {
                self.spi.transfer_in_place(&mut bounce.0[..chunk.len()])?;
                chunk.copy_from_slice(&bounce.0[..chunk.len()]);
                Ok(())
            })?;
        }
        Ok(())
    }

    fn write_chunked(&mut self, buffer: &[u8]) -> Result<(), SPI::Error> {
        for chunk in buffer.chunks(SD_SPI_CHUNK_BYTES) {
            with_sd_bounce(|bounce| {
                bounce.0[..chunk.len()].copy_from_slice(chunk);
                self.spi.transfer_in_place(&mut bounce.0[..chunk.len()])
            })?;
        }
        Ok(())
    }

    fn transfer_chunked(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), SPI::Error> {
        let common = read.len().min(write.len());
        let (read_common, read_tail) = read.split_at_mut(common);
        let (write_common, write_tail) = write.split_at(common);

        for (read_chunk, write_chunk) in read_common
            .chunks_mut(SD_SPI_CHUNK_BYTES)
            .zip(write_common.chunks(SD_SPI_CHUNK_BYTES))
        {
            with_sd_bounce(|bounce| {
                bounce.0[..write_chunk.len()].copy_from_slice(write_chunk);
                self.spi
                    .transfer_in_place(&mut bounce.0[..write_chunk.len()])?;
                read_chunk.copy_from_slice(&bounce.0[..read_chunk.len()]);
                Ok(())
            })?;
        }
        if !read_tail.is_empty() {
            self.read_with_sd_clocks(read_tail)?;
        }
        if !write_tail.is_empty() {
            self.write_chunked(write_tail)?;
        }
        Ok(())
    }

    fn transfer_in_place_chunked(&mut self, buffer: &mut [u8]) -> Result<(), SPI::Error> {
        for chunk in buffer.chunks_mut(SD_SPI_CHUNK_BYTES) {
            with_sd_bounce(|bounce| {
                bounce.0[..chunk.len()].copy_from_slice(chunk);
                self.spi.transfer_in_place(&mut bounce.0[..chunk.len()])?;
                chunk.copy_from_slice(&bounce.0[..chunk.len()]);
                Ok(())
            })?;
        }
        Ok(())
    }
}

type SdSpi<'a> = SdSpiDevice<'a, SpiDmaBus<'static, Async>, Output<'static>>;

type SdCardDevice<'a> = CountingDevice<SdCard<SdSpi<'a>, SdDelay>>;
pub(crate) type SdRoot<'a> = Directory<'a, SdCardDevice<'a>, StaticTime, 8, 8, 1>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SdSessionError {
    CardInit,
    Volume,
    Root,
}

/// Each per-page-turn SD access shares the SPI bus with the panel, so the
/// card is re-acquired every call. A *cold* acquire runs the SD SPI init
/// handshake (CMD0/ACMD41), specced to take up to hundreds of milliseconds
/// — far more than the section read itself. While the device stays awake
/// the card never loses power or its SPI-mode init, so after the first
/// successful acquire we remember its `CardType` and skip the handshake on
/// later sessions via `mark_card_as_init`. Deep sleep resets the chip and
/// clears this static, forcing one honest cold acquire on wake. A warm
/// acquire that can't open the volume falls back to a cold one, so a wrong
/// guess is at worst as slow as before — never a failed read.
const WARM_CARD_NONE: u8 = 0;
static WARM_CARD_CODE: AtomicU8 = AtomicU8::new(WARM_CARD_NONE);

fn remembered_card_type() -> Option<CardType> {
    match WARM_CARD_CODE.load(Ordering::Relaxed) {
        1 => Some(CardType::SD1),
        2 => Some(CardType::SD2),
        3 => Some(CardType::SDHC),
        _ => None,
    }
}

fn remember_card_type(card_type: CardType) {
    let code = match card_type {
        CardType::SD1 => 1,
        CardType::SD2 => 2,
        CardType::SDHC => 3,
    };
    WARM_CARD_CODE.store(code, Ordering::Relaxed);
}

fn forget_card_warmth() {
    WARM_CARD_CODE.store(WARM_CARD_NONE, Ordering::Relaxed);
}

/// Kept out of line: the VolumeManager/SdCard session state is multi-KB
/// and must not be pooled into every caller's frame.
#[inline(never)]
pub(crate) fn with_root<R>(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    f: impl for<'a> FnOnce(&SdRoot<'a>) -> R,
) -> Result<R, SdSessionError> {
    epd.deselect_display();
    sd_cs.set_high();
    esp_println::println!(
        "sd: session enter t_ms={}",
        embassy_time::Instant::now().as_millis()
    );

    // The callback is consumed only once the root dir is open, so a warm
    // acquire that bails before then leaves it intact for the cold retry.
    let mut pending = Some(f);
    let mut result = Err(SdSessionError::CardInit);
    if let Some(card_type) = remembered_card_type() {
        result = run_sd_session(epd, sd_cs, Some(card_type), &mut pending);
        if result.is_err() {
            esp_println::println!("sd: warm reuse failed, cold retry");
            forget_card_warmth();
        }
    }
    if pending.is_some() {
        result = run_sd_session(epd, sd_cs, None, &mut pending);
    }

    esp_println::println!("sd: session exit");
    sd_cs.set_high();
    let _ = epd
        .spi_mut()
        .apply_config(&SpiConfig::default().with_frequency(Rate::from_hz(DISPLAY_FREQ_HZ)));
    result
}

/// One SD acquire + open-root + callback. `assume_init` skips the init
/// handshake for a card known to still be warm. The callback is taken from
/// `f` only after the root dir opens, so any earlier failure returns with
/// `f` untouched for the caller to retry cold.
#[allow(unsafe_code)]
#[inline(never)]
fn run_sd_session<R, F>(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    assume_init: Option<CardType>,
    f: &mut Option<F>,
) -> Result<R, SdSessionError>
where
    F: for<'a> FnOnce(&SdRoot<'a>) -> R,
{
    // Identification phase: 400 kHz with at least 74 wake clocks while no
    // chip select is asserted, per the SD spec and embedded-sdmmc's docs.
    // Harmless for an already-initialised card (it ignores the bus while
    // deselected), so the warm path runs it too rather than special-casing.
    {
        let spi = epd.spi_mut();
        let _ = spi
            .apply_config(&SpiConfig::default().with_frequency(Rate::from_khz(SD_IDENT_FREQ_KHZ)));
        let mut wake = [0xFFu8; 10];
        let _ = BlockingSpiBus::transfer_in_place(spi, &mut wake);
        let _ = BlockingSpiBus::flush(spi);
    }

    let spi = SdSpiDevice {
        spi: epd.spi_mut(),
        cs: sd_cs,
        delay: SdDelay,
    };
    let card = SdCard::new(spi, SdDelay);
    match assume_init {
        Some(card_type) => {
            // SAFETY: the card has stayed powered and in SPI mode since the
            // cold acquire that recorded this type; reads below skip
            // re-init because the type is already set. A stale guess
            // surfaces as an open_volume failure and a cold retry, so this
            // never reads with the wrong addressing mode silently.
            unsafe { card.mark_card_as_init(card_type) };
        }
        None => {
            esp_println::println!("sd: card probe");
            if card.num_bytes().is_err() {
                return Err(SdSessionError::CardInit);
            }
            esp_println::println!("sd: card ready");
            if let Some(card_type) = card.get_card_type() {
                remember_card_type(card_type);
            }
        }
    }

    // Card acquired: switch to the in-spec data rate for the rest of the
    // session.
    card.spi(|device| {
        let _ = device
            .spi
            .apply_config(&SpiConfig::default().with_frequency(Rate::from_mhz(SD_DATA_FREQ_MHZ)));
    });
    let volume_mgr: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(CountingDevice(card), StaticTime, 5000);
    // Bind the outcome so the open_volume scrutinee temporary (which borrows
    // volume_mgr) is dropped at the `;` while volume_mgr is still alive,
    // rather than racing volume_mgr's own drop at the function tail.
    let result = match volume_mgr.open_volume(VolumeIdx(0)) {
        Ok(volume) => {
            esp_println::println!("sd: volume open");
            let raw_volume = volume.to_raw_volume();
            if let Ok(raw_root) = volume_mgr.open_root_dir(raw_volume) {
                esp_println::println!("sd: root open");
                let root = Directory::new(raw_root, &volume_mgr);
                let callback = f.take().expect("sd session callback present");
                let value = callback(&root);
                esp_println::println!("sd: root callback done");
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

/// The upload phase: one SD session held open for the rest of the sync
/// session, writing browser-sent books to /BOOKS as they stream in.
/// Returns only after every open FAT handle has been dropped and the
/// display SPI clock has been restored. Sleep is re-queued for the normal
/// display shutdown path; wireless Exit receives an explicit stopped
/// acknowledgement.
pub(crate) async fn upload_session(epd: &mut Epd, sd_cs: &mut Output<'static>) {
    crate::upload::UPLOAD_SESSION_ACTIVE.store(true, portable_atomic::Ordering::SeqCst);
    epd.deselect_display();
    sd_cs.set_high();
    esp_println::println!("upload: session enter");
    // The M0S owner state lives in the loaned session heap, carved out by
    // the wifi task right after donation (contiguity: see
    // `claim_session_owner_early`) and picked up here, before the first
    // operation. The image arrives pristine: this mount's proofs start
    // empty and the catalog loads lazily on the first logical-book
    // operation.
    let source_owner = crate::source_owner::take_session_owner();

    {
        let spi = epd.spi_mut();
        let _ = spi
            .apply_config(&SpiConfig::default().with_frequency(Rate::from_khz(SD_IDENT_FREQ_KHZ)));
        let mut wake = [0xFFu8; 10];
        let _ = BlockingSpiBus::transfer_in_place(spi, &mut wake);
        let _ = BlockingSpiBus::flush(spi);
    }

    let spi = SdSpiDevice {
        spi: epd.spi_mut(),
        cs: sd_cs,
        delay: SdDelay,
    };
    let card = SdCard::new(spi, SdDelay);
    if card.num_bytes().is_err() {
        esp_println::println!("upload: card init failed");
        let exit = refuse_uploads_until_exit().await;
        // `card` has no Drop; its borrow of the EPD bus ends at its last
        // use above, so the SPI clock restore below is free to re-borrow.
        finish_upload_session(epd, exit).await;
        return;
    }
    card.spi(|device| {
        let _ = device
            .spi
            .apply_config(&SpiConfig::default().with_frequency(Rate::from_mhz(SD_DATA_FREQ_MHZ)));
    });
    let volume_mgr: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(CountingDevice(card), StaticTime, 5000);
    let Ok(volume) = volume_mgr.open_volume(VolumeIdx(0)) else {
        esp_println::println!("upload: volume open failed");
        let exit = refuse_uploads_until_exit().await;
        drop(volume_mgr);
        finish_upload_session(epd, exit).await;
        return;
    };
    let raw_volume = volume.to_raw_volume();
    let Ok(raw_root) = volume_mgr.open_root_dir(raw_volume) else {
        esp_println::println!("upload: root open failed");
        let exit = refuse_uploads_until_exit().await;
        let _ = volume_mgr.close_volume(raw_volume);
        drop(volume_mgr);
        finish_upload_session(epd, exit).await;
        return;
    };
    let root = Directory::new(raw_root, &volume_mgr);
    // New books invalidate the catalog snapshot: the next boot's cache
    // load misses and runs a full scan, which is how uploads surface.
    if let Ok(xteink) = root.open_dir("XTEINK") {
        let _ = upload_store::remove_file_reclaiming_clusters(&xteink, "CATALOG.BIN");
        esp_println::println!("upload: catalog snapshot invalidated");
    }
    // The namespace directories (/BOOKS, XTEINK/SRC) are opened per
    // request inside the serve loop — created only on a proven NotFound —
    // so a transient lookup failure costs one refused request, retryably,
    // not the namespace for the rest of the mounted session. Every handle
    // the loop opens dies inside it, so by the close-and-acknowledge tail
    // below only `root` remains: UPLOAD_STOPPED must not be sent (nor
    // Sleep re-queued) while the volume is mounted.
    let exit = serve_uploads(&root, source_owner).await;
    drop(root);
    let _ = volume_mgr.close_volume(raw_volume);
    drop(volume_mgr);
    finish_upload_session(epd, exit).await;
}

/// Open `name` under `parent`, creating it only when the lookup itself
/// proved `NotFound`. Every other open error propagates: falling back to
/// creation on, say, a transient card-read error would misread the
/// `DirAlreadyExists` that `make_dir_in_dir`'s own existence recheck then
/// returns as "no such directory". `DirAlreadyExists` from the create is
/// still answered by opening what is there — the lookup and the recheck
/// disagreeing means the first read lied.
fn open_or_create_dir<
    'p,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    parent: &'p Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
) -> Result<Directory<'p, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>, embedded_sdmmc::Error<D::Error>>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match parent.open_dir(name) {
        Err(embedded_sdmmc::Error::NotFound) => {}
        outcome => return outcome,
    }
    match parent.make_dir_in_dir(name) {
        Ok(()) | Err(embedded_sdmmc::Error::DirAlreadyExists) => parent.open_dir(name),
        Err(error) => Err(error),
    }
}

/// The serving loop of a mounted session: writes and deletes books as
/// begins arrive, until Sleep or wireless Exit ends the session. The
/// /BOOKS shelf and the XTEINK/SRC namespace are each opened per request:
/// a directory that cannot be opened fails only that request — legacy
/// begins answer failure (draining any streaming body), logical-book
/// operations refuse `storage_unavailable`, which stays honestly
/// retryable because the next request re-attempts the open — and a
/// partially usable card still serves whatever namespace works.
async fn serve_uploads<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    mut source_owner: Option<&mut SourceOwnerState>,
) -> UploadSessionExit
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    loop {
        let begin = match select(
            select(UPLOAD_BEGINS.receive(), SOURCE_OPS.receive()),
            select(UPLOAD_STOP_REQUESTS.receive(), DISPLAY_COMMANDS.receive()),
        )
        .await
        {
            Either::First(Either::First(begin)) => begin,
            Either::First(Either::Second(op)) => {
                // Opened fresh for this operation and dead again after
                // it. The child directory borrows the parent handle, so
                // both bindings live for the operation.
                let xteink = open_or_create_dir(root, "XTEINK").ok();
                let src = xteink.as_ref().and_then(|xteink| {
                    open_or_create_dir(xteink, source_store::layout::SOURCE_DIR).ok()
                });
                if src.is_none() {
                    esp_println::println!("source: SRC namespace unavailable");
                }
                match serve_source_op(op, src.as_ref(), source_owner.as_deref_mut()).await {
                    Ok(()) => continue,
                    Err(exit) => return exit,
                }
            }
            Either::Second(Either::First(())) => return UploadSessionExit::Wireless,
            Either::Second(Either::Second(DisplayCommand::Sleep { generation })) => {
                return UploadSessionExit::Sleep { generation }
            }
            // The wireless screen is already painted; renders queued during
            // the upload phase describe views this session never shows.
            Either::Second(Either::Second(DisplayCommand::Render(_))) => continue,
        };
        let ok = if begin.delete {
            if begin.in_books {
                // A delete begin is never followed by chunks, so a failed
                // BOOKS lookup answers failure directly.
                match open_or_create_dir(root, "BOOKS") {
                    Ok(books) => {
                        let removed = upload_store::remove_file_reclaiming_clusters(
                            &books,
                            begin.name.as_str(),
                        ) == upload_store::RemoveStatus::Removed;
                        if removed {
                            upload_store::delete_upload_sidecars(root, begin.name.as_str());
                        }
                        removed
                    }
                    Err(_) => false,
                }
            } else {
                upload_store::remove_file_reclaiming_clusters(root, begin.name.as_str())
                    == upload_store::RemoveStatus::Removed
            }
        } else {
            match open_or_create_dir(root, "BOOKS") {
                Ok(books) => match write_one_book(root, &books, &begin).await {
                    UploadWrite::Finished(name) => name.is_some(),
                    UploadWrite::Interrupted(exit) => return exit,
                },
                // The body is already streaming; consume it and fail the
                // request, exactly as a refused begin does.
                Err(_) => {
                    esp_println::println!("upload: BOOKS unavailable");
                    match drain_until_end().await {
                        Ok(()) => false,
                        Err(exit) => return exit,
                    }
                }
            }
        };
        esp_println::println!(
            "upload: '{}' {} ok={}",
            begin.name,
            if begin.delete { "delete" } else { "write" },
            ok
        );
        UPLOAD_RESULTS.send(ok).await;
    }
}

/// Restore the display's SPI clock, clear the session-active flag, and hand
/// control to whichever consumer forced the exit: Sleep re-queues the
/// display command for the normal shutdown path (progress flush, sleep
/// frame, panel power-down); wireless Exit gets its stopped
/// acknowledgement so the reset proceeds over a closed filesystem.
async fn finish_upload_session(epd: &mut Epd, exit: UploadSessionExit) {
    let _ = epd
        .spi_mut()
        .apply_config(&SpiConfig::default().with_frequency(Rate::from_hz(DISPLAY_FREQ_HZ)));
    crate::upload::UPLOAD_SESSION_ACTIVE.store(false, portable_atomic::Ordering::SeqCst);
    match exit {
        // The re-queued Sleep keeps its generation so the power task's
        // handshake still recognizes the eventual acknowledgement as its own.
        UploadSessionExit::Sleep { generation } => {
            // The book server may be stranded mid-request (blocked on a
            // returned buffer or a result that will never come). Every
            // loaned buffer is back in the channels by now — the writer
            // recycles each chunk before receiving the next — so the
            // server can fail the request and reclaim them. Matters only
            // when sleep turns out non-terminal; a completed deep sleep
            // resets before the server is polled again.
            crate::UPLOAD_INTERRUPTS.signal(());
            // A wireless Exit racing this Sleep may have sent its stop
            // request after the writer stopped listening (the active flag
            // clears only above, so exit_after_uploads still saw a live
            // session). Answer it here — the volume is already closed —
            // or that task waits forever on an ack no one would send. No
            // await separates the flag store from this drain, and none
            // separates the exit task's flag load from its send, so on
            // this single-threaded executor a request is either drained
            // here or never sent.
            if UPLOAD_STOP_REQUESTS.try_receive().is_ok() {
                UPLOAD_STOPPED.send(()).await;
            }
            DISPLAY_COMMANDS
                .send(DisplayCommand::Sleep { generation })
                .await
        }
        UploadSessionExit::Wireless => UPLOAD_STOPPED.send(()).await,
    }
}

#[derive(Clone, Copy)]
enum UploadSessionExit {
    /// Carries the interrupting Sleep's generation for the re-queue.
    Sleep {
        generation: u32,
    },
    Wireless,
}

enum UploadWrite {
    Finished(Option<UploadName>),
    Interrupted(UploadSessionExit),
}

async fn write_one_book<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    books: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    begin: &UploadBegin,
) -> UploadWrite
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // The probe/sidecar/replace state machine lives in upload-store (where
    // the host fault-injection tests exercise it); this shell owns only the
    // chunk streaming between begin and commit/abort.
    let begun = upload_store::PendingUpload::begin(
        root,
        books,
        &begin.name,
        begin.identity_hash,
        begin.label.as_str(),
    );
    let Ok(pending) = begun else {
        return match drain_until_end().await {
            Ok(()) => UploadWrite::Finished(None),
            Err(exit) => UploadWrite::Interrupted(exit),
        };
    };
    let malformed = pending.skipped_malformed_sidecars();
    if malformed > 0 {
        esp_println::println!(
            "upload: {} malformed identity sidecar(s) treated as absent",
            malformed
        );
    }
    let mut failed = false;
    let mut aborted = false;
    loop {
        let chunk = match next_upload_chunk().await {
            Ok(chunk) => chunk,
            Err(exit) => {
                // The transaction's abort path closes the staged file and
                // reclaims its cluster chain; the original book (if this
                // was a replace) is untouched by design.
                pending.abort(root, books);
                return UploadWrite::Interrupted(exit);
            }
        };
        if !failed && !chunk.abort {
            if let Some(buffer) = &chunk.buffer {
                // One blocking whole-chunk write, on purpose. Pacing this
                // as 512-B slices with a yield between them (to keep
                // net_task fed under the theory that the 10-30 ms write
                // starves TCP) was tried and measured on hardware
                // 2026-07-11: it cost ~1 s per 3.2 MB upload and bought
                // nothing — TCP rides out the stall via buffering. Don't
                // reintroduce pacing without a timed upload A/B.
                if pending
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
        pending.abort(root, books);
        return UploadWrite::Finished(None);
    }
    // commit closes the file and retires the replaced copies only if the
    // close succeeded; a failed close discards the target and returns None.
    UploadWrite::Finished(pending.commit(root, books))
}

/// Consumes one file's worth of chunks without a file to write into.
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

/// One upload chunk, or the session-ending interrupt that arrived instead.
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
            Either::Second(Either::Second(DisplayCommand::Sleep { generation })) => {
                return Err(UploadSessionExit::Sleep { generation })
            }
            Either::Second(Either::Second(DisplayCommand::Render(_))) => {}
        }
    }
}

async fn recycle(chunk: UploadChunk) {
    if let Some(buffer) = chunk.buffer {
        UPLOAD_RETURNS.send(buffer).await;
    }
}

// ---------------------------------------------------------------------------
// M0S logical-book operations (the storage-owner side)
// ---------------------------------------------------------------------------

/// Send one event to the Wi-Fi task without deafening the session-exit
/// signals: a stalled consumer must never wedge the storage owner past a
/// Sleep or wireless Exit. Events are `Copy`, so a send abandoned by an
/// interleaved `Render` command retries with the same value.
async fn send_source_event(event: SourceEvent) -> Result<(), UploadSessionExit> {
    loop {
        match select(
            SOURCE_EVENTS.send(event),
            select(UPLOAD_STOP_REQUESTS.receive(), DISPLAY_COMMANDS.receive()),
        )
        .await
        {
            Either::First(()) => return Ok(()),
            Either::Second(Either::First(())) => return Err(UploadSessionExit::Wireless),
            Either::Second(Either::Second(DisplayCommand::Sleep { generation })) => {
                return Err(UploadSessionExit::Sleep { generation })
            }
            Either::Second(Either::Second(DisplayCommand::Render(_))) => {}
        }
    }
}

/// Load the catalog and idempotency state for this session if not yet
/// loaded. `idem: None` marks a fresh session (the caller reset it at
/// session entry); a load that failed retries here on the next operation.
fn ensure_source_ready<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    src: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &mut SourceOwnerState,
) -> Result<(), Refusal>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if owner.idem.is_none() || !owner.ws.catalog_is_valid() {
        load_catalog(src, &mut owner.ws).map_err(refusal_for_publish)?;
    }
    if owner.idem.is_none() {
        owner.idem = Some(IdempotencyStore::load(src, &mut owner.ws).map_err(refusal_for_publish)?);
    }
    Ok(())
}

/// Device-minted identity for a new generation, from the hardware RNG.
fn mint_identity() -> FreshIdentity {
    let rng = esp_hal::rng::Rng::new();
    let mut logical_book_id = [0u8; 16];
    let mut book_token = [0u8; 16];
    for chunk in logical_book_id.chunks_mut(4) {
        chunk.copy_from_slice(&rng.random().to_le_bytes());
    }
    for chunk in book_token.chunks_mut(4) {
        chunk.copy_from_slice(&rng.random().to_le_bytes());
    }
    FreshIdentity {
        logical_book_id,
        book_token,
    }
}

/// The classic-ZIP/ZIP64 gate over a persisted candidate: the real
/// implementation behind `source-store`'s `validate_container` hook.
/// Bounded stack (a 512-byte scratch plus the gate's fixed state); the
/// gate reads only zip structure, never payloads.
fn container_gate_passes<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    dir: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(file) = dir.open_file_in_dir(name, embedded_sdmmc::Mode::ReadOnly) else {
        return false;
    };
    let len = file.length();
    let mut reader = GateReadAt { file, len };
    let mut scratch = [0u8; 512];
    let verdict = proto::epub::validate_source_container(
        &mut reader,
        proto::epub::SourceContainerLimits::V1,
        &mut scratch,
    );
    let closed = reader.file.close();
    if let Err(error) = &verdict {
        esp_println::println!("source: container gate refused: {:?}", error);
    }
    verdict.is_ok() && closed.is_ok()
}

struct GateReadAt<'a, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: embedded_sdmmc::File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    len: u32,
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    proto::epub::ReadAt for GateReadAt<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    type Error = ();

    fn len(&mut self) -> Result<u32, Self::Error> {
        Ok(self.len)
    }

    fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<usize, Self::Error> {
        self.file.seek_from_start(offset).map_err(|_| ())?;
        self.file.read(out).map_err(|_| ())
    }
}

/// Execute one logical-book operation and answer on `SOURCE_EVENTS`.
/// `Err` propagates a session-ending interrupt, exactly like the legacy
/// writer; every refusal is an event, never a hang.
async fn serve_source_op<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    op: SourceOp,
    src: Option<&Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
    owner: Option<&mut SourceOwnerState>,
) -> Result<(), UploadSessionExit>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let (Some(src), Some(owner)) = (src, owner) else {
        return send_source_event(SourceEvent::Refused(Refusal::StorageUnavailable)).await;
    };
    if let Err(refusal) = ensure_source_ready(src, owner) {
        return send_source_event(SourceEvent::Refused(refusal)).await;
    }
    match op {
        SourceOp::Upload(upload) => serve_source_upload(src, owner, &upload).await,
        SourceOp::Delete(delete) => {
            let SourceOwnerState { ws, idem } = owner;
            let Some(idem) = idem.as_mut() else {
                return send_source_event(SourceEvent::Refused(Refusal::StorageUnavailable)).await;
            };
            let request = DeleteRequest {
                epoch: delete.epoch,
                nonce: delete.nonce,
                book_token: delete.book_token,
            };
            let event = match delete_book(src, idem, &request, ws) {
                DeleteOutcome::Deleted {
                    logical_book_id, ..
                } => SourceEvent::Deleted { logical_book_id },
                outcome => SourceEvent::Refused(refusal_for_delete(&outcome)),
            };
            send_source_event(event).await
        }
        SourceOp::Recover(recover) => {
            let request = RecoveryRequest {
                epoch: recover.epoch,
                nonce: recover.nonce,
                book_token: recover.book_token,
                observed_length: recover.observed_length,
                observed_sha256: recover.observed_sha256,
                display_label: recover.display_label,
            };
            // The gate validates the slot file of the book being
            // recovered; resolve it up front. An unknown token leaves the
            // gate a no-op — recovery rejects it before the gate runs.
            let slot_name =
                source_store::ops::find_authoritative_by_token(&owner.ws, &recover.book_token)
                    .and_then(|entry| source_store::layout::source_slot_name(entry.physical_slot));
            // Token collisions re-mint rather than surface: the client
            // cannot act on a collision between two random values.
            let mut outcome = RecoveryOutcome::RejectedIdentityCollision;
            for _ in 0..4 {
                let SourceOwnerState { ws, idem } = owner;
                let Some(idem) = idem.as_mut() else {
                    break;
                };
                outcome = recover_book(src, idem, &request, mint_identity().book_token, ws, || {
                    slot_name
                        .as_ref()
                        .is_some_and(|name| container_gate_passes(src, name.as_str()))
                });
                if !matches!(outcome, RecoveryOutcome::RejectedIdentityCollision) {
                    break;
                }
            }
            let event = match outcome {
                RecoveryOutcome::Recovered(result) => SourceEvent::Committed(commit_for(
                    &result,
                    recover.observed_length,
                    recover.observed_sha256,
                    recovered_label(owner, &result),
                )),
                outcome => SourceEvent::Refused(refusal_for_recovery(&outcome)),
            };
            send_source_event(event).await
        }
        SourceOp::List => {
            let mut count: u16 = 0;
            for slot in 0..MAX_SOURCE_SLOTS {
                match listed_book_at(&owner.ws, slot) {
                    Ok(Some(entry)) => {
                        send_source_event(SourceEvent::ListEntry(entry)).await?;
                        count += 1;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        return send_source_event(SourceEvent::Refused(
                            Refusal::StorageUnavailable,
                        ))
                        .await;
                    }
                }
            }
            send_source_event(SourceEvent::ListEnd { count }).await
        }
        SourceOp::Capabilities => {
            let SourceOwnerState { ws, idem } = owner;
            let Some(idem) = idem.as_mut() else {
                return send_source_event(SourceEvent::Refused(Refusal::StorageUnavailable)).await;
            };
            if ensure_epoch_headroom(src, idem, ws).is_err() {
                return send_source_event(SourceEvent::Refused(Refusal::StorageUnavailable)).await;
            }
            let state = &idem.state;
            let current = state.current_epoch_receipts() as u64;
            let caps = SourceCaps {
                idempotency_epoch: state.current_epoch,
                max_new_requests_this_epoch: (MAX_RECEIPTS_PER_EPOCH as u64)
                    .saturating_sub(current),
                retained_previous_epoch: state.receipts().len() as u64 - current,
            };
            send_source_event(SourceEvent::Capabilities(caps)).await
        }
    }
}

/// The commit event for an upload or recovery result. Length, digest, and
/// label come from the request that the transaction just proved exact.
fn commit_for(
    result: &UploadResult,
    source_length: u64,
    source_sha256: [u8; 32],
    display_label: source_store::bodies::DisplayLabel,
) -> SourceCommit {
    SourceCommit {
        logical_book_id: result.logical_book_id,
        book_token: result.book_token,
        source_generation: result.source_generation,
        source_length,
        source_sha256,
        display_label,
    }
}

/// The label a recovery actually committed: the request's replacement if
/// one was supplied, else the label already in the (reloaded) catalog.
fn recovered_label(
    owner: &SourceOwnerState,
    result: &UploadResult,
) -> source_store::bodies::DisplayLabel {
    owner
        .ws
        .entries
        .iter()
        .flatten()
        .find(|entry| entry.metadata.book_token == result.book_token)
        .map(|entry| entry.metadata.display_label)
        .unwrap_or_else(source_store::bodies::DisplayLabel::placeholder)
}

/// One M0S create/replace: begin (with collision re-mint), stream the
/// body from the shared ping-pong, finish with the container gate over
/// the persisted candidate.
async fn serve_source_upload<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    src: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &mut SourceOwnerState,
    upload: &SourceUploadOp,
) -> Result<(), UploadSessionExit>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let request = source_store::upload::UploadRequest {
        epoch: upload.epoch,
        nonce: upload.nonce,
        declared_length: upload.declared_length,
        declared_sha256: upload.declared_sha256,
        display_label: upload.display_label,
        replace_token: upload.replace_token,
    };
    let mut txn = None;
    for _ in 0..4 {
        let SourceOwnerState { ws, idem } = owner;
        let Some(idem) = idem.as_mut() else {
            return send_source_event(SourceEvent::Refused(Refusal::StorageUnavailable)).await;
        };
        match begin_upload(src, idem, &request, mint_identity(), ws) {
            UploadBeginOutcome::Started(started) => {
                txn = Some(started);
                break;
            }
            UploadBeginOutcome::Replayed(result) => {
                // The original result, re-served; the client's body is
                // never streamed (the Wi-Fi side answers on this event).
                let commit = commit_for(
                    &result,
                    upload.declared_length,
                    upload.declared_sha256,
                    upload.display_label,
                );
                return send_source_event(SourceEvent::Committed(commit)).await;
            }
            UploadBeginOutcome::RejectedIdentityCollision => continue,
            outcome => {
                return send_source_event(SourceEvent::Refused(refusal_for_begin(&outcome))).await;
            }
        }
    }
    let Some(mut txn) = txn else {
        return send_source_event(SourceEvent::Refused(Refusal::Conflict)).await;
    };
    send_source_event(SourceEvent::UploadStarted).await?;

    // Stream the body. A failed SD write keeps draining chunks so the
    // Wi-Fi side never blocks on a full channel, exactly like the legacy
    // writer.
    let mut failed = false;
    let mut aborted = false;
    loop {
        let chunk = match next_upload_chunk().await {
            Ok(chunk) => chunk,
            Err(exit) => {
                abort_upload(src, txn);
                return Err(exit);
            }
        };
        if !failed && !aborted && !chunk.abort {
            if let Some(buffer) = &chunk.buffer {
                if source_upload_chunk(src, &mut txn, &buffer[..chunk.len.min(buffer.len())])
                    .is_err()
                {
                    failed = true;
                }
            }
        }
        aborted |= chunk.abort;
        let last = chunk.last;
        recycle(chunk).await;
        if last || aborted {
            break;
        }
    }
    if aborted || failed {
        abort_upload(src, txn);
        let refusal = if aborted {
            Refusal::ClientAborted
        } else {
            Refusal::StorageIo
        };
        return send_source_event(SourceEvent::Refused(refusal)).await;
    }

    // Finish: durable sync, independent reread, container gate, metadata
    // publication with final revalidation.
    let candidate: heapless::String<12> = {
        let mut name = heapless::String::new();
        let _ = name.push_str(txn.candidate_name());
        name
    };
    let SourceOwnerState { ws, idem } = owner;
    let Some(idem) = idem.as_mut() else {
        abort_upload(src, txn);
        return send_source_event(SourceEvent::Refused(Refusal::StorageUnavailable)).await;
    };
    let event = match source_store::upload::finish_upload(src, idem, txn, ws, || {
        container_gate_passes(src, candidate.as_str())
    }) {
        Ok(result) => SourceEvent::Committed(commit_for(
            &result,
            upload.declared_length,
            upload.declared_sha256,
            upload.display_label,
        )),
        Err(error) => SourceEvent::Refused(refusal_for_upload_error(error)),
    };
    send_source_event(event).await
}

/// Session setup stalled before a filesystem existed (card, volume, or
/// root failed): answer every request with failure until Sleep or
/// wireless Exit ends the session, and report which one did. Every
/// channel a request can arrive on is served — legacy begins fail, and
/// logical-book operations are refused `storage_unavailable` — because
/// an unanswered request leaves the Wi-Fi task waiting on an event that
/// would never come. Touches no SD state — the caller closes whatever
/// handles it still holds before acknowledging.
async fn refuse_uploads_until_exit() -> UploadSessionExit {
    loop {
        match select(
            select(UPLOAD_BEGINS.receive(), SOURCE_OPS.receive()),
            select(UPLOAD_STOP_REQUESTS.receive(), DISPLAY_COMMANDS.receive()),
        )
        .await
        {
            Either::First(Either::First(begin)) => {
                // A delete begin is never followed by chunks; draining for
                // one would trade the refusal for a deadlock.
                let drained = if begin.delete {
                    Ok(())
                } else {
                    drain_until_end().await
                };
                match drained {
                    Ok(()) => UPLOAD_RESULTS.send(false).await,
                    Err(exit) => return exit,
                }
            }
            // No byte may follow a refused source op — the Wi-Fi task
            // streams an upload body only after `UploadStarted`.
            Either::First(Either::Second(_)) => {
                if let Err(exit) =
                    send_source_event(SourceEvent::Refused(Refusal::StorageUnavailable)).await
                {
                    return exit;
                }
            }
            Either::Second(Either::First(())) => return UploadSessionExit::Wireless,
            Either::Second(Either::Second(DisplayCommand::Sleep { generation })) => {
                return UploadSessionExit::Sleep { generation }
            }
            Either::Second(Either::Second(DisplayCommand::Render(_))) => {}
        }
    }
}
