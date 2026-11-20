//! A platform-installed line sink. No chip, allocator or output prefix here.

use core::{cell::Cell, fmt::Arguments};
use critical_section::Mutex;

type Sink = for<'a> fn(Arguments<'a>);
static SINK: Mutex<Cell<Option<Sink>>> = Mutex::new(Cell::new(None));

/// Install before spawning tasks. The sink appends one newline per call.
pub fn install(sink: Sink) {
    critical_section::with(|cs| SINK.borrow(cs).set(Some(sink)));
}

pub fn write(args: Arguments<'_>) {
    // Copy the function pointer, then release the guard before formatting or I/O.
    let sink = critical_section::with(|cs| SINK.borrow(cs).get());
    if let Some(sink) = sink {
        sink(args);
    }
}

#[macro_export]
macro_rules! runtime_log {
    ($($arg:tt)*) => { $crate::log::write(core::format_args!($($arg)*)) };
}
