use ttfm::search;
use super::{default_scope, inject_path_scope};
use anyhow::Result;
use tempfile::tempdir;

define_cases! {
    label_calc_arith_projection: {
        setup: |_dir| Ok(()),
        modify: None,
        format_query: default_scope,
        query: "(size: / 1024)",
        assert: |_res, _dir| Ok(()),
    },
    label_calc_arith_complex: {
        setup: |_dir| Ok(()),
        modify: None,
        format_query: default_scope,
        query: "extension:rs & (size: * 2)",
        assert: |_res, _dir| Ok(()),
    },
    label_calc_grouped_set_arith: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), vec![0u8; 100])?;
            std::fs::write(dir.join("b.txt"), vec![0u8; 200])?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "(extension:rs & size:) * 2",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "should return results");
            assert_eq!(res.results[0].raw_repr(), "200", "size(100) * 2 = 200");
            Ok(())
        },
    },
    label_calc_arith_units: {
        setup: |dir| {
            std::fs::write(dir.join("test.txt"), "some content")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "(size: / 2) :> 100MB",
        assert: |_res, _dir| Ok(()),
    },
    label_calc_arith_units_reverse: {
        setup: |dir| {
            std::fs::write(dir.join("test.txt"), "some content")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "100MB :< (size: / 2)",
        assert: |_res, _dir| Ok(()),
    },
}

/// 複合比較クエリ (成功/エラー混在のため移行不可)
#[test]
fn test_complex_comparisons() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry).initialize_tables().unwrap();
    let db_dir_cache = ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) = (db_dir_store, db_dir_registry, db_dir_cache);

    ttfm::indexing::Indexer::new(&store, &registry).run(root, None::<&fn(usize)>, false).unwrap();

    let query_agg_agg = "sum(size:) > count(extension:rs)";
    assert!(
        search::search(&store, &registry, &cache, query_agg_agg, Default::default()).is_ok(),
        "Agg vs Agg should be valid"
    );

    let query_agg_calc = "(sum(size:) / 1024) > 100";
    assert!(
        search::search(&store, &registry, &cache, query_agg_calc, Default::default()).is_ok(),
        "Agg calculation vs Literal should be valid"
    );

    let query_proj_calc = "size: :> (1024 * 1024)";
    assert!(
        search::search(&store, &registry, &cache, query_proj_calc, Default::default()).is_ok(),
        "Proj vs Calculation with label op should be valid"
    );

    let query_forbidden = "size: > 100";
    let res_forbidden = search::search(&store, &registry, &cache, query_forbidden, Default::default());
    assert!(
        res_forbidden.is_err(),
        "size: > 100 should be a syntax error according to design"
    );

    let query_agg_proj = "max(size:) == size:";
    let res_agg_proj = search::search(&store, &registry, &cache, query_agg_proj, Default::default());
    assert!(res_agg_proj.is_err(), "max(size:) == size: should be a syntax error if both sides must be scalar");
}

/// 算術射影の構文確認 (複数クエリのため部分的に残留)
#[test]
fn test_arithmetic_projection_syntax() -> Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry).initialize_tables()?;
    let db_dir_cache = ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) = (db_dir_store, db_dir_registry, db_dir_cache);

    let result = search::search(&store, &registry, &cache, "(size: / 1024)", Default::default());
    assert!(
        result.is_ok(),
        "Failed to parse arithmetic projection: {:?}",
        result.err()
    );

    let result2 = search::search(&store, &registry, &cache, "extension:rs & (size: * 2)", Default::default());
    assert!(
        result2.is_ok(),
        "Failed to parse complex query: {:?}",
        result2.err()
    );

    Ok(())
}
