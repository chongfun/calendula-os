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
#[cfg(feature = "bench-selftest")]
pub mod bench_selftest;
pub mod catalog;
pub mod channels;
pub mod log;
pub mod pad;
pub mod upload;
pub mod wifi_server;

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

        // Waking from a sleep that began on a reading page dispatches OpenBook
        // on Restored without closing out a placeholder book or flashing Home,
        // and emits the first Boot render once Loaded arrives.
        let mut wake_task = pin!(app::run_with_options(None, true, true));
        assert!(matches!(wake_task.as_mut().poll(&mut cx), Poll::Pending));
        assert!(matches!(
            STORAGE_COMMANDS.try_receive(),
            Ok(StorageCommand::LoadCatalogCache)
        ));
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
        assert!(matches!(wake_task.as_mut().poll(&mut cx), Poll::Pending));

        // X4 Pro restores the frontlight before the saved SD book. That
        // metadata must not release the initial paint while Home is a
        // boot-time placeholder.
        LIBRARY_EVENTS
            .try_send(LibraryEvent::FrontlightRestored {
                brightness: 30,
                warmth: 50,
            })
            .unwrap();
        assert!(matches!(wake_task.as_mut().poll(&mut cx), Poll::Pending));
        assert!(DISPLAY_COMMANDS.try_receive().is_err());

        let book_id = ReaderSource::sd(2).book_id();
        LIBRARY_EVENTS
            .try_send(LibraryEvent::Restored {
                book_id,
                chapter: 3,
                page: 42,
                page_count: 100,
                reading_orientation: DisplayOrientation::PortraitButtonsLeft as u8,
                refresh_policy: RefreshPolicy::FastOnly as u8,
                font_size: 1,
                line_spacing: 1,
                font_weight: 0,
                font_family: 0,
                front_buttons: 0,
                alignment: 0,
                image_mode: 0,
            })
            .unwrap();
        assert!(matches!(wake_task.as_mut().poll(&mut cx), Poll::Pending));
        let Ok(StorageCommand::OpenBook {
            book_id: opened_id,
            index,
            chapter,
            previous,
            catalog_epoch,
            ..
        }) = STORAGE_COMMANDS.try_receive()
        else {
            panic!("wake into reading must dispatch OpenBook on Restored");
        };
        assert_eq!(opened_id, book_id);
        assert_eq!(index, 2);
        assert_eq!(chapter, 3);
        assert_eq!(previous, None);
        assert_eq!(catalog_epoch, None);
        assert!(DISPLAY_COMMANDS.try_receive().is_err());

        // Scanned arriving while OpenBook is in flight must not paint early.
        LIBRARY_EVENTS
            .try_send(LibraryEvent::Scanned {
                count: 5,
                catalog_epoch: 1,
            })
            .unwrap();
        assert!(matches!(wake_task.as_mut().poll(&mut cx), Poll::Pending));
        assert!(DISPLAY_COMMANDS.try_receive().is_err());

        LIBRARY_EVENTS
            .try_send(LibraryEvent::Loaded {
                book_id,
                pages: 100,
                complete: true,
                chapters: 10,
                current_chapter: 3,
                chapter_pages: [0; MAX_SD_CHAPTERS],
                chapter_subsections: [0; MAX_SD_CHAPTER_SUBSECTIONS / 8],
                position: Some(42),
                text_replaced: true,
            })
            .unwrap();
        assert!(matches!(wake_task.as_mut().poll(&mut cx), Poll::Pending));
        let DisplayCommand::Render(request) = DISPLAY_COMMANDS.try_receive().unwrap() else {
            panic!("Loaded must produce the deferred boot render");
        };
        assert_eq!(request.view, AppView::Reading);
        assert_eq!(request.kind, RenderKind::Boot);
        assert_eq!(request.book_id, book_id);
        assert_eq!(request.page, 42);
        assert!(DISPLAY_COMMANDS.try_receive().is_err());

        // A user can press Continue before boot-time restore finishes.
        // When the saved SD book arrives, Reading -> Reading still owes
        // an OpenBook: the built-in book is the actual prior state.
        while STORAGE_COMMANDS.try_receive().is_ok() {}
        let mut continue_task = pin!(app::run(None));
        assert!(matches!(continue_task.as_mut().poll(&mut cx), Poll::Pending));
        assert!(matches!(
            STORAGE_COMMANDS.try_receive(),
            Ok(StorageCommand::LoadCatalogCache)
        ));
        INPUT_EVENTS
            .try_send(InputEvent::Sample {
                button: Some(Button::Confirm),
                aux_raw: 0,
                nav_raw: 0,
                page_raw: 0,
                battery_mv: 3900,
                battery_percent: 75,
            })
            .unwrap();
        assert!(matches!(continue_task.as_mut().poll(&mut cx), Poll::Pending));
        let DisplayCommand::Render(request) = DISPLAY_COMMANDS.try_receive().unwrap() else {
            panic!("Continue must render the built-in book");
        };
        assert_eq!(request.view, AppView::Reading);
        assert_eq!(request.book_id, 1);
        while STORAGE_COMMANDS.try_receive().is_ok() {}

        LIBRARY_EVENTS
            .try_send(LibraryEvent::Restored {
                book_id,
                chapter: 3,
                page: 42,
                page_count: 100,
                reading_orientation: DisplayOrientation::PortraitButtonsLeft as u8,
                refresh_policy: RefreshPolicy::FastOnly as u8,
                font_size: 1,
                line_spacing: 1,
                font_weight: 0,
                font_family: 0,
                front_buttons: 0,
                alignment: 0,
                image_mode: 0,
            })
            .unwrap();
        assert!(matches!(continue_task.as_mut().poll(&mut cx), Poll::Pending));
        let Ok(StorageCommand::OpenBook {
            book_id: opened_id,
            index,
            chapter,
            previous,
            ..
        }) = STORAGE_COMMANDS.try_receive()
        else {
            panic!("restoring a different book while Reading must dispatch OpenBook");
        };
        assert_eq!(opened_id, book_id);
        assert_eq!(index, 2);
        assert_eq!(chapter, 3);
        assert!(previous.is_some());
    }
}
