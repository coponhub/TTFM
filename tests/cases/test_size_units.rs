use super::default_scope;
use std::fs::File;
use std::io::Write;
use tempfile::tempdir;
use ttfm::FileManager;

define_cases! {
    size_large_gt_1pb: {
        setup: |_dir| Ok(()),
        modify: None,
        format_query: default_scope,
        query: "size: :> 1PB",
        assert: |_res, _dir| Ok(()),
    },
    size_large_eq_1tb: {
        setup: |_dir| Ok(()),
        modify: None,
        format_query: default_scope,
        query: "size: := 1TB",
        assert: |_res, _dir| Ok(()),
    },
}

#[test]
fn test_size_unit_queries() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let mut f1 = File::create(root.join("half_mb.bin"))?;
    f1.write_all(&vec![0u8; 512 * 1024])?;

    let mut f2 = File::create(root.join("one_and_half_mb.bin"))?;
    f2.write_all(&vec![0u8; (1.5 * 1024.0 * 1024.0) as usize])?;

    let mut f3 = File::create(root.join("ten_mb.bin"))?;
    f3.write_all(&vec![0u8; 10 * 1024 * 1024])?;

    File::create(root.join("empty.txt"))?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let cases = vec![
        ("size: :>= 512KB & is_dir:false", 3),
        ("size: :< 1MB & is_dir:false", 2),
        ("size: := 512KiB", 1),
        ("size: :<= 1.5MB & is_dir:false", 3),
        ("size: :> 1.5MiB & is_dir:false", 1),
        ("size: :> 600KB & size: :< 11MB & is_dir:false", 2),
        ("size: :>= 1MB & size: :<= 1.5MB & is_dir:false", 1),
        ("size: :^= 0B & is_dir:false", 3),
        ("size: := 10m & is_dir:false", 1),
        ("size: := 512k & is_dir:false", 1),
    ];

    for (query, expected) in cases {
        let results = fm.search(query, Default::default())?;
        assert_eq!(
            results.results.len(),
            expected,
            "Query '{}' failed. Expected {}, got {}",
            query,
            expected,
            results.results.len()
        );
    }

    Ok(())
}
