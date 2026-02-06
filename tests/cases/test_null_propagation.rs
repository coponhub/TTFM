/// NULL伝播の統合テスト
///
/// テスト対象:
/// - 空データに対する集約比較が NULL を返す
/// - データありで条件TRUEの場合に TRUE を返す
/// - データありで条件FALSEの場合に FALSE を返す
use tempfile::tempdir;
use ttfm::FileManager;

/// 空データに対する集約比較は NULL を返すべき
#[test]
fn test_null_propagation_empty_data() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // ファイルを1つ作成（extension:txt）
    std::fs::write(root.join("a.txt"), "content")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // extension:nonexistent にマッチするデータがない場合、avg() は NULL を返す
    // avg(NULL) == avg(NULL) は NULL == NULL → NULL
    let res = fm.search(
        "avg(extension:nonexistent & size:) == avg(extension:nonexistent & size:)",
        Default::default(),
    )?;

    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].name, "NULL");

    Ok(())
}

/// データありで条件TRUEの場合
#[test]
fn test_null_propagation_data_true() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("a.txt"), vec![0u8; 100])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // avg(size:) == avg(size:) はデータありなら TRUE
    let res = fm.search("avg(size:) == avg(size:)", Default::default())?;

    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].name, "TRUE");

    Ok(())
}

/// データありで条件FALSEの場合
#[test]
fn test_null_propagation_data_false() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("a.txt"), vec![0u8; 100])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(size:) > 1000 は FALSE (100 < 1000)
    let res = fm.search("sum(size:) > 1000", Default::default())?;

    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].name, "FALSE");

    Ok(())
}

/// 単体集約クエリ（比較なし）でデータがない場合、NULL を返すべき
#[test]
fn test_null_propagation_single_aggregation_empty() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join("db"); // .ttfm ではない場所に
    let src_dir = root.join("src");
    std::fs::create_dir(&src_dir)?;

    // 完全に空のディレクトリをインデックス
    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&src_dir, None::<&fn(usize)>, false)?;

    // 単体集約（存在しないタグを条件に含めて確実に空集合にする）
    let res = fm.search("max(extension:nonexistent & size:)", Default::default())?;

    assert_eq!(res.results.len(), 1);
    // 現在は "0" が返るが、"NULL" を期待するように変更
    assert_eq!(res.results[0].name, "NULL");

    // NULL でも型情報 (scalar) は保持されているべき
    let types = res.results[0].get_all_values("type");
    assert!(types.contains(&"scalar".to_string()));

    Ok(())
}
