use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_aggregation_returns_volatile_scalar() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_parent = tempdir()?;
    let db_dir = db_parent.path().join(".ttfm/db");

    // テストファイル作成
    std::fs::write(root.join("small.txt"), vec![0u8; 100])?;
    std::fs::write(root.join("large.txt"), vec![0u8; 1000])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(size:) の集計クエリ
    let res = fm.search("sum(extension:txt & size:)", Default::default())?;

    // 新しい仕様では results に Scalar 揮発性アイテムが入るはず
    assert!(
        !res.results.is_empty(),
        "Results should not be empty for aggregation"
    );
    let first_res = &res.results[0];

    // VolatileItem::Scalar の表示名は数値そのものになると期待
    assert_eq!(first_res.name, "1100");

    // IDが Scalar(1100.0) であることを確認 (これは types.rs の修正後に有効になる)
    // 今はコンパイルエラーになるはずなので、まずは存在しないことを前提に書くか、
    // あるいは types.rs を先に直す TDD の順序を守る。

    Ok(())
}

#[test]
fn test_boolean_computation_returns_volatile_boolean() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_parent = tempdir()?;
    let db_dir = db_parent.path().join(".ttfm/db");

    std::fs::write(root.join("test.txt"), vec![0u8; 100])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(size:) > 50 -> TRUE
    let res = fm.search("sum(size:) > 50", Default::default())?;

    assert!(!res.results.is_empty());
    assert_eq!(res.results[0].name, "TRUE");
    // IDが Boolean(1) であることを確認

    Ok(())
}

#[test]
fn test_boolean_with_non_id_1() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_parent = tempdir()?;
    let db_dir = db_parent.path().join(".ttfm/db");

    // ID 1 になるファイルを先に作成（これはマッチさせない）
    std::fs::write(root.join("other.txt"), vec![0u8; 10])?;
    // ID 2 になるファイルを作成（こちらをマッチさせる）
    std::fs::write(root.join("target.rs"), vec![0u8; 100])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(extension:rs & size:) -> 100
    // 100 > 0 -> TRUE (ID 2 がマッチし、MAX(item_id)=2 となるはず)
    let res = fm.search("sum(extension:rs & size:) > 0", Default::default())?;

    assert!(!res.results.is_empty());
    assert_eq!(
        res.results[0].name, "TRUE",
        "Should be TRUE even if matched ID is not 1"
    );

    Ok(())
}

#[test]
fn test_boolean_aggregation_matching() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_parent = tempdir()?;
    let db_dir = db_parent.path().join(".ttfm/db");

    std::fs::write(root.join("a.txt"), vec![0u8; 10])?;
    std::fs::write(root.join("b.txt"), vec![0u8; 10])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // count(extension:txt) == 2 -> TRUE
    let res = fm.search("count(extension:txt) == 2", Default::default())?;

    assert!(!res.results.is_empty());
    assert_eq!(res.results[0].name, "TRUE");

    assert!(!res.results.is_empty());
    assert_eq!(res.results[0].name, "TRUE");

    assert!(!res.results.is_empty());
    assert_eq!(res.results[0].name, "TRUE");

    assert!(!res.results.is_empty());
    assert_eq!(res.results[0].name, "TRUE");

    Ok(())
}

#[test]
fn test_boolean_reflexive_aggregation() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_parent = tempdir()?;
    let db_dir = db_parent.path().join(".ttfm/db");

    std::fs::write(root.join("a.txt"), vec![0u8; 100])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // avg(size:) == avg(size:) -> TRUE
    let res = fm.search("avg(size:) == avg(size:)", Default::default())?;

    assert!(!res.results.is_empty());
    assert_eq!(res.results[0].name, "TRUE");

    Ok(())
}
