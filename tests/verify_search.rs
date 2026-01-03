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
        ("(directory:work_docs & filename:work_docs) | (extension:pdf & filename:project_beta_report.pdf)", 2),
        
        // --- Deeply nested parentheses ---
        ("((extension:pdf | extension:txt) & filename:project_alpha_report.pdf) | directory:work_docs", 2),
        
        // Folder specific searches (directories are also entries with filenames)
        ("filename:work_docs", 1),            // work_docs
        ("filename:personal_photos | filename:temp_backup | filename:backup_2023.zip", 3),
        ("filename:work_docs & directory:work_docs", 1),     
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