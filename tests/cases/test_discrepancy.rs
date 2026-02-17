use anyhow::Result;
use tempfile::tempdir;
use ttfm::FileManager;

/// count(Projection) はラベルの種類数を、count(TypedTag) はアイテム数を数えることを検証する。
/// ネスト内 (parentdir: &: count(...)) での挙動をテストする。
#[test]
fn test_nest_count_projection_counts_distinct_labels() -> Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // dirA: 10個のファイルがあるが、拡張子は .txt のみ (1種類)
    let dira = root.join("dirA");
    std::fs::create_dir(&dira)?;
    for i in 0..10 {
        std::fs::write(dira.join(format!("file{}.txt", i)), "content")?;
    }

    // dirB: 3個のファイルがあり、拡張子は3種類 (.txt, .html, .rs)
    let dirb = root.join("dirB");
    std::fs::create_dir(&dirb)?;
    std::fs::write(dirb.join("a.txt"), "t")?;
    std::fs::write(dirb.join("b.html"), "h")?;
    std::fs::write(dirb.join("c.rs"), "r")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: parentdir: &: count(extension:)
    // count(extension:) は Projection → 拡張子の「種類数」を数える
    // dirA: 1種類 (txt), dirB: 3種類 (txt, html, rs)
    let res =
        fm.search("parentdir: &: count(extension:)", Default::default())?;

    for r in &res.results {
        println!("  {} -> {:?}", r.name, r.tags);
    }

    let dira_nvalue = res
        .results
        .iter()
        .find(|r| r.name.contains("dirA"))
        .and_then(|r| {
            r.tags.entries.iter().find_map(|e| {
                if format!("{:?}", e.label).contains("nvalue") {
                    Some(format!("{:?}", e.label))
                } else {
                    None
                }
            })
        });
    let dirb_nvalue = res
        .results
        .iter()
        .find(|r| r.name.contains("dirB"))
        .and_then(|r| {
            r.tags.entries.iter().find_map(|e| {
                if format!("{:?}", e.label).contains("nvalue") {
                    Some(format!("{:?}", e.label))
                } else {
                    None
                }
            })
        });

    println!("dirA nvalue: {:?}", dira_nvalue);
    println!("dirB nvalue: {:?}", dirb_nvalue);

    // dirA は 1 種類 (txt のみ), dirB は 3 種類 (txt, html, rs)
    assert!(
        dira_nvalue
            .as_ref()
            .map_or(false, |s| s.contains("Integer(1)")),
        "dirA should have nvalue=1 (1 extension type), got: {:?}",
        dira_nvalue
    );
    assert!(
        dirb_nvalue
            .as_ref()
            .map_or(false, |s| s.contains("Integer(3)")),
        "dirB should have nvalue=3 (3 extension types), got: {:?}",
        dirb_nvalue
    );

    Ok(())
}

/// count(TypedTag) はアイテム数を数えることを検証する。
#[test]
fn test_nest_count_typedtag_counts_items() -> Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // dirA: txt が 5 個
    let dira = root.join("dirA");
    std::fs::create_dir(&dira)?;
    for i in 0..5 {
        std::fs::write(dira.join(format!("file{}.txt", i)), "content")?;
    }

    // dirB: txt が 2 個
    let dirb = root.join("dirB");
    std::fs::create_dir(&dirb)?;
    std::fs::write(dirb.join("a.txt"), "t")?;
    std::fs::write(dirb.join("b.txt"), "t")?;
    std::fs::write(dirb.join("c.html"), "h")?; // html は対象外

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: parentdir: &: count(extension:txt)
    // count(extension:txt) は TypedTag → アイテム数を数える
    // dirA: 5, dirB: 2
    let res =
        fm.search("parentdir: &: count(extension:txt)", Default::default())?;

    for r in &res.results {
        println!("  {} -> {:?}", r.name, r.tags);
    }

    let dira_nvalue = res
        .results
        .iter()
        .find(|r| r.name.contains("dirA"))
        .and_then(|r| {
            r.tags.entries.iter().find_map(|e| {
                if format!("{:?}", e.label).contains("nvalue") {
                    Some(format!("{:?}", e.label))
                } else {
                    None
                }
            })
        });
    let dirb_nvalue = res
        .results
        .iter()
        .find(|r| r.name.contains("dirB"))
        .and_then(|r| {
            r.tags.entries.iter().find_map(|e| {
                if format!("{:?}", e.label).contains("nvalue") {
                    Some(format!("{:?}", e.label))
                } else {
                    None
                }
            })
        });

    println!("dirA nvalue: {:?}", dira_nvalue);
    println!("dirB nvalue: {:?}", dirb_nvalue);

    // dirA は 5 (txt ファイル 5 個), dirB は 2 (txt ファイル 2 個)
    assert!(
        dira_nvalue
            .as_ref()
            .map_or(false, |s| s.contains("Integer(5)")),
        "dirA should have nvalue=5 (5 txt files), got: {:?}",
        dira_nvalue
    );
    assert!(
        dirb_nvalue
            .as_ref()
            .map_or(false, |s| s.contains("Integer(2)")),
        "dirB should have nvalue=2 (2 txt files), got: {:?}",
        dirb_nvalue
    );

    Ok(())
}
