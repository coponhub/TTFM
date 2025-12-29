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
        ("project & report", 2),
        ("project & -draft", 2),
        ("project & (alpha | beta)", 3),
        ("image & 2024", 2),
        ("image & 2024 & -png", 1),
        ("-(txt | zip | pdf | jpg | png)", 4), // 3 folders + 1 root directory
        // Folder specific searches
        ("docs", 1),            // work_docs
        ("photos | backup", 3), // personal_photos, temp_backup, backup_2023.zip
        ("work & docs", 1),     // work_docs
        ("docs & -work", 0),    // none
        // Error cases (implicit AND logic check)
        ("project report", 0), 
    ];

    for (query, expected_count) in test_cases {
        print!("Query: '{:<25}' -> ", query);
        let results = fm.search(query).unwrap_or_default();
        
        if results.len() == expected_count {
            println!("OK ({} hits)", results.len());
        } else {
            println!("FAIL (Expected {}, got {})", expected_count, results.len());
            for r in &results {
                println!(" - Found: {}", r);
            }
            panic!("Test failed for query: '{}'", query);
        }
    }

    Ok(())
}