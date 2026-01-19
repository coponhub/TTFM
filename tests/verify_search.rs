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

        let response = fm.search(query).unwrap_or_default();

        if response.results.len() == expected_count {
            println!("OK ({} hits)", response.results.len());
        } else {
            println!(
                "FAIL (Expected {}, got {})",
                expected_count,
                response.results.len()
            );

            for r in &response.results {
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
    assert_eq!(res.results.len(), 2); // medium, large

    // 2. 連鎖比較 (Between)
    println!("Testing '200 < size: < 800'...");
    let res = fm.search("200 < size: < 800")?;
    assert_eq!(res.results.len(), 1);
    assert!(res.results[0].name.contains("medium.txt"));

    // 3. カスタムタグ (TRY_CAST 経由)
    println!("Testing custom tag with TRY_CAST...");
    let item_id = res.results[0].id.to_string();
    fm.tag_item(&item_id, "width:640")?;

    let res = fm.search("width: > 500")?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].id.to_string(), item_id);

    // 4. 複数条件の組み合わせ
    println!("Testing multiple conditions...");
    let res = fm.search("size: > 300 & width: > 500")?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].name, "medium.txt");

    // 5. スペースなしフォーマット
    println!("Testing 'width:>500' (no spaces)...");
    let res = fm.search("width:>500")?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].id.to_string(), item_id);

    // 6. その他の比較演算子 (>=, <=, ==, ^=)
    println!("Testing other operators...");

    // >= (Inclusive)
    println!("Testing 'size: >= 500'...");
    let res = fm.search("size: >= 500")?;
    assert_eq!(res.results.len(), 2, "size: >= 500 failed"); // medium(500), large(1000)

    // <= (Inclusive)
    println!("Testing 'size: <= 500'...");
    // 0-byte directory might match <= 500, so exclude directory explicitly
    let res = fm.search("size: <= 500 & is_dir:false")?;
    assert_eq!(res.results.len(), 2, "size: <= 500 failed"); // small(100), medium(500)

    // == (Equal)
    println!("Testing 'size: == 500'...");
    let res = fm.search("size: == 500")?;
    assert_eq!(res.results.len(), 1, "size: == 500 failed"); // medium(500)

    // ^= (Not Equal)
    println!("Testing 'size: ^= 500'...");
    let res = fm.search("size: ^= 500 & is_dir:false")?;
    assert_eq!(res.results.len(), 2); // small(100), large(1000)

    // ^ (Not Equal shorthand)
    let res = fm.search("size: ^ 500 & is_dir:false")?;
    assert_eq!(res.results.len(), 2); // small(100), large(1000)

    // Custom tag exact match (Cast check)
    let res = fm.search("width: == 640")?;
    assert_eq!(res.results.len(), 1);

    // Custom tag not equal
    let res = fm.search("width: ^= 999")?;
    // width:640 is set on one item. Others don't have width.
    // If we search for 'width: ^= 999', it effectively means "Has 'width' AND width != 999".
    // Since only one item has 'width' (640), and 640 != 999, it should match that one item.
    // NOTE: The current logic for generic generic tags is: type='...' AND TRY_CAST(label) != ...
    // So it implicitely filters by type='width'.
    assert_eq!(res.results.len(), 1);

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

    for r in results.results {
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

    assert_eq!(results.results.len(), 1);

    assert!(results.results[0]
        .primary_value()
        .unwrap()
        .contains("alpha"));

    // 2. 複数のワイルドカード

    let results = fm.search("filename:project*").unwrap();

    assert_eq!(results.results.len(), 2);

    // 3. ワイルドカードなし (完全一致として動作)

    let results = fm.search("filename:project").unwrap();

    assert_eq!(results.results.len(), 0);

    let results = fm.search("filename:project_alpha.pdf").unwrap();
    assert_eq!(results.results.len(), 1);

    // 4. ? (任意の一文字)
    let results = fm.search("filename:project_alph?.pdf").unwrap();
    assert_eq!(results.results.len(), 1);
    assert_eq!(results.results[0].name, "project_alpha.pdf");

    // 5. [...] (文字セット) - alpha と beta 両方を拾いたいなら [ab]*
    let results = fm.search("filename:project_[ab]*").unwrap();
    assert_eq!(results.results.len(), 2); // alpha and beta

    // 6. [!...] (否定文字セット) - beta のみを拾う
    let results = fm.search("filename:project_[!a]eta*").unwrap();
    assert_eq!(results.results.len(), 1); // beta only

    // 7. クォート内でのGlob (無効化されるはず)
    let results = fm.search("filename:\"project_[ab]*\"").unwrap();
    assert_eq!(results.results.len(), 0); // リテラル一致を試みるため0件

    // 8. クォートでの完全一致
    let results = fm.search("filename:\"project_alpha.pdf\"").unwrap();
    assert_eq!(results.results.len(), 1);

    // 9. Type側の引用符
    let results = fm.search("\"filename\":project_alpha.pdf").unwrap();
    assert_eq!(results.results.len(), 1);

    // 10. Type側のGlob (実装済みか確認)
    // filename が対象になるはず
    let results = fm.search("*name:project_alpha.pdf").unwrap();
    assert_eq!(results.results.len(), 2);

    // 11. Type Wildcard + Value Prefix (Regression Test)
    // "exte*:^pd" should match project_alpha.pdf (extension: pdf)
    // This confirms that 'exte*' parses as a typed tag (not comparison) and '^pd' becomes 'pd*' glob.
    let results = fm.search("exte*:^pd").unwrap();
    assert!(
        results.results.len() > 0,
        "Should match .pdf files via 'exte*' type glob and '^pd' value prefix"
    );

    // 12. Type側のGlob ([...] / [!...])
    let results = fm.search("[f]ilename:project_alpha.pdf").unwrap();
    assert_eq!(results.results.len(), 1);

    let results = fm.search("[!f]ilename:project_alpha.pdf").unwrap();
    assert_eq!(results.results.len(), 0); // filename にはマッチしないはず

    // 13. Type側のGlob (?)
    let results = fm.search("file?ame:project_alpha.pdf").unwrap();
    assert_eq!(results.results.len(), 1);

    // 13. バックスラッシュ・エスケープ
    // テスト用の特殊ファイルを作成
    let special_file = root.join("[WIP]_test.txt");
    std::fs::File::create(&special_file)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // バックスラッシュなしだとGlobとして解釈され、マッチしない可能性がある（または意図しないマッチ）
    // ここでは \[WIP\] とすることでリテラルとして扱う
    let results = fm.search(r"filename:\[WIP\]_*").unwrap();
    assert_eq!(results.results.len(), 1);
    assert_eq!(results.results[0].name, "[WIP]_test.txt");

    Ok(())
}

#[test]
fn test_complex_search_combinations() {
    // Setup FM with dedicated temp dir
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // Create deterministic test data
    // 15 .rs files
    for i in 0..15 {
        std::fs::write(root.join(format!("test_src_{}.rs", i)), "").unwrap();
    }
    // Main file
    std::fs::write(root.join("main.rs"), "").unwrap();
    std::fs::write(root.join("lib.rs"), "").unwrap();
    // Mod file
    std::fs::write(root.join("mod.rs"), "").unwrap();
    // Other types
    std::fs::write(root.join("Cargo.toml"), "").unwrap();
    std::fs::write(root.join("LICENSE"), "").unwrap();
    std::fs::write(root.join("readme.txt"), "").unwrap();

    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // 1. Type Glob + Value Glob (exte*:r*)
    // Should match 15 test_src + main + lib + mod = 18 files. (Extension is "rs")
    let results = fm.search("exte*:r*").unwrap();
    assert!(
        results.results.len() >= 18,
        "exte*:r* should match all rs files"
    );

    // 2. Type Glob + AND + Comparison (exte*:rs & size:>0)
    // All files are empty (size 0), so this should be 0.
    // Wait, let's make main.rs non-empty
    std::fs::write(root.join("main.rs"), "content").unwrap();
    // Re-index to update metadata
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    let results = fm.search("exte*:rs & size:>0").unwrap();
    assert_eq!(
        results.results.len(),
        1,
        "exte*:rs & size:>0 should match main.rs"
    );

    // 3. Value Glob + OR + Prefix (name:*.toml | name:^LIC)
    let results = fm.search("name:*.toml | name:^LIC").unwrap();
    // Verify correct items are present (ignoring extra matches)
    assert!(
        results.results.iter().any(|r| r.name == "Cargo.toml"),
        "Should find Cargo.toml"
    );
    assert!(
        results.results.iter().any(|r| r.name == "LICENSE"),
        "Should find LICENSE"
    );

    // 4. Type Glob + Difference + Value Glob (exte*:rs - name:mod*)
    let all_rs = fm.search("exte*:rs").unwrap().results.len();
    let results_diff = fm.search("exte*:rs - name:mod*").unwrap();
    assert_eq!(
        results_diff.results.len(),
        all_rs - 1,
        "Difference should remove mod.rs"
    );
    assert!(results_diff.results.iter().all(|r| r.name != "mod.rs"));

    // 5. Grouping + Glob Types + Comparison
    // (exte*:rs | exte*:toml) & size:>0 -> main.rs only
    let results = fm.search("(exte*:rs | exte*:toml) & size:>0").unwrap();
    assert!(results.results.len() >= 1);
    assert!(results.results.iter().all(|r| r.name == "main.rs"));

    // 6. Value Glob + Value Prefix AND (name:*.rs & name:^mai)
    let results = fm.search("name:*.rs & name:^mai").unwrap();
    assert!(results.results.len() >= 1);
    assert!(results.results.iter().all(|r| r.name == "main.rs"));

    // 7. Bracket Glob (name:[m]ain.rs)
    let results = fm.search("name:[m]ain.rs").unwrap();
    assert!(results.results.len() >= 1);
    assert!(results.results.iter().any(|r| r.name == "main.rs"));

    // 8. Type Glob ('?' wildcard) + Value Exact (exte*:r?)
    let results = fm.search("exte*:r?").unwrap();
    assert!(results.results.len() >= 18);

    // 9. Double Glob (nam*:*.rs)
    let results = fm.search("nam*:*.rs").unwrap();
    assert!(results.results.len() >= 18);

    // 10. Type Prefix + Value Glob (item_kind:^fi & name:*.rs)
    // Matches 'file' type items which are .rs
    let results = fm.search("item_kind:^fi & name:*.rs").unwrap();
    assert!(results.results.len() >= 18);
}

#[test]
fn test_escaping_behavior() {
    use std::fs::File;
    // Setup explicit temp dir for file creation
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Create special files
    File::create(root.join("colon:file.txt")).unwrap();
    File::create(root.join("space file.txt")).unwrap();
    File::create(root.join("caret^file.txt")).unwrap();
    File::create(root.join("normal.txt")).unwrap();

    let db_dir = root.join(".ttfm/db");
    let fm = FileManager::new_with_db_dir(&db_dir).unwrap();
    fm.index_directory(root, None::<&fn(usize)>, false).unwrap();

    // 1. Escaped Colon (Using raw string)
    let res = fm.search(r"name:colon\:file.txt").unwrap();
    assert!(res.results.len() >= 1, "Should match escaped colon");
    assert!(res.results.iter().any(|r| r.name == "colon:file.txt"));

    // 2. Escaped Space
    let res = fm.search(r"name:space\ file.txt").unwrap();
    assert!(res.results.len() >= 1, "Should match escaped space");
    assert!(res.results.iter().any(|r| r.name == "space file.txt"));

    // 3. Escaped Caret
    let res = fm.search(r"name:caret\^file.txt").unwrap();
    assert!(res.results.len() >= 1, "Should match escaped caret");
    assert!(res.results.iter().any(|r| r.name == "caret^file.txt"));

    // 4. Quoted Colon
    let res = fm.search(r#"name:"colon:file.txt""#).unwrap();
    assert!(res.results.len() >= 1, "Should match quoted colon");

    // 5. Double Escape (colon + glob)
    let res = fm.search(r"name:colon\:*.txt").unwrap();
    assert!(res.results.len() >= 1, "Should match colon + glob");

    // 6. Mixed Logic
    let res = fm.search(r"name:colon\:* | name:space\ *").unwrap();
    assert!(res.results.len() >= 2, "Should match combined");
}
