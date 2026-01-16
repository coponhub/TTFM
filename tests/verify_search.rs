use std::fs::File;
use tempfile::tempdir;
use ttfm::FileManager;

#[test]
fn verify_complex_search_patterns() -> anyhow::Result<()> {
    // 1. Setup Environment
    let dir = tempdir()?;
    let root = dir.path();

    let files = vec![
        "project_alpha_report.pdf",
        "project_beta_report.pdf",
        "project_alpha_draft.txt",
        "image_vacation_2024.jpg",
        "image_work_2024.png",
        "backup_2023.zip",
    ];

    println!("Creating test files and folders in {:?}...", root);
    for name in &files {
        File::create(root.join(name))?;
    }

    // Create specific folders
    std::fs::create_dir(root.join("work_docs"))?;
    std::fs::create_dir(root.join("personal_photos"))?;
    std::fs::create_dir(root.join("temp_backup"))?;

    // 2. Index
    println!("Indexing...");
    // Use a custom index path inside the temp dir
    let index_path = root.join("test_index.parquet");
    let fm = FileManager::new_with_index_path(&index_path)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 3. Verify Queries
    let test_cases = vec![
        ("filename:project_alpha_report.pdf | filename:project_beta_report.pdf", 2),
        ("filename:project_alpha_report.pdf & ^(filename:project_alpha_draft.txt)", 1),
        ("filename:project_alpha_report.pdf | filename:project_beta_report.pdf", 2),
        ("filename:image_vacation_2024.jpg | filename:image_work_2024.png", 2),
        ("filename:image_vacation_2024.jpg", 1),
        ("^(extension:txt | extension:zip | extension:pdf | extension:jpg | extension:png)", 4), // 3 folders + 1 root directory

        // --- Added cases to verify set-based search fixes ---
        ("extension:pdf & filename:project_alpha_report.pdf", 1),   // Cross-attribute AND
        ("extension:txt & filename:project_alpha_draft.txt", 1), // Cross-attribute AND
        ("directory:work_docs", 1),                   // Keyword search on directory (full name)
        ("directory:personal_photos", 1),
        ("filename:backup_2023.zip", 1),

        // --- Complex nested combinations ---
        ("(extension:pdf | extension:txt) & filename:project_alpha_report.pdf", 1),
        ("extension:pdf & (filename:project_alpha_report.pdf | filename:project_beta_report.pdf)", 2),
        ("(directory:work_docs & name:work_docs) | (extension:pdf & filename:project_beta_report.pdf)", 2),

        // --- Deeply nested parentheses ---
        ("((extension:pdf | extension:txt) & filename:project_alpha_report.pdf) | directory:work_docs", 2),

        // Folder specific searches (directories are also entries with filenames)
        ("filename:work_docs", 0),            // filename no longer matches directories
        ("name:work_docs", 2),                // name matches both directory and label item
        ("filename:personal_photos | filename:temp_backup | filename:backup_2023.zip", 1),
        ("name:work_docs & directory:work_docs", 1),     
    ];

    for (query, expected_count) in test_cases {
        print!("Query: '{:<25}' -> ", query);

        let results = fm.search(query).unwrap_or_default();

        if results.len() == expected_count {
            println!("OK ({} hits)", results.len());
        } else {
            println!(
                "FAIL (Expected {}, got {})",
                expected_count,
                results.len()
            );

            for r in &results {
                println!(" - Found: {:?}", r);
            }

            panic!("Test failed for query: '{}'", query);
        }
    }

    Ok(())
}

#[test]
fn test_comparison_logic() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();

    // Override TTFM_HOME to point to temp dir to avoid user config interference
    unsafe {
        std::env::set_var("TTFM_HOME", root.join(".ttfm"));
    }

    // Create a data directory for indexing target
    let data_dir = root.join("data");
    std::fs::create_dir(&data_dir)?;

    // ファイル作成 (サイズを変える)
    std::fs::write(data_dir.join("small.txt"), vec![0u8; 100])?;
    std::fs::write(data_dir.join("medium.txt"), vec![0u8; 500])?;
    std::fs::write(data_dir.join("large.txt"), vec![0u8; 1000])?;

    // Use a specific DB directory inside the temp dir to ensure isolation
    let db_dir = root.join(".ttfm/db");
    let fm = ttfm::FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(&data_dir, None::<&fn(usize)>, false)?;

    // 1. 基本的なサイズ比較
    println!("Testing 'size: > 300'...");
    let res = fm.search("size: > 300")?;
    assert_eq!(res.len(), 2); // medium, large

    // 2. 連鎖比較 (Between)
    println!("Testing '200 < size: < 800'...");
    let res = fm.search("200 < size: < 800")?;
    assert_eq!(res.len(), 1);
    assert!(res[0].name.contains("medium.txt"));

    // 3. カスタムタグ (TRY_CAST 経由)
    println!("Testing custom tag with TRY_CAST...");
    let item_id = res[0].id.to_string();
    fm.tag_item(&item_id, "width:640")?;

    let res = fm.search("width: > 500")?;
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].id.to_string(), item_id);

    // 4. 複数条件の組み合わせ
    println!("Testing multiple conditions...");
    let res = fm.search("size: > 300 & width: > 500")?;
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].name, "medium.txt");

    // 5. スペースなしフォーマット
    println!("Testing 'width:>500' (no spaces)...");
    let res = fm.search("width:>500")?;
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].id.to_string(), item_id);

    // 6. その他の比較演算子 (>=, <=, ==, ^=)
    println!("Testing other operators...");

    // >= (Inclusive)
    println!("Testing 'size: >= 500'...");
    let res = fm.search("size: >= 500")?;
    assert_eq!(res.len(), 2, "size: >= 500 failed"); // medium(500), large(1000)

    // <= (Inclusive)
    println!("Testing 'size: <= 500'...");
    // 0-byte directory might match <= 500, so exclude directory explicitly
    let res = fm.search("size: <= 500 & is_dir:false")?;
    assert_eq!(res.len(), 2, "size: <= 500 failed"); // small(100), medium(500)

    // == (Equal)
    println!("Testing 'size: == 500'...");
    let res = fm.search("size: == 500")?;
    assert_eq!(res.len(), 1, "size: == 500 failed"); // medium(500)

    // ^= (Not Equal)
    println!("Testing 'size: ^= 500'...");
    let res = fm.search("size: ^= 500 & is_dir:false")?;
    assert_eq!(res.len(), 2); // small(100), large(1000)

    // ^ (Not Equal shorthand)
    let res = fm.search("size: ^ 500 & is_dir:false")?;
    assert_eq!(res.len(), 2); // small(100), large(1000)

    // Custom tag exact match (Cast check)
    let res = fm.search("width: == 640")?;
    assert_eq!(res.len(), 1);

    // Custom tag not equal
    let res = fm.search("width: ^= 999")?;
    // width:640 is set on one item. Others don't have width.
    // If we search for 'width: ^= 999', it effectively means "Has 'width' AND width != 999".
    // Since only one item has 'width' (640), and 640 != 999, it should match that one item.
    // NOTE: The current logic for generic generic tags is: type='...' AND TRY_CAST(label) != ...
    // So it implicitely filters by type='width'.
    assert_eq!(res.len(), 1);

    Ok(())
}

#[test]

fn test_or_negation_complex_behavior() {
    let dir = tempdir().unwrap();

    let root = dir.path();

    let db_dir = root.join(".ttfm/db");

    // 1. ファイル準備 (.rs と .txt)

    std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();

    std::fs::write(root.join("readme.txt"), "hello").unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // 2. クエリ実行: item_kind:type | ^(extension:rs)

    let query = "item_kind:type | ^(extension:rs)";

    let results = fm.search(query).unwrap();

    let mut found_type_item = false;

    let mut found_txt_file = false;

    let mut found_rs_file = false;

    for r in results {
        if r.item_kind != "file" && r.item_kind == "type" {
            found_type_item = true;
        }

        if r.item_kind == "file" {
            if r.tags.iter().any(|(t, v)| t == "extension" && v == "txt") {
                found_txt_file = true;
            }

            if r.tags.iter().any(|(t, v)| t == "extension" && v == "rs") {
                found_rs_file = true;
            }
        }
    }

    assert!(found_type_item, "Should find system items");

    assert!(found_txt_file, "Should find readme.txt");

    assert!(!found_rs_file, "Should NOT find main.rs");
}

#[test]
fn test_glob_search_behavior() -> anyhow::Result<()> {
    let dir = tempdir().unwrap();

    let root = dir.path();

    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("project_alpha.pdf"), "").unwrap();

    std::fs::write(root.join("project_beta.txt"), "").unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // 1. ワイルドカードによる部分一致

    let results = fm.search("filename:*alpha*").unwrap();

    assert_eq!(results.len(), 1);

    assert!(results[0].primary_value().unwrap().contains("alpha"));

    // 2. 複数のワイルドカード

    let results = fm.search("filename:project*").unwrap();

    assert_eq!(results.len(), 2);

    // 3. ワイルドカードなし (完全一致として動作)

    let results = fm.search("filename:project").unwrap();

    assert_eq!(results.len(), 0);

    let results = fm.search("filename:project_alpha.pdf").unwrap();
    assert_eq!(results.len(), 1);

    // 4. ? (任意の一文字)
    let results = fm.search("filename:project_alph?.pdf").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "project_alpha.pdf");

    // 5. [...] (文字セット) - alpha と beta 両方を拾いたいなら [ab]*
    let results = fm.search("filename:project_[ab]*").unwrap();
    assert_eq!(results.len(), 2); // alpha and beta

    // 6. [!...] (否定文字セット) - beta のみを拾う
    let results = fm.search("filename:project_[!a]eta*").unwrap();
    assert_eq!(results.len(), 1); // beta only

    // 7. クォート内でのGlob (無効化されるはず)
    let results = fm.search("filename:\"project_[ab]*\"").unwrap();
    assert_eq!(results.len(), 0); // リテラル一致を試みるため0件

    // 8. クォートでの完全一致
    let results = fm.search("filename:\"project_alpha.pdf\"").unwrap();
    assert_eq!(results.len(), 1);

    // 9. Type側の引用符
    let results = fm.search("\"filename\":project_alpha.pdf").unwrap();
    assert_eq!(results.len(), 1);

    // 10. Type側のGlob (実装済みか確認)
    // filename が対象になるはず
    let results = fm.search("*name:project_alpha.pdf").unwrap();
    assert_eq!(results.len(), 1);

    // 11. Type側のGlob ([...] / [!...])
    let results = fm.search("[f]ilename:project_alpha.pdf").unwrap();
    assert_eq!(results.len(), 1);

    let results = fm.search("[!f]ilename:project_alpha.pdf").unwrap();
    assert_eq!(results.len(), 0); // filename にはマッチしないはず

    // 12. Type側のGlob (?)
    let results = fm.search("file?ame:project_alpha.pdf").unwrap();
    assert_eq!(results.len(), 1);

    // 13. バックスラッシュ・エスケープ
    // テスト用の特殊ファイルを作成
    let special_file = root.join("[WIP]_test.txt");
    std::fs::File::create(&special_file)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // バックスラッシュなしだとGlobとして解釈され、マッチしない可能性がある（または意図しないマッチ）
    // ここでは \[WIP\] とすることでリテラルとして扱う
    let results = fm.search(r"filename:\[WIP\]_*").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "[WIP]_test.txt");

    Ok(())
}
