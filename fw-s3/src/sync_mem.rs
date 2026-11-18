//! One-way memory loan for the Wi-Fi sync session.
//!
//! Dismantles the EPUB reader scratch buffers into raw heap regions for
//! `esp-alloc` and socket/HTTP scratch slices when entering wireless mode.
//! Exiting wireless mode resets the chip to restore reading buffers cleanly.
#![allow(unsafe_code)]

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use esp_alloc::{HeapRegion, MemoryCapability};
use portable_atomic::{AtomicBool, Ordering};

pub struct RawRegion {
    pub ptr: *mut u8,
    pub len: usize,
}

// SAFETY: A RawRegion represents an exclusive one-way ownership transfer of a
// static allocation that the display/reader side never touches again until reset.
unsafe impl Send for RawRegion {}

pub struct SyncLoan {
    pub heap_a: RawRegion,
    pub heap_b: RawRegion,
    pub heap_c: RawRegion,
    pub tcp_rx: &'static mut [u8],
    pub tcp_tx: &'static mut [u8],
    pub http_a: &'static mut [u8],
    pub http_b: &'static mut [u8],
    pub wifi: Option<app_core::WifiCredentials>,
    pub wifi_hint: Option<app_core::WifiApHint>,
    pub catalog_len: usize,
}

struct PrevFbSlot(UnsafeCell<MaybeUninit<display::fb::Framebuffer>>);

// SAFETY: Exclusive single-take access is enforced by `PREV_FB_TAKEN`.
unsafe impl Sync for PrevFbSlot {}

#[link_section = ".dram2_prev_fb"]
static PREV_FB_SLOT: PrevFbSlot = PrevFbSlot(UnsafeCell::new(MaybeUninit::uninit()));
static PREV_FB_TAKEN: AtomicBool = AtomicBool::new(false);

pub fn take_prev_fb() -> Option<&'static mut display::fb::Framebuffer> {
    if PREV_FB_TAKEN.swap(true, Ordering::SeqCst) {
        return None;
    }
    // SAFETY: `PREV_FB_TAKEN` guarantees this is the sole caller and we initialize
    // the NOLOAD slot before returning the mutable reference.
    unsafe {
        let slot = &mut *PREV_FB_SLOT.0.get();
        Some(slot.write(display::fb::Framebuffer::new()))
    }
}

static HEAP_DONATED: AtomicBool = AtomicBool::new(false);

pub fn donate_heap(heap_a: RawRegion, heap_b: RawRegion, heap_c: RawRegion) {
    if HEAP_DONATED.swap(true, Ordering::SeqCst) {
        return;
    }
    // SAFETY: Each loaned region addresses an exclusively owned 'static buffer
    // released by `dismantle_scratch`, with Internal capability on S3 SRAM.
    unsafe {
        esp_alloc::HEAP.add_region(HeapRegion::new(
            heap_a.ptr,
            heap_a.len,
            MemoryCapability::Internal.into(),
        ));
        esp_alloc::HEAP.add_region(HeapRegion::new(
            heap_b.ptr,
            heap_b.len,
            MemoryCapability::Internal.into(),
        ));
        if heap_c.len > 0 {
            esp_alloc::HEAP.add_region(HeapRegion::new(
                heap_c.ptr,
                heap_c.len,
                MemoryCapability::Internal.into(),
            ));
        }
    }
}

pub(crate) fn dismantle_scratch(
    scratch: &'static mut storage::book_build::ReaderCacheScratch<'static>,
) -> SyncLoan {
    let regions = scratch.into_raw_regions();
    let raw = |parts: storage::book_build::RawParts| RawRegion {
        ptr: parts.ptr,
        len: parts.len,
    };
    // SAFETY: Every pointer points into a distinct static allocation yielded by
    // consuming `ReaderCacheScratch`.
    unsafe {
        SyncLoan {
            heap_a: raw(regions.inflate),
            heap_b: raw(regions.xhtml),
            heap_c: raw(regions.decoder),
            tcp_rx: core::slice::from_raw_parts_mut(regions.opf.ptr, regions.opf.len),
            tcp_tx: core::slice::from_raw_parts_mut(regions.compressed.ptr, regions.compressed.len),
            http_a: core::slice::from_raw_parts_mut(regions.container.ptr, regions.container.len),
            http_b: core::slice::from_raw_parts_mut(regions.tail.ptr, regions.tail.len),
            wifi: None,
            wifi_hint: None,
            catalog_len: 0,
        }
    }
}
