#[cfg(test)]
mod tests {
    use std::fs;
    use tempfile::tempdir;
    use ttfm::search;
    use ttfm::SearchOptions;

    #[test]
    fn test_verify_count_logic() {
        let db_dir = tempdir().unwrap();
        let index_dir = tempdir().unwrap();

        let registry = ttfm::tag::TagRegistry::with_standard();
        let store = ttfm::db::Store::open(db_dir.path()).unwrap();
        ttfm::indexing::Indexer::new(&store, &registry).initialize_tables().unwrap();
        let cache = ttfm::CacheManager::new(store.db_dir.join("cache"), 0);

        for i in 1..=5 {
            fs::write(
                index_dir.path().join(format!("file{}.txt", i)),
                "content",
            )
            .unwrap();
        }

        ttfm::indexing::Indexer::new(&store, &registry)
            .run(index_dir.path(), None::<&fn(usize)>, false)
            .unwrap();

        let res_comp = search::search(&store, &registry, &cache, "count()", SearchOptions::default()).unwrap();
        println!("count() result: {}", res_comp.results[0].raw_repr());

        let res_wild =
            search::search(&store, &registry, &cache, "count(*:*)", SearchOptions::default()).unwrap();
        println!("count(*:*) result: {}", res_wild.results[0].raw_repr());

        let items = search::search(&store, &registry, &cache, "path:", SearchOptions::default()).unwrap();
        println!(
            "Total items found by path: projection: {}",
            items.results.len()
        );
    }
}
