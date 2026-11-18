#![no_std]
#![forbid(unsafe_code)]

//! Shared runtime behavior; executors, peripheral handles and memory loans
//! belong to each concrete firmware executable.

pub use app_core::*;
pub use channels::*;

pub mod app;
pub mod app_desc;
#[cfg(feature = "bench-selftest")]
pub mod bench;
pub mod catalog;
pub mod channels;
pub mod log;
pub mod pad;
pub mod upload;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use core::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };
    use std::{string::String, sync::Mutex};

    static LOG: Mutex<String> = Mutex::new(String::new());

    // One test owns the global runtime channels and line sink. Other tests
    // exercise local state, so parallel test execution cannot consume these.
    #[test]
    fn boot_waits_for_storage_and_emits_a_home_frame_after_the_answer() {
        log::install(|args| {
            use core::fmt::Write;
            writeln!(LOG.lock().unwrap(), "{args}").unwrap();
        });
        runtime_log!("bench: sample n={} ok={}", 7, true);
        assert_eq!(*LOG.lock().unwrap(), "bench: sample n=7 ok=true\n");

        let mut task = pin!(app::run(None));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(task.as_mut().poll(&mut cx), Poll::Pending));
        assert!(matches!(
            STORAGE_COMMANDS.try_receive(),
            Ok(StorageCommand::LoadCatalogCache)
        ));
        assert!(DISPLAY_COMMANDS.try_receive().is_err());

        // The shared app now waits briefly for a real battery sample before
        // the first paint. Seed it before the catalog reply, as X4 does.
        INPUT_EVENTS
            .try_send(InputEvent::Sample {
                button: None,
                aux_raw: 0,
                nav_raw: 0,
                page_raw: 0,
                battery_mv: 3900,
                battery_percent: 75,
            })
            .unwrap();
        assert!(matches!(task.as_mut().poll(&mut cx), Poll::Pending));
        assert!(DISPLAY_COMMANDS.try_receive().is_err());

        LIBRARY_EVENTS
            .try_send(LibraryEvent::Scanned {
                count: 0,
                catalog_epoch: 1,
            })
            .unwrap();
        assert!(matches!(task.as_mut().poll(&mut cx), Poll::Pending));
        let DisplayCommand::Render(request) = DISPLAY_COMMANDS.try_receive().unwrap() else {
            panic!("the catalog answer must produce the first render");
        };
        assert_eq!(request.view, AppView::Home);
        assert_eq!(request.kind, RenderKind::Boot);
        assert!(DISPLAY_COMMANDS.try_receive().is_err());
    }
}
