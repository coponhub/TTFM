use std::fs::File;
use std::io::Write;
use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn test_size_unit_queries() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // テスト用のファイル作成
    // 1. 512 KB ( dokładnie 524,288 bytes)
    let mut f1 = File::create(root.join("half_mb.bin"))?;
    f1.write_all(&vec![0u8; 512 * 1024])?;

    // 2. 1.5 MB ( dokładnie 1,572,864 bytes)
    let mut f2 = File::create(root.join("one_and_half_mb.bin"))?;
    f2.write_all(&vec![0u8; (1.5 * 1024.0 * 1024.0) as usize])?;

    // 3. 10 MB ( dokładnie 10,485,760 bytes)
    let mut f3 = File::create(root.join("ten_mb.bin"))?;
    f3.write_all(&vec![0u8; 10 * 1024 * 1024])?;

    // 4. 0 byte (empty)
    File::create(root.join("empty.txt"))?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 検証パターン
    let cases = vec![
        // 基本的な比較
        ("size: :>= 512KB & is_dir:false", 3), // half, one_half, ten
        ("size: :< 1MB & is_dir:false", 2),    // empty, half
        ("size: := 512KiB", 1),                // half_mb.bin
        // 小数点
        ("size: :<= 1.5MB & is_dir:false", 3), // empty, half, one_half
        ("size: :> 1.5MiB & is_dir:false", 1), // ten_mb.bin
        // 連鎖比較 (Chain comparison logic for Literal < Type is not yet supported in execution layer, so using AND)
        ("size: :> 600KB & size: :< 11MB & is_dir:false", 2), // one_half, ten
        ("size: :>= 1MB & size: :<= 1.5MB & is_dir:false", 1), // one_half
        // 不一致
        ("size: :^= 0B & is_dir:false", 3), // half, one_half, ten
        // 単位のバリエーション (大文字小文字・ショートハンド)
        ("size: := 10m & is_dir:false", 1),
        ("size: := 512k & is_dir:false", 1),
    ];

    for (query, expected) in cases {
        println!("Testing query: '{}'", query);
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

#[test]
fn test_large_size_normalization() -> anyhow::Result<()> {
    // 巨大なサイズのパースと、それがSQLに正しく落ちるかのロジック的な検証
    let dir = tempdir()?;
    let db_dir = dir.path().join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir)?;

    // 正しくi64として扱われることを確認（ここではエラーにならないことを確認）
    let res = fm.search("size: :> 1PB", Default::default());
    assert!(res.is_ok()); // 結果は0件で良いが、パースエラーにならないことが重要

    // 1TB = 1099511627776
    let res = fm.search("size: := 1TB", Default::default());
    assert!(res.is_ok());

    Ok(())
}
