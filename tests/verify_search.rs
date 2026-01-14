use ttfm::FileManager;
use std::fs::File;
use tempfile::tempdir;

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
        ("filename:project_alpha_report.pdf & -filename:project_alpha_draft.txt", 1),
        ("filename:project_alpha_report.pdf | filename:project_beta_report.pdf", 2),
        ("filename:image_vacation_2024.jpg | filename:image_work_2024.png", 2),
        ("filename:image_vacation_2024.jpg", 1),
        ("-(extension:txt | extension:zip | extension:pdf | extension:jpg | extension:png)", 4), // 3 folders + 1 root directory
        
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

                println!("FAIL (Expected {}, got {})", expected_count, results.len());

                for r in &results {

                    println!(" - Found: {:?}", r);

                }

                panic!("Test failed for query: '{}'", query);

            }

        }

    

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

    

        // 2. クエリ実行: item_kind:type | -extension:rs

        let query = "item_kind:type | -extension:rs";

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

    fn test_glob_search_behavior() {

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

    }

    