/// 集約機能 (Aggregation) の統合テスト
///
/// テスト対象:
/// - `count(query)`: アイテム数をカウント
/// - `count(projection:)`: ユニークラベル数をカウント
/// - `sum(projection:)`, `avg(projection:)`, `max(projection:)`, `min(projection:)`: 数値集計
/// - 集約比較: `sum(size:) > 1GB` 等の真偽値返却
use path_slash::PathExt;
use tempfile::tempdir;
use ttfm::FileManager;

/// count(query) - アイテム数をカウント
#[test]
fn test_count_items() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // テストファイル作成
    std::fs::write(root.join("a.txt"), "a")?;
    std::fs::write(root.join("b.txt"), "b")?;
    std::fs::write(root.join("c.rs"), "c")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // count(extension:txt) -> 2 (a.txt, b.txt)
    let res = fm.search("count(extension:txt)", Default::default())?;

    // スカラ結果が含まれていることを確認
    assert!(!res.results.is_empty());
    assert_eq!(res.results[0].name, "2");

    Ok(())
}

/// count(projection:) - ユニークラベル数をカウント
#[test]
fn test_count_unique_labels() -> anyhow::Result<()> {
    let temp_parent = tempdir()?;
    let root = temp_parent.path().join("root");
    let db_dir = temp_parent.path().join("db");
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&db_dir)?;

    std::fs::write(root.join("a.txt"), "")?;
    std::fs::write(root.join("b.txt"), "")?;
    std::fs::write(root.join("c.rs"), "")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&root, None::<&fn(usize)>, false)?;

    // count(extension:) -> 2 (txt, rs)
    // ノイズ（隠しファイル等）を避けるため、作成したファイルがあるディレクトリに限定
    let root_str = root.to_slash_lossy();
    let res = fm.search(&format!("count(parentdir:\"{}\" & extension:)", root_str), Default::default())?;
    assert_eq!(res.results[0].name, "2");

    Ok(())
}

/// sum(projection:) - 数値の合計
#[test]
fn test_sum_projection() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // サイズの異なるファイルを作成
    std::fs::write(root.join("small.txt"), vec![0u8; 100])?;
    std::fs::write(root.join("large.txt"), vec![0u8; 1000])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(size:) は合計サイズ (1100) を返すはず
    // 親ディレクトリ等のサイズが入る可能性があるため、拡張子で絞り込む
    let res = fm.search("sum(extension:txt & size:)", Default::default())?;
    assert_eq!(res.results[0].name, "1100");

    Ok(())
}

/// 集約比較 (TRUE)
#[test]
fn test_aggregation_comparison_true() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("small.txt"), vec![0u8; 100])?;
    std::fs::write(root.join("large.txt"), vec![0u8; 1000])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(size:) > 500 -> TRUE (1100 > 500)
    let res = fm.search("sum(size:) > 500", Default::default())?;

    assert!(!res.results.is_empty());
    assert_eq!(res.results[0].name, "TRUE");
    assert!(res.results[0].id.is_volatile());
    assert_eq!(res.results[0].item_kind, ttfm::ItemKind::Volatile);

    Ok(())
}

/// 集約比較 (FALSE)
#[test]
fn test_aggregation_comparison_false() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("small.txt"), vec![0u8; 100])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(size:) > 1000 -> FALSE (100 < 1000)
    let res = fm.search("sum(size:) > 1000", Default::default())?;

    assert!(!res.results.is_empty());
    assert_eq!(res.results[0].name, "FALSE");
    assert!(res.results[0].id.is_volatile());
    assert_eq!(res.results[0].item_kind, ttfm::ItemKind::Volatile);

    Ok(())
}

/// 集約比較 (!=)
#[test]
fn test_aggregation_comparison_ne() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("a.txt"), "a")?;
    std::fs::write(root.join("b.txt"), "b")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // count(extension:txt) ^ 0 -> TRUE (2 ^ 0)
    let res1 = fm.search("count(extension:txt) ^ 0", Default::default())?;
    assert_eq!(
        res1.total_count,
        Some(1),
        "Should match root directory (calc is true)"
    );

    // count(extension:txt) ^ 2 -> FALSE (2 ^ 2 is false)
    let res2 = fm.search("count(extension:txt) ^ 2", Default::default())?;
    assert_eq!(res2.results[0].name, "FALSE");

    Ok(())
}

/// count(type:) - 存在するタグタイプ（extension, size, mtime 等）の数をカウント
#[test]
fn test_count_type_projection() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("test.txt"), "content")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // システム標準で name, extension, size, mtime, hash, item_kind, origin, rank があるはず
    // 少なくとも 3種類以上はある
    let res = fm.search("count(type:)", Default::default())?;
    let val: f64 = res.results[0].name.parse().unwrap();

    // 現在のバグだと 1 になると予想される
    // 期待値は > 1
    assert!(val > 1.0, "Expected multiple types, got {}", val);

    Ok(())
}

/// count(directory:) - ディレクトリ数をカウント
#[test]
fn test_count_directory_projection() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // ディレクトリ構造作成: root/subdir
    std::fs::create_dir(root.join("subdir"))?;
    std::fs::write(root.join("test.txt"), "content")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // ディレクトリが1つあるはず (subdir)
    let res = fm.search("count(directory:)", Default::default())?;
    let val: f64 = res.results[0].name.parse().unwrap();

    // 現在のバグだと 0 になると予想される
    assert!(val >= 1.0, "Expected at least 1 directory, got {}", val);

    Ok(())
}

/// その他のシステムカラム (item_id, item_kind, rank, origin, path, parentdir, filename) の集約テスト
/// これらが type='xxx' フィルタで誤って0件にならないか確認
#[test]
fn test_system_columns_aggregation() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // ファイル作成: root/test.txt
    std::fs::write(root.join("test.txt"), "content")?;
    // サブディレクトリも作成してバリエーションを持たせる
    std::fs::create_dir(root.join("sub"))?;
    std::fs::write(root.join("sub/test2.txt"), "content2")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 1. count(item_id:) -> 全アイテム数 (test.txt, sub, test2.txt = 3)
    let res = fm.search("count(item_id:)", Default::default())?;
    let val: f64 = res.results[0].name.parse().unwrap();
    assert!(val >= 3.0, "count(item_id) failed: {}", val);

    // 2. count(item_kind:) -> 'file', 'directory' など少なくとも1つ以上
    let res = fm.search("count(item_kind:)", Default::default())?;
    let val: f64 = res.results[0].name.parse().unwrap();
    // file と directory があるので 2 になる可能性が高い、少なくとも 0 ではない
    assert!(val >= 1.0, "count(item_kind) failed: {}", val);

    // 3. count(rank:) -> ランク。デフォルト0だとしても1種類はある
    let res = fm.search("count(rank:)", Default::default())?;
    let val: f64 = res.results[0].name.parse().unwrap();
    assert!(val >= 1.0, "count(rank) failed: {}", val);

    // 4. count(origin:) -> 'system', 'user'。少なくとも system はある
    let res = fm.search("count(origin:)", Default::default())?;
    let val: f64 = res.results[0].name.parse().unwrap();
    assert!(val >= 1.0, "count(origin) failed: {}", val);

    // 5. count(path:) -> パスはユニークなのでアイテム数と同じはず
    let res = fm.search("count(path:)", Default::default())?;
    let val: f64 = res.results[0].name.parse().unwrap();
    assert!(val >= 3.0, "count(path) failed: {}", val);

    // 6. count(parentdir:) -> root, root/sub など複数
    let res = fm.search("count(parentdir:)", Default::default())?;
    let val: f64 = res.results[0].name.parse().unwrap();
    assert!(val >= 1.0, "count(parentdir) failed: {}", val);

    // 7. count(filename:) -> test.txt, test2.txt, sub など
    let res = fm.search("count(filename:)", Default::default())?;
    let val: f64 = res.results[0].name.parse().unwrap();
    assert!(val >= 1.0, "count(filename) failed: {}", val);

    Ok(())
}

/// max(mtime:) と日付文字列の比較
#[test]
fn test_max_mtime_date_comparison() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 古いファイルと新しいファイルを作成
    std::fs::write(root.join("old.txt"), "old")?;
    std::thread::sleep(std::time::Duration::from_secs(1));
    std::fs::write(root.join("new.txt"), "new")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // max(mtime:) を取得
    let res1 = fm.search("max(mtime:)", Default::default())?;
    println!("max(mtime:) = {:?}", res1.results.get(0).map(|r| &r.name));

    // max(mtime:) < 2027-01-01 を比較
    let res2 = fm.search("max(mtime:) < 2027-01-01", Default::default())?;
    println!("max(mtime:) < 2027-01-01 = {:?}", res2);

    // 今日の日付より前なので TRUE になるはず
    assert_eq!(res2.results.len(), 1);
    assert_eq!(res2.results[0].name, "TRUE");

    Ok(())
}

/// max(filter & mtime:) と日付文字列の比較（AND条件内のProjection）
#[test]
fn test_max_mtime_with_filter_date_comparison() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("test.txt"), "content")?;
    std::thread::sleep(std::time::Duration::from_secs(1));
    std::fs::write(root.join("test.rs"), "code")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // max(extension:txt & mtime:) < 2027-02-01
    let res = fm.search(
        "max(extension:txt & mtime:) < 2027-02-01",
        Default::default(),
    )?;

    // 今日の日付より前なので TRUE になるはず
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].name, "TRUE");

    Ok(())
}

/// max(mtime:) == YYYY (Equal Comparison with Date Expansion)
///
/// This test verifies the fix for the bug where date equality comparison (which expands to a range AND)
/// returns real items instead of a scalar boolean result.
#[test]
fn test_aggregation_comparison_date_equal() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // Create a file (mtime will be "now", i.e., 2026)
    std::fs::write(root.join("test.txt"), "content")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 1. max(mtime:) == 2026 -> TRUE
    // This query expands to: max(mtime:) >= 2026-01-01 AND max(mtime:) <= 2026-12-31
    // The bug causes this to be treated as a normal filter query, returning the file "test.txt"
    // instead of a virtual item "TRUE".
    let res = fm.search("max(mtime:) == 2026", Default::default())?;

    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].name, "TRUE");
    assert!(res.results[0].id.is_volatile()); // Should be Volatile(...)

    // 2. max(mtime:) == 2025 -> FALSE
    let res_false = fm.search("max(mtime:) == 2025", Default::default())?;
    assert_eq!(res_false.results.len(), 1);
    assert_eq!(res_false.results[0].name, "FALSE");
    assert!(res_false.results[0].id.is_volatile()); // Should be Volatile(...)

    Ok(())
}

struct TestContext {
    _dir: tempfile::TempDir,
    db_dir: std::path::PathBuf,
    root: std::path::PathBuf,
}

impl TestContext {
    fn new() -> Self {
        let _dir = tempdir().unwrap();
        let root = _dir.path().to_path_buf();
        let db_dir = _dir.path().join("db");
        std::fs::create_dir(&db_dir).unwrap();
        Self { _dir, db_dir, root }
    }

    fn create_file_with_mtime(&self, name: &str, mtime_iso: &str) {
        let path = self.root.join(name);
        std::fs::File::create(&path).unwrap();
        let dt = chrono::DateTime::parse_from_rfc3339(mtime_iso).unwrap();
        let mtime = filetime::FileTime::from_unix_time(dt.timestamp(), 0);
        filetime::set_file_mtime(&path, mtime).unwrap();
    }

    fn search(&self, query: &str) -> ttfm::response::SearchResponse {
        let fm = FileManager::new_with_db_dir(&self.db_dir).unwrap();
        fm.index_directory(&self.root, None::<&fn(usize)>, false)
            .unwrap();
        fm.search(query, ttfm::SearchOptions::default()).unwrap()
    }
}

#[test]
fn test_max_mtime_with_year_filter() {
    let context = TestContext::new();
    context.create_file_with_mtime("a.rs", "2025-06-15T12:00:00Z"); // In 2025
    context.create_file_with_mtime("b.rs", "2024-12-31T23:59:59Z"); // Out
    context.create_file_with_mtime("c.txt", "2025-01-01T00:00:00Z"); // In but not rs

    // Query: extension:rs & mtime:2025 & mtime:
    // Should match only a.rs
    // mtime of a.rs is 1750075200 (approx)
    // max(mtime:) should return that value.

    let res = context.search("max(extension:rs & mtime:2025 & mtime:)");
    assert!(!res.results.is_empty());
    let scalar: f64 = res.results[0].name.parse().unwrap();
    println!("Scalar result: {}", scalar);

    // 2025-06-15T12:00:00Z = 1750075200
    assert!(scalar > 1700000000.0);
}

#[test]
fn test_max_on_empty() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // MAX on nonexistent tag -> Should be NULL and numeric/consistent
    let res = fm.search("max(nonexistent_tag:)", Default::default())?;

    if let Some(r) = res.results.first() {
        let types = r.get_all_values("type");
        // User requires type:numeric for NULL aggregation results
        assert!(
            types.contains(&"numeric".to_string()),
            "Expected type:numeric, got {:?}",
            types
        );
    } else {
        panic!("Expected a result for MAX aggregation");
    }

    Ok(())
}

#[test]
fn test_agg_agg_comparison_equal_reflexive() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 空のファイル（size=0）を作成
    std::fs::write(root.join("empty.txt"), "")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(size:0) == sum(size:0) -> TRUE
    let res = fm.search("sum(size:0) == sum(size:0)", Default::default())?;

    assert!(!res.results.is_empty(), "Result should not be empty");
    assert_eq!(res.results[0].name, "TRUE");
    assert!(res.results[0].id.is_volatile());

    Ok(())
}

#[test]
fn test_string_agg_extension() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("a.txt"), "")?;
    std::fs::write(root.join("b.rs"), "")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // extension: は通常 String 扱いになるため、sum は string_agg になるはず
    let res = fm.search("sum(extension:)", Default::default())?;
    assert!(!res.results.is_empty());
    let val = &res.results[0].name;
    // DuckDB の string_agg は順序が保証されないが、txt と rs が含まれているはず
    assert!(
        val.contains("txt") && val.contains("rs") && val.contains(", "),
        "Unexpected sum(extension:): {}",
        val
    );

    Ok(())
}

#[test]
fn test_string_agg_with_filter() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 複数のファイルを作成
    std::fs::write(root.join("a.rs"), "")?;
    std::fs::write(root.join("b.rs"), "")?;
    std::fs::write(root.join("c.txt"), "")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 条件付き文字列集計: extension:rs に一致するアイテムの extension: を結合
    let res =
        fm.search("sum(extension:rs & extension:)", Default::default())?;

    assert!(!res.results.is_empty(), "Result should not be empty");
    let name = &res.results[0].name;
    // 期待値は "rs, rs"
    assert!(
        name.contains("rs"),
        "Result should contain 'rs', but got: {}",
        name
    );
    assert!(
        name.contains(","),
        "Result should be a joined string, but got: {}",
        name
    );

    Ok(())
}

#[test]
fn test_string_agg_arithmetic_addition() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // ファイル作成
    std::fs::write(root.join("a.rs"), "")?;
    std::fs::write(root.join("b.txt"), "")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 文字列集計結果同士の結合
    // sum(string) はカンマ区切り。+ も文字列なら結合。
    let res = fm.search("sum(extension:rs & extension:) + ' - ' + sum(extension:txt & extension:)", Default::default())?;

    assert!(!res.results.is_empty(), "Result should not be empty");
    let name = &res.results[0].name;
    // 期待値: "rs - txt"
    assert!(
        name.contains("rs"),
        "Result should contain 'rs', but got: {}",
        name
    );
    assert!(
        name.contains("txt"),
        "Result should contain 'txt', but got: {}",
        name
    );
    assert!(
        name.contains(" - "),
        "Result should contain separator ' - ', but got: {}",
        name
    );

    Ok(())
}

/// count() - 引数なしで全アイテム数をカウント（count(\*:\*) と同等）
#[test]
fn test_count_empty_args() -> anyhow::Result<()> {
    // 修正: root と db_dir を分離する
    let temp_parent = tempdir()?;
    let root = temp_parent.path().join("root");
    let db_dir = temp_parent.path().join("db");
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&db_dir)?;

    // テスト用アイテムの作成 (計5アイテム)
    std::fs::write(root.join("a.rs"), "")?;
    std::fs::write(root.join("b.rs"), "")?;
    std::fs::write(root.join("c.txt"), "")?;
    std::fs::create_dir(root.join("subdir1"))?;
    std::fs::create_dir(root.join("subdir2"))?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&root, None::<&fn(usize)>, false)?;

    // 5. count() と count(*:*) の結果が一致することを確認 (トップレベル)
    let res_any_top = fm.search("count()", Default::default())?;
    let res_wild_top = fm.search("count(*:*)", Default::default())?;
    assert_eq!(res_any_top.results[0].name, res_wild_top.results[0].name);

    // 6. フィルタ挙動の確認 (差集合を用いた間接的検証)
    // 全体数から「フィルタ外」の数を引くことで、
    // 「フィルタ内」の数を算出し、それが期待値(5)と一致することを確認する。
    // 式: count() - count(*:* - parentdir:"root")
    // 両オペランドがスカラー(Aggregation)のため、トップレベルでも括弧なしで算術演算として解釈される。
    let root_str = root.to_slash_lossy();
    let query_indirect = format!("count() - count(*:* - parentdir:\"{}\")", root_str);
    let res_indirect = fm.search(&query_indirect, Default::default())?;
    
    assert_eq!(res_indirect.results[0].name, "5");

    Ok(())
}

/// count() > 2 - 比較演算との組み合わせ
#[test]
fn test_count_empty_comparison() -> anyhow::Result<()> {
    let temp_parent = tempdir()?;
    let root = temp_parent.path().join("root");
    let db_dir = temp_parent.path().join("db");
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&db_dir)?;

    std::fs::write(root.join("a.txt"), "")?;
    std::fs::write(root.join("b.txt"), "")?;
    std::fs::write(root.join("c.txt"), "")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&root, None::<&fn(usize)>, false)?;

    // count() > 2 と count(*:*) > 2 の一致確認 (トップレベル)
    // フィルタ済みの検証は、Nest実装待ちのため行わない
    let res = fm.search("count() > 2", Default::default())?;
    let res_wild = fm.search("count(*:*) > 2", Default::default())?;
    
    // 全アイテム数は5なので TRUE
    assert_eq!(res.results[0].name, "TRUE");
    assert_eq!(res.results[0].name, res_wild.results[0].name);

    Ok(())
}

/// count() + 1 - 算術演算との組み合わせ
#[test]
fn test_count_empty_arithmetic() -> anyhow::Result<()> {
    let temp_parent = tempdir()?;
    let root = temp_parent.path().join("root");
    let db_dir = temp_parent.path().join("db");
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&db_dir)?;

    std::fs::write(root.join("a.txt"), "")?;
    std::fs::write(root.join("b.txt"), "")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&root, None::<&fn(usize)>, false)?;

    // count() + 1 と count(*:*) + 1 の一致確認 (トップレベル)
    // システムタグなどが含まれるため絶対値は不定だが、計算結果は一致するはず
    let res1 = fm.search("count() + 1", Default::default())?;
    let res1_wild = fm.search("count(*:*) + 1", Default::default())?;
    
    // 結果が数値であることを確認（"6"決め打ちは避ける）
    assert!(res1.results[0].name.parse::<i64>().is_ok());
    assert_eq!(res1.results[0].name, res1_wild.results[0].name);

    Ok(())
}

/// count() == count(type:) - 他の集計との比較
#[test]
fn test_count_empty_with_other_scalar() -> anyhow::Result<()> {
    let temp_parent = tempdir()?;
    let root = temp_parent.path().join("root");
    let db_dir = temp_parent.path().join("db");
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&db_dir)?;

    std::fs::write(root.join("a.txt"), "")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&root, None::<&fn(usize)>, false)?;

    // アイテム数(2: a.txt, root) と タグタイプ数(複数) の比較
    let res = fm.search("count() == count(type:)", Default::default())?;
    assert!(res.results[0].name == "TRUE" || res.results[0].name == "FALSE");

    Ok(())
}
