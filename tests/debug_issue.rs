use ttfm::FileManager;
use std::fs::File;
use tempfile::tempdir;

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
    
    for path in &results {
        println!("Hit: {}", path);
    }

    // 期待値:
    // main.rs -> OK
    // lib.rs -> OK
    // sub_folder -> NG (拡張子なし、parentはsrc)
    // strange.rs -> ? (拡張子はrsと判定されるか？ ディレクトリなら除外すべきか？)
    // sub_folder/mod.rs -> NG (parentは src/sub_folder なので parent:src にはヒットしないはず)

    // もし "sub_folder" がヒットしているなら、extension判定がバグっている
    let sub_hits = results.iter().any(|p| p.contains("sub_folder") && !p.ends_with(".rs"));
    if sub_hits {
        println!("ISSUE REPRODUCED: 'sub_folder' was found in results!");
    } else {
        println!("'sub_folder' was correctly filtered out.");
    }

    // もし "strange.rs" (フォルダ) がヒットしているなら、ディレクトリ除外が必要
    let strange_hits = results.iter().any(|p| p.contains("strange.rs") && !p.ends_with("ignored.txt"));
    if strange_hits {
        println!("ISSUE REPRODUCED: 'strange.rs' (folder) was found in results!");
    }

    Ok(())
}
