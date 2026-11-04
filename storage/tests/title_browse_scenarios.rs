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
                // These tests inject an already-known metadata title, not a
                // foreground BookCache title awaiting v13 volume discovery.
                let known = storage::library_sd::read_catalog_record_at(root, usize::from(row))
                    .expect("updated catalog row");
                let proved = storage::library_sd::update_catalog_probe(
                    root,
                    usize::from(row),
                    &known,
                    Some(title),
                    || {},
                );
                assert!(matches!(
                    proved,
                    storage::library_sd::CatalogTitleUpdate::Changed { .. }
                        | storage::library_sd::CatalogTitleUpdate::Unchanged
                ));
                return;
            }
        }
        panic!("missing catalog row {path}");
    });
}

/// A FAT child can have a legal long filename while its full locator is
/// too long for the Library. Counting, membership, and filename paging must
/// agree to exclude it, rather than publishing an unreadable row.
#[test]
fn unaddressable_children_do_not_enter_library_counts_or_pages() {
    let card = Card::blank();
    let first = "A".repeat(124);
    let second = "B".repeat(125);
    let base = format!("BOOKS/{first}/{second}");
    card.put(&format!("{base}/Extra.epub"), b"x");
    card.put(&format!("{base}/Nested/Inside.epub"), b"x");

    let folder = proto::library_path::LibraryPath::parse(&format!("{first}/{second}"))
        .expect("folder path fits");
    assert!(folder.child("Extra.epub").is_err());
    assert!(folder.child("Nested").is_err());
    card.session(|root| {
        let listing = upload_store::library::open_listing(root, &folder)
            .expect("read the folder")
            .expect("folder exists");
        let mut visited = 0;
        let (counts, cursor) = listing
            .counts_visiting_books(root, None, |_, _| visited += 1)
            .expect("visit live members");
        assert_eq!(counts.total(), 0);
        assert_eq!(counts.books(), 0);
        assert_eq!(visited, 0);
        assert!(cursor.is_none());

        let mut window: [upload_store::library::LibraryRow; 4] = Default::default();
        assert_eq!(
            listing
                .page(root, counts, 0, &mut window, &mut None)
                .expect("page walk")
                .expect("page available"),
            0
        );
        assert_eq!(listing.counts(root).unwrap(), counts);
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

/// The low-memory filename pager keeps the two physical roots as separate
/// regions; a validated title index then merges their books into one region.
#[test]
fn root_filename_fallback_keeps_separate_regions_until_index_adoption() {
    let card = Card::blank();
    for path in [
        "BOOKS/Bravo.epub",
        "BOOKS/Zulu.epub",
        "Alpha.epub",
        "BOOKS/Folder/Nested.epub",
    ] {
        card.put(path, b"x");
    }
    // Establish a catalog, then make its title index stale without changing
    // either root's older books. The queued-only wake shows the live fallback.
    drop(Device::wake(&card));
    card.put("BOOKS/Charlie.epub", b"x");
    let mut device = Device::wake_queued_only(&card);
    assert!(device.store.title_folder_range().is_none());
    assert!(device.store.title_index_requested());
    assert_eq!(
        device.rows(),
        [
            "Bravo.epub",
            "Charlie.epub",
            "Zulu.epub",
            "Alpha.epub",
            "Folder"
        ]
    );
    let (root, locator, _) = reader_cache::browse::row_book(&device.store, 3).unwrap();
    assert_eq!((root, locator.as_str()), (BookRoot::CardRoot, "Alpha.epub"));

    device.settle();
    device.open_library();
    assert!(device.store.title_folder_range().is_some());
    assert_eq!(
        device.rows(),
        [
            "Alpha.epub",
            "Bravo.epub",
            "Charlie.epub",
            "Zulu.epub",
            "Folder"
        ]
    );
    let (root, locator, _) = reader_cache::browse::row_book(&device.store, 0).unwrap();
    assert_eq!((root, locator.as_str()), (BookRoot::CardRoot, "Alpha.epub"));
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
    // Discovery can retire the index while Home is visible; entering
    // Library completes the fallback relist and adopts the rebuilt index.
    device.open_library();
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
            // Probe initially finds no title; the later open must learn it.
            vec![0; support::epub("Aardvark", 2, 99).len()]
        } else {
            vec![1]
        };
        card.put(&format!("BOOKS/Book {i:02}.epub"), &bytes);
    }
    drop(Device::wake(&card));
    // Keep the identity and NoMetadataTitle marker from that first scan,
    // so the foreground open (not idle discovery) first learns the title.
    let replacement = support::epub("Aardvark", 2, 99);
    card.delete("BOOKS/Book 19.epub");
    card.put("BOOKS/Book 19.epub", &replacement);
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
    // Discovery can now learn the real title before the book is opened.
    // Seed a different, already-probed title so the foreground open still
    // exercises title-index invalidation and the stale row-address fence.
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "Zulu.epub", "Zulu");
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
    let alpha = support::epub("Zulu", 2, 11);
    let charlie = support::epub("Aardvark", 2, 22);
    card.put("BOOKS/Alpha.epub", &vec![0; alpha.len()]);
    card.put("BOOKS/Bravo.epub", b"unused");
    card.put("BOOKS/Charlie.epub", &vec![0; charlie.len()]);
    drop(Device::wake(&card));
    // Equal-size replacements keep the prior NoMetadataTitle markers,
    // allowing the two foreground opens to exercise title overlays.
    card.delete("BOOKS/Alpha.epub");
    card.put("BOOKS/Alpha.epub", &alpha);
    card.delete("BOOKS/Charlie.epub");
    card.put("BOOKS/Charlie.epub", &charlie);
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
    // With idle metadata discovery, all 17 real titles may be known at
    // wake. Seed distinct outdated labels so foreground opens must update
    // the overlay, keeping the cap-and-fallback path under test.
    drop(Device::wake(&card));
    for i in 0..17 {
        persist_title(
            &card,
            BookRoot::Library,
            &format!("Book {i:02}.epub"),
            &format!("Book {i:02} Placeholder"),
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

#[test]
fn non_epub_shelf_files_do_not_poison_title_index_membership() {
    let card = Card::blank();
    card.put("BOOKS/Book.epub", b"book");
    card.put("BOOKS/notes.txt", b"notes");
    card.put("BOOKS/cover.jpg", b"image");
    card.put("BOOKS/Nested/Another.epub", b"other");
    card.put("BOOKS/Nested/metadata.json", b"metadata");
    let mut device = Device::wake(&card);
    assert_eq!(device.store.catalog_count(), 2);
    // The metadata pass can defer adoption until the Library relists.
    device.open_library();
    assert!(device.store.title_index_snapshot().is_some());
    assert_eq!(device.rows(), ["Book.epub", "Nested"]);
    assert!(!device.store.title_catalog_stale());
    device.choose("Nested");
    assert_eq!(device.rows(), ["Another.epub"]);
    assert!(device.store.title_folder_range().is_some());
    assert!(!device.store.stale_catalog_refresh_requested());
}

#[test]
fn browse_rollback_does_not_restore_an_invalidated_title_range() {
    let card = Card::blank();
    card.put("BOOKS/Book.epub", b"book");
    let mut device = Device::wake(&card);
    device.open_library();
    assert!(device.store.title_folder_range().is_some());
    let point = device.store.browse_checkpoint();
    device.store.invalidate_title_index();
    device.store.request_title_index();
    device.store.restore_browse(point);
    assert_eq!(device.store.title_folder_range(), None);
    storage::library_sd::ensure_folder_page(&mut device.card, &mut device.store, 0, true);
    assert_eq!(device.rows(), ["Book.epub"]);
}

#[test]
fn corrupted_title_run_during_folder_entry_uses_live_filename_order() {
    let card = Card::blank();
    card.put("BOOKS/Root.epub", b"root");
    card.put("BOOKS/Folder/Nested.epub", b"nested");
    let mut device = Device::wake(&card);
    device.open_library();
    device.point_at("Folder");
    let snapshot = device
        .store
        .title_index_snapshot()
        .and_then(storage::title_index_disk::Snapshot::from_bytes)
        .unwrap();
    let path = if snapshot.slot() == 0 {
        "READER/TITLEIDX.BIN"
    } else {
        "READER/TITLEID2.BIN"
    };
    let mut bytes = card.read(path).unwrap();
    let run_start = storage::title_index_disk::SNAPSHOT_BYTES
        + if bytes[6] == 0 {
            0
        } else {
            usize::from(snapshot.count()) * 2
        };
    bytes[run_start] ^= 1;
    card.delete(path);
    card.put_short(path, &bytes);
    device.press_only(app_core::Button::Confirm);
    device.run_queued();
    assert_eq!(device.app.library_depth, 1);
    assert_eq!(device.rows(), ["Nested.epub"]);
    assert!(device.store.title_index_snapshot().is_none());
    assert!(device.store.title_index_requested());
}

#[test]
fn indexed_page_corruption_retires_index_and_recovers_filename_paging() {
    let card = Card::blank();
    for n in 0..24 {
        card.put(&format!("BOOKS/Book {n:02}.epub"), b"x");
    }
    let mut device = Device::wake(&card);
    device.open_library();
    assert!(device.store.title_folder_range().is_some());
    let snapshot = device
        .store
        .title_index_snapshot()
        .and_then(storage::title_index_disk::Snapshot::from_bytes)
        .unwrap();
    let path = if snapshot.slot() == 0 {
        "READER/TITLEIDX.BIN"
    } else {
        "READER/TITLEID2.BIN"
    };
    let mut bytes = card.read(path).unwrap();
    let run_start = storage::title_index_disk::SNAPSHOT_BYTES
        + if bytes[6] == 0 {
            0
        } else {
            usize::from(snapshot.count()) * 2
        };
    bytes[run_start] ^= 1;
    card.delete(path);
    card.put_short(path, &bytes);
    storage::library_sd::ensure_folder_page(&mut device.card, &mut device.store, 23, true);
    assert_eq!(device.store.title_folder_range(), None);
    assert!(device.store.title_index_snapshot().is_none());
    assert!(device.store.title_index_requested());
    // Until the app is told the new selection, all filename-row addresses
    // remain withheld, even on a second render.
    storage::library_sd::ensure_folder_page(&mut device.card, &mut device.store, 23, true);
    assert!(device.store.folder_row(23).is_none());
    assert!(device.store.title_fallback_pending());
    device.settle();
    assert!(!device.store.title_fallback_pending());
    assert_eq!(device.app.selection, 0);
    storage::library_sd::ensure_folder_page(&mut device.card, &mut device.store, 23, true);
    assert_eq!(
        device.store.folder_row(23).unwrap().name.as_str(),
        "Book 23.epub"
    );
}

/// A refused card read must restart the cooperative title-index job, not
/// discard the request. Repeated refusals back off and a healthy slice clears
/// that backoff; no further reader command is required to publish title order.
#[test]
fn transient_title_index_io_refusal_retries_without_another_command() {
    let card = Card::blank();
    card.put("BOOKS/Alpha.epub", b"a");
    card.put("BOOKS/Zulu.epub", b"z");
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "Zulu.epub", "Aardvark");

    let mut device = Device::wake_queued_only(&card);
    assert!(device.store.title_index_requested());
    assert!(device.store.title_index_snapshot().is_none());

    for expected_refusals in 1..=2 {
        card.disk.refuse_read_in(Some(0));
        device.step_background();
        assert!(!card.disk.read_refusal_armed());
        assert!(device.store.title_index_requested());
        assert_eq!(
            device
                .task
                .background_attempts(&device.store, device.frame()),
            expected_refusals
        );
        assert!(device.task.background_owed(&device.store, device.frame()));
    }

    card.disk.refuse_read_in(None);
    device.step_background();
    assert_eq!(
        device
            .task
            .background_attempts(&device.store, device.frame()),
        0
    );
    device.settle();
    assert!(device.store.title_index_snapshot().is_some());
    device.open_library();
    assert_eq!(device.rows(), ["Zulu.epub", "Alpha.epub"]);
}

/// A write failure during the later index-building phases must not have
/// its retry history erased by successful Inspect/hash/sort slices on the
/// next attempt. Only the retry itself should be delayed.
#[test]
fn late_title_index_write_refusals_increase_restart_backoff() {
    use app_core::storage_loop::{background_retry_delay_ms, owed_work_delay_ms, OwedWork};

    let card = Card::blank();
    card.put("BOOKS/Alpha.epub", b"a");
    card.put("BOOKS/Zulu.epub", b"z");
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "Zulu.epub", "Aardvark");
    let mut device = Device::wake_queued_only(&card);
    assert!(device.store.title_index_requested());

    for failures in 1..=3_u8 {
        // A single write will fail, but only after multiple successful
        // cooperative slices reach the index-file mutation.
        card.disk.refuse_next_writes(1);
        let mut preceding_slices = 0;
        while device
            .task
            .background_attempts(&device.store, device.frame())
            != failures
        {
            assert!(preceding_slices < 1000, "no index-file write was attempted");
            device.step_background();
            if device
                .task
                .background_attempts(&device.store, device.frame())
                == failures
            {
                break;
            }
            preceding_slices += 1;
            assert_eq!(
                device
                    .task
                    .background_attempts(&device.store, device.frame()),
                0,
                "an active job must not back off on each successful slice"
            );
        }
        assert!(
            preceding_slices >= 2,
            "the failure must happen after successful intermediate slices"
        );
        assert!(device.store.title_index_requested());
        assert!(device.task.background_owed(&device.store, device.frame()));
        assert_eq!(
            owed_work_delay_ms(
                OwedWork::BuildSlice,
                device
                    .task
                    .background_attempts(&device.store, device.frame())
            ),
            background_retry_delay_ms(failures)
        );
    }

    // A new generation must not inherit the old generation's failures,
    // even if the previous job has already been discarded.
    device.store.invalidate_title_index();
    device.store.request_title_index();
    card.disk.refuse_read_in(Some(0));
    device.step_background();
    assert_eq!(
        device
            .task
            .background_attempts(&device.store, device.frame()),
        1
    );

    card.disk.refuse_read_in(None);
    device.settle();
    assert_eq!(
        device
            .task
            .background_attempts(&device.store, device.frame()),
        0
    );
    assert!(device.store.title_index_snapshot().is_some());
    device.open_library();
    assert_eq!(device.rows(), ["Zulu.epub", "Alpha.epub"]);
}

/// A failed title-index restart is suspended while a book's pagination
/// walk owns the next background slice. Its retry history must not delay
/// that unrelated walk, but must still apply when Library becomes eligible.
#[test]
fn title_index_retry_backoff_does_not_delay_an_active_book_build() {
    use app_core::storage_loop::{background_retry_delay_ms, owed_work_delay_ms, OwedWork};

    let card = Card::blank();
    card.put("BOOKS/Alpha.epub", b"a");
    card.put("BOOKS/Zulu.epub", b"z");
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "Zulu.epub", "Aardvark");
    let mut device = Device::wake_queued_only(&card);
    assert!(device.store.title_index_requested());
    // Test backoff of the full index reader rather than its new boot-time
    // read-only check: finish metadata discovery with index work suspended.
    device.store.stop_title_index_request();
    for _ in 0..device.store.catalog_count() {
        device.step_background();
    }
    device.store.request_title_index();

    for attempt in 1..=5 {
        card.disk.refuse_read_in(Some(0));
        device.step_background();
        assert_eq!(
            device
                .task
                .background_attempts(&device.store, device.frame()),
            attempt
        );
    }
    card.disk.refuse_read_in(None);
    let mut reading = device.frame().expect("boot produced a frame");
    reading.view = app_core::AppView::Reading;
    storage::task::apply_build_outcome(
        &mut device.task.background_build,
        storage::book_build::BookBuildOutcome::Started,
        reading.book_id,
    );

    let attempts = device
        .task
        .background_attempts(&device.store, Some(reading));
    assert_eq!(attempts, 0, "the book build must not inherit index backoff");
    assert_eq!(
        owed_work_delay_ms(OwedWork::BuildSlice, attempts),
        owed_work_delay_ms(OwedWork::BuildSlice, 0)
    );

    // Once the same suspended book build permits index work in Library,
    // its accumulated refusal history must still govern the index restart.
    reading.view = app_core::AppView::Library;
    let attempts = device
        .task
        .background_attempts(&device.store, Some(reading));
    assert_eq!(attempts, 5);
    assert_eq!(
        owed_work_delay_ms(OwedWork::BuildSlice, attempts),
        background_retry_delay_ms(5)
    );

    // An active build's normal steps never clear the index failure history.
    reading.view = app_core::AppView::Reading;
    assert_eq!(
        device
            .task
            .background_attempts(&device.store, Some(reading)),
        0
    );
}

#[test]
fn invalid_catalog_stops_title_index_retries() {
    let card = Card::blank();
    card.put("BOOKS/Alpha.epub", b"a");
    card.put("BOOKS/Zulu.epub", b"z");
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "Zulu.epub", "Aardvark");

    let mut device = Device::wake_queued_only(&card);
    assert!(device.store.title_index_requested());
    // Finish metadata discovery without the boot-time index validation,
    // so the next slice tests a full index reader against a corrupt catalog.
    device.store.stop_title_index_request();
    for _ in 0..device.store.catalog_count() {
        device.step_background();
    }
    device.store.request_title_index();
    let mut catalog = card.read("READER/CATALOG.BIN").unwrap();
    catalog[0] ^= 1;
    card.delete("READER/CATALOG.BIN");
    card.put_short("READER/CATALOG.BIN", &catalog);

    device.step_background();
    assert!(!device.store.title_index_requested());
    assert_eq!(
        device
            .task
            .background_attempts(&device.store, device.frame()),
        0
    );
}

#[test]
fn transient_adoption_read_failure_keeps_ready_snapshot_for_retry() {
    let card = Card::blank();
    card.put("BOOKS/Alpha.epub", b"x");
    card.put("BOOKS/Zulu.epub", b"x");
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "Zulu.epub", "Aardvark");
    let mut device = Device::wake_queued_only(&card);
    device.press_only(app_core::Button::Back);
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
    device.store.begin_folder_page(0);
    card.disk.refuse_read_in(Some(0));
    device.run_queued();
    assert!(device.saw(|event| matches!(event, app_core::LibraryEvent::RowFailed { .. })));
    assert!(device.store.title_index_requested());
    assert!(device.store.title_index_snapshot().is_none());
    card.disk.refuse_read_in(None);
    // No unrelated command is needed to reannounce a retained ready index.
    assert!(device.task.background_owed(&device.store, device.frame()));
    assert!(
        device
            .task
            .background_attempts(&device.store, device.frame())
            > 0
    );
    device.step_background();
    device.run_queued();
    device.settle();
    assert!(device.store.title_index_snapshot().is_some());
    assert_eq!(
        device.store.folder_row(0).unwrap().display_label.as_str(),
        "Aardvark"
    );
}

#[test]
fn corrupt_index_cannot_retarget_a_previous_title_order_selection() {
    let card = Card::blank();
    for n in 0..24 {
        card.put(&format!("BOOKS/Book {n:02}.epub"), b"x");
    }
    drop(Device::wake(&card));
    for n in 0..24 {
        persist_title(
            &card,
            BookRoot::Library,
            &format!("Book {n:02}.epub"),
            &format!("Title {:02}", 23 - n),
        );
    }
    let mut device = Device::wake(&card);
    device.open_library();
    storage::library_sd::ensure_folder_page(&mut device.card, &mut device.store, 0, true);
    // Select the first book in title order, not filename order.
    assert_eq!(
        device.store.folder_row(0).unwrap().name.as_str(),
        "Book 23.epub"
    );
    let snapshot = device
        .store
        .title_index_snapshot()
        .and_then(storage::title_index_disk::Snapshot::from_bytes)
        .unwrap();
    let path = if snapshot.slot() == 0 {
        "READER/TITLEIDX.BIN"
    } else {
        "READER/TITLEID2.BIN"
    };
    let mut bytes = card.read(path).unwrap();
    let start = storage::title_index_disk::SNAPSHOT_BYTES
        + if bytes[6] == 0 {
            0
        } else {
            usize::from(snapshot.count()) * 2
        };
    bytes[start] ^= 1;
    card.delete(path);
    card.put_short(path, &bytes);
    // Force the indexed page to refill with the selected book still resident.
    device.store.begin_folder_page(0);
    storage::library_sd::ensure_folder_page(&mut device.card, &mut device.store, 0, true);
    assert!(device.store.title_fallback_pending());
    assert!(device.store.folder_rows().is_empty());
    // First background slice commits filename-order fallback and tells the
    // app which row is selected before any address becomes usable.
    device.step_background();
    assert_eq!(device.app.selection, 0);
    assert_eq!(
        device.store.folder_row(0).unwrap().name.as_str(),
        "Book 00.epub"
    );
    // Once the new title index is published, adoption follows the same
    // physical book from filename position 0 to title position 23.
    device.settle();
    assert_eq!(device.app.selection, 23);
    assert_eq!(
        device.store.folder_row(23).unwrap().name.as_str(),
        "Book 00.epub"
    );
}

// An invalid index discovered while relisting (not crossing a page) must
// honor an explicit physical-book landing rather than its old numeric rank.
#[test]
fn corrupted_index_during_relist_relands_on_the_same_book() {
    let card = Card::blank();
    card.put("BOOKS/A.epub", b"a");
    card.put("BOOKS/Z.epub", b"z");
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "A.epub", "Zulu");
    persist_title(&card, BookRoot::Library, "Z.epub", "Alpha");
    let mut device = Device::wake(&card);
    device.open_library();
    storage::library_sd::ensure_folder_page(&mut device.card, &mut device.store, 0, true);
    assert_eq!(device.store.folder_row(0).unwrap().name.as_str(), "Z.epub");
    let old_epoch = device.store.browse_epoch();
    let snapshot = device
        .store
        .title_index_snapshot()
        .and_then(storage::title_index_disk::Snapshot::from_bytes)
        .unwrap();
    let path = if snapshot.slot() == 0 {
        "READER/TITLEIDX.BIN"
    } else {
        "READER/TITLEID2.BIN"
    };
    let mut bytes = card.read(path).unwrap();
    let run_start = storage::title_index_disk::SNAPSHOT_BYTES
        + if bytes[6] == 0 {
            0
        } else {
            usize::from(snapshot.count()) * 2
        };
    bytes[run_start] ^= 1;
    card.delete(path);
    card.put_short(path, &bytes);
    let selected = proto::library_path::LibraryPath::parse("Z.epub").unwrap();
    let listed = card
        .session(|root| {
            reader_cache::browse::relist_on_book_sized(
                &mut device.store,
                root,
                true,
                BookRoot::Library,
                &selected,
                1,
            )
        })
        .expect("corruption falls back to the live folder");
    assert_eq!(listed.selection, 1);
    assert_eq!(device.store.folder_row(1).unwrap().name.as_str(), "Z.epub");
    assert_eq!(device.store.browse_epoch(), old_epoch + 1);
    assert!(device.store.title_index_snapshot().is_none());
}

// When the selected book is not resident, no physical identity can be
// established; fallback must land at a neutral cursor rather than reusing
// the old title-sorted row number.
#[test]
fn corrupt_index_relist_without_a_resident_anchor_starts_at_top() {
    let card = Card::blank();
    card.put("BOOKS/A.epub", b"a");
    card.put("BOOKS/Z.epub", b"z");
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "A.epub", "Zulu");
    persist_title(&card, BookRoot::Library, "Z.epub", "Alpha");
    let mut device = Device::wake(&card);
    device.open_library();
    storage::library_sd::ensure_folder_page(&mut device.card, &mut device.store, 0, true);
    let snapshot = device
        .store
        .title_index_snapshot()
        .and_then(storage::title_index_disk::Snapshot::from_bytes)
        .unwrap();
    let path = if snapshot.slot() == 0 {
        "READER/TITLEIDX.BIN"
    } else {
        "READER/TITLEID2.BIN"
    };
    let mut bytes = card.read(path).unwrap();
    let run_start = storage::title_index_disk::SNAPSHOT_BYTES
        + if bytes[6] == 0 {
            0
        } else {
            usize::from(snapshot.count()) * 2
        };
    bytes[run_start] ^= 1;
    card.delete(path);
    card.put_short(path, &bytes);
    // Selection 1 referred to A.epub in title order. Without its
    // resident row we cannot carry that numeric address into filename order.
    let _ = device
        .store
        .browse_mut()
        .choose(1, "A.epub", app_core::browse::Row::Book);
    device.store.begin_folder_page(0);
    let listed = card
        .session(|root| reader_cache::browse::list_here(&mut device.store, root, true))
        .expect("filename fallback without a selected book");
    assert_eq!(listed.selection, 0);
    assert_eq!(device.store.folder_row(0).unwrap().name.as_str(), "A.epub");
}

// One transient card fault while recovering a corrupt page must not require
// a user press to recover the library.
#[test]
fn failed_fallback_relist_retries_in_background_without_another_command() {
    let card = Card::blank();
    card.put("BOOKS/Book.epub", b"x");
    let mut device = Device::wake(&card);
    device.open_library();
    let snapshot = device
        .store
        .title_index_snapshot()
        .and_then(storage::title_index_disk::Snapshot::from_bytes)
        .unwrap();
    let path = if snapshot.slot() == 0 {
        "READER/TITLEIDX.BIN"
    } else {
        "READER/TITLEID2.BIN"
    };
    let mut bytes = card.read(path).unwrap();
    let run_start = storage::title_index_disk::SNAPSHOT_BYTES
        + if bytes[6] == 0 {
            0
        } else {
            usize::from(snapshot.count()) * 2
        };
    bytes[run_start] ^= 1;
    card.delete(path);
    card.put_short(path, &bytes);
    device.store.begin_folder_page(0);
    storage::library_sd::ensure_folder_page(&mut device.card, &mut device.store, 0, true);
    assert!(device.store.title_fallback_pending());
    card.disk.refuse_read_in(Some(0));
    device.step_background();
    assert!(device.store.title_fallback_pending());
    assert!(
        device
            .task
            .background_attempts(&device.store, device.frame())
            > 0
    );
    assert!(device.task.background_owed(&device.store, device.frame()));
    card.disk.refuse_read_in(None);
    device.step_background();
    assert!(!device.store.title_fallback_pending());
    assert_eq!(
        device
            .task
            .background_attempts(&device.store, device.frame()),
        0
    );
    assert!(device.saw(|event| matches!(
        event,
        app_core::LibraryEvent::FolderListed {
            request_id: None,
            ..
        }
    )));
    assert_eq!(
        device.store.folder_row(0).unwrap().name.as_str(),
        "Book.epub"
    );
}

/// If the card refuses a read while relisting a selected physical book,
/// the recovery slice must not retry with a filename-order top row. Keep the
/// same anchor across the refusal and publish it on the next healthy slice.
#[test]
fn anchored_corrupt_index_recovery_preserves_selected_book_after_read_fault() {
    let card = Card::blank();
    card.put("BOOKS/A.epub", b"a");
    card.put("BOOKS/Z.epub", b"z");
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "A.epub", "Zulu");
    persist_title(&card, BookRoot::Library, "Z.epub", "Alpha");

    let mut device = Device::wake(&card);
    device.open_library();
    assert_eq!(device.store.folder_row(0).unwrap().name.as_str(), "Z.epub");
    let selected = device.store.folder_row(0).unwrap().clone();
    device.store.title_page_corrupt(Some(selected.clone()));
    assert!(device.store.title_fallback_pending());

    // Mirror background_step: release the fence during the attempted
    // relist, then restore it when the attempt refuses the read.
    device.store.set_title_fallback_pending(false);
    // Arm the fault after opening the card root: the next read belongs to
    // the anchored relist, not to mounting the card.
    let result = card.session(|root| {
        card.disk.refuse_read_in(Some(0));
        reader_cache::browse::relist_on_anchor(&mut device.store, root, true, &selected)
    });
    assert_eq!(result, reader_cache::browse::AnchorRelist::Unreadable);
    assert!(!card.disk.read_refusal_armed());
    device
        .store
        .restore_title_fallback_after_failed_relist(Some(selected));
    assert_eq!(
        device.store.title_fallback_anchor().unwrap().name.as_str(),
        "Z.epub"
    );

    // Force the actual background recovery attempt to refuse its first
    // in-session read, after the volume and root have already mounted.
    // A mount failure alone would also return None but exercise no anchor.
    device.card.refuse_one_read_after_mount();
    let event_start = device.log.len();
    device.step_background();
    assert!(!card.disk.read_refusal_armed());
    assert!(device.store.title_fallback_pending());
    assert!(
        !device.log[event_start..]
            .iter()
            .any(|event| matches!(event, app_core::LibraryEvent::FolderListed { .. })),
        "a refused anchored read must not publish a different selection"
    );
    assert!(
        device
            .task
            .background_attempts(&device.store, device.frame())
            > 0
    );

    card.disk.refuse_read_in(None);
    device.step_background();
    assert!(!device.store.title_fallback_pending());
    assert_eq!(device.app.selection, 1);
    assert_eq!(device.store.folder_row(1).unwrap().name.as_str(), "Z.epub");
}

/// A second unsolicited root relist must not replace or retire the
/// physical anchor while the dedicated corruption recovery is still owed.
#[test]
fn generic_relist_does_not_bypass_pending_title_fallback() {
    let card = Card::blank();
    card.put("BOOKS/A.epub", b"a");
    card.put("BOOKS/Z.epub", b"z");
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "A.epub", "Zulu");
    persist_title(&card, BookRoot::Library, "Z.epub", "Alpha");

    let mut device = Device::wake(&card);
    device.open_library();
    let anchor = device
        .store
        .folder_row(0)
        .expect("selected title row")
        .clone();
    assert_eq!(anchor.name.as_str(), "Z.epub");
    device.store.title_page_corrupt(Some(anchor));
    let pending_epoch = device.store.browse_epoch();
    assert!(device.store.title_fallback_pending());

    storage::task::relist_library_folder(
        &mut device.card,
        &mut device.host,
        &mut device.store,
        true,
    );
    assert!(device.store.title_fallback_pending());
    assert_eq!(device.store.browse_epoch(), pending_epoch);
    assert_eq!(
        device.store.title_fallback_anchor().unwrap().name.as_str(),
        "Z.epub"
    );
    assert!(device.host.events.is_empty(), "no unsolicited root listing");

    device.step_background();
    assert!(!device.store.title_fallback_pending());
    assert_eq!(device.app.selection, 1);
    assert_eq!(device.store.folder_row(1).unwrap().name.as_str(), "Z.epub");
}

/// When a nested folder disappears while title-index fallback is owed,
/// recover at the Library root with a fresh browse epoch, without a press or
/// catalog rescan. Its selected book cannot be relisted in a deleted folder.
#[test]
fn missing_current_folder_recovery_returns_to_library_root() {
    let card = Card::blank();
    card.put("BOOKS/A.epub", b"a");
    card.put("BOOKS/Fiction/Nested/Selected.epub", b"x");
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Fiction");
    device.choose("Nested");
    assert_eq!(device.app.library_depth, 2);
    assert_eq!(device.store.browse().path().as_str(), "Fiction/Nested");
    let anchor = device.store.folder_row(0).expect("selected book").clone();
    assert_eq!(anchor.name.as_str(), "Selected.epub");
    device.store.title_page_corrupt(Some(anchor));
    let old_epoch = device.store.browse_epoch();

    card.delete("BOOKS/Fiction/Nested/Selected.epub");
    card.remove_folder("BOOKS/Fiction/Nested");

    let old_events = device.log.len();
    device.step_background();
    assert!(!device.store.title_fallback_pending());
    assert!(device.store.browse().is_root());
    assert_eq!(device.app.library_depth, 0);
    assert_eq!(device.app.selection, 0);
    assert_eq!(device.store.browse_epoch(), device.app.library_browse_epoch);
    assert!(device.store.browse_epoch() > old_epoch);
    assert_eq!(device.store.folder_row(0).unwrap().name.as_str(), "A.epub");
    assert!(device.log[old_events..].iter().any(|event| matches!(
        event,
        app_core::LibraryEvent::FolderListed {
            request_id: None,
            depth: 0,
            ..
        }
    )));
    assert!(!device.log[old_events..]
        .iter()
        .any(|event| matches!(event, app_core::LibraryEvent::Scanned { .. })));
}

/// The same deleted-folder recovery applies when the corrupted page had no
/// resident book to identify: there is no physical anchor to fall back to.
#[test]
fn missing_current_folder_without_anchor_returns_to_root() {
    let card = Card::blank();
    card.put("BOOKS/A.epub", b"a");
    card.put("BOOKS/Fiction/Nested/Selected.epub", b"x");
    let mut device = Device::wake(&card);
    device.open_library();
    device.choose("Fiction");
    device.choose("Nested");
    assert_eq!(device.app.library_depth, 2);
    device.store.title_page_corrupt(None);
    assert!(device.store.title_fallback_anchor().is_none());
    card.delete("BOOKS/Fiction/Nested/Selected.epub");
    card.remove_folder("BOOKS/Fiction/Nested");

    device.step_background();
    assert!(!device.store.title_fallback_pending());
    assert!(device.store.browse().is_root());
    assert_eq!(device.app.library_depth, 0);
    assert_eq!(device.app.library_browse_epoch, device.store.browse_epoch());
    assert_eq!(device.store.folder_row(0).unwrap().name.as_str(), "A.epub");
}

/// A complete live directory walk may prove an old selected book was
/// removed; in that case the library can safely reset to the filename top.
#[test]
fn anchored_corrupt_index_recovery_resets_only_when_book_missing() {
    let card = Card::blank();
    card.put("BOOKS/A.epub", b"a");
    card.put("BOOKS/Z.epub", b"z");
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "A.epub", "Zulu");
    persist_title(&card, BookRoot::Library, "Z.epub", "Alpha");

    let mut device = Device::wake(&card);
    device.open_library();
    let selected = device.store.folder_row(0).unwrap().clone();
    assert_eq!(selected.name.as_str(), "Z.epub");
    device.store.title_page_corrupt(Some(selected));
    card.delete("BOOKS/Z.epub");

    device.step_background();
    assert!(!device.store.title_fallback_pending());
    assert_eq!(device.app.selection, 0);
    assert_eq!(device.store.folder_row(0).unwrap().name.as_str(), "A.epub");
}

// A resident page alone is not evidence of the app's current selection:
// Browse::selection can lag behind navigation performed by the app reducer.
#[test]
fn corrupt_index_plain_relist_never_infers_an_implicit_book_anchor() {
    let card = Card::blank();
    card.put("BOOKS/A.epub", b"a");
    card.put("BOOKS/Z.epub", b"z");
    drop(Device::wake(&card));
    persist_title(&card, BookRoot::Library, "A.epub", "Zulu");
    persist_title(&card, BookRoot::Library, "Z.epub", "Alpha");
    let mut device = Device::wake(&card);
    device.open_library();
    storage::library_sd::ensure_folder_page(&mut device.card, &mut device.store, 0, true);
    assert_eq!(device.store.folder_row(0).unwrap().name.as_str(), "Z.epub");
    let snapshot = device
        .store
        .title_index_snapshot()
        .and_then(storage::title_index_disk::Snapshot::from_bytes)
        .unwrap();
    let path = if snapshot.slot() == 0 {
        "READER/TITLEIDX.BIN"
    } else {
        "READER/TITLEID2.BIN"
    };
    let mut bytes = card.read(path).unwrap();
    let run_start = storage::title_index_disk::SNAPSHOT_BYTES
        + if bytes[6] == 0 {
            0
        } else {
            usize::from(snapshot.count()) * 2
        };
    bytes[run_start] ^= 1;
    card.delete(path);
    card.put_short(path, &bytes);
    let old_epoch = device.store.browse_epoch();
    let listed = card
        .session(|root| reader_cache::browse::list_here(&mut device.store, root, true))
        .expect("corruption falls back to the live folder");
    assert_eq!(listed.selection, 0);
    assert_eq!(device.store.folder_row(0).unwrap().name.as_str(), "A.epub");
    assert_eq!(device.store.browse_epoch(), old_epoch + 1);
}

/// An invalid index can retire row addresses while a parent-folder listing
/// is under way. If the live fallback page then refuses a read, RowFailed
/// alone cannot tell the app about that new browse generation. The failed
/// move must be rolled back and fenced until a background relist delivers it.
#[test]
fn failed_leave_after_index_invalidation_resynchronizes_without_rescan() {
    let card = Card::blank();
    card.put("BOOKS/A.epub", b"a");
    card.put("BOOKS/Z.epub", b"z");
    card.put("BOOKS/Folder/Inner.epub", b"inner");

    // Sweep a single transient block refusal through the parent relist.
    // Only a refusal after the index proof has been invalidated exercises
    // the failed-move rollback; earlier refusals leave the old epoch intact.
    for reads_before_refusal in 0..160 {
        let mut device = Device::wake(&card);
        device.open_library();
        device.choose("Folder");
        assert_eq!(device.app.library_depth, 1);
        let epoch_before = device.store.browse_epoch();
        assert_eq!(device.app.library_browse_epoch, epoch_before);

        let snapshot = device
            .store
            .title_index_snapshot()
            .and_then(storage::title_index_disk::Snapshot::from_bytes)
            .expect("the folder has an indexed parent");
        let path = if snapshot.slot() == 0 {
            "READER/TITLEIDX.BIN"
        } else {
            "READER/TITLEID2.BIN"
        };
        let valid = card.read(path).expect("the index is on the card");
        let mut corrupt = valid.clone();
        let count = usize::from(snapshot.count());
        // TIX4 keeps the two prefix tables after both sorted runs; only
        // the selected prefix copy belongs to the active run.
        let prefix_start = storage::title_index_disk::SNAPSHOT_BYTES
            + count * 4
            + if valid[6] == 0 { 0 } else { (count + 1) * 16 };
        corrupt[prefix_start] ^= 1;
        card.delete(path);
        card.put_short(path, &corrupt);

        let before_events = device.log.len();
        card.disk.refuse_read_in(Some(reads_before_refusal));
        device.press_only(app_core::Button::Back);
        device.run_queued();
        let hit = device.log[before_events..]
            .iter()
            .any(|event| matches!(event, app_core::LibraryEvent::RowFailed { .. }))
            && device.store.browse_epoch() != epoch_before;
        card.disk.refuse_read_in(None);

        // Each attempt begins with an intact on-card index. An in-memory
        // snapshot from this attempt is enough to detect the injected tear.
        card.delete(path);
        card.put_short(path, &valid);

        if !hit {
            continue;
        }

        assert_eq!(device.app.library_depth, 1, "the failed leave rolled back");
        assert_eq!(device.store.browse().path().as_str(), "Folder");
        assert_eq!(
            device.app.library_browse_epoch, epoch_before,
            "RowFailed has no replacement epoch"
        );
        assert!(
            device.store.title_fallback_pending(),
            "old row addresses must stay fenced until the app is informed"
        );
        assert!(device.store.folder_rows().is_empty());
        assert!(device.task.background_owed(&device.store, device.frame()));

        // No new press, rescan, or index rebuild is required to regain a
        // usable folder and a matching browse epoch.
        device.step_background();
        assert!(!device.store.title_fallback_pending());
        assert_eq!(device.app.library_browse_epoch, device.store.browse_epoch());
        assert_eq!(device.app.library_depth, 1);
        assert!(
            !device.log[before_events..]
                .iter()
                .any(|event| matches!(event, app_core::LibraryEvent::Scanned { .. })),
            "recovery did not rescan the catalog"
        );
        device.press(app_core::Button::Back);
        assert_eq!(device.app.library_depth, 0, "navigation works again");
        return;
    }
    panic!("the read-fault sweep did not hit a failed post-invalidation page");
}

/// The adoption relist can retire an old indexed order before a transient
/// card fault prevents the replacement filename page from loading. RowFailed
/// does not carry the new epoch; background recovery must publish one before
/// accepting further row addresses.
#[test]
fn failed_adoption_after_order_retirement_recovers_without_another_command() {
    let card = Card::blank();
    card.put("BOOKS/A.epub", b"a");
    card.put("BOOKS/Z.epub", b"z");
    card.put("BOOKS/Nested/Inner.epub", b"inner");

    // The single-read fault is injected at each possible point in adoption.
    // Only a failure *after* the title-to-filename transition exercises this
    // particular rollback. Earlier failures are distinct transient cases.
    for reads_before_refusal in 0..160 {
        let mut device = Device::wake(&card);
        device.open_library();
        assert!(device.store.title_folder_range().is_some());
        let old_epoch = device.store.browse_epoch();
        assert_eq!(device.app.library_browse_epoch, old_epoch);

        // Request revalidation of the already published index, leaving the
        // old title-order folder page in place while a ready replacement is
        // announced to the app. Do not run the adoption command yet.
        device.store.request_title_index();
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

        // The catalog/index is still valid but the live folder has changed,
        // so adoption's title_range must switch to filename order and retire
        // the browse generation before attempting the new page read.
        card.put("BOOKS/Extra.epub", b"extra");
        let before_events = device.log.len();
        card.disk.refuse_read_in(Some(reads_before_refusal));
        device.run_queued();
        let hit = device.log[before_events..]
            .iter()
            .any(|event| matches!(event, app_core::LibraryEvent::RowFailed { .. }))
            && device.store.browse_epoch() != old_epoch;
        card.disk.refuse_read_in(None);

        if hit {
            assert_eq!(device.app.library_browse_epoch, old_epoch);
            assert!(device.store.title_fallback_pending());
            assert!(device.store.folder_rows().is_empty());
            assert!(device.task.background_owed(&device.store, device.frame()));

            // Restore the original live folder while the card is readable,
            // avoiding an unrelated stale-catalog scan during verification.
            card.delete("BOOKS/Extra.epub");
            // A single background slice produces the only event carrying the
            // new generation, without a user press or catalog rescan.
            device.step_background();
            assert!(!device.store.title_fallback_pending());
            assert_eq!(device.app.library_browse_epoch, device.store.browse_epoch());
            assert!(device.log[before_events..].iter().any(|event| matches!(
                event,
                app_core::LibraryEvent::FolderListed {
                    request_id: None,
                    ..
                }
            )));
            assert!(!device.log[before_events..]
                .iter()
                .any(|event| matches!(event, app_core::LibraryEvent::Scanned { .. })));
            // Navigation from the new listing must no longer fail its epoch
            // check, even though its previous adoption failed.
            device.choose("Nested");
            assert_eq!(device.app.library_depth, 1);
            assert_eq!(device.rows(), ["Inner.epub"]);
            return;
        }
        card.delete("BOOKS/Extra.epub");
    }
    panic!("fault sweep never hit adoption after index-order retirement");
}
