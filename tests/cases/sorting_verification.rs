use tempfile::tempdir;
use ttfm::db::TargetTable;
use ttfm::FileManager;

#[test]
fn test_parquet_physical_order() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // Create files ensures correct sorting check
    // 1. a.rs -> extension:rs
    // 2. z.txt -> extension:txt
    // type 'extension' is same. label_str 'rs' < 'txt'.
    std::fs::write(root.join("z.txt"), "content").unwrap();
    std::fs::write(root.join("a.rs"), "content").unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // Verify base_tags.parquet order
    let path = fm.path_for_target(TargetTable::BaseTags);

    // Read raw rows without ORDER BY
    // DuckDB read_parquet typically follows physical order.
    // We extract type and label_str.
    let rows: Vec<(String, Option<String>)> = fm
        .get_connection()
        .prepare(&format!(
            "SELECT type, label_str FROM read_parquet('{}')",
            path.to_string_lossy()
        ))
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert!(!rows.is_empty(), "Should have base tags");

    // Check if sorted manually
    let mut is_sorted = true;
    for i in 0..rows.len() - 1 {
        let current = &rows[i];
        let next = &rows[i + 1];
        if current > next {
            is_sorted = false;
            println!("Unsorted at index {}: {:?} > {:?}", i, current, next);
            break;
        }
    }

    assert!(
        is_sorted,
        "Physical order in BaseTags parquet must be sorted: {:?}",
        rows
    );
}
