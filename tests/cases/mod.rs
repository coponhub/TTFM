use std::path::Path;
use ttfm::FileManager;
use ttfm::response::SearchResponse;

// ──────────────────────────────────────────────
// 共通テストケース構造体
// ──────────────────────────────────────────────

pub(super) struct QueryTestCase {
    pub name: &'static str,
    pub setup: fn(&Path) -> anyhow::Result<()>,
    /// DBのインデックス完了後に、ファイルに対してタグ付け等の操作を行うためのオプションのフック
    pub modify: Option<fn(&FileManager, &Path) -> anyhow::Result<()>>,
    /// クエリを実行前に加工する関数。デフォルトは `default_scope`。
    /// outer-agg クエリ等、特殊なスコープ付与が必要なケースで上書きする。
    pub format_query: fn(&str, &Path) -> String,
    pub query: &'static str,
    pub assert: fn(&SearchResponse, &Path) -> anyhow::Result<()>,
}

pub(super) struct SharedFixture {
    pub root: tempfile::TempDir,
    pub db_dir: std::path::PathBuf,
}

// ──────────────────────────────────────────────
// マクロ: CASES定義 + テスト関数の自動生成
// ──────────────────────────────────────────────

macro_rules! define_cases {
    ($( $name:ident: { $($field:tt)* } ),* $(,)?) => {
        static CASES: &[crate::cases::QueryTestCase] = &[
            $(crate::cases::QueryTestCase { name: stringify!($name), $($field)* }),*
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
                {
                    let fm = ttfm::FileManager::new_with_db_dir(&db_dir).expect("FM create");
                    fm.index_directory(root.path(), None::<&fn(usize)>, false)
                        .expect("index_directory");
                    for case in CASES {
                        if let Some(modify) = case.modify {
                            let case_dir = root.path().join(case.name);
                            modify(&fm, &case_dir).unwrap_or_else(|e| {
                                panic!("Modify failed for '{}': {}", case.name, e)
                            });
                        }
                    }
                }
                crate::cases::SharedFixture { root, db_dir }
            })
        }

        fn run_case(name: &'static str) -> anyhow::Result<()> {
            let fix = get_fixture();
            let fm = ttfm::FileManager::new_with_db_dir(&fix.db_dir)?;
            let case = CASES.iter().find(|c| c.name == name).unwrap();
            let case_dir = fix.root.path().join(case.name);
            let query = (case.format_query)(case.query, &case_dir);
            let res = fm.search(&query, ttfm::SearchOptions::default())?;
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

pub(super) fn get_nvalue(item: &ttfm::SearchResult) -> Option<String> {
    item.tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string())
}

pub(super) fn get_nvalue_f64(item: &ttfm::SearchResult) -> Option<f64> {
    item.tags
        .entries
        .iter()
        .find(|e| e.label.tag_type().as_str() == "nvalue")
        .map(|e| match e.label.value() {
            ttfm::types::LabelValue::Double(d_bits) => f64::from_bits(d_bits),
            ttfm::types::LabelValue::Integer(i) => i as f64,
            _ => panic!("Unexpected nvalue type"),
        })
}

/// デフォルト: `(Q) & path:<dir>/*` — 通常の nest / 比較クエリ用
pub(super) fn default_scope(query: &str, dir: &Path) -> String {
    format!("({}) & path:{}/*", query, dir.to_string_lossy())
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
                return format!("{}({}(({}) {}))", outer_fn, inner_fn, inner, filter);
            }
        }
    }

    for agg in &["sum(", "count(", "avg(", "max(", "min("] {
        if query.starts_with(agg) && query.ends_with(')') {
            let inner = &query[agg.len()..query.len() - 1];
            return format!("{}(({}) {})", &agg[..agg.len() - 1], inner, filter);
        }
    }

    if query.contains(" - count(") {
        if let Some(pos) = query.find("count(") {
            let prefix = &query[..pos + 6];
            if let Some(end) = query.rfind(')') {
                let inner = &query[pos + 6..end];
                let suffix = &query[end..];
                return format!("{}(({}) {}){}", prefix, inner, filter, suffix);
            }
        }
    }

    if query.contains("sum(") && query.contains(" * ") {
        if let Some(pos) = query.find("sum(") {
            let prefix = &query[..pos + 4];
            if let Some(end) = query.rfind(')') {
                let inner = &query[pos + 4..end];
                let suffix = &query[end..];
                return format!("{}(({}) {}){}", prefix, inner, filter, suffix);
            }
        }
    }

    if query.starts_with('(') && query.ends_with(')') && query.contains(") + (") {
        if let Some(mid) = query.find(") + (") {
            let left = &query[1..mid];
            let right = &query[mid + 5..query.len() - 1];
            return format!("(({}) {}) + (({}) {})", left, filter, right, filter);
        }
    }

    format!("({}) {}", query, filter)
}

// ──────────────────────────────────────────────
// サブモジュール
// ──────────────────────────────────────────────

pub mod cache_paged_test;
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
pub mod test_search_progress;
pub mod test_size_units;
pub mod test_strict_grammar;
pub mod test_validation;
pub mod test_validation_toplevel;
pub mod test_volatile_typed_tags;
pub mod verify_search;
pub mod verify_search_all;
pub mod wasm_plugin_test;
