use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_aggregation_with_calculation() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 10KB のファイル作成用のサブディレクトリ
    let data_dir = root.join("data");
    std::fs::create_dir(&data_dir)?;

    // 10KB のファイルを作成
    let size = 10 * 1024;
    std::fs::write(data_dir.join("test.txt"), vec![0u8; size])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&data_dir, None::<&fn(usize)>, false)?;

    // 1. 基本的な集計 (確認用)
    // ファイルのみを対象にする: sum(extension:txt & size:)
    // 集合演算と混在する場合は、Projection部分を括弧で囲むのが安全
    let query_base = "sum(extension:txt & (size:))";
    let res_base = fm.search(query_base, Default::default())?;
    let total_size: i64 = res_base.results[0].name.parse()?;
    assert_eq!(total_size, size as i64);

    // 2. 算術演算を含む集計: sum(extension:txt & (size: - 1000))
    // ファイルは1つなので 10240 - 1000 = 9240
    let query_calc = "sum(extension:txt & ((size: - 1000)))";
    let res_calc = fm.search(query_calc, Default::default())?;
    assert_eq!(
        res_calc.results[0].name, "9240",
        "sum(size: - 1000) should be 9240"
    );

    // 3. 複数の演算子を含む集計: sum(extension:txt & (size: - (1000 / 2)))
    // 10240 - 500 = 9740
    // 優先順位の曖昧さを避けるため、さらに括弧を追加
    let query_complex = "sum(extension:txt & ((size: - (1000 / 2))))";
    let res_complex = fm.search(query_complex, Default::default())?;
    assert_eq!(
        res_complex.results[0].name, "9740",
        "sum(size: - 1000 / 2) should be 9740"
    );

    Ok(())
}

/// 存在しないタグに対する算術演算のテスト
/// TRY_CAST による VARCHAR -> DOUBLE 変換が正しく適用されることを確認
#[test]
fn test_aggregation_with_unknown_tag_arithmetic() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let data_dir = root.join("data");
    std::fs::create_dir(&data_dir)?;
    std::fs::write(data_dir.join("test.txt"), "content")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&data_dir, None::<&fn(usize)>, false)?;

    // 存在しないタグに対する算術演算: sum(unknown_tag: + 1)
    // unknown_tag は存在しないため label_str は NULL となり、
    // TRY_CAST(NULL AS DOUBLE) + 1 = NULL となるはず
    let query = "sum(unknown_tag: + 1)";
    let res = fm.search(query, Default::default())?;
    
    // 結果は NULL (name = "NULL")
    assert_eq!(res.results[0].name, "NULL", "sum of unknown tag + 1 should be NULL");
    
    // type タグが numeric であることを確認
    let type_tag = res.results[0]
        .tags
        .entries
        .iter()
        .find(|t| t.label.tag_type().as_str() == "type")
        .map(|t| t.label.as_str());
    assert_eq!(type_tag.as_deref(), Some("numeric"), "type should be 'numeric' for NULL aggregation result");

    Ok(())
}

/// ラベル比較や集合演算を含む複雑な集計式のテスト
#[test]
fn test_aggregation_with_complex_expression() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let data_dir = root.join("data");
    std::fs::create_dir(&data_dir)?;
    std::fs::write(data_dir.join("test.txt"), "content")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&data_dir, None::<&fn(usize)>, false)?;

    // 複雑な集計式: sum((non_existant_tag: :> size:) & size:)
    // non_existant_tag は存在しないため NULL となり、比較や演算の結果も NULL となるはず
    let query = "sum((non_existant_tag: :> size:) & size:)";
    let res = fm.search(query, Default::default())?;
    
    // 結果は NULL
    assert_eq!(res.results[0].name, "NULL", "complex sum expression should be NULL");

    Ok(())
}
