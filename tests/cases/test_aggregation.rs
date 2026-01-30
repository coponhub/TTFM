/// 集約機能 (Aggregation) の統合テスト
///
/// テスト対象:
/// - `count(query)`: アイテム数をカウント
/// - `count(projection:)`: ユニークラベル数をカウント
/// - `sum(projection:)`, `avg(projection:)`, `max(projection:)`, `min(projection:)`: 数値集計
/// - 集約比較: `sum(size:) > 1GB` 等の真偽値返却
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
    assert_eq!(res.scalar, Some(2.0));
    assert!(res.results.is_empty());

    Ok(())
}

/// count(projection:) - ユニークラベル数をカウント
#[test]
fn test_count_unique_labels() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("a.txt"), "")?;
    std::fs::write(root.join("b.txt"), "")?;
    std::fs::write(root.join("c.rs"), "")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // count(extension:) -> 2 (txt, rs のユニーク数)
    let res = fm.search("count(extension:)", Default::default())?;
    assert_eq!(res.scalar, Some(2.0));

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
    assert_eq!(res.scalar, Some(1100.0));

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
    assert_eq!(res.results[0].id.as_i64(), 1);

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
    assert_eq!(res.results[0].id.as_i64(), 0);

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

    // count(extension:txt) != 0 -> TRUE (2 != 0)
    let res1 = fm.search("count(extension:txt) != 0", Default::default())?;
    assert_eq!(res1.results[0].name, "TRUE");

    // count(extension:txt) != 2 -> FALSE (2 != 2 is false)
    let res2 = fm.search("count(extension:txt) != 2", Default::default())?;
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

    // 現在のバグだと 1 になると予想される
    // 期待値は > 1
    assert!(
        res.scalar.unwrap() > 1.0,
        "Expected multiple types, got {}",
        res.scalar.unwrap()
    );

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

    // 現在のバグだと 0 になると予想される
    assert!(
        res.scalar.unwrap() >= 1.0,
        "Expected at least 1 directory, got {}",
        res.scalar.unwrap()
    );

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
    assert!(
        res.scalar.unwrap() >= 3.0,
        "count(item_id) failed: {}",
        res.scalar.unwrap()
    );

    // 2. count(item_kind:) -> 'file', 'directory' など少なくとも1つ以上
    let res = fm.search("count(item_kind:)", Default::default())?;
    // file と directory があるので 2 になる可能性が高い、少なくとも 0 ではない
    assert!(
        res.scalar.unwrap() >= 1.0,
        "count(item_kind) failed: {}",
        res.scalar.unwrap()
    );

    // 3. count(rank:) -> ランク。デフォルト0だとしても1種類はある
    let res = fm.search("count(rank:)", Default::default())?;
    assert!(
        res.scalar.unwrap() >= 1.0,
        "count(rank) failed: {}",
        res.scalar.unwrap()
    );

    // 4. count(origin:) -> 'system', 'user'。少なくとも system はある
    let res = fm.search("count(origin:)", Default::default())?;
    assert!(
        res.scalar.unwrap() >= 1.0,
        "count(origin) failed: {}",
        res.scalar.unwrap()
    );

    // 5. count(path:) -> パスはユニークなのでアイテム数と同じはず
    let res = fm.search("count(path:)", Default::default())?;
    assert!(
        res.scalar.unwrap() >= 3.0,
        "count(path) failed: {}",
        res.scalar.unwrap()
    );

    // 6. count(parentdir:) -> root, root/sub など複数
    let res = fm.search("count(parentdir:)", Default::default())?;
    assert!(
        res.scalar.unwrap() >= 1.0,
        "count(parentdir) failed: {}",
        res.scalar.unwrap()
    );

    // 7. count(filename:) -> test.txt, test2.txt, sub など
    let res = fm.search("count(filename:)", Default::default())?;
    assert!(
        res.scalar.unwrap() >= 1.0,
        "count(filename) failed: {}",
        res.scalar.unwrap()
    );

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
    println!("max(mtime:) = {:?}", res1.scalar);

    // max(mtime:) < 2026-02-01 を比較
    let res2 = fm.search("max(mtime:) < 2026-02-01", Default::default())?;
    println!("max(mtime:) < 2026-02-01 = {:?}", res2);

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
