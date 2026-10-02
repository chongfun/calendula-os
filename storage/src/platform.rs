//! What the storage code needs from the chip: the SHA unit, a random source,
//! and a campaign build's recovery report.
//!
//! `fw` installs it once at boot, so code deep in a card session reaches it
//! without a handle. Tests run on the software defaults.

use core::cell::Cell;
use core::sync::atomic::{AtomicU32, Ordering};

use critical_section::Mutex;
use proto::source::{Sha256Engine, SoftSha256};

/// Runs a closure with a SHA-256 engine.
pub type ShaRunner = fn(&mut dyn FnMut(&mut dyn Sha256Engine));

/// The chip's half of the storage code.
pub struct Platform {
    /// Run the closure with a SHA-256 engine, the chip's unit when free.
    pub with_sha256: ShaRunner,
    /// A random word, for minting book ids.
    pub random_u32: fn() -> u32,
    /// What a campaign build reports after recovering interrupted installs.
    #[cfg(feature = "powercut-selftest")]
    pub report_install_recovery: fn(&upload_store::install::InstallRecovery),
}

static INSTALLED: Mutex<Cell<Option<&'static Platform>>> = Mutex::new(Cell::new(None));

static SOFTWARE: Platform = Platform {
    with_sha256: software_sha256,
    random_u32: software_random,
    #[cfg(feature = "powercut-selftest")]
    report_install_recovery: |_| {},
};

/// Install the chip's platform. Called once, at boot, before any session.
pub fn install(platform: &'static Platform) {
    critical_section::with(|cs| INSTALLED.borrow(cs).set(Some(platform)));
}

fn current() -> &'static Platform {
    critical_section::with(|cs| INSTALLED.borrow(cs).get()).unwrap_or(&SOFTWARE)
}

/// Run `f` with the platform's SHA-256 engine.
pub fn with_sha256<R>(f: impl FnOnce(&mut dyn Sha256Engine) -> R) -> R {
    let mut f = Some(f);
    let mut result = None;
    (current().with_sha256)(&mut |engine| {
        if let Some(f) = f.take() {
            result = Some(f(engine));
        }
    });
    match result {
        Some(result) => result,
        // The platform did not call back, so hash in software.
        None => match f.take() {
            Some(f) => f(&mut SoftSha256::new()),
            None => unreachable!("the closure ran or it did not"),
        },
    }
}

/// A random word from the platform.
pub fn random_u32() -> u32 {
    (current().random_u32)()
}

/// Report a campaign build's install recovery.
#[cfg(feature = "powercut-selftest")]
pub fn report_install_recovery(outcome: &upload_store::install::InstallRecovery) {
    (current().report_install_recovery)(outcome)
}

fn software_sha256(f: &mut dyn FnMut(&mut dyn Sha256Engine)) {
    f(&mut SoftSha256::new())
}

/// xorshift32, deterministic for tests.
fn software_random() -> u32 {
    static STATE: AtomicU32 = AtomicU32::new(0x2545_f491);
    let mut x = STATE.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    STATE.store(x, Ordering::Relaxed);
    x
}
