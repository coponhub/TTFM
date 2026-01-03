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
        ("filename:project & filename:report", 2),
        ("filename:project & -filename:draft", 2),
        ("filename:project & (filename:alpha | filename:beta)", 3),
        ("filename:image & filename:2024", 2),
        ("filename:image & filename:2024 & -extension:png", 1),
        ("-(extension:txt | extension:zip | extension:pdf | extension:jpg | extension:png)", 4), // 3 folders + 1 root directory
        
        // --- Added cases to verify set-based search fixes ---
        ("extension:pdf & filename:alpha", 1),   // Cross-attribute AND
        ("extension:txt & filename:project", 1), // Cross-attribute AND
        ("directory:work", 1),                   // Keyword search on directory
        ("directory:personal", 1),
        ("filename:backup & -extension:zip", 1), // Folder vs File with same name stem
        
        // --- Complex nested combinations ---
        ("(extension:pdf | extension:txt) & filename:alpha", 2), // (pdf OR txt) AND alpha
        ("extension:pdf & (filename:alpha | filename:beta)", 2), // pdf AND (alpha OR beta)
        ("(directory:work & filename:docs) | (extension:pdf & filename:beta)", 2), // (dir AND work) OR (pdf AND beta)
        ("filename:project & -(extension:pdf | extension:txt)", 0), // project AND NOT (pdf OR txt)
        
        // --- Deeply nested parentheses ---
        ("((extension:pdf | extension:txt) & filename:alpha) | directory:work", 3), // ((A|B)&C)|D
        
        // Folder specific searches (directories are also entries with filenames)
        ("filename:docs", 1),            // work_docs
        ("filename:photos | filename:backup", 3), // personal_photos, temp_backup, backup_2023.zip
        ("filename:work & filename:docs", 1),     // work_docs
        ("filename:docs & -filename:work", 0),    // none
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