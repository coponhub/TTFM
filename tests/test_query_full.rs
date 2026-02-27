use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_binder_error() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let db_dir = dir.path().join(".ttfm/db");

    // Create test files
    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::write(src_dir.join("a.rs"), "a")?;
    std::fs::write(src_dir.join("b.txt"), "b")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(dir.path(), None::<&fn(usize)>, false)?;

    let q = r#"((parentdir: &: count(extension:rs)) / (parentdir: &: count())) :> 1"#;
    match fm.search(q, Default::default()) {
        Ok(res) => eprintln!("SUCCESS: {:?}", res),
        Err(e) => {
            eprintln!("ERROR: {}", e);
            panic!("Search failed");
        }
    }
    Ok(())
}
