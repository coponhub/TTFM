// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
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

use crate::cases::has_item_tags;
use std::fs::File;
use tempfile::tempdir;
use ttfm::types::ItemId;
use ttfm::{search, tagging};

// ──────────────────────────────────────────────
// スタンドアロン: bare クエリの挙動確認
// ──────────────────────────────────────────────

/// bare `extension:` クエリが拡張子なしファイルに対して空ラベルを生まないことを確認
#[test]
fn test_projection_no_empty_labels() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    File::create(root.join("file_with_ext.txt"))?;
    File::create(root.join("file_no_ext"))?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let res =
        search::search(&store, &registry, "extension:", Default::default())?;

    assert!(
        res.results.iter().any(|r| r.raw_repr() == "txt"),
        "Output should contain 'txt' label for file_with_ext.txt"
    );
    let has_empty = res.results.iter().any(|r| r.raw_repr().is_empty());
    assert!(
        !has_empty,
        "Output should NOT contain empty label name. Found labels: {:?}",
        res.results.iter().map(|r| r.raw_repr()).collect::<Vec<_>>()
    );
    Ok(())
}

// ──────────────────────────────────────────────
// define_cases! 移行済みケース
// ──────────────────────────────────────────────

define_cases! {
    test_arithmetic_projection_eav: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), b"hello")?;
            std::fs::write(dir.join("b.txt"), b"hello!!")?;
            Ok(())
        },
        modify: None,
        format_query: super::default_scope,
        query: "size: + mtime:",
        assert: |res, _dir| {
            assert!(
                res.results.len() >= 2,
                "size: + mtime: should return at least 2 groups. Got: {:?}",
                res.results.iter().map(|r| r.raw_repr()).collect::<Vec<_>>()
            );
            let vals: Vec<f64> = res
                .results
                .iter()
                .map(|r| r.raw_repr().parse::<f64>().unwrap_or_else(|_| panic!("name should be numeric, got: {}", r.raw_repr())))
                .collect();
            for &v in &vals {
                assert!(v > 0.0, "calc value should be > 0, got: {}", v);
            }
            let has_pair_with_diff_2 = vals
                .iter()
                .any(|&vi| vals.iter().any(|&vj| (vj - vi - 2.0).abs() < 0.001));
            assert!(
                has_pair_with_diff_2,
                "Expected a pair of results differing by exactly 2 (b.txt size=7 vs a.txt size=5). Got vals: {:?}",
                vals
            );
            Ok(())
        },
    },
    test_arithmetic_projection_eav_subtraction: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), b"hello")?;
            std::fs::write(dir.join("b.txt"), b"hello!!")?;
            Ok(())
        },
        modify: None,
        format_query: super::default_scope,
        query: "mtime: - size:",
        assert: |res, _dir| {
            assert!(
                res.results.len() >= 2,
                "mtime: - size: should return at least 2 groups. Got: {:?}",
                res.results.iter().map(|r| r.raw_repr()).collect::<Vec<_>>()
            );
            let vals: Vec<f64> = res
                .results
                .iter()
                .map(|r| r.raw_repr().parse::<f64>().unwrap_or_else(|_| panic!("name should be numeric, got: {}", r.raw_repr())))
                .collect();
            for &v in &vals {
                assert!(v > 0.0, "mtime - size should be > 0 (timestamp >> file size), got: {}", v);
            }
            let has_pair_with_diff_minus2 = vals
                .iter()
                .any(|&vi| vals.iter().any(|&vj| (vj - vi + 2.0).abs() < 0.001));
            assert!(
                has_pair_with_diff_minus2,
                "Expected a pair differing by -2 (mtime same, b.txt size=7 vs a.txt size=5). Got vals: {:?}",
                vals
            );
            Ok(())
        },
    },
    test_arithmetic_projection_eav_with_literal: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), b"hello")?;
            std::fs::write(dir.join("b.txt"), b"hello!!")?;
            Ok(())
        },
        modify: None,
        format_query: super::default_scope,
        query: "size: / 1024",
        assert: |res, _dir| {
            assert!(
                res.results.len() >= 2,
                "size: / 1024 should return at least 2 groups. Got: {:?}",
                res.results.iter().map(|r| r.raw_repr()).collect::<Vec<_>>()
            );
            let vals: Vec<f64> = res
                .results
                .iter()
                .map(|r| r.raw_repr().parse::<f64>().unwrap_or_else(|_| panic!("name should be numeric, got: {}", r.raw_repr())))
                .collect();
            let has_pair_with_diff = vals
                .iter()
                .any(|&vi| vals.iter().any(|&vj| (vj - vi - 2.0 / 1024.0).abs() < 1e-6));
            assert!(
                has_pair_with_diff,
                "Expected a pair differing by 2/1024 (size diff / 1024). Got vals: {:?}",
                vals
            );
            Ok(())
        },
    },
    // ── 表示ルーティング検証 ───────────────────────────────────────────────────
    // has_projection_results() が正しく true/false を返すことを保証する。
    // Lv.2 プロジェクション (nvalue のみ) → true、ラベル比較 → false (Lv.1 フラットリスト)
    test_display_routing_nvalue_projection: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), b"fn main() {}")?;
            std::fs::write(dir.join("b.rs"), b"pub fn foo() {}")?;
            std::fs::write(dir.join("c.txt"), b"hello")?;
            Ok(())
        },
        modify: None,
        format_query: super::default_scope,
        query: "extension: &: count()",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "should return results");
            assert!(
                res.has_projection_results(),
                "extension: &: count() (Lv.2 projection) should be routed as projection. names={:?}",
                res.results.iter().map(|r| r.raw_repr()).collect::<Vec<_>>()
            );
            Ok(())
        },
    },
    test_display_routing_parentdir_label_cmp: {
        setup: |dir| {
            std::fs::write(dir.join("x.rs"), b"x")?;
            std::fs::write(dir.join("y.rs"), b"y")?;
            Ok(())
        },
        modify: None,
        format_query: super::default_scope,
        query: "parentdir: &: count() :> 1",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "should return results");
            assert!(
                !res.has_projection_results(),
                "parentdir: &: count() :> 1 (Lv.1 flat list) should NOT be projection. names={:?}",
                res.results.iter().map(|r| r.raw_repr()).collect::<Vec<_>>()
            );
            Ok(())
        },
    },
}

// ──────────────────────────────────────────────
// 移行不可: 多クエリ / 複雑な構造アサーション
// ──────────────────────────────────────────────

#[test]
fn test_projection_queries() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // テストデータの作成
    File::create(root.join("test.rs")).unwrap();
    File::create(root.join("test.txt")).unwrap();
    std::fs::create_dir(root.join("test_dir")).unwrap();

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .initialize_tables()
        .unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(root, None::<&fn(usize)>, false)
        .unwrap();

    // 1. extension: (投影 - 転置: Label → Items)
    // 投影結果はラベル値（rs, txt）のリストとして返される
    let results =
        search::search(&store, &registry, "extension:", Default::default())
            .unwrap();
    println!(
        "Matches for 'extension:': {:?}",
        results
            .results
            .iter()
            .map(|r| r.raw_repr())
            .collect::<Vec<String>>()
    );
    assert_eq!(
        results.results.len(),
        2,
        "extension: should return 2 label values (rs, txt). Found: {:?}",
        results
            .results
            .iter()
            .map(|r| r.raw_repr())
            .collect::<Vec<String>>()
    );
    // 転置: results には label items が格納される（name="rs", name="txt"）
    assert!(results.results.iter().any(|r| r.raw_repr() == "rs"));
    assert!(results.results.iter().any(|r| r.raw_repr() == "txt"));
    assert!(has_item_tags(&results.results));

    // 2. directory: (投影 -> is_dir:true + projection:filename - 転置)
    let results =
        search::search(&store, &registry, "directory:", Default::default())
            .unwrap();
    println!(
        "Matches for 'directory:': {:?}",
        results
            .results
            .iter()
            .map(|r| r.raw_repr())
            .collect::<Vec<String>>()
    );
    // 転置: label items として filename 値が返される（test_dir など）
    assert!(
        results.results.len() >= 1,
        "directory: should return at least 1 label (test_dir filename)"
    );
    assert!(results.results.iter().any(|r| r.raw_repr() == "test_dir"));
    // 仮想ラベル directory: は内部で filename を投影する
    assert!(has_item_tags(&results.results));

    // 3. filename: (投影 -> is_dir:false + projection:filename - 転置)
    let results =
        search::search(&store, &registry, "filename:", Default::default())
            .unwrap();
    println!(
        "Matches for 'filename:': {:?}",
        results
            .results
            .iter()
            .map(|r| r.raw_repr())
            .collect::<Vec<String>>()
    );
    // 転置: label items として filename 値が返される（:test.rs, :test.txt）
    assert_eq!(
        results.results.len(),
        2,
        "filename: should return 2 label values (test.rs, test.txt). Found: {:?}",
        results.results.iter().map(|r| r.raw_repr()).collect::<Vec<String>>()
    );
    // 転置後は全て label items
    assert!(results
        .results
        .iter()
        .all(|r| r.item_kind == ttfm::ItemKind::Volatile));
    // 仮想ラベル filename: は内部で filename を投影する
    assert!(has_item_tags(&results.results));

    // 4. origin:system
    // 全てのアイテムは system 由来のタグを持つはず（初期状態）
    let results =
        search::search(&store, &registry, "origin:system", Default::default())
            .unwrap();
    assert!(results.results.len() >= 3);

    // 5. 複合クエリ
    let results = search::search(
        &store,
        &registry,
        "extension: & directory:",
        Default::default(),
    )
    .unwrap();
    assert_eq!(
        results.results.len(),
        0,
        "No directories should have an extension in this test"
    );

    // 6. type: (全アイテムヒット確認 + SType網羅性確認)
    let results =
        search::search(&store, &registry, "type:", Default::default()).unwrap();
    assert!(results.results.len() >= 3, "type: should match all items");
    assert!(has_item_tags(&results.results));

    // 転置: results には label items が格納され、各 label の name がタグタイプ名
    // 結果に含まれる全てのタグタイプ（label の name）を収集
    let mut found_types = std::collections::HashSet::new();
    for r in &results.results {
        found_types.insert(r.raw_repr().clone());
    }

    // 主要なSTypeが含まれているか確認
    // アイテムに実際に付与されている属性（item_kind/rank/origin/name）は
    // type: プロジェクションに載るべき（lens_schema::complement_type_groups
    // によるクエリ側での Fixed 属性合成）。
    let expected_types = vec!["item_kind", "name", "rank", "origin"];
    for t in expected_types {
        assert!(
            found_types.contains(t),
            "type: projection results should contain label with name '{}'. Found types: {:?}",
            t,
            found_types
        );
    }

    // 7. typedtag: (全アイテムヒット確認 + 値の検証)
    let results =
        search::search(&store, &registry, "tag:", Default::default()).unwrap();
    println!("Matches for 'tag:': {} items", results.results.len());
    assert!(results.results.len() >= 3, "tag: should match all items");
    assert!(has_item_tags(&results.results));

    // 検証: アイテムが tag タグを持っているか
    let has_tag = results
        .results
        .iter()
        .any(|r| r.get_tag_value("tag").is_some());
    assert!(has_tag, "Items should have 'tag' tag values in Item");

    // 追加検証: extension: 結果の中身
    let ext_results =
        search::search(&store, &registry, "extension:", Default::default())
            .unwrap();
    for r in &ext_results.results {
        // test.rs は extension:rs を持つ
        if r.raw_repr() == "test.rs" {
            let ext = r
                .get_tag_value("extension")
                .expect("test.rs should have extension tag");
            assert_eq!(ext, "rs");
        }
    }
    // 8. rank: (投影 -> rank column)
    // rank は oneview 上の全ての行で有効な値を持つカラムだが、
    // プロジェクションクエリとしては type='rank' ではなく rank column のユニーク値を期待する。
    let results =
        search::search(&store, &registry, "rank:", Default::default()).unwrap();
    // 全てのアイテムは初期状態で rank=0 のはず (あるいは計算された値)
    // 実装が未対応なら0件になる
    println!("Matches for 'rank:': {} items", results.results.len());
    // NOTE: 現在の実装では rank: は type='rank' を検索してしまい、0件になる可能性がある。
    // ユーザーの指摘により、これをサポートすべきか確認するフェーズ。
    // 一旦アサーションは入れず、挙動を確認する。
    if has_item_tags(&results.results) {
        // サポートされている場合
        assert!(
            !results.results.is_empty(),
            "rank: should return items if supported"
        );
    }

    // 9. category: (投影 -> type='category')
    // label は SType::Label (仮想タグ) として予約されているため、
    // 任意のタグ名のテストには category を使用する。
    let note_id =
        tagging::add_item(&store, &registry, "note", "Category Test Note")
            .unwrap();
    tagging::tag_item(
        &store,
        &registry,
        &note_id.to_string(),
        "category:important",
    )
    .unwrap();

    let results =
        search::search(&store, &registry, "category:", Default::default())
            .unwrap();
    assert!(
        results.results.len() >= 1,
        "category: should match items with category tag"
    );
    assert!(has_item_tags(&results.results));
    // 転置: results には label items が格納され、name が "important" であることを確認
    let has_val = results.results.iter().any(|r| {
        r.item_kind == ttfm::ItemKind::Volatile && r.raw_repr() == "important"
    });
    assert!(has_val, "Should find 'important' category label");

    // 10. label: (Volatile Tag -> All Labels)
    // label: は「全てのタグのラベル」を集約する揮発性プロジェクション。
    let results =
        search::search(&store, &registry, "label:", Default::default())
            .unwrap();
    // 全てのアイテムは何かしらのラベル（name, item_kind 等）を持つためヒットする
    assert!(
        results.results.len() >= 3,
        "label: should match all tagged items"
    );
    assert!(has_item_tags(&results.results));
}

#[test]
fn test_projection_returns_label_volatile_items() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // テストデータの作成
    File::create(root.join("test.rs")).unwrap();
    File::create(root.join("test.txt")).unwrap();
    File::create(root.join("another.rs")).unwrap();

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir).unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .initialize_tables()
        .unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(root, None::<&fn(usize)>, false)
        .unwrap();

    // extension: で投影
    let results =
        search::search(&store, &registry, "extension:", Default::default())
            .unwrap();

    // 検証1: type_for_projection が設定されている
    assert!(has_item_tags(&results.results));

    // 検証2: results に label items が格納されている
    assert!(
        !results.results.is_empty(),
        "projection should return label items"
    );

    // 検証3: 各 Item が Volatile ID を持っている
    for item in &results.results {
        // ID が Volatile(u64) であることを確認
        if let ItemId::Volatile(_) = item.id {
            // 検証4: item_kind が Label である
            assert_eq!(
                item.item_kind,
                ttfm::ItemKind::Volatile,
                "Label volatile item should have item_kind=Label"
            );

            // 検証5: name が空ではない（ラベル値）
            assert!(
                !item.raw_repr().is_empty(),
                "Label volatile item name should not be empty"
            );

            // 検証6: tags に "item:name#id" 形式のタグが含まれている
            // Type="item", Label="name#id" 形式であることを確認
            let has_item_ref = item.tags.entries.iter().any(|entry| {
                entry.label.tag_type().as_str() == "item"
                    && entry.label.as_str().contains('#')
            });
            assert!(
                has_item_ref,
                "Label volatile item should contain Type='item' tags with Label='name#id', found: {:?}",
                item.tags.entries.iter().map(|e| format!("{}:{}", e.label.tag_type().as_str(), e.label.as_str())).collect::<Vec<_>>()
            );

            // 検証7: item_count に total_count が保存されている
            assert!(
                item.item_count.is_some(),
                "Label volatile item should have item_count (total_count)"
            );

            let total_count_str = item.item_count.as_ref().unwrap().as_str();
            let total_count: usize = total_count_str
                .parse()
                .expect("item_count should be parseable as usize");
            assert!(total_count > 0, "total_count should be greater than 0");

            // 検証8: tagsの数が100件以下である（100件制限）かつtotal_count以下である
            assert!(
                item.tags.entries.len() <= 100,
                "tags count should be <= 100 (with 100-item limit), found {}",
                item.tags.entries.len()
            );
            assert!(
                item.tags.entries.len() <= total_count || total_count > 100,
                "tags count ({}) should be <= total_count ({}) or total_count should be > 100",
                item.tags.entries.len(),
                total_count
            );
        } else {
            panic!(
                "Projection should return Label volatile items, but got: {:?}",
                item.id
            );
        }
    }

    // 検証9: "rs" ラベルが存在する（test.rs, another.rs）
    let rs_label = results.results.iter().find(|item| item.raw_repr() == "rs");
    assert!(
        rs_label.is_some(),
        "Should find 'rs' label in projection results"
    );

    if let Some(rs_item) = rs_label {
        // rs ラベルは2つのファイルを参照しているはず
        let item_ref_count = rs_item.tags.entries.len();
        assert!(
            item_ref_count >= 2,
            "rs label should reference at least 2 files (test.rs, another.rs), found {}",
            item_ref_count
        );

        // item:test.rs または item:another.rs が含まれているか確認
        let has_test_rs = rs_item
            .tags
            .entries
            .iter()
            .any(|entry| entry.label.as_str().contains("test.rs"));
        let has_another_rs = rs_item
            .tags
            .entries
            .iter()
            .any(|entry| entry.label.as_str().contains("another.rs"));
        assert!(
            has_test_rs || has_another_rs,
            "rs label should contain references to test.rs or another.rs"
        );
    }
}

// `scan_hash:` は内部専用カラムの lens descriptor 削除後、未登録タグとして
// 扱われるべき（Binder Error にならない）。
#[test]
fn test_scan_hash_is_treated_as_unregistered_tag() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    File::create(root.join("a.txt"))?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let results =
        search::search(&store, &registry, "scan_hash:", Default::default())?;
    assert!(
        results.results.is_empty(),
        "scan_hash: should be an unregistered tag with no matches, not a Binder Error"
    );

    Ok(())
}

// LabelSetOp（`type: | extension:` 等）で合成される type 側ラベルにも
// item_kind/rank/origin のような Fixed 属性が含まれるべき（`type:` 単体と同じ集合）。
#[test]
fn test_label_set_op_includes_fixed_attributes() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    File::create(root.join("test.rs"))?;
    File::create(root.join("test.txt"))?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let results = search::search(
        &store,
        &registry,
        "type: | extension:",
        Default::default(),
    )?;
    let found: std::collections::HashSet<String> = results
        .results
        .iter()
        .map(|r| r.raw_repr().clone())
        .collect();

    for t in ["item_kind", "rank", "origin"] {
        assert!(
            found.contains(t),
            "type: | extension: should contain label '{}'. Found: {:?}",
            t,
            found
        );
    }

    Ok(())
}

// `count(type:)` は bare `type:` プロジェクションが返すラベル数と一致すべき
// （Fixed 属性合成が集約経路にも反映されていること）。
#[test]
fn test_count_type_matches_bare_type_projection() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    File::create(root.join("test.rs"))?;
    File::create(root.join("test.txt"))?;

    let registry = ttfm::tag::TagRegistry::with_standard();
    let store = ttfm::db::Store::open(&db_dir)?;
    ttfm::indexing::Indexer::new(&store, &registry).initialize_tables()?;
    ttfm::indexing::Indexer::new(&store, &registry).run(
        root,
        None::<&fn(usize)>,
        false,
    )?;

    let bare_results =
        search::search(&store, &registry, "type:", Default::default())?;
    let bare_count = bare_results.results.len();

    let count_results =
        search::search(&store, &registry, "count(type:)", Default::default())?;
    let counted: f64 = count_results.results[0].raw_repr().parse()?;

    assert_eq!(
        counted as usize, bare_count,
        "count(type:) ({}) should equal bare 'type:' label count ({})",
        counted, bare_count
    );
    // test.rs/test.txt のみのセットアップでは、item_kind/rank/origin を含む
    // Fixed 属性合成後、少なくとも 13 種類の type ラベルが存在するはず
    // （size/file_id/mtime/parentdir/stem/filename/is_dir/path/name/extension
    // の既存10種 + item_kind/rank/origin の3種）。
    assert!(
        bare_count >= 13,
        "Expected at least 13 types after Fixed attribute synthesis, got {}",
        bare_count
    );

    Ok(())
}
