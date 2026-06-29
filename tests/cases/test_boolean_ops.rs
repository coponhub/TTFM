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

use tempfile::tempdir;
use ttfm::search;

#[test]
fn test_boolean_arithmetic_ops() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join("db");

    // システムの TTFM_HOME の干渉を避けるため、環境変数を上書き
    std::env::set_var("TTFM_HOME", root);

    let data_dir = root.join("data");
    std::fs::create_dir(&data_dir)?;

    // ディレクトリ1つ、ファイル2つを作成
    // is_dir: はディレクトリなら true, ファイルなら false
    std::fs::create_dir(data_dir.join("subdir"))?;
    std::fs::write(data_dir.join("file1.txt"), "content1")?;
    std::fs::write(data_dir.join("file2.txt"), "content2")?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    let cache = ttfm::CacheManager::new(store.db_dir.join("cache"), 0);
    ttfm::indexing::Indexer::new(&store, &registry).run(
        &data_dir,
        None::<&fn(usize)>,
        false,
    )?;

    // デバッグ: 全アイテム数を確認 (実ファイル/ディレクトリのみ。インデックスルートの data は除く)
    let all_items =
        search::search(&store, &registry, &cache, "", Default::default())?;
    let files_only: Vec<_> = all_items
        .results
        .iter()
        .filter(|r| {
            r.item_kind == ttfm::ItemKind::File && r.raw_repr() != "data"
        })
        .collect();
    assert_eq!(
        files_only.len(),
        3,
        "Total file items should be 3. Found: {:?}",
        files_only.iter().map(|r| r.raw_repr()).collect::<Vec<_>>()
    );

    // 1. Boolean の sum 集計 (TRUE=1, FALSE=0)
    // ディレクトリが2つ (data, subdir)、ファイルが2つなので sum(is_dir:) は 2 となるはず
    let query_sum = "sum(is_dir:)";
    let res_sum = search::search(
        &store,
        &registry,
        &cache,
        query_sum,
        Default::default(),
    )?;
    assert_eq!(
        res_sum.results[0].raw_repr(),
        "2",
        "sum(is_dir:) should be 2 (data and subdir)"
    );

    // 1b. フィルタ付きの sum 集計
    let query_sum_filter = "sum(name:subdir & is_dir:)";
    let res_sum_filter = search::search(
        &store,
        &registry,
        &cache,
        query_sum_filter,
        Default::default(),
    )?;
    assert_eq!(
        res_sum_filter.results[0].raw_repr(),
        "1",
        "sum(name:subdir & is_dir:) should be 1"
    );

    // 2. Boolean への算術演算 (is_dir: + 1) を sum で検証
    // アイテム 4 つ: ディレクトリ 2 つ (1+1=2), ファイル 2 つ (0+1=1)
    // 合計: 2*2 + 1*2 = 6
    let query_calc = "sum(is_dir: + 1)";
    let res_calc = search::search(
        &store,
        &registry,
        &cache,
        query_calc,
        Default::default(),
    )?;
    assert_eq!(
        res_calc.results[0].raw_repr(),
        "6",
        "sum(is_dir: + 1) should be 6. Found: {}",
        res_calc.results[0].raw_repr()
    );

    // 3. Boolean 同士の比較 (is_dir:true)
    let query_cmp = "is_dir:true";
    let res_cmp = search::search(
        &store,
        &registry,
        &cache,
        query_cmp,
        Default::default(),
    )?;
    // data と subdir がヒットするはずなので 2
    assert_eq!(
        res_cmp.results.len(),
        2,
        "2 directories should match. Found: {:?}",
        res_cmp
            .results
            .iter()
            .map(|r| r.raw_repr())
            .collect::<Vec<_>>()
    );

    Ok(())
}
