use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_volatile_item_typed_tags_integer() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let src_dir = root.join("src");
    std::fs::create_dir(&src_dir)?;

    std::fs::write(src_dir.join("a.txt"), vec![0u8; 123])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&src_dir, None::<&fn(usize)>, false)?;

    // sum(name:a.txt & size:) -> 123
    let res = fm.search("sum(name:a.txt & size:)", Default::default())?;

    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].name, "123");

    // "type" タグが "integer" であることを確認
    assert!(res.results[0]
        .get_all_values("type")
        .contains(&"integer".to_string()));
    // "value" タグが "123" であることを確認
    assert!(res.results[0]
        .get_all_values("value")
        .contains(&"123".to_string()));

    Ok(())
}

#[test]
fn test_volatile_item_typed_tags_double() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let src_dir = root.join("src");
    std::fs::create_dir(&src_dir)?;

    std::fs::write(src_dir.join("a.txt"), vec![0u8; 100])?;
    std::fs::write(src_dir.join("b.txt"), vec![0u8; 200])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&src_dir, None::<&fn(usize)>, false)?;

    // avg((name:a.txt | name:b.txt) & size:) -> 150.0
    let res = fm
        .search("avg((name:a.txt | name:b.txt) & size:)", Default::default())?;

    assert_eq!(res.results.len(), 1);
    assert!(res.results[0].name.contains("150"));

    // "type" タグが "double" であることを確認
    assert!(res.results[0]
        .get_all_values("type")
        .contains(&"double".to_string()));
    // "value" タグが "150" を含むことを確認
    assert!(res.results[0]
        .get_all_values("value")
        .iter()
        .any(|v| v.contains("150")));

    Ok(())
}

#[test]
fn test_volatile_item_typed_tags_boolean() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let src_dir = root.join("src");
    std::fs::create_dir(&src_dir)?;

    std::fs::write(src_dir.join("a.txt"), vec![0u8; 100])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&src_dir, None::<&fn(usize)>, false)?;

    // sum(name:a.txt & size:) == 100 -> TRUE
    let res =
        fm.search("sum(name:a.txt & size:) == 100", Default::default())?;

    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].name, "TRUE");

    // "type" タグが "boolean" であることを確認
    assert!(res.results[0]
        .get_all_values("type")
        .contains(&"boolean".to_string()));
    // "value" タグが "true" (または TRUE) であることを確認
    let vals = res.results[0].get_all_values("value");
    assert!(vals.iter().any(|v| v.to_lowercase() == "true"));

    Ok(())
}
