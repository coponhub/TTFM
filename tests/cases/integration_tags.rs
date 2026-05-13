use std::fs::File;
use tempfile::tempdir;
use ttfm::types::ItemId;
use ttfm::FileManager;

define_cases! {
    integration_file_tagging: {
        setup: |dir| {
            File::create(dir.join("doc.txt"))?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            let query = format!("extension:txt & path:{}/*", dir.to_string_lossy());
            let res = fm.search(&query, Default::default())?;
            anyhow::ensure!(!res.results.is_empty(), "No txt file found in case dir");
            let item = res.results[0].primary_value().unwrap_or_default();
            fm.tag_item(&item, "status:reviewed")?;
            Ok(())
        }),
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
        modify: Some(|fm, _dir| {
            let note_id = fm.add_item("note", "Meeting Memo")?;
            fm.tag_item(&note_id.to_string(), "category:meeting")?;
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
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    let file_path = dir.path().join("dummy.txt");
    File::create(&file_path).unwrap();
    fm.index_directory(dir.path(), None::<&fn(usize)>, false)
        .unwrap();
    let registered_paths =
        fm.search("extension:txt", Default::default()).unwrap();
    let item = registered_paths.results[0].primary_value().unwrap();

    fm.tag_item(&item, "project:mars").unwrap();

    let tag_id = fm.get_or_create_item("tag", "project:mars").unwrap();
    fm.tag_item(&tag_id.to_string(), "priority:high").unwrap();

    let results = fm
        .search("priority:high & item_kind:tag", Default::default())
        .unwrap();
    assert_eq!(results.results.len(), 1);
    assert_eq!(results.results[0].id, ItemId::from(tag_id));
    assert_eq!(results.results[0].primary_value().unwrap(), "project:mars");

    let file_results = fm.search("project:mars", Default::default()).unwrap();
    assert!(file_results
        .results
        .iter()
        .any(|r| r.item_kind == ttfm::ItemKind::File));
}

#[test]
fn test_system_item_metadata_integration() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    File::create(dir.path().join("test.rs")).unwrap();
    File::create(dir.path().join("no_ext")).unwrap();

    fm.index_directory(dir.path(), None::<&fn(usize)>, false)
        .unwrap();

    let ext_list = fm.search("type:extension", Default::default()).unwrap();
    assert!(
        !ext_list.results.iter().any(|r| r.raw_repr() == "extension:"),
        "Empty extension tag should not exist"
    );

    let results_physical = fm
        .search("item_kind:tag & label:rs", Default::default())
        .unwrap();
    assert!(
        results_physical.results.is_empty(),
        "Physical tag item should NOT be created automatically"
    );

    let results_proj = fm.search("extension:", Default::default()).unwrap();
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
        .any(|entry| entry.label.as_str().contains("test.rs"));
    assert!(has_test_rs, "rs label should contain reference to test.rs");
}
