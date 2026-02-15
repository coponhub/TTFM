#[cfg(test)]
mod tests {
    use crate::FileManager;
    use crate::types::SearchOptions;
    use tempfile::tempdir;
    use std::fs;

    #[test]
    fn test_verify_count_logic() {
        let db_dir = tempdir().unwrap();
        let index_dir = tempdir().unwrap();
        
        let fm = FileManager::new(db_dir.path()).unwrap();
        
        for i in 1..=5 {
            fs::write(index_dir.path().join(format!("file{}.txt", i)), "content").unwrap();
        }
        
        fm.index_directory(index_dir.path(), None::<&fn(usize)>, false).unwrap();
        
        let res_comp = fm.search("count()", SearchOptions::default()).unwrap();
        println!("count() result: {}", res_comp.results[0].name);
        
        let res_wild = fm.search("count(*:*)", SearchOptions::default()).unwrap();
        println!("count(*:*) result: {}", res_wild.results[0].name);
        
        let items = fm.search("path:", SearchOptions::default()).unwrap();
        println!("Total items found by path: projection: {}", items.results.len());
    }
}
