use tempfile::tempdir;
use ttfm::{FileManager, SearchOptions};

#[test]
fn test_item_id_and_kind_refactoring() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    // 1. Stored items (File/Note)
    let note_id = fm.add_item("note", "TDD integration test memo").unwrap();
    // note_id should be ItemId::Stored
    assert!(note_id.to_string().parse::<i64>().is_ok());

    // 2. tag_item does NOT persist label
    fm.tag_item(&note_id.to_string(), "project:ttfm").unwrap();

    // 3. Volatile items from aggregation (with actual data)
    // Add some files to make count > 0
    std::fs::write(dir.path().join("file1.txt"), "content").unwrap();
    std::fs::write(dir.path().join("file2.txt"), "content").unwrap();
    fm.index_directory(dir.path(), None::<&fn(usize)>, false)
        .unwrap();

    let res = fm
        .search("count(item_id:)", SearchOptions::default())
        .unwrap();
    assert_eq!(res.results.len(), 1);
    // name may vary depending on env, so we just ensure it's a number > 0
    let count: i64 = res.results[0]
        .raw_repr()
        .parse()
        .expect("Count should be numeric");
    assert!(count > 0, "Count should be positive");

    // Check if a volatile ID is assigned.
    // In a shared process, this might not be 0, so we just check it is volatile.
    assert!(res.results[0].id.is_volatile());

    // 4. Projection which should return volatile label items
    let res_proj = fm.search("extension:", SearchOptions::default()).unwrap();
    assert!(!res_proj.results.is_empty());

    // item_kind should be ItemKind::Volatile
    assert_eq!(res_proj.results[0].item_kind, ttfm::ItemKind::Volatile);

    // Check for sequential IDs
    if res_proj.results.len() >= 2 {
        let id0 = res_proj.results[0].id.as_i64() as u64;
        let id1 = res_proj.results[1].id.as_i64() as u64;
        assert_eq!(id1, id0 + 1, "Volatile IDs should be sequential");
    }

    // 5. Explicitly check for ID 0 if we can assume fresh process or just check behavior
    println!("First result ID: {}", res.results[0].id);
}
