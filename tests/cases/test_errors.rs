// Copyright (C) 2026 coponhub
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use ttfm::search;
use anyhow::Result;
use tempfile::tempdir;

// =========================================================================
// Over-aggregation error tests (集約過剰エラー)
// =========================================================================

fn make_store_registry_cache(
) -> Result<(ttfm::db::Store, ttfm::tag::TagRegistry, ttfm::CacheManager)>
{
    let dir = tempdir()?;
    let db_dir = dir.path().join(".ttfm/db");
    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    let cache =
        ttfm::CacheManager::new(store.db_dir.join("cache"), 0);
    // dir を drop させないためリーク（テスト内の短命な用途）
    std::mem::forget(dir);
    Ok((store, registry, cache))
}

fn assert_over_agg_error(query: &str) {
    let (store, registry, cache) = make_store_registry_cache().unwrap();
    let result =
        search::search(&store, &registry, &cache, query, Default::default());
    assert!(
        result.is_err(),
        "Expected error for over-aggregation query '{query}', but got Ok"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("scalar"),
        "Error for '{query}' should mention 'scalar', got: {msg}"
    );
}

#[test]
fn test_over_aggregation_count_count() {
    assert_over_agg_error("count(count())");
}

#[test]
fn test_over_aggregation_sum_count() {
    assert_over_agg_error("sum(count())");
}

#[test]
fn test_over_aggregation_count_sum() {
    assert_over_agg_error("count(sum(size:))");
}

#[test]
fn test_over_aggregation_nested_with_nvalue() {
    // ユーザーが実際に panic を確認したケース
    assert_over_agg_error("sum(count(extension: &: count()))");
}

#[test]
fn test_mismatched_comparison_error_message() -> Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let files_dir = root.join("files");
    std::fs::create_dir(&files_dir)?;

    std::fs::write(files_dir.join("test.txt"), "content")?;

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry).initialize_tables()?;
    let db_dir_cache = ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) = (db_dir_store, db_dir_registry, db_dir_cache);
    ttfm::indexing::Indexer::new(&store, &registry).run(&files_dir, None::<&fn(usize)>, false)?;

    // size: > 100 という形式（本来は :> であるべき）を実行
    let result = search::search(&store, &registry, &cache, "size: > 100", Default::default());

    assert!(
        result.is_err(),
        "Should fail for mismatched comparison operator"
    );

    let err_msg = format!("{}", result.unwrap_err());

    assert!(
        err_msg.contains("Invalid operator '>'"),
        "Error message should point out the invalid operator"
    );
    assert!(
        err_msg.contains("Scalar comparison cannot be applied to a Projection"),
        "Error message should explain why it's invalid"
    );
    assert!(
        err_msg.contains("Did you mean: 'size: :> 100'"),
        "Error message should suggest a correct alternative"
    );

    Ok(())
}

#[test]
fn test_repro_mismatched_group_by_keys_error_msg() -> Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    let files_dir = root.join("files");
    std::fs::create_dir(&files_dir)?;

    std::fs::write(files_dir.join("test.txt"), "content")?;

    let db_dir_registry = ttfm::tag::TagRegistry::with_standard();
    let db_dir_store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&db_dir_store, &db_dir_registry).initialize_tables()?;
    let db_dir_cache = ttfm::CacheManager::new(db_dir_store.db_dir.join("cache"), 0);
    let (store, registry, cache) = (db_dir_store, db_dir_registry, db_dir_cache);
    ttfm::indexing::Indexer::new(&store, &registry).run(&files_dir, None::<&fn(usize)>, false)?;

    // --- Investigation ---
    // 1.1 正常系: プレーンなプロジェクション同士の演算（size: + mtime:）
    let ok_query1 = "(size: + mtime:) :> 10";
    let ok_result1 = search::search(&store, &registry, &cache, ok_query1, Default::default());
    if let Err(e) = &ok_result1 {
        println!("QUERY 1 ERROR: {:?}", e);
    }
    assert!(
        ok_result1.is_ok(),
        "Simple projection arithmetic should succeed (Top level)"
    );

    // 1.2 Nest 内での非集約タグ算術: 仕様上は最後の値（Calculation）を使って比較する
    // (parentdir: &: (size: + mtime:)) :> 10 → parentdir ごとに size+mtime を評価して比較
    let query2 = "(parentdir: &: (size: + mtime:)) :> 10";
    let result2 = search::search(&store, &registry, &cache, query2, Default::default());
    assert!(
        result2.is_ok(),
        "Arithmetic over non-aggregated tags within Nest should succeed: {:?}",
        result2.err()
    );

    // 2. Phase 3 以降: 異なるキーを持つ Nest 同士の算術は Level 3+ Nest として解決される
    // (parentdir: &: count()) / (extension: &: count()) はエラーではなく、
    // 深いネスト (merged_keys = [parentdir, extension]) として解釈される
    let query = "((parentdir: &: count()) / (extension: &: count())) :> 1";
    let result = search::search(&store, &registry, &cache, query, Default::default());

    assert!(
        result.is_ok(),
        "Mixed-key Nest arithmetic should succeed in Phase 3 (Level 3+ Nest), got: {:?}",
        result.err()
    );

    Ok(())
}
