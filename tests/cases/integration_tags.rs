// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use std::fs::File;
use tempfile::tempdir;
use ttfm::types::ItemId;
use ttfm::{search, tagging};

define_cases! {
    integration_file_tagging: {
        setup: |dir| {
            File::create(dir.join("doc.txt"))?;
            Ok(())
        },
        tags: &[("doc.txt", "status:reviewed")],
        format_query: super::default_scope,
        query: "status:reviewed",
        assert: |res, dir| {
            assert_eq!(res.results.len(), 1);
            assert!(
                res.results[0].primary_value().unwrap_or_default().contains("doc.txt"),
                "Expected doc.txt in result, got: {:?}",
                res.results[0].primary_value()
            );
            let _ = dir;
            Ok(())
        },
    },
    integration_note_tagging: {
        setup: |_dir| Ok(()),
        modify: Some(|store, registry, _dir| {
            tagging::add_item(store, registry, "note", "Meeting Memo")?;
            ttfm::edit::edit(
                store,
                registry,
                "item_kind:note",
                Some("category:meeting"),
                ttfm::edit::QueryType::Tag,
                None,
                ttfm::edit::WriteOptions { yes: true },
                &mut Vec::new(),
            )?;
            Ok(())
        }),
        format_query: |q, _| q.to_string(),
        query: "category:meeting & item_kind:note",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1);
            assert_eq!(res.results[0].item_kind, ttfm::ItemKind::Note);
            assert_eq!(res.results[0].primary_value().unwrap_or_default(), "Meeting Memo");
            Ok(())
        },
    },
}

// ──────────────────────────────────────────────
// 複雑なテスト (define_cases! 移行不可)
// ──────────────────────────────────────────────

#[test]
fn test_integration_tag_tagging() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .initialize_tables()
        .unwrap();

    let file_path = dir.path().join("dummy.txt");
    File::create(&file_path).unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .run_single(dir.path(), None::<&fn(usize)>, false)
        .unwrap();
    let registered_paths = search::search_nowarn(
        &store,
        &registry,
        "extension:txt",
        Default::default(),
    )
    .unwrap();
    let item_id = registered_paths.results[0].id.as_i64();

    super::tag_item_id(&store, &registry, item_id, "project:mars").unwrap();

    let tag_id =
        tagging::get_or_create_item(&store, &registry, "tag", "project:mars")
            .unwrap();
    super::tag_item_id(&store, &registry, tag_id, "priority:high").unwrap();

    let results = search::search_nowarn(
        &store,
        &registry,
        "priority:high & item_kind:tag",
        Default::default(),
    )
    .unwrap();
    assert_eq!(results.results.len(), 1);
    assert_eq!(results.results[0].id, ItemId::from(tag_id));
    assert_eq!(results.results[0].primary_value().unwrap(), "project:mars");

    let file_results = search::search_nowarn(
        &store,
        &registry,
        "project:mars",
        Default::default(),
    )
    .unwrap();
    assert!(file_results
        .results
        .iter()
        .any(|r| r.item_kind == ttfm::ItemKind::File));
}

#[test]
fn test_system_item_metadata_integration() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .initialize_tables()
        .unwrap();

    File::create(dir.path().join("test.rs")).unwrap();
    File::create(dir.path().join("no_ext")).unwrap();

    ttfm::indexing::Indexer::new(&store, &registry)
        .run_single(dir.path(), None::<&fn(usize)>, false)
        .unwrap();

    let ext_list = search::search_nowarn(
        &store,
        &registry,
        "type:extension",
        Default::default(),
    )
    .unwrap();
    assert!(
        !ext_list
            .results
            .iter()
            .any(|r| r.raw_repr() == "extension:"),
        "Empty extension tag should not exist"
    );

    let results_physical = search::search_nowarn(
        &store,
        &registry,
        "item_kind:tag & label:rs",
        Default::default(),
    )
    .unwrap();
    assert!(
        results_physical.results.is_empty(),
        "Physical tag item should NOT be created automatically"
    );

    let results_proj = search::search_nowarn(
        &store,
        &registry,
        "extension:",
        Default::default(),
    )
    .unwrap();
    assert!(
        !results_proj.results.is_empty(),
        "Should find label items via projection"
    );
    let rs_label = results_proj
        .results
        .iter()
        .find(|r| r.raw_repr() == "rs")
        .expect("rs label not found");
    assert_eq!(
        rs_label.item_kind,
        ttfm::ItemKind::Volatile,
        "Should be a label item"
    );
    let has_test_rs = rs_label
        .tags
        .entries
        .iter()
        .any(|entry| entry.typed_tag.as_str().contains("test.rs"));
    assert!(has_test_rs, "rs label should contain reference to test.rs");
}
