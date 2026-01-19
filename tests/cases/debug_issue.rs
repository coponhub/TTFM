use std::fs::File;
use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn debug_parent_and_extension_search() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();

    // 1. ファイル構造の作成
    // src/
    //   main.rs
    //   lib.rs
    //   sub_folder/  <-- これがヒットしてしまうか確認
    //     mod.rs
    //   strange.rs/  <-- 拡張子っぽい名前のフォルダ
    //     ignored.txt

    let src = root.join("src");
    std::fs::create_dir(&src)?;

    File::create(src.join("main.rs"))?;
    File::create(src.join("lib.rs"))?;

    let sub = src.join("sub_folder");
    std::fs::create_dir(&sub)?;
    File::create(sub.join("mod.rs"))?;

    let strange = src.join("strange.rs");
    std::fs::create_dir(&strange)?;
    File::create(strange.join("ignored.txt"))?;

    // 2. インデックス作成
    let index_path = root.join("debug_index.parquet");
    let fm = FileManager::new_with_index_path(&index_path)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 3. 検索実行: parentdir:src & extension:rs
    let results = fm.search("parentdir:src & extension:rs")?;

    for path in &results.results {
        println!("Hit: {:?}", path);
    }

    // 期待値:
    // main.rs -> OK
    // lib.rs -> OK
    // sub_folder -> NG (拡張子なし、parentはsrc)
    // strange.rs -> ? (拡張子はrsと判定されるか？ ディレクトリなら除外すべきか？)
    // sub_folder/mod.rs -> NG (parentは src/sub_folder なので parent:src にはヒットしないはず)

    // もし "sub_folder" がヒットしているなら、extension判定がバグっている
    let sub_hits = results.results.iter().any(|p| {
        let val = p.primary_value().unwrap_or("");
        val.contains("sub_folder") && !val.ends_with(".rs")
    });
    assert!(!sub_hits, "'sub_folder' directory found in results!");

    // もし "strange.rs" (フォルダ) がヒットしているなら、ディレクトリ除外が必要
    let strange_hits = results.results.iter().any(|p| {
        let val = p.primary_value().unwrap_or("");
        val.contains("strange.rs") && !val.ends_with("ignored.txt")
    });
    assert!(!strange_hits, "'strange.rs' directory found in results!");

    Ok(())
}
