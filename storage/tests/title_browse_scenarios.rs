//! Physical Library rows use persisted labels while keeping exact addresses.
mod support;

use proto::library_path::BookRoot;
use support::{Card, Device};

fn persist_title(card: &Card, at: BookRoot, path: &str, title: &str) {
    card.session(|root| {
        for row in 0..100u16 {
            let Some(record) = storage::library_sd::read_catalog_record_at(root, usize::from(row))
            else {
                break;
            };
            if record.root == Some(at) && record.path.as_str() == path {
                let result = storage::library_sd::update_catalog_title(
                    root,
                    usize::from(row),
                    (record.source_hash, record.byte_size),
                    title,
                    || {},
                );
                assert!(matches!(
                    result,
                    storage::library_sd::CatalogTitleUpdate::Changed { .. }
                        | storage::library_sd::CatalogTitleUpdate::Unchanged
                ));
                return;
            }
        }
        panic!("missing catalog row {path}");
    });
}

#[test]
fn persisted_titles_merge_roots_and_keep_folders_and_addresses() {
    let card = Card::blank();
    for path in [
        "BOOKS/Z_1984.epub",
        "BOOKS/A_DUNE.epub",
        "BOOKS/B_HOBBIT.epub",
        "C01.epub",
        "BOOKS/Folder/Nested.epub",
    ] {
        card.put(path, b"not opened");
    }
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "Z_1984.epub", "1984");
    persist_title(&card, BookRoot::Library, "A_DUNE.epub", "Dune");
    persist_title(&card, BookRoot::CardRoot, "C01.epub", "A Christmas Carol");
    let mut device = Device::wake(&card);
    device.open_library();
    let rows = device.store.folder_rows();
    let labels: Vec<_> = rows
        .iter()
        .map(|row| {
            if row.is_dir {
                row.name.as_str()
            } else {
                row.display_label.as_str()
            }
        })
        .collect();
    assert_eq!(
        labels,
        ["1984", "A Christmas Carol", "B HOBBIT", "Dune", "Folder"]
    );
    assert_eq!(rows[1].at, BookRoot::CardRoot);
    assert_eq!(rows[1].name.as_str(), "C01.epub");
    let (at, locator, size) = reader_cache::browse::row_book(&device.store, 1).unwrap();
    assert_eq!(
        (at, locator.as_str(), size),
        (BookRoot::CardRoot, "C01.epub", 10)
    );
    device.choose("Folder");
    assert_eq!(device.store.folder_rows()[0].name.as_str(), "Nested.epub");
    device.press(app_core::Button::Back);
    assert_eq!(device.app.library_depth, 0);
    assert_eq!(device.app.selection, 4);
    assert_eq!(device.store.folder_row(4).unwrap().name.as_str(), "Folder");
}

#[test]
fn completed_index_adopts_the_selection_current_when_the_app_accepts_it() {
    let card = Card::blank();
    for name in ["Alpha.epub", "Bravo.epub", "Charlie.epub"] {
        card.put(&format!("BOOKS/{name}"), b"x");
    }
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "Alpha.epub", "Zulu");
    persist_title(&card, BookRoot::Library, "Charlie.epub", "Aardvark");
    let mut device = Device::wake_queued_only(&card);
    device.press_only(app_core::Button::Back);
    device.press_only(app_core::Button::Next);
    // Let the storage task finish while the notification has not yet been
    // folded. The reader moves once more in that interval.
    for _ in 0..1000 {
        device.step_background_unseen();
        if device
            .host
            .events
            .iter()
            .any(|event| matches!(event, app_core::LibraryEvent::TitleIndexReady { .. }))
        {
            break;
        }
    }
    assert!(device
        .host
        .events
        .iter()
        .any(|event| matches!(event, app_core::LibraryEvent::TitleIndexReady { .. })));
    device.press_only(app_core::Button::Next);
    device.step_background();
    device.settle();
    assert_eq!(device.app.selection, 0);
    assert_eq!(
        device.store.folder_row(0).unwrap().name.as_str(),
        "Charlie.epub"
    );
}

#[test]
fn leaving_during_index_adoption_keeps_the_selected_book_on_return() {
    let card = Card::blank();
    for name in ["Alpha.epub", "Bravo.epub", "Charlie.epub"] {
        card.put(&format!("BOOKS/{name}"), b"x");
    }
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "Charlie.epub", "Aardvark");
    let mut device = Device::wake_queued_only(&card);
    device.press_only(app_core::Button::Back);
    device.press_only(app_core::Button::Next);
    device.press_only(app_core::Button::Next);
    for _ in 0..1000 {
        device.step_background();
        if matches!(
            device.app.library_browse,
            app_core::LibraryBrowse::Relisting { .. }
        ) {
            break;
        }
    }
    assert!(matches!(
        device.app.library_browse,
        app_core::LibraryBrowse::Relisting { .. }
    ));
    device.press_only(app_core::Button::Back);
    assert_eq!(device.app.view, app_core::AppView::Home);
    device.settle();
    device.open_library();
    assert_eq!(device.app.selection, 0);
    assert_eq!(
        device.store.folder_row(0).unwrap().name.as_str(),
        "Charlie.epub"
    );
}

#[test]
fn leaving_before_index_completion_keeps_the_scrolled_book_on_return() {
    let card = Card::blank();
    for name in ["Alpha.epub", "Bravo.epub", "Charlie.epub"] {
        card.put(&format!("BOOKS/{name}"), b"x");
    }
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "Charlie.epub", "Aardvark");
    let mut device = Device::wake_queued_only(&card);
    device.press_only(app_core::Button::Back);
    device.press_only(app_core::Button::Next);
    device.press_only(app_core::Button::Next);
    device.press_only(app_core::Button::Back);
    assert_eq!(device.app.view, app_core::AppView::Home);
    device.settle();
    device.open_library();
    assert_eq!(device.app.selection, 0);
    assert_eq!(
        device.store.folder_row(0).unwrap().name.as_str(),
        "Charlie.epub"
    );
}

#[test]
fn ready_notification_racing_a_folder_move_is_retried_in_the_new_folder() {
    let card = Card::blank();
    card.put("BOOKS/Folder/Book.epub", b"x");
    drop(Device::wake(&card));
    let mut device = Device::wake_queued_only(&card);
    device.press_only(app_core::Button::Back);
    for _ in 0..1000 {
        device.step_background_unseen();
        if device
            .host
            .events
            .iter()
            .any(|event| matches!(event, app_core::LibraryEvent::TitleIndexReady { .. }))
        {
            break;
        }
    }
    assert!(device
        .host
        .events
        .iter()
        .any(|event| matches!(event, app_core::LibraryEvent::TitleIndexReady { .. })));
    device.press_only(app_core::Button::Confirm);
    device.step_background();
    device.run_queued();
    device.settle();
    assert_eq!(device.app.library_depth, 1);
    assert!(device.app.library_browse.is_idle());
    assert!(device.store.title_index_snapshot().is_some());
    assert_eq!(device.rows(), ["Book.epub"]);
}

#[test]
fn title_learned_during_a_build_restarts_it_against_the_new_catalog() {
    let card = Card::blank();
    card.put("BOOKS/Alpha.epub", b"unused");
    card.put("BOOKS/Zulu.epub", &support::epub("Aardvark", 2, 99));
    drop(Device::wake(&card));
    let mut device = Device::wake_queued_only(&card);
    for _ in 0..3 {
        device.step_background();
    }
    device.press_only(app_core::Button::Back);
    device.press_only(app_core::Button::Next);
    device.press_only(app_core::Button::Confirm);
    device.run_queued();
    device.settle();
    assert_eq!(device.app.view, app_core::AppView::Reading);
    assert_eq!(
        device.store.folder_row(0).unwrap().name.as_str(),
        "Zulu.epub"
    );
    assert_eq!(device.app.library_return_selection, Some(0));
    let snapshot = device
        .store
        .title_index_snapshot()
        .and_then(storage::title_index_disk::Snapshot::from_bytes)
        .unwrap();
    assert_eq!(
        card.session(storage::title_index_disk::validate),
        Ok(snapshot)
    );
}

#[test]
fn catalog_refresh_cancels_an_inflight_index_before_publishing() {
    let card = Card::blank();
    card.put("BOOKS/First.epub", b"a");
    drop(Device::wake(&card));
    let mut device = Device::wake_queued_only(&card);
    for _ in 0..3 {
        device.step_background();
    }
    card.put("BOOKS/Second.epub", b"b");
    device.send(app_core::StorageCommand::RefreshCatalog);
    device.settle();
    let snapshot = device
        .store
        .title_index_snapshot()
        .and_then(storage::title_index_disk::Snapshot::from_bytes)
        .unwrap();
    assert_eq!(snapshot.count(), 2);
    assert_eq!(
        card.session(storage::title_index_disk::validate),
        Ok(snapshot)
    );
    device.open_library();
    assert_eq!(device.rows(), ["First.epub", "Second.epub"]);
}

#[test]
fn sorted_pages_keep_all_books_across_the_window_boundary() {
    let card = Card::blank();
    for i in 0..23 {
        card.put(&format!("BOOKS/Book {i:02}.epub"), b"x");
    }
    drop(Device::wake(&card));
    for i in 0..23 {
        persist_title(
            &card,
            BookRoot::Library,
            &format!("Book {i:02}.epub"),
            &format!("Title {:02}", 22 - i),
        );
    }
    let mut device = Device::wake(&card);
    device.open_library();
    let mut seen = Vec::new();
    for selection in 0..23u16 {
        storage::library_sd::ensure_folder_page(
            &mut device.card,
            &mut device.store,
            selection,
            true,
        );
        let row = device
            .store
            .folder_row(usize::from(selection))
            .expect("row in page");
        seen.push((
            row.name.as_str().to_owned(),
            row.display_label.as_str().to_owned(),
        ));
    }
    assert_eq!(
        seen,
        (0..23)
            .map(|i| (format!("Book {:02}.epub", 22 - i), format!("Title {i:02}")))
            .collect::<Vec<_>>()
    );
    for selection in (0..23u16).rev() {
        storage::library_sd::ensure_folder_page(
            &mut device.card,
            &mut device.store,
            selection,
            true,
        );
        assert_eq!(
            device
                .store
                .folder_row(usize::from(selection))
                .unwrap()
                .display_label
                .as_str(),
            format!("Title {selection:02}")
        );
    }
}

#[test]
fn a_new_live_book_forces_the_filename_fallback() {
    let card = Card::blank();
    card.put("BOOKS/Z.epub", &support::epub("000", 2, 123));
    let mut original = Device::wake(&card);
    original.open_library();
    original.choose("Z.epub");
    original.sleep();
    card.put("BOOKS/A.epub", b"x");
    let mut device = Device::wake_queued_only(&card);
    assert_eq!(device.rows(), ["A.epub", "Z.epub"]);
    // Validation runs in background slices; until it completes the live
    // filename listing is safe even though freshness is not yet known.
    assert!(device.store.title_index_requested());
    assert_eq!(device.store.title_folder_range(), None);
    device.settle();
    device.open_library();
    assert_eq!(device.rows(), ["Z.epub", "A.epub"]);
    assert_eq!(
        device.app.selection, 1,
        "the fallback-selected book stays selected after sorting"
    );
}

#[test]
fn learned_title_relands_the_same_book_after_crossing_a_page() {
    let card = Card::blank();
    for i in 0..20 {
        let bytes = if i == 19 {
            support::epub("Aardvark", 2, 99)
        } else {
            vec![1]
        };
        card.put(&format!("BOOKS/Book {i:02}.epub"), &bytes);
    }
    let mut device = Device::wake(&card);
    device.open_library();
    let old_epoch = device.store.browse_epoch();
    for selection in 1..20u16 {
        device.press(app_core::Button::Next);
        storage::library_sd::ensure_folder_page(
            &mut device.card,
            &mut device.store,
            selection,
            true,
        );
    }
    assert_eq!(
        device.store.folder_row(19).unwrap().name.as_str(),
        "Book 19.epub"
    );
    device.press(app_core::Button::Confirm);
    assert_eq!(
        device.app.view,
        app_core::AppView::Reading,
        "{:?}",
        device.log
    );
    assert!(device.store.browse_epoch() != old_epoch);
    assert_eq!(
        device.store.folder_row(0).unwrap().name.as_str(),
        "Book 19.epub"
    );
    assert_eq!(device.app.library_return_selection, Some(0));
    while device.app.view == app_core::AppView::Reading {
        device.press(app_core::Button::Back);
    }
    device.open_library();
    assert_eq!(device.app.selection, 0);
    assert_eq!(
        device.store.folder_row(0).unwrap().display_label.as_str(),
        "Aardvark"
    );
}

#[test]
fn an_immediate_return_after_learning_a_title_uses_the_new_order() {
    let card = Card::blank();
    card.put("BOOKS/Alpha.epub", b"unused");
    card.put("BOOKS/Zulu.epub", &support::epub("Aardvark", 2, 99));
    let mut device = Device::wake(&card);
    device.open_library();
    device.press_only(app_core::Button::Next);
    device.press_only(app_core::Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.view, app_core::AppView::Reading);
    while device.app.view == app_core::AppView::Reading {
        device.press_only(app_core::Button::Back);
        device.run_queued();
    }
    device.press_only(app_core::Button::Back);
    device.run_queued();
    assert_eq!(device.app.view, app_core::AppView::Library);
    assert_eq!(device.app.selection, 0);
    assert_eq!(
        device.store.folder_row(0).unwrap().name.as_str(),
        "Zulu.epub"
    );
    assert_eq!(
        device.store.folder_row(0).unwrap().display_label.as_str(),
        "Aardvark"
    );
}

#[test]
fn a_row_address_from_before_a_title_move_cannot_open_another_book() {
    let card = Card::blank();
    card.put("BOOKS/Alpha.epub", b"unused");
    card.put("BOOKS/Zulu.epub", &support::epub("Aardvark", 2, 99));
    let mut device = Device::wake(&card);
    device.open_library();
    let old_epoch = device.store.browse_epoch();
    device.press_only(app_core::Button::Next);
    device.press_only(app_core::Button::Confirm);
    device.run_queued();
    assert_ne!(device.store.browse_epoch(), old_epoch);
    let before = device.log.len();
    device.send(app_core::StorageCommand::ChooseLibraryRow {
        request_id: 99,
        index: 0,
        browse_epoch: old_epoch,
    });
    device.run_queued();
    assert!(device.log[before..]
        .iter()
        .any(|event| matches!(event, app_core::LibraryEvent::RowFailed { request_id: 99 })));
    assert!(!device.log[before..].iter().any(|event| matches!(
        event,
        app_core::LibraryEvent::RowIsBook { request_id: 99, .. }
    )));
}

#[test]
fn successive_opens_reorder_titles_before_background_rebuild_runs() {
    let card = Card::blank();
    card.put("BOOKS/Alpha.epub", &support::epub("Zulu", 2, 11));
    card.put("BOOKS/Bravo.epub", b"unused");
    card.put("BOOKS/Charlie.epub", &support::epub("Aardvark", 2, 22));
    let mut device = Device::wake(&card);
    device.open_library();
    device.press_only(app_core::Button::Confirm);
    device.run_queued();
    while device.app.view == app_core::AppView::Reading {
        device.press_only(app_core::Button::Back);
        device.run_queued();
    }
    device.press_only(app_core::Button::Back);
    device.run_queued();
    assert_eq!(device.app.selection, 2);
    assert_eq!(
        device.store.folder_row(2).unwrap().display_label.as_str(),
        "Zulu"
    );
    device.press_only(app_core::Button::Previous);
    device.press_only(app_core::Button::Confirm);
    device.run_queued();
    while device.app.view == app_core::AppView::Reading {
        device.press_only(app_core::Button::Back);
        device.run_queued();
    }
    device.press_only(app_core::Button::Back);
    device.run_queued();
    assert_eq!(device.app.selection, 0);
    assert_eq!(
        device.store.folder_row(0).unwrap().display_label.as_str(),
        "Aardvark"
    );
    assert_eq!(
        device.store.folder_row(2).unwrap().display_label.as_str(),
        "Zulu"
    );
    assert_eq!(device.store.title_overlays().len(), 2);
}

#[test]
fn overlay_limit_falls_back_without_changing_the_selected_file() {
    let card = Card::blank();
    for i in 0..17 {
        card.put(
            &format!("BOOKS/Book {i:02}.epub"),
            &support::epub(&format!("Book {i:02}"), 2, 100 + i),
        );
    }
    let mut device = Device::wake(&card);
    device.open_library();
    for i in 0..17u16 {
        storage::library_sd::ensure_folder_page(&mut device.card, &mut device.store, i, true);
        assert_eq!(
            device
                .store
                .folder_row(usize::from(i))
                .unwrap()
                .name
                .as_str(),
            format!("Book {i:02}.epub")
        );
        device.press_only(app_core::Button::Confirm);
        device.run_queued();
        assert_eq!(device.app.view, app_core::AppView::Reading);
        while device.app.view == app_core::AppView::Reading {
            device.press_only(app_core::Button::Back);
            device.run_queued();
        }
        device.press_only(app_core::Button::Back);
        device.run_queued();
        assert_eq!(device.app.selection, i);
        if i < 16 {
            device.press_only(app_core::Button::Next);
        }
    }
    assert!(device.store.title_index_snapshot().is_none());
    assert!(device.store.title_index_requested());
    device.settle();
    assert!(device.store.title_index_snapshot().is_some());
    assert_eq!(device.app.selection, 16);
    assert_eq!(
        device.store.folder_row(16).unwrap().name.as_str(),
        "Book 16.epub"
    );
}

#[test]
fn equal_and_case_variant_titles_remain_distinct_and_deterministic() {
    let card = Card::blank();
    for path in [
        "BOOKS/Same.epub",
        "Same.epub",
        "BOOKS/Other.epub",
        "BOOKS/Lower.epub",
    ] {
        card.put(path, b"x");
    }
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "Same.epub", "Dune");
    persist_title(&card, BookRoot::CardRoot, "Same.epub", "Dune");
    persist_title(&card, BookRoot::Library, "Other.epub", "Dune");
    persist_title(&card, BookRoot::Library, "Lower.epub", "dune");
    let mut device = Device::wake(&card);
    device.open_library();
    let rows = device.store.folder_rows();
    assert_eq!(
        rows.iter()
            .map(|row| (row.display_label.as_str(), row.name.as_str(), row.at))
            .collect::<Vec<_>>(),
        [
            ("Dune", "Other.epub", BookRoot::Library),
            ("Dune", "Same.epub", BookRoot::Library),
            ("Dune", "Same.epub", BookRoot::CardRoot),
            ("dune", "Lower.epub", BookRoot::Library),
        ]
    );
    for (index, expected) in [(1, BookRoot::Library), (2, BookRoot::CardRoot)] {
        let (root, path, _) = reader_cache::browse::row_book(&device.store, index).unwrap();
        assert_eq!((root, path.as_str()), (expected, "Same.epub"));
        let storage::library_sd::RowChoice::Book(catalog_row) =
            storage::library_sd::choose_library_row(
                &mut device.card,
                &mut device.store,
                index,
                true,
            )
        else {
            panic!("the duplicate title resolves to a catalog row");
        };
        let record = card
            .session(|root| {
                storage::library_sd::read_catalog_record_at(root, usize::from(catalog_row))
            })
            .unwrap();
        assert_eq!(record.root, Some(expected));
        assert_eq!(record.path.as_str(), "Same.epub");
    }
}

#[test]
fn decoded_multibyte_titles_at_the_catalog_limit_sort_without_splitting() {
    let card = Card::blank();
    card.put("BOOKS/A.epub", b"x");
    card.put("BOOKS/B.epub", b"x");
    drop(Device::wake(&card));
    let longer = "é".repeat(32);
    let shorter = format!("{}a", "é".repeat(31));
    persist_title(&card, BookRoot::Library, "A.epub", &longer);
    persist_title(&card, BookRoot::Library, "B.epub", &shorter);
    let mut device = Device::wake(&card);
    device.open_library();
    let rows = device.store.folder_rows();
    assert_eq!(rows[0].name.as_str(), "B.epub");
    assert_eq!(rows[0].display_label.as_str(), shorter);
    assert_eq!(rows[1].name.as_str(), "A.epub");
    assert_eq!(rows[1].display_label.as_str(), longer);
}

#[test]
fn reland_refuses_a_replacement_with_the_same_name_and_a_new_size() {
    let card = Card::blank();
    card.put("BOOKS/Book.epub", b"one");
    let mut device = Device::wake(&card);
    let path = proto::library_path::LibraryPath::parse("Book.epub").unwrap();
    card.delete("BOOKS/Book.epub");
    card.put("BOOKS/Book.epub", b"different size");
    let listed = card.session(|root| {
        reader_cache::browse::relist_on_book_sized(
            &mut device.store,
            root,
            true,
            BookRoot::Library,
            &path,
            3,
        )
    });
    assert!(listed.is_none(), "a replacement is not the selected file");
}

#[test]
fn a_file_replaced_after_the_page_was_loaded_is_not_opened_under_the_old_row() {
    let card = Card::blank();
    card.put("BOOKS/Book.epub", &support::epub("Original", 2, 2));
    let mut cached = Device::wake(&card);
    cached.open_library();
    cached.choose("Book.epub");
    cached.sleep();
    let mut device = Device::wake(&card);
    device.open_library();
    card.delete("BOOKS/Book.epub");
    card.put("BOOKS/Book.epub", b"replacement is longer");
    let start = device.log.len();
    device.press(app_core::Button::Confirm);
    assert_eq!(device.app.view, app_core::AppView::Library);
    assert!(device.log[start..]
        .iter()
        .any(|event| matches!(event, app_core::LibraryEvent::RowFailed { .. })));
    assert!(!device.log[start..]
        .iter()
        .any(|event| matches!(event, app_core::LibraryEvent::RowIsBook { .. })));
}

#[test]
fn live_file_lookup_does_not_treat_a_read_fault_as_absence() {
    let card = Card::blank();
    card.put("BOOKS/Book.epub", b"bytes");
    let mut device = Device::wake(&card);
    let path = proto::library_path::LibraryPath::parse("Book.epub").unwrap();
    card.disk.refuse_read_in(Some(0));
    assert_eq!(
        storage::library_sd::live_book(&mut device.card, BookRoot::Library, &path, 5),
        storage::library_sd::LiveBook::Unreadable
    );
    card.disk.refuse_read_in(None);
    assert_eq!(
        storage::library_sd::live_book(&mut device.card, BookRoot::Library, &path, 6),
        storage::library_sd::LiveBook::Missing
    );
    assert_eq!(
        storage::library_sd::live_book(&mut device.card, BookRoot::Library, &path, 5),
        storage::library_sd::LiveBook::Present
    );
}

#[test]
fn a_failed_rescan_cannot_use_the_old_catalog_to_open_a_replaced_file() {
    let card = Card::blank();
    card.put("BOOKS/Book.epub", &support::epub("Original", 2, 2));
    let mut device = Device::wake(&card);
    device.open_library();
    card.delete("BOOKS/Book.epub");
    card.put("BOOKS/Book.epub", b"replacement is longer");
    card.disk.refuse_next_writes(u32::MAX);
    let before = device.log.len();
    device.press_only(app_core::Button::Confirm);
    device.run_queued();
    assert!(device.rescan_owed());
    device.rescan();
    card.disk.refuse_next_writes(0);
    assert!(device.log[before..]
        .iter()
        .any(|event| matches!(event, app_core::LibraryEvent::RowFailed { .. })));
    assert!(!device.log[before..]
        .iter()
        .any(|event| matches!(event, app_core::LibraryEvent::RowIsBook { .. })));
    assert_eq!(device.app.view, app_core::AppView::Library);
}

#[test]
fn a_replacement_between_pick_and_open_cannot_use_the_old_cache() {
    let card = Card::blank();
    card.put("BOOKS/Book.epub", &support::epub("Original", 2, 2));
    let mut cached = Device::wake(&card);
    cached.open_library();
    cached.choose("Book.epub");
    cached.sleep();
    let mut device = Device::wake(&card);
    device.open_library();
    device.press_only(app_core::Button::Confirm);
    assert!(device.handle_one_with(|_| {})); // RowIsBook queues its open.
    assert_eq!(device.app.view, app_core::AppView::Reading);
    card.delete("BOOKS/Book.epub");
    card.put("BOOKS/Book.epub", b"replacement is longer");
    let before = device.log.len();
    device.run_queued();
    assert!(device.log[before..]
        .iter()
        .any(|event| matches!(event, app_core::LibraryEvent::BookOpenFailed { .. })));
    assert!(!device.log[before..]
        .iter()
        .any(|event| matches!(event, app_core::LibraryEvent::Loaded { .. })));
}

#[test]
fn a_refused_read_never_exposes_a_partially_filled_sorted_page() {
    let card = Card::blank();
    for i in 0..24 {
        card.put(&format!("BOOKS/Book {i:02}.epub"), b"x");
    }
    let mut device = Device::wake(&card);
    device.open_library();
    let mut late_faults = 0;
    for fail_after in 0..45 {
        storage::library_sd::ensure_folder_page(&mut device.card, &mut device.store, 0, true);
        card.disk.refuse_read_in(Some(fail_after));
        storage::library_sd::ensure_folder_page(&mut device.card, &mut device.store, 23, true);
        if !card.disk.read_refusal_armed() {
            assert!(
                device.store.folder_rows().is_empty(),
                "read {fail_after} exposed a partial page"
            );
            if fail_after >= 10 {
                late_faults += 1;
            }
        }
        card.disk.refuse_read_in(None);
    }
    assert!(
        late_faults > 0,
        "the sweep reached faults during indexed page reads"
    );
}
