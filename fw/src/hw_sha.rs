//! The SHA-256 a whole book is hashed with: the C3's SHA unit, lent out one
//! digest at a time.
//!
//! Proving a move or settling a replacement reads the whole book and hashes
//! it, and in software that was 6.8 s of an 11.7 MB book on the X3, a third
//! of the proof. Measured there, the unit hashes a MiB in 56 ms against
//! software's 611, and the whole book's digest fell from 15.4 s to 8.7 s, of
//! which 8.0 s is reading it.
//!
//! Nothing else on the chip drives it. The Wi-Fi blob's supplicant links its
//! own software SHA-1 and SHA-256 (`sha1_vector`, `sha256_vector` and the
//! HMACs are defined in `libwpa_supplicant.a`, and no blob library refers to
//! a ROM or hardware SHA symbol), so hashing during an upload session shares
//! the unit with nobody.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::{raw::CriticalSectionRawMutex, Mutex};
use esp_hal::sha::{Sha, Sha256, ShaDigest};
use proto::source::{Sha256Engine, SoftSha256, SHA256_BYTES};

/// Taken out for the length of one caller, and put back after: the lock is
/// held only to move it, not across a hash.
static UNIT: Mutex<CriticalSectionRawMutex, RefCell<Option<Sha<'static>>>> =
    Mutex::new(RefCell::new(None));

/// SHA-256 of `abc`, FIPS 180-2 appendix B.1.
const ABC_SHA256: [u8; SHA256_BYTES] = [
    0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22, 0x23,
    0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00, 0x15, 0xad,
];

/// Hand the unit over at boot.
pub fn install(sha: Sha<'static>) {
    UNIT.lock(|unit| *unit.borrow_mut() = Some(sha));
}

/// Run `f` with the unit as its engine, or with software SHA-256 when the
/// unit is lent out already, was not installed, or fails a known-answer
/// check. A digest is a claim about bytes, so an engine that cannot prove it
/// computes SHA-256 does not get to make one.
pub fn with_sha256<R>(f: impl FnOnce(&mut dyn Sha256Engine) -> R) -> R {
    let Some(sha) = UNIT.lock(|unit| unit.borrow_mut().take()) else {
        return f(&mut SoftSha256::new());
    };
    let mut hardware = HwSha256 {
        state: State::Idle(sha),
    };
    let result = if hardware.answers(b"abc", &ABC_SHA256) {
        f(&mut hardware)
    } else {
        esp_println::println!("sha: the unit failed its known answer; hashing in software");
        f(&mut SoftSha256::new())
    };
    if let Some(sha) = hardware.release() {
        UNIT.lock(|unit| *unit.borrow_mut() = Some(sha));
    }
    result
}

enum State {
    Idle(Sha<'static>),
    Hashing(ShaDigest<'static, Sha256, Sha<'static>>),
    /// Only between taking the unit out of one state and putting it in the
    /// next, inside a single method.
    Moving,
}

struct HwSha256 {
    state: State,
}

impl HwSha256 {
    fn answers(&mut self, input: &[u8], expected: &[u8; SHA256_BYTES]) -> bool {
        self.start();
        self.update(input);
        self.finish() == *expected
    }

    fn release(self) -> Option<Sha<'static>> {
        match self.state {
            State::Idle(sha) => Some(sha),
            State::Hashing(digest) => Some(digest.cancel()),
            State::Moving => None,
        }
    }

    fn take(&mut self) -> Option<Sha<'static>> {
        match core::mem::replace(&mut self.state, State::Moving) {
            State::Idle(sha) => Some(sha),
            State::Hashing(digest) => Some(digest.cancel()),
            State::Moving => None,
        }
    }
}

impl Sha256Engine for HwSha256 {
    fn start(&mut self) {
        if let Some(sha) = self.take() {
            self.state = State::Hashing(sha.start_owned::<Sha256>());
        }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        if let State::Hashing(digest) = &mut self.state {
            while !bytes.is_empty() {
                bytes = match nb::block!(digest.update(bytes)) {
                    Ok(rest) => rest,
                    Err(infallible) => match infallible {},
                };
            }
        }
    }

    fn finish(&mut self) -> [u8; SHA256_BYTES] {
        let mut out = [0u8; SHA256_BYTES];
        if let State::Hashing(digest) = &mut self.state {
            if let Err(infallible) = nb::block!(digest.finish(&mut out)) {
                match infallible {}
            }
        }
        if let Some(sha) = self.take() {
            self.state = State::Idle(sha);
        }
        out
    }
}
