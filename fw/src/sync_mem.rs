//! One-way memory loan for the Wi-Fi sync session.
//!
//! The radio blob wants roughly 100 KB of heap that this firmware does
//! not have while the reader pipeline owns its scratch. Sync is
//! therefore a terminal mode: the display task dismantles the EPUB
//! scratch into raw byte regions, the wifi task donates them (plus the
//! otherwise unused dram2 boot-loader shadow segment) to esp-alloc, and
//! the only way back to reading is the software reset that ends the
//! session. Nothing here may be touched by reader code after the loan;
//! the display task enforces that by refusing scratch-using storage
//! commands once it has handed the memory over.
#![allow(unsafe_code)]

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use esp_alloc::{HeapRegion, MemoryCapability};
// riscv32imc has no CAS; portable-atomic provides swap on single-core.
use portable_atomic::{AtomicBool, Ordering};

pub(crate) use crate::board::DRAM2_HEAP_BYTES;

/// A loanable byte region described by raw parts. Heap donations use raw
/// pointers rather than slices because the scratch-struct region is
/// repurposed padding-and-all, and no safe reference to it survives.
pub struct RawRegion {
    pub ptr: *mut u8,
    pub len: usize,
}

// Safety: a RawRegion is a one-way transfer of exclusive ownership; the
// loaning side never touches the memory again.
unsafe impl Send for RawRegion {}

/// Everything the display task hands the wifi task: two heap regions
/// (the dismantled scratch struct and the XHTML window) plus initialized
/// buffers reused directly as socket and HTTP scratch.
pub struct SyncLoan {
    pub heap_a: RawRegion,
    pub heap_b: RawRegion,
    pub tcp_rx: &'static mut [u8],
    pub tcp_tx: &'static mut [u8],
    pub http_a: &'static mut [u8],
    pub http_b: &'static mut [u8],
    /// Credentials from /XTEINK/WIFI.BIN; `None` sends the wifi task into
    /// the onboarding portal unless the build carries compile-time ones.
    pub wifi: Option<app_core::WifiCredentials>,
    /// Bytes of catalog listing written into `http_b` by the display task
    /// (`flag|open_name|label` lines) for the shelf page to serve.
    pub catalog_len: usize,
}

struct Dram2(UnsafeCell<MaybeUninit<[u8; DRAM2_HEAP_BYTES]>>);

// Safety: access is gated by DRAM2_TAKEN below.
unsafe impl Sync for Dram2 {}

/// NOLOAD section: never zero-initialized at boot, which a heap region
/// does not need.
#[link_section = ".dram2_uninit"]
static DRAM2_HEAP: Dram2 = Dram2(UnsafeCell::new(MaybeUninit::uninit()));
static DRAM2_TAKEN: AtomicBool = AtomicBool::new(false);

struct PrevFbSlot(UnsafeCell<MaybeUninit<display::fb::Framebuffer>>);

// Safety: access is gated by PREV_FB_TAKEN below.
unsafe impl Sync for PrevFbSlot {}

/// The previous-frame framebuffer lives in dram2 instead of .bss so the
/// radio's static demand fits in main DRAM. NOLOAD, so the display task
/// claims it through here, which writes a cleared framebuffer first.
#[link_section = ".dram2_uninit"]
static PREV_FB_SLOT: PrevFbSlot = PrevFbSlot(UnsafeCell::new(MaybeUninit::uninit()));
static PREV_FB_TAKEN: AtomicBool = AtomicBool::new(false);

/// Hands out the dram2-resident previous-frame buffer, exactly once.
pub fn take_prev_fb() -> Option<&'static mut display::fb::Framebuffer> {
    if PREV_FB_TAKEN.swap(true, Ordering::SeqCst) {
        return None;
    }
    // Safety: the flag above makes this the only reference, and writing
    // a fresh framebuffer initializes the NOLOAD memory before any read.
    unsafe {
        let slot = &mut *PREV_FB_SLOT.0.get();
        Some(slot.write(display::fb::Framebuffer::new()))
    }
}

/// Donates dram2 plus the two loaned regions to the esp-alloc heap the
/// radio blob allocates from. Callable once; esp-alloc supports exactly
/// three regions, which is precisely what the sync session uses.
pub fn donate_heap(heap_a: RawRegion, heap_b: RawRegion) {
    if DRAM2_TAKEN.swap(true, Ordering::SeqCst) {
        return;
    }
    let dram2_ptr = DRAM2_HEAP.0.get().cast::<u8>();
    // Safety: each region is exclusively owned for the rest of the
    // session (dram2 by the flag above, the loans by the one-way
    // handoff), 'static by construction, and non-empty.
    unsafe {
        esp_alloc::HEAP.add_region(HeapRegion::new(
            dram2_ptr,
            DRAM2_HEAP_BYTES,
            MemoryCapability::Internal.into(),
        ));
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
    }
}
