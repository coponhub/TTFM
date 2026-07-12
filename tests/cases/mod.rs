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

use std::path::Path;
use ttfm::response::SearchResponse;

// ──────────────────────────────────────────────
// 共通テストケース構造体
// ──────────────────────────────────────────────

pub(super) struct QueryTestCase {
    pub name: &'static str,
    pub setup: fn(&Path) -> anyhow::Result<()>,
    /// DBのインデックス完了後に、ファイルに対してタグ付け等の操作を行うためのオプションのフック
    pub modify: Option<
        fn(
            &ttfm::db::Store,
            &ttfm::tag::TagRegistry,
            &Path,
        ) -> anyhow::Result<()>,
    >,
    /// 宣言的タグ指定: (case_dir 相対パス, タグ列) のリスト。
    /// 全ケース分をまとめて1回の write で適用するため、modify フックでの
    /// tag_item 逐次呼び出しより大幅に速い。単純なタグ付与はこちらを使う。
    pub tags: &'static [(&'static str, &'static str)],
    /// クエリを実行前に加工する関数。デフォルトは `default_scope`。
    /// outer-agg クエリ等、特殊なスコープ付与が必要なケースで上書きする。
    pub format_query: fn(&str, &Path) -> String,
    pub query: &'static str,
    pub assert: fn(&SearchResponse, &Path) -> anyhow::Result<()>,
}

impl QueryTestCase {
    /// define_cases! の struct update 用デフォルト値。
    /// ケース定義で省略されたフィールドはここから補われる。
    pub(super) const DEFAULTS: QueryTestCase = QueryTestCase {
        name: "",
        setup: |_| Ok(()),
        modify: None,
        tags: &[],
        format_query: default_scope,
        query: "",
        assert: |_, _| Ok(()),
    };
}

pub(super) struct SharedFixture {
    pub root: tempfile::TempDir,
    pub store: std::sync::Mutex<ttfm::db::Store>,
    pub registry: ttfm::tag::TagRegistry,
}

// ──────────────────────────────────────────────
// マクロ: CASES定義 + テスト関数の自動生成
// ──────────────────────────────────────────────

macro_rules! define_cases {
    ($( $name:ident: { $($field:tt)* } ),* $(,)?) => {
        static CASES: &[crate::cases::QueryTestCase] = &[
            $(crate::cases::QueryTestCase {
                name: stringify!($name),
                $($field)*
                ..crate::cases::QueryTestCase::DEFAULTS
            }),*
        ];

        static FIXTURE: std::sync::OnceLock<crate::cases::SharedFixture>
            = std::sync::OnceLock::new();

        fn get_fixture() -> &'static crate::cases::SharedFixture {
            FIXTURE.get_or_init(|| {
                let root = tempfile::TempDir::new().expect("Failed to create temp dir");
                let db_dir = root.path().join(".ttfm_test/db");
                for case in CASES {
                    let case_dir = root.path().join(case.name);
                    std::fs::create_dir_all(&case_dir).unwrap_or_else(|e| {
                        panic!("Failed to create dir for '{}': {}", case.name, e)
                    });
                    (case.setup)(&case_dir).unwrap_or_else(|e| {
                        panic!("Setup failed for '{}': {}", case.name, e)
                    });
                }
                let store = ttfm::db::Store::open(&db_dir).expect("Store::open");
                let registry = ttfm::tag::TagRegistry::with_standard();
                ttfm::indexing::Indexer::new(&store, &registry)
                    .initialize_tables()
                    .expect("initialize_tables");
                ttfm::indexing::Indexer::new(&store, &registry)
                    .run(root.path(), None::<&fn(usize)>, false)
                    .expect("index_directory");
                // 宣言的タグ指定（tags フィールド）を全ケース分集めて 1 回の write で適用
                let tag_specs: Vec<(std::path::PathBuf, &str)> = CASES
                    .iter()
                    .flat_map(|case| {
                        let case_dir = root.path().join(case.name);
                        case.tags.iter().map(move |(rel, tags)| {
                            (case_dir.join(rel), *tags)
                        })
                    })
                    .collect();
                crate::cases::apply_tags_batch(&store, &registry, &tag_specs)
                    .unwrap_or_else(|e| panic!("apply_tags_batch failed: {}", e));
                for case in CASES {
                    if let Some(modify) = case.modify {
                        let case_dir = root.path().join(case.name);
                        modify(&store, &registry, &case_dir).unwrap_or_else(|e| {
                            panic!("Modify failed for '{}': {}", case.name, e)
                        });
                    }
                }
                crate::cases::SharedFixture {
                    root,
                    store: std::sync::Mutex::new(store),
                    registry,
                }
            })
        }

        fn run_case(name: &'static str) -> anyhow::Result<()> {
            let fix = get_fixture();
            let store = fix.store.lock().unwrap().try_clone()?;
            let case = CASES.iter().find(|c| c.name == name).unwrap();
            let case_dir = fix.root.path().join(case.name);
            let query = (case.format_query)(case.query, &case_dir);
            let res = ttfm::search::search(
                &store,
                &fix.registry,
                &query,
                ttfm::SearchOptions::default(),
            )?;
            (case.assert)(&res, &case_dir)
        }

        $(
            #[test]
            fn $name() -> anyhow::Result<()> {
                run_case(stringify!($name))
            }
        )*
    }
}

// ──────────────────────────────────────────────
// 共通ヘルパー関数
// ──────────────────────────────────────────────

pub(super) fn get_nvalue(item: &ttfm::Item) -> Option<String> {
    item.tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string())
}

pub(super) fn get_nvalue_f64(item: &ttfm::Item) -> Option<f64> {
    item.tags
        .entries
        .iter()
        .find(|e| e.label.tag_type().as_str() == "nvalue")
        .map(|e| match e.label.value() {
            ttfm::types::Bitical::Double(d) => d,
            ttfm::types::Bitical::Integer(i) => i as f64,
            _ => panic!("Unexpected nvalue type"),
        })
}

/// 結果が item: タグを持つか（グループ表示 / projection パス）を判定します。
pub(super) fn has_item_tags(results: &[ttfm::Item]) -> bool {
    results.iter().any(|r| {
        r.tags
            .entries
            .iter()
            .any(|e| e.label.tag_type() == ttfm::types::TagType::from("item"))
    })
}

/// デフォルト: `(Q) & path:<dir>/*` — 通常の nest / 比較クエリ用
pub(super) fn default_scope(query: &str, dir: &Path) -> String {
    format!("({}) & path:{}/*", query, dir.to_string_lossy())
}

/// 宣言的タグ指定（QueryTestCase::tags）の一括適用。
/// path→item_id を 1 クエリで解決し、modify で WriteAction を収集して
/// write_and_refresh を 1 回だけ呼ぶ（parquet 書き換え＋ビュー再作成が全体で1回）。
pub(super) fn apply_tags_batch(
    store: &ttfm::db::Store,
    registry: &ttfm::tag::TagRegistry,
    specs: &[(std::path::PathBuf, &str)],
) -> anyhow::Result<()> {
    use ttfm::types::{Intrinsic, ItemId, Rank, Tags};

    if specs.is_empty() {
        return Ok(());
    }

    let loc_path = store.path_for_target(ttfm::TargetTable::Locations);
    let in_list = specs
        .iter()
        .map(|(p, _)| format!("'{}'", p.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT path, item_id FROM read_parquet('{}') WHERE path IN ({})",
        loc_path.to_string_lossy(),
        in_list
    );
    let mut ids = std::collections::HashMap::new();
    let mut stmt = store.conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (path, id) = row?;
        ids.insert(path, id);
    }

    let mut actions = Vec::new();
    for (path, tags) in specs {
        let path_str = path.to_string_lossy();
        let id = *ids.get(path_str.as_ref()).ok_or_else(|| {
            anyhow::anyhow!("apply_tags_batch: not indexed: {}", path.display())
        })?;
        let item = ttfm::Item {
            id: ItemId::Stored(id),
            item_kind: ttfm::ItemKind::File,
            representative: vec![],
            rank: Rank::default(),
            intrinsic: Intrinsic::default(),
            tags: Tags::new(),
            item_count: None,
        };
        actions.extend(ttfm::edit::modify::modify(
            &item,
            Some(tags),
            ttfm::edit::QueryType::Tag,
            registry,
        )?);
    }
    ttfm::edit::write::write_and_refresh(store, registry, actions)?;
    Ok(())
}

/// クエリ内の `path:` を `path:<dir>/` に書き換えて相対サブパスを絶対パスに解決し、
/// さらに `& path:<dir>/*` で隔離する。
pub(super) fn scope_path_from_dir(query: &str, dir: &Path) -> String {
    let prefix = dir.to_string_lossy();
    let q = query.replace("path:", &format!("path:{}/", prefix));
    format!("({q}) & path:{prefix}/*")
}

/// 各Projectionなどへの個別注入を避け、可能な限り「Nest 全体」をパスフィルタで包みます。
pub(super) fn inject_path_scope(query: &str, dir: &Path) -> String {
    let p = dir.to_string_lossy();
    let filter = format!("& path:{}/*", p);

    for outer in &["sum(", "count(", "avg(", "max(", "min("] {
        for inner_agg in &["sum(", "count(", "avg(", "max(", "min("] {
            let prefix = format!("{}{}", outer, inner_agg);
            if query.starts_with(&prefix[..]) && query.ends_with("))") {
                let inner = &query[prefix.len()..query.len() - 2];
                let outer_fn = &outer[..outer.len() - 1];
                let inner_fn = &inner_agg[..inner_agg.len() - 1];
                return format!(
                    "{}({}(({}) {}))",
                    outer_fn, inner_fn, inner, filter
                );
            }
        }
    }

    for agg in &["sum(", "count(", "avg(", "max(", "min("] {
        if query.starts_with(agg) {
            let bytes = query.as_bytes();
            let mut depth = 1i32;
            let mut i = agg.len();
            while i < query.len() && depth > 0 {
                match bytes[i] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                if depth > 0 {
                    i += 1;
                }
            }
            if depth == 0 && i == query.len() - 1 {
                let inner = &query[agg.len()..i];
                return format!(
                    "{}(({}) {})",
                    &agg[..agg.len() - 1],
                    inner,
                    filter
                );
            }
        }
    }

    if query.starts_with('(') && query.ends_with(')') && query.contains(") + (")
    {
        if let Some(mid) = query.find(") + (") {
            let left = &query[1..mid];
            let right = &query[mid + 5..query.len() - 1];
            return format!(
                "(({}) {}) + (({}) {})",
                left, filter, right, filter
            );
        }
    }

    // 汎用: クエリ内の全アグリゲーション呼び出しにスコープを注入する。
    const AGGS: &[&str] = &["sum(", "count(", "avg(", "max(", "min("];
    if AGGS.iter().any(|&a| query.contains(a)) {
        let bytes = query.as_bytes();
        let n = bytes.len();
        let mut out = String::with_capacity(n + 64);
        let mut i = 0;
        while i < n {
            let rest = &query[i..];
            let mut matched = false;
            for &agg in AGGS {
                if rest.starts_with(agg) {
                    out.push_str(agg);
                    i += agg.len();
                    let inner_start = i;
                    let mut depth = 1i32;
                    while i < n && depth > 0 {
                        match bytes[i] {
                            b'(' => depth += 1,
                            b')' => depth -= 1,
                            _ => {}
                        }
                        if depth > 0 {
                            i += 1;
                        }
                    }
                    let inner = &query[inner_start..i];
                    if inner.is_empty() {
                        out.push_str("*:*");
                    } else {
                        out.push('(');
                        out.push_str(inner);
                        out.push(')');
                    }
                    out.push(' ');
                    out.push_str(&filter);
                    out.push(')');
                    i += 1;
                    matched = true;
                    break;
                }
            }
            if !matched {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
        return out;
    }

    format!("({}) {}", query, filter)
}

// ──────────────────────────────────────────────
// サブモジュール
// ──────────────────────────────────────────────

pub mod cache_paged_test;
pub mod identifier_test;
pub mod indexing_integration;
pub mod integration_tags;
pub mod rank_test;
pub mod robustness_test;
pub mod sorting_verification;
pub mod test_aggregation;
pub mod test_aggregation_calc;
pub mod test_boolean_ops;
pub mod test_calculation;
mod test_chain_comparison;
pub mod test_computation_fetching;
pub mod test_date_regression;
pub mod test_discrepancy;
pub mod test_edit;
pub mod test_errors;
pub mod test_item_refactoring;
pub mod test_label_calc;
pub mod test_literal_ops;
pub mod test_nest;
pub mod test_null_propagation;
pub mod test_optimize_sql;
pub mod test_projection;
pub mod test_query_full;
pub mod test_reverse_patterns;
pub mod test_scalar_format;
pub mod test_search_order;
pub mod test_search_progress;
pub mod test_size_units;
pub mod test_strict_grammar;
pub mod test_type_definitions;
pub mod test_validation;
pub mod test_validation_toplevel;
pub mod test_volatile_typed_tags;
pub mod verify_search;
pub mod verify_search_all;
pub mod wasm_plugin_test;
pub mod write_engine;
