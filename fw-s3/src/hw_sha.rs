//! Hardware-accelerated SHA-256 on the ESP32-S3 SHA peripheral, lent out one
//! digest at a time with software fallback.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::{raw::CriticalSectionRawMutex, Mutex};
use embassy_time::{Duration, Instant};
use esp_hal::sha::{Sha, Sha256, ShaDigest};
use proto::source::{Sha256Engine, SoftSha256, SHA256_BYTES};

static UNIT: Mutex<CriticalSectionRawMutex, RefCell<Option<Sha<'static>>>> =
    Mutex::new(RefCell::new(None));

/// Bounded timeout per hardware SHA operation before retiring the unit to software.
const HW_SHA_OP_TIMEOUT: Duration = Duration::from_millis(25);

/// SHA-256 of `abc`, FIPS 180-2 appendix B.1.
const ABC_SHA256: [u8; SHA256_BYTES] = [
    0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22, 0x23,
    0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00, 0x15, 0xad,
];

/// Install the SHA peripheral handle at boot.
pub fn install(sha: Sha<'static>) {
    UNIT.lock(|unit| *unit.borrow_mut() = Some(sha));
}

/// Run `f` with the hardware SHA-256 unit after a known-answer self-check,
/// falling back to software hashing if unavailable or unhealthy.
pub fn with_sha256<R>(f: impl FnOnce(&mut dyn Sha256Engine) -> R) -> R {
    let Some(sha) = UNIT.lock(|unit| unit.borrow_mut().take()) else {
        return f(&mut SoftSha256::new());
    };
    let mut hardware = HwSha256 {
        state: State::Idle(sha),
        stalled: false,
        software: SoftSha256::new(),
    };
    let result = if hardware.answers(b"abc", &ABC_SHA256) {
        f(&mut hardware)
    } else {
        esp_println::println!("sha: unit failed known answer; hashing in software");
        f(&mut SoftSha256::new())
    };
    if !hardware.stalled {
        if let Some(sha) = hardware.release() {
            UNIT.lock(|unit| *unit.borrow_mut() = Some(sha));
        }
    } else {
        esp_println::println!("sha: stalled unit retired; staying on software fallback");
    }
    result
}

enum State {
    Idle(Sha<'static>),
    Hashing(ShaDigest<'static, Sha256, Sha<'static>>),
    Moving,
}

struct HwSha256 {
    state: State,
    stalled: bool,
    // Keep a software stream in lockstep: FnOnce cannot replay input after
    // an in-flight hardware timeout, and a source digest must be exact.
    software: SoftSha256,
}

impl HwSha256 {
    fn answers(&mut self, input: &[u8], expected: &[u8; SHA256_BYTES]) -> bool {
        self.start();
        self.update(input);
        let digest = self.finish();
        !self.stalled && digest == *expected
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
        self.software.start();
        if self.stalled {
            return;
        }
        if let Some(sha) = self.take() {
            self.state = State::Hashing(sha.start_owned::<Sha256>());
        }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        self.software.update(bytes);
        if self.stalled {
            return;
        }
        if let State::Hashing(digest) = &mut self.state {
            while !bytes.is_empty() {
                let deadline = Instant::now() + HW_SHA_OP_TIMEOUT;
                loop {
                    match digest.update(bytes) {
                        Ok(rest) => {
                            bytes = rest;
                            break;
                        }
                        Err(nb::Error::WouldBlock) => {
                            if Instant::now() >= deadline {
                                self.stalled = true;
                                return;
                            }
                        }
                        Err(nb::Error::Other(infallible)) => match infallible {},
                    }
                }
            }
        }
    }

    fn finish(&mut self) -> [u8; SHA256_BYTES] {
        let software_digest = self.software.finish();
        if self.stalled {
            return software_digest;
        }

        let mut out = [0u8; SHA256_BYTES];
        if let State::Hashing(digest) = &mut self.state {
            let deadline = Instant::now() + HW_SHA_OP_TIMEOUT;
            loop {
                match digest.finish(&mut out) {
                    Ok(()) => break,
                    Err(nb::Error::WouldBlock) => {
                        if Instant::now() >= deadline {
                            self.stalled = true;
                            return software_digest;
                        }
                    }
                    Err(nb::Error::Other(infallible)) => match infallible {},
                }
            }
        }
        if let Some(sha) = self.take() {
            self.state = State::Idle(sha);
        }
        if out != software_digest {
            // A silent SHA peripheral failure must not become a book identity.
            self.stalled = true;
            return software_digest;
        }
        out
    }
}
