/// ネスト演算子 (`&:`) の統合テスト
use std::path::Path;
use std::sync::OnceLock;
use tempfile::tempdir;
use tempfile::TempDir;
use ttfm::response::SearchResponse;
use ttfm::FileManager;
use ttfm::SearchOptions;

// ──────────────────────────────────────────────
// 共有フィクスチャ基盤
// ──────────────────────────────────────────────

struct NestTestCase {
    name: &'static str,
    setup: fn(&Path) -> anyhow::Result<()>,
    /// DBのインデックス完了後に、ファイルに対してタグ付け等の操作を行うためのオプションのフック
    modify: Option<fn(&FileManager, &Path) -> anyhow::Result<()>>,
    /// クエリを実行前に加工する関数。デフォルトは `default_scope`。
    /// outer-agg クエリ等、特殊なスコープ付与が必要なケースで上書きする。
    format_query: fn(&str, &Path) -> String,
    query: &'static str,
    assert: fn(&SearchResponse, &Path) -> anyhow::Result<()>,
}

struct SharedFixture {
    root: TempDir,
    db_dir: std::path::PathBuf,
}

static FIXTURE: OnceLock<SharedFixture> = OnceLock::new();

fn get_fixture() -> &'static SharedFixture {
    FIXTURE.get_or_init(|| {
        let root = TempDir::new().expect("Failed to create temp dir");
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
            let fm = FileManager::new_with_db_dir(&db_dir).expect("FM create");
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
        SharedFixture { root, db_dir }
    })
}

// ──────────────────────────────────────────────
// スコープ付与ヘルパー関数
// ──────────────────────────────────────────────

/// デフォルト: `(Q) & path:<dir>/*` — 通常の nest / 比較クエリ用
fn default_scope(query: &str, dir: &Path) -> String {
    format!("({}) & path:{}/*", query, dir.to_string_lossy())
}

/// クエリ内の `path:` を `path:<dir>/` に書き換えて相対サブパスを絶対パスに解決し、
/// さらに `& path:<dir>/*` で隔離する。
///
/// 例: `"extension: | path:sub/*"` + dir=/tmp/abc
///   → `"(extension: | path:/tmp/abc/sub/*) & path:/tmp/abc/*"`
fn scope_path_from_dir(query: &str, dir: &Path) -> String {
    let prefix = dir.to_string_lossy();
    let q = query.replace("path:", &format!("path:{}/", prefix));
    format!("({q}) & path:{prefix}/*")
}

/// 各Projectionなどへの個別注入を避け、可能な限り「Nest 全体」をパスフィルタで包みます。
/// これにより、Nestの論理構造を保護しつつ、実行範囲を対象ディレクトリに制限します。
fn inject_path_scope(query: &str, dir: &Path) -> String {
    let p = dir.to_string_lossy();
    let filter = format!("& path:{}/*", p);

    // 2段階集約: outer_agg(inner_agg(INNER)) → outer_agg(inner_agg((INNER) & path:...))
    for outer in &["sum(", "count(", "avg(", "max(", "min("] {
        for inner_agg in &["sum(", "count(", "avg(", "max(", "min("] {
            let prefix = format!("{}{}", outer, inner_agg);
            if query.starts_with(&prefix[..]) && query.ends_with("))") {
                let inner = &query[prefix.len()..query.len() - 2];
                let outer_fn = &outer[..outer.len() - 1];
                let inner_fn = &inner_agg[..inner_agg.len() - 1];
                let res = format!("{}({}(({}) {}))", outer_fn, inner_fn, inner, filter);
                println!(
                    "DEBUG: [inject_path_scope] original='{}' -> transformed='{}'",
                    query, res
                );
                return res;
            }
        }
    }

    // 集計関数: agg(INNER)
    for agg in &["sum(", "count(", "avg(", "max(", "min("] {
        if query.starts_with(agg) && query.ends_with(')') {
            let inner = &query[agg.len()..query.len() - 1];
            let res =
                format!("{}(({}) {})", &agg[..agg.len() - 1], inner, filter);
            println!(
                "DEBUG: [inject_path_scope] original='{}' -> transformed='{}'",
                query, res
            );
            return res;
        }
    }

    // 算術演算との組み合わせ: 100 - count(INNER)
    if query.contains(" - count(") {
        if let Some(pos) = query.find("count(") {
            let prefix = &query[..pos + 6];
            if let Some(end) = query.rfind(')') {
                let inner = &query[pos + 6..end];
                let suffix = &query[end..];
                let res =
                    format!("{}(({}) {}){}", prefix, inner, filter, suffix);
                println!("DEBUG: [inject_path_scope] original='{}' -> transformed='{}'", query, res);
                return res;
            }
        }
    }

    // 算術演算との組み合わせ: sum(INNER) * 2
    if query.contains("sum(") && query.contains(" * ") {
        if let Some(pos) = query.find("sum(") {
            let prefix = &query[..pos + 4];
            if let Some(end) = query.rfind(')') {
                let inner = &query[pos + 4..end];
                let suffix = &query[end..];
                let res =
                    format!("{}(({}) {}){}", prefix, inner, filter, suffix);
                println!("DEBUG: [inject_path_scope] original='{}' -> transformed='{}'", query, res);
                return res;
            }
        }
    }

    // 算術演算: (A) + (B)  ※ query[0]='(' かつ query 末尾=')' 前提
    if query.starts_with('(') && query.ends_with(')') && query.contains(") + (")
    {
        if let Some(mid) = query.find(") + (") {
            // query[..mid+1] = "(A)" → left = A (先頭の '(' と mid の ')' を除外)
            let left = &query[1..mid];
            // query[mid+4..] = "(B)" → right = B (先頭の '(' と末尾の ')' を除外)
            let right = &query[mid + 5..query.len() - 1];
            let res =
                format!("(({}) {}) + (({}) {})", left, filter, right, filter);
            println!(
                "DEBUG: [inject_path_scope] original='{}' -> transformed='{}'",
                query, res
            );
            return res;
        }
    }

    // その他 (通常の Nest 等): 全体を包む
    let res = format!("({}) {}", query, filter);
    println!(
        "DEBUG: [inject_path_scope] original='{}' -> transformed='{}'",
        query, res
    );
    res
}

// ──────────────────────────────────────────────
// 共通ヘルパー
// ──────────────────────────────────────────────

fn get_nvalue(item: &ttfm::SearchResult) -> Option<String> {
    item.tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string())
}

fn get_nvalue_f64(item: &ttfm::SearchResult) -> Option<f64> {
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

// ──────────────────────────────────────────────
// マクロ: CASES定義 + テスト関数の自動生成
// ──────────────────────────────────────────────

macro_rules! define_cases {
    ($( $name:ident: { $($field:tt)* } ),* $(,)?) => {
        static CASES: &[NestTestCase] = &[
            $(NestTestCase { name: stringify!($name), $($field)* }),*
        ];

        $(
            #[test]
            fn $name() -> anyhow::Result<()> {
                run_case(stringify!($name))
            }
        )*
    }
}

fn run_case(name: &'static str) -> anyhow::Result<()> {
    let fix = get_fixture();
    let fm = FileManager::new_with_db_dir(&fix.db_dir)?;
    let case = CASES.iter().find(|c| c.name == name).unwrap();
    let case_dir = fix.root.path().join(case.name);
    let query = (case.format_query)(case.query, &case_dir);
    let res = fm.search(&query, SearchOptions::default())?;
    (case.assert)(&res, &case_dir)
}

// ──────────────────────────────────────────────
// 全E2Eテストケースの定義
// ──────────────────────────────────────────────

define_cases! {
    // ── 基本 nest クエリ（default_scope） ─────────────────────
    count_e2e: {
        setup: |dir| {
            let src = dir.join("src");
            let docs = dir.join("docs");
            std::fs::create_dir_all(&src)?;
            std::fs::create_dir_all(&docs)?;
            std::fs::write(src.join("photo1.jpg"), "jpg1")?;
            std::fs::write(src.join("photo2.jpg"), "jpg2")?;
            std::fs::write(src.join("image.png"), "png1")?;
            std::fs::write(docs.join("scan.jpg"), "jpg3")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: count(extension:jpg)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some(), "Should be projection");
            let src = res.results.iter().find(|r| r.name.contains("src")).expect("src");
            let docs = res.results.iter().find(|r| r.name.contains("docs")).expect("docs");
            assert_eq!(get_nvalue(src).as_deref(), Some("2"), "src: 2 jpg");
            assert_eq!(get_nvalue(docs).as_deref(), Some("1"), "docs: 1 jpg");
            Ok(())
        },
    },
    sum_e2e: {
        setup: |dir| {
            let sub = dir.join("sub");
            std::fs::create_dir_all(&sub)?;
            std::fs::write(sub.join("a.txt"), vec![0u8; 100])?;
            std::fs::write(sub.join("b.txt"), vec![0u8; 200])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: sum(size:)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let sub = res.results.iter().find(|r| r.name.contains("sub")).expect("sub");
            assert_eq!(get_nvalue(sub).as_deref(), Some("300"), "sub sum=300");
            Ok(())
        },
    },
    no_regression_plain_projection: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "")?;
            std::fs::write(dir.join("b.txt"), "")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "extension:",
        assert: |res, _dir| {
            assert_eq!(res.type_for_projection, Some(ttfm::types::TagType::from("extension")));
            assert!(res.results.iter().any(|r| r.name == "rs"));
            assert!(res.results.iter().any(|r| r.name == "txt"));
            for item in &res.results {
                let has_nvalue = item.tags.entries.iter()
                    .any(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"));
                assert!(!has_nvalue, "Plain projection should NOT have nvalue for '{}'", item.name);
            }
            Ok(())
        },
    },
    extension_left_count: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "a")?;
            std::fs::write(dir.join("b.rs"), "bb")?;
            std::fs::write(dir.join("c.txt"), "ccc")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "extension: &: count(*:*)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let rs = res.results.iter().find(|r| r.name == "rs").expect("rs");
            let txt = res.results.iter().find(|r| r.name == "txt").expect("txt");
            assert_eq!(get_nvalue(rs).as_deref(), Some("2"), "rs count=2");
            assert_eq!(get_nvalue(txt).as_deref(), Some("1"), "txt count=1");
            Ok(())
        },
    },
    extension_left_sum_size: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), vec![0u8; 100])?;
            std::fs::write(dir.join("b.rs"), vec![0u8; 200])?;
            std::fs::write(dir.join("c.txt"), vec![0u8; 50])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "extension: &: sum(size:)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let rs = res.results.iter().find(|r| r.name == "rs").expect("rs");
            let txt = res.results.iter().find(|r| r.name == "txt").expect("txt");
            assert_eq!(get_nvalue(rs).as_deref(), Some("300"), "rs sum=300");
            assert_eq!(get_nvalue(txt).as_deref(), Some("50"), "txt sum=50");
            Ok(())
        },
    },
    max_size: {
        setup: |dir| {
            let sub = dir.join("sub");
            std::fs::create_dir_all(&sub)?;
            std::fs::write(sub.join("small.txt"), vec![0u8; 10])?;
            std::fs::write(sub.join("large.txt"), vec![0u8; 500])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: max(size:)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let sub = res.results.iter().find(|r| r.name.contains("sub")).expect("sub");
            assert_eq!(get_nvalue(sub).as_deref(), Some("500"), "max=500");
            Ok(())
        },
    },
    min_size: {
        setup: |dir| {
            let sub = dir.join("sub");
            std::fs::create_dir_all(&sub)?;
            std::fs::write(sub.join("small.txt"), vec![0u8; 10])?;
            std::fs::write(sub.join("large.txt"), vec![0u8; 500])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: min(size:)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let sub = res.results.iter().find(|r| r.name.contains("sub")).expect("sub");
            assert_eq!(get_nvalue(sub).as_deref(), Some("10"), "min=10");
            Ok(())
        },
    },
    avg_size: {
        setup: |dir| {
            let sub = dir.join("sub");
            std::fs::create_dir_all(&sub)?;
            std::fs::write(sub.join("a.txt"), vec![0u8; 100])?;
            std::fs::write(sub.join("b.txt"), vec![0u8; 200])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: avg(size:)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let sub = res.results.iter().find(|r| r.name.contains("sub")).expect("sub");
            let nv: f64 = get_nvalue(sub).expect("nvalue").parse().expect("numeric");
            assert!((nv - 150.0).abs() < 1.0, "avg~150, got {}", nv);
            Ok(())
        },
    },
    count_all: {
        setup: |dir| {
            let alpha = dir.join("alpha");
            let beta = dir.join("beta");
            std::fs::create_dir_all(&alpha)?;
            std::fs::create_dir_all(&beta)?;
            std::fs::write(alpha.join("x.txt"), "x")?;
            std::fs::write(alpha.join("y.txt"), "y")?;
            std::fs::write(alpha.join("z.txt"), "z")?;
            std::fs::write(beta.join("w.txt"), "w")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: count(*:*)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let alpha = res.results.iter().find(|r| r.name.contains("alpha")).expect("alpha");
            let beta = res.results.iter().find(|r| r.name.contains("beta")).expect("beta");
            assert_eq!(get_nvalue(alpha).as_deref(), Some("3"), "alpha=3");
            assert_eq!(get_nvalue(beta).as_deref(), Some("1"), "beta=1");
            Ok(())
        },
    },
    filename_left: {
        setup: |dir| {
            std::fs::write(dir.join("hello.txt"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "filename: &: sum(size:)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let hello = res.results.iter().find(|r| r.name == "hello.txt").expect("hello.txt");
            assert_eq!(get_nvalue(hello).as_deref(), Some("100"), "sum=100");
            Ok(())
        },
    },
    comparison_count_gt: {
        setup: |dir| {
            let src = dir.join("src");
            let docs = dir.join("docs");
            std::fs::create_dir_all(&src)?;
            std::fs::create_dir_all(&docs)?;
            std::fs::write(src.join("a.jpg"), "a")?;
            std::fs::write(src.join("b.jpg"), "b")?;
            std::fs::write(src.join("c.jpg"), "c")?;
            std::fs::write(docs.join("d.jpg"), "d")?;
            std::fs::write(docs.join("e.txt"), "e")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (count(extension:jpg) > 1)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|&n| n == "a.jpg" || n == "b.jpg" || n == "c.jpg"),
                "src items should appear: {:?}", names);
            assert!(!names.iter().any(|&n| n == "d.jpg"),
                "docs should be excluded: {:?}", names);
            Ok(())
        },
    },
    comparison_sum_gt: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), vec![0u8; 50])?;
            std::fs::write(dir.join("b.rs"), vec![0u8; 60])?;
            std::fs::write(dir.join("c.txt"), vec![0u8; 30])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "extension: &: (sum(size:) > 100)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|&n| n == "a.rs" || n == "b.rs"),
                "rs (sum=110) should appear: {:?}", names);
            assert!(!names.iter().any(|&n| n == "c.txt"),
                "txt (sum=30) excluded: {:?}", names);
            Ok(())
        },
    },
    context_propagation: {
        setup: |dir| {
            std::fs::write(dir.join("a.html"), vec![0u8; 100])?;
            std::fs::write(dir.join("b.html"), vec![0u8; 200])?;
            std::fs::write(dir.join("c.txt"), vec![0u8; 50])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "stem:a & extension: &: sum(size:)",
        assert: |res, _dir| {
            let html = res.results.iter().find(|r| r.name == "html").expect("html");
            assert_eq!(get_nvalue(html).as_deref(), Some("100"), "html=100");
            assert!(res.results.iter().find(|r| r.name == "txt").is_none(), "txt filtered");
            Ok(())
        },
    },
    pick_filter: {
        setup: |dir| {
            let dir_a = dir.join("dirA");
            let dir_b = dir.join("dirB");
            std::fs::create_dir(&dir_a)?;
            std::fs::create_dir(&dir_b)?;
            std::fs::write(dir_a.join("f1.jpg"), "1")?;
            std::fs::write(dir_a.join("f2.jpg"), "2")?;
            for i in 0..11 {
                std::fs::write(dir_b.join(format!("g{}.jpg", i)), "x")?;
            }
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (count(extension:jpg) > 10)",
        assert: |res, _dir| {
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(!names.iter().any(|&n| n == "f1.jpg" || n == "f2.jpg"),
                "dirA excluded: {:?}", names);
            assert!(names.iter().any(|n| n.starts_with('g')),
                "dirB included: {:?}", names);
            Ok(())
        },
    },
    scenario_a: {
        setup: |dir| {
            let dira = dir.join("dirA");
            let dirb = dir.join("dirB");
            std::fs::create_dir(&dira)?;
            std::fs::create_dir(&dirb)?;
            std::fs::write(dira.join("f1.html"), "html")?;
            std::fs::write(dira.join("f2.jpg"), "jpg")?;
            std::fs::write(dirb.join("f3.html"), "html")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "extension:html & parentdir: &: count(extension:html) > 0",
        assert: |res, _dir| {
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|&n| n == "f1.html" || n == "f3.html"),
                "html files should appear: {:?}", names);
            Ok(())
        },
    },
    scenario_b: {
        setup: |dir| {
            let dira = dir.join("dirA");
            let dirb = dir.join("dirB");
            std::fs::create_dir(&dira)?;
            std::fs::create_dir(&dirb)?;
            std::fs::write(dira.join("f1.txt"), vec![0u8; 100])?;
            std::fs::write(dirb.join("f2.txt"), vec![0u8; 100])?;
            std::fs::write(dirb.join("f3.txt"), vec![0u8; 200])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (avg(size:) == sum(size:))",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            let parentdirs: Vec<String> = res.results.iter()
                .flat_map(|r| r.tags.entries.iter()
                    .filter(|e| e.label.tag_type().as_str() == "parentdir")
                    .map(|e| e.label.as_str().to_string()))
                .collect();
            assert!(parentdirs.iter().any(|p| p.contains("dirA")),
                "dirA should appear: {:?}", parentdirs);
            assert!(!parentdirs.iter().any(|p| p.contains("dirB")),
                "dirB excluded: {:?}", parentdirs);
            Ok(())
        },
    },
    scenario_stem_wildcard: {
        setup: |dir| {
            let dira = dir.join("dirA");
            let dirb = dir.join("dirB");
            std::fs::create_dir(&dira)?;
            std::fs::create_dir(&dirb)?;
            std::fs::write(dira.join("apple.html"), "h")?;
            std::fs::write(dira.join("banana.html"), "h")?;
            std::fs::write(dira.join("cherry.jpg"), "j")?;
            std::fs::write(dirb.join("apple.html"), "h")?;
            std::fs::write(dirb.join("grape.txt"), "t")?;
            std::fs::write(dirb.join("berry.html"), "h")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "extension:html & parentdir: &: count(stem:*a*) == 2",
        assert: |res, _dir| {
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|&n| n == "apple.html" || n == "banana.html"),
                "dirA html expected: {:?}", names);
            assert_eq!(names.len(), 2, "Only 2 items: {:?}", names);
            Ok(())
        },
    },
    chained_comparison: {
        setup: |dir| {
            let dira = dir.join("dirA");
            let dirb = dir.join("dirB");
            let dirc = dir.join("dirC");
            std::fs::create_dir_all(&dira)?;
            std::fs::create_dir_all(&dirb)?;
            std::fs::create_dir_all(&dirc)?;
            std::fs::write(dira.join("a.txt"), vec![0u8; 60])?;
            std::fs::write(dira.join("b.txt"), vec![0u8; 40])?;
            std::fs::write(dirb.join("c.txt"), vec![0u8; 150])?;
            std::fs::write(dirb.join("d.txt"), vec![0u8; 200])?;
            std::fs::write(dirc.join("e.txt"), vec![0u8; 10])?;
            std::fs::write(dirc.join("f.txt"), vec![0u8; 20])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (200 > sum(size:) > 50)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|&n| n == "a.txt" || n == "b.txt"), "dirA included: {:?}", names);
            assert!(!names.iter().any(|&n| n == "c.txt" || n == "d.txt"), "dirB excluded: {:?}", names);
            assert!(!names.iter().any(|&n| n == "e.txt" || n == "f.txt"), "dirC excluded: {:?}", names);
            Ok(())
        },
    },
    arithmetic_mul: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("file1"), vec![0u8; 10])?;
            std::fs::write(dir1.join("file2"), vec![0u8; 20])?;
            std::fs::write(dir2.join("file3"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (sum(size:) * count(size:))",
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("60"), "dir1: 30*2=60");
            let d2 = res.results.iter().find(|r| r.name.contains("dir2")).expect("dir2");
            assert_eq!(get_nvalue(d2).as_deref(), Some("100"), "dir2: 100*1=100");
            Ok(())
        },
    },
    arithmetic_add: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("file1"), vec![0u8; 10])?;
            std::fs::write(dir1.join("file2"), vec![0u8; 20])?;
            std::fs::write(dir2.join("file3"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (sum(size:) + count(size:))",
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("32"), "dir1: 30+2=32");
            Ok(())
        },
    },
    arithmetic_sub: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("file1"), vec![0u8; 10])?;
            std::fs::write(dir1.join("file2"), vec![0u8; 20])?;
            std::fs::write(dir2.join("file3"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (sum(size:) - count(size:))",
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("28"), "dir1: 30-2=28");
            Ok(())
        },
    },
    arithmetic_div: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("file1"), vec![0u8; 10])?;
            std::fs::write(dir1.join("file2"), vec![0u8; 20])?;
            std::fs::write(dir2.join("file3"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (sum(size:) / count(size:))",
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("15"), "dir1: 30/2=15");
            Ok(())
        },
    },
    arithmetic_avg_sum: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("file1"), vec![0u8; 10])?;
            std::fs::write(dir1.join("file2"), vec![0u8; 20])?;
            std::fs::write(dir2.join("file3"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (avg(size:) + sum(size:))",
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("45"), "dir1: avg(15)+sum(30)=45");
            Ok(())
        },
    },
    arithmetic_max_lit: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("file1"), vec![0u8; 10])?;
            std::fs::write(dir1.join("file2"), vec![0u8; 20])?;
            std::fs::write(dir2.join("file3"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (max(size:) * 2)",
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("40"), "dir1: max(20)*2=40");
            Ok(())
        },
    },
    arithmetic_lit_min: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("file1"), vec![0u8; 10])?;
            std::fs::write(dir1.join("file2"), vec![0u8; 20])?;
            std::fs::write(dir2.join("file3"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (1000 / min(size:))",
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("100"), "dir1: 1000/min(10)=100");
            Ok(())
        },
    },
    arithmetic_nested: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("file1"), vec![0u8; 10])?;
            std::fs::write(dir1.join("file2"), vec![0u8; 20])?;
            std::fs::write(dir2.join("file3"), vec![0u8; 100])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: ((sum(size:) + 10) * count(size:))",
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("80"), "dir1: (30+10)*2=80");
            Ok(())
        },
    },
    or_merged_projection: {
        setup: |dir| {
            let dira = dir.join("dirA");
            let dirb = dir.join("dirB");
            let dirc = dir.join("dirC");
            std::fs::create_dir(&dira)?;
            std::fs::create_dir(&dirb)?;
            std::fs::create_dir(&dirc)?;
            std::fs::write(dira.join("main.rs"), vec![0u8; 10])?;
            std::fs::write(dirb.join("a.txt"), vec![0u8; 20])?;
            std::fs::write(dirb.join("b.txt"), vec![0u8; 30])?;
            std::fs::write(dirc.join("c.txt"), vec![0u8; 40])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (count(extension:rs) > 0) | parentdir: &: (count(*:*) > 1)",
        assert: |res, _dir| {
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|&n| n == "main.rs"), "dirA included: {:?}", names);
            assert!(names.iter().any(|&n| n == "a.txt" || n == "b.txt"), "dirB included: {:?}", names);
            assert!(!names.iter().any(|&n| n == "c.txt"), "dirC excluded: {:?}", names);
            Ok(())
        },
    },
    arithmetic_null_propagation: {
        setup: |dir| {
            let dir_rs = dir.join("dir_rs");
            let dir_txt = dir.join("dir_txt");
            std::fs::create_dir(&dir_rs)?;
            std::fs::create_dir(&dir_txt)?;
            std::fs::write(dir_rs.join("main.rs"), vec![0u8; 10])?;
            std::fs::write(dir_txt.join("readme.txt"), vec![0u8; 50])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (sum(size:) + count(extension:rs))",
        assert: |res, _dir| {
            let dir_rs = res.results.iter().find(|r| r.name.contains("dir_rs")).expect("dir_rs");
            assert_eq!(get_nvalue(dir_rs).as_deref(), Some("11"), "10+1=11");
            let dir_txt = res.results.iter().find(|r| r.name.contains("dir_txt")).expect("dir_txt");
            assert_eq!(get_nvalue(dir_txt).as_deref(), Some("50"), "50+0=50");
            Ok(())
        },
    },
    filter_empty_groups: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir_all(&dir1)?;
            std::fs::create_dir_all(&dir2)?;
            std::fs::write(dir1.join("a.rs"), "code")?;
            std::fs::write(dir2.join("b.txt"), "text")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: count(extension:rs)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|&n| n.contains("dir1")), "dir1 included");
            assert!(!names.iter().any(|&n| n.contains("dir2")), "dir2 excluded");
            Ok(())
        },
    },
    dedup_keys: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "content")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: parentdir: &: count()",
        assert: |_res, _dir| Ok(()),
    },
    level3_projection: {
        setup: |dir| {
            let work = dir.join("work");
            std::fs::create_dir(&work)?;
            std::fs::write(work.join("a.rs"), "content")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: filename:",
        assert: |res, _dir| {
            assert!(res.results.iter().any(|r| r.name.contains("work") && r.name.contains("a.rs")),
                "work/a.rs expected: {:?}", res.results.iter().map(|r| &r.name).collect::<Vec<_>>());
            Ok(())
        },
    },
    level3_projection_with_agg: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir_all(&dir1)?;
            std::fs::create_dir_all(&dir2)?;
            std::fs::write(dir1.join("a.rs"), "content")?;    // 7
            std::fs::write(dir1.join("a.txt"), "0123456789")?; // 10
            std::fs::write(dir2.join("a.rs"), "abcde")?;       // 5
            std::fs::write(dir2.join("a.txt"), "xyz")?;         // 3
            std::fs::write(dir2.join("b.txt"), "ok")?;          // 2
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: extension: &: sum(size:)",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 4, "4 groups: {:?}", res.results);
            let find_nv = |pdir: &str, ext: &str| -> f64 {
                let g = res.results.iter()
                    .find(|r| r.name.contains(pdir) && r.name.contains(ext))
                    .unwrap_or_else(|| panic!("Should find {}/{}", pdir, ext));
                get_nvalue_f64(g).expect("nvalue")
            };
            assert!((find_nv("dir1", "rs") - 7.0).abs() < 0.001);
            assert!((find_nv("dir1", "txt") - 10.0).abs() < 0.001);
            assert!((find_nv("dir2", "rs") - 5.0).abs() < 0.001);
            assert!((find_nv("dir2", "txt") - 5.0).abs() < 0.001);
            Ok(())
        },
    },
    level3_projection_with_agg_filter: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir_all(&dir1)?;
            std::fs::create_dir_all(&dir2)?;
            std::fs::write(dir1.join("a.rs"), "content")?; // 7
            std::fs::write(dir1.join("b.txt"), "x")?;       // 1
            std::fs::write(dir2.join("a.rs"), "abcde")?;   // 5
            std::fs::write(dir2.join("c.txt"), "ok")?;      // 2
            std::fs::write(dir2.join("d.txt"), "z")?;        // 1
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: extension: &: (sum(size:) > 2)",
        assert: |res, _dir| {
            assert_eq!(res.type_for_projection, None);
            let files: Vec<_> = res.results.iter().filter(|r| {
                !r.tags.entries.iter().any(|e| {
                    e.label.tag_type().to_string() == "is_dir" && e.label.as_str() == "true"
                })
            }).collect();
            assert_eq!(files.len(), 4, "4 files: {:?}", files);
            let names: Vec<_> = files.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|&n| n.contains("a.rs")));
            assert!(names.iter().any(|&n| n.contains("c.txt")));
            assert!(names.iter().any(|&n| n.contains("d.txt")));
            assert!(!names.iter().any(|&n| n.contains("b.txt")), "b.txt filtered");
            Ok(())
        },
    },
    level4_nest: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir_all(&dir1)?;
            std::fs::create_dir_all(&dir2)?;
            std::fs::write(dir1.join("a.rs"), "content")?;
            std::fs::write(dir1.join("b.rs"), "content")?;
            std::fs::write(dir2.join("c.txt"), "content2")?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: extension: &: size:",
        assert: |res, _dir| {
            let dir1_rs = res.results.iter().filter(|r| r.name.contains("dir1") && r.name.contains("rs")).count();
            let dir2_txt = res.results.iter().filter(|r| r.name.contains("dir2") && r.name.contains("txt")).count();
            assert_eq!(dir1_rs, 1);
            assert_eq!(dir2_txt, 1);
            Ok(())
        },
    },
    level3_agg_internal_filter: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            std::fs::create_dir_all(&dir1)?;
            std::fs::write(dir1.join("small.rs"), "12345")?;           // 5
            std::fs::write(dir1.join("large.rs"), "0123456789ABCDE")?; // 15
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: extension: &: sum(size: :> 10 & size:)",
        assert: |res, _dir| {
            let dir1_rs = res.results.iter()
                .find(|r| r.name.contains("dir1") && r.name.contains("rs"))
                .expect("dir1/rs");
            let val = get_nvalue_f64(dir1_rs).expect("nvalue");
            assert_eq!(val, 15.0, "only >10 bytes included: {}", val);
            Ok(())
        },
    },
    query_vs_calc_e2e: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), vec![0u8; 10])?;
            std::fs::write(dir.join("b.rs"), vec![0u8; 20])?;
            std::fs::write(dir.join("c.rs"), vec![0u8; 30])?;
            std::fs::write(dir.join("d.txt"), vec![0u8; 5])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "extension: &: (sum(size:) > (sum(size:) / 2))",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none(), "flat list");
            assert!(!res.results.is_empty(), "has results");
            Ok(())
        },
    },
    // ── outer-agg クエリ（scope_with_sum / scope_with_count） ──
    agg_over_nvalue_sum_count: {
        setup: |dir| {
            let src = dir.join("src");
            let docs = dir.join("docs");
            std::fs::create_dir_all(&src)?;
            std::fs::create_dir_all(&docs)?;
            std::fs::write(src.join("a.jpg"), "a")?;
            std::fs::write(src.join("b.jpg"), "b")?;
            std::fs::write(docs.join("c.jpg"), "c")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(parentdir: &: count(extension:jpg))",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            assert_eq!(res.results.len(), 1);
            let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
            assert_eq!(val, 3.0, "scalar 3.0, got {}", val);
            Ok(())
        },
    },
    agg_over_nvalue_count: {
        setup: |dir| {
            let src = dir.join("src");
            let docs = dir.join("docs");
            std::fs::create_dir_all(&src)?;
            std::fs::create_dir_all(&docs)?;
            std::fs::write(src.join("a.jpg"), "a")?;
            std::fs::write(src.join("b.jpg"), "b")?;
            std::fs::write(docs.join("c.jpg"), "c")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count(parentdir: &: count(extension:jpg))",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            assert_eq!(res.results.len(), 1);
            let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
            assert_eq!(val, 2.0, "scalar 2.0, got {}", val);
            Ok(())
        },
    },
    agg_over_nvalue_with_comparison: {
        setup: |dir| {
            let src = dir.join("src");
            let docs = dir.join("docs");
            std::fs::create_dir_all(&src)?;
            std::fs::create_dir_all(&docs)?;
            std::fs::write(src.join("a.jpg"), "a")?;
            std::fs::write(src.join("b.jpg"), "b")?;
            std::fs::write(src.join("c.jpg"), "c")?;
            std::fs::write(docs.join("d.jpg"), "d")?;
            std::fs::write(docs.join("e.txt"), "e")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count(parentdir: &: (count(extension:jpg) > 1))",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            assert_eq!(res.results.len(), 1);
            let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
            assert_eq!(val, 1.0, "scalar 1.0, got {}", val);
            Ok(())
        },
    },
    agg_over_nvalue_sum_with_comparison: {
        setup: |dir| {
            let src = dir.join("src");
            let docs = dir.join("docs");
            std::fs::create_dir_all(&src)?;
            std::fs::create_dir_all(&docs)?;
            std::fs::write(src.join("a.jpg"), "a")?;
            std::fs::write(src.join("b.jpg"), "b")?;
            std::fs::write(src.join("c.jpg"), "c")?;
            std::fs::write(docs.join("d.jpg"), "d")?;
            std::fs::write(docs.join("e.txt"), "e")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(parentdir: &: (count(extension:jpg) > 1))",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            assert_eq!(res.results.len(), 1);
            let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
            assert_eq!(val, 3.0, "scalar 3.0, got {}", val);
            Ok(())
        },
    },
    agg_calc_wrap: {
        setup: |dir| {
            let src = dir.join("src");
            let docs = dir.join("docs");
            std::fs::create_dir_all(&src)?;
            std::fs::create_dir_all(&docs)?;
            std::fs::write(src.join("a.jpg"), "a")?;
            std::fs::write(src.join("b.jpg"), "b")?;
            std::fs::write(src.join("c.jpg"), "c")?;
            std::fs::write(docs.join("d.jpg"), "d")?;
            std::fs::write(docs.join("e.txt"), "e")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "100 - count(parentdir: &: (count(extension:jpg) > 1))",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            assert_eq!(res.results.len(), 1);
            let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
            assert_eq!(val, 99.0, "99.0, got {}", val);
            Ok(())
        },
    },
    agg_sum_calc_wrap: {
        setup: |dir| {
            let src = dir.join("src");
            let docs = dir.join("docs");
            std::fs::create_dir_all(&src)?;
            std::fs::create_dir_all(&docs)?;
            std::fs::write(src.join("a.jpg"), "a")?;
            std::fs::write(src.join("b.jpg"), "b")?;
            std::fs::write(src.join("c.jpg"), "c")?;
            std::fs::write(docs.join("d.jpg"), "d")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(parentdir: &: (count(extension:jpg) > 1)) * 2",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            assert_eq!(res.results.len(), 1);
            let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
            assert_eq!(val, 6.0, "6.0, got {}", val);
            Ok(())
        },
    },
    // ── mixed-key（各オペランドに個別でpath注入） ──────────────
    mixed_key_calculation: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("a.rs"), "rust")?;
            std::fs::write(dir1.join("b.rs"), "rust")?;
            std::fs::write(dir2.join("c.rs"), "rust")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "(parentdir: &: count()) + (extension: &: count())",
        assert: |res, _dir| {
            let dir1_group = res.results.iter()
                .find(|r| r.name.contains("dir1") && r.name.contains("rs"))
                .expect("(dir1, rs)");
            assert_eq!(get_nvalue_f64(dir1_group), Some(5.0), "dir1 nvalue=5");
            let dir2_group = res.results.iter()
                .find(|r| r.name.contains("dir2") && r.name.contains("rs"))
                .expect("(dir2, rs)");
            assert_eq!(get_nvalue_f64(dir2_group), Some(4.0), "dir2 nvalue=4");
            Ok(())
        },
    },
    mixed_key_arithmetic_deepens_nest: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            std::fs::create_dir_all(&dir1)?;
            std::fs::write(dir1.join("a.rs"), "a")?; // 1 byte
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "(parentdir: &: sum(size:)) + (extension: &: sum(size:))",
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1, "1 merged group");
            let group = &res.results[0];
            assert!(group.name.contains("rs"), "key has rs: {}", group.name);
            assert_eq!(get_nvalue_f64(group), Some(2.0), "1+1=2");
            Ok(())
        },
    },
    // ── unnest クエリ（scope_with_sum / scope_with_count） ────────
    unnest_sum_basic: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("a.rs"), "content")?;    // 7
            std::fs::write(dir1.join("b.rs"), "0123456789")?; // 10
            std::fs::write(dir2.join("c.rs"), "abcde")?;      // 5
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(parentdir: &: size:)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some(), "projection");
            let dir1_r = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            let dir2_r = res.results.iter().find(|r| r.name.contains("dir2")).expect("dir2");
            assert_eq!(get_nvalue_f64(dir1_r), Some(17.0), "dir1 sum=17");
            assert_eq!(get_nvalue_f64(dir2_r), Some(5.0), "dir2 sum=5");
            Ok(())
        },
    },
    unnest_count_basic: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("a.rs"), "x")?;
            std::fs::write(dir1.join("b.txt"), "y")?;
            std::fs::write(dir2.join("c.rs"), "z")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count(parentdir: &: extension:)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some(), "projection");
            let dir1_r = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            let dir2_r = res.results.iter().find(|r| r.name.contains("dir2")).expect("dir2");
            assert_eq!(get_nvalue_f64(dir1_r), Some(2.0), "dir1 count=2");
            assert_eq!(get_nvalue_f64(dir2_r), Some(1.0), "dir2 count=1");
            Ok(())
        },
    },
    unnest_deep: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("a.rs"), "content")?;    // 7
            std::fs::write(dir1.join("b.txt"), "0123456789")?; // 10
            std::fs::write(dir2.join("c.rs"), "abcde")?;      // 5
            std::fs::write(dir2.join("d.txt"), "xyz")?;        // 3
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(parentdir: &: extension: &: size:)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some(), "projection");
            assert_eq!(res.results.len(), 4, "4 groups");
            let find_nv = |pdir: &str, ext: &str| -> f64 {
                let g = res.results.iter()
                    .find(|r| r.name.contains(pdir) && r.name.contains(ext))
                    .unwrap_or_else(|| panic!("{}/{}", pdir, ext));
                get_nvalue_f64(g).expect("nvalue")
            };
            assert_eq!(find_nv("dir1", "rs"), 7.0);
            assert_eq!(find_nv("dir1", "txt"), 10.0);
            assert_eq!(find_nv("dir2", "rs"), 5.0);
            assert_eq!(find_nv("dir2", "txt"), 3.0);
            Ok(())
        },
    },
    unnest_regression_plain_agg: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "content")?;    // 7
            std::fs::write(dir.join("b.rs"), "0123456789")?; // 10
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(extension:rs & size:)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none(), "scalar");
            assert_eq!(res.results[0].name, "17", "sum=17");
            Ok(())
        },
    },
    unnest_regression_nvalue_agg: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("a.rs"), "x")?;
            std::fs::write(dir1.join("b.rs"), "y")?;
            std::fs::write(dir2.join("c.rs"), "z")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(parentdir: &: count())",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none(), "scalar");
            let val: f64 = res.results[0].name.parse().expect("numeric");
            assert_eq!(val, 5.0, "sum of counts=5");
            Ok(())
        },
    },
    unnest_depth4_to_3: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("a.rs"), "content")?;    // 7
            std::fs::write(dir1.join("b.rs"), "0123456789")?; // 10
            std::fs::write(dir2.join("c.txt"), "abcde")?;     // 5
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(parentdir: &: extension: &: filename: &: size:)",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some(), "projection");
            assert_eq!(res.results.len(), 3, "3 groups");
            let find_nv = |pdir: &str, ext: &str, fname: &str| -> f64 {
                let g = res.results.iter()
                    .find(|r| r.name.contains(pdir) && r.name.contains(ext) && r.name.contains(fname))
                    .unwrap_or_else(|| panic!("{}/{}/{}", pdir, ext, fname));
                get_nvalue_f64(g).expect("nvalue")
            };
            assert_eq!(find_nv("dir1", "rs", "a.rs"), 7.0);
            assert_eq!(find_nv("dir1", "rs", "b.rs"), 10.0);
            assert_eq!(find_nv("dir2", "txt", "c.txt"), 5.0);
            Ok(())
        },
    },
    unnest_multistage_4_to_0: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("a.rs"), "content")?;    // 7
            std::fs::write(dir1.join("b.rs"), "0123456789")?; // 10
            std::fs::write(dir2.join("c.rs"), "abcde")?;      // 5
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(sum(parentdir: &: extension: &: size:))",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none(), "scalar");
            let val: f64 = res.results[0].name.parse().expect("numeric");
            assert_eq!(val, 22.0, "22.0, got {}", val);
            Ok(())
        },
    },
    unnest_multistage_3_to_0: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir(&dir1)?;
            std::fs::create_dir(&dir2)?;
            std::fs::write(dir1.join("a.rs"), "x")?;
            std::fs::write(dir1.join("b.txt"), "y")?;
            std::fs::write(dir2.join("c.rs"), "z")?;
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(count(parentdir: &: extension:))",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none(), "scalar");
            let val: f64 = res.results[0].name.parse().expect("numeric");
            assert_eq!(val, 3.0, "3.0, got {}", val);
            Ok(())
        },
    },
    unnest_multistage_with_context: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "12345")?;            // 5
            std::fs::write(dir.join("b.txt"), "123456789012345")?; // 15
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "count(extension: &: parentdir: &: (sum(size:) > 10))",
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none(), "scalar");
            let val: f64 = res.results[0].name.parse().expect("numeric");
            assert_eq!(val, 1.0, "1 valid group, got {}", val);
            Ok(())
        },
    },
    level3_arithmetic_add: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            std::fs::create_dir_all(&dir1)?;
            std::fs::write(dir1.join("a.rs"), "content")?; // size: 7

            let dir2 = dir.join("dir2");
            std::fs::create_dir_all(&dir2)?;
            std::fs::write(dir2.join("b.rs"), "0123456789")?; // size: 10
            std::fs::write(dir2.join("c.rs"), "abcde")?; // size: 5
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (size: + 1)",
        assert: |res, _dir| {
            res.results
                .iter()
                .find(|r| r.name.contains("dir1") && r.name.contains("8"))
                .expect("Should find dir1 8.0 result");

            res.results
                .iter()
                .find(|r| r.name.contains("dir2") && r.name.contains("11"))
                .expect("Should find dir2 11.0 result (b.rs)");
            res.results
                .iter()
                .find(|r| r.name.contains("dir2") && r.name.contains("6"))
                .expect("Should find dir2 6.0 result (c.rs)");
            Ok(())
        },
    },
    level3_arithmetic_mul_size: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            std::fs::create_dir_all(&dir1)?;
            std::fs::write(dir1.join("a.rs"), "content")?; // size: 7

            let dir2 = dir.join("dir2");
            std::fs::create_dir_all(&dir2)?;
            std::fs::write(dir2.join("b.rs"), "0123456789")?; // size: 10
            std::fs::write(dir2.join("c.rs"), "abcde")?; // size: 5
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "parentdir: &: (size: * 2)",
        assert: |res, _dir| {
            res.results
                .iter()
                .find(|r| r.name.contains("dir1") && r.name.contains("14"))
                .expect("Should find dir1 14.0 result");
            res.results
                .iter()
                .find(|r| r.name.contains("dir2") && r.name.contains("20"))
                .expect("Should find dir2 20.0 result");
            res.results
                .iter()
                .find(|r| r.name.contains("dir2") && r.name.contains("10"))
                .expect("Should find dir2 10.0 result");
            Ok(())
        },
    },
    level3_arithmetic_width_height: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            std::fs::create_dir_all(&dir1)?;
            std::fs::write(dir1.join("a.rs"), "content")?; // size: 7

            let dir2 = dir.join("dir2");
            std::fs::create_dir_all(&dir2)?;
            std::fs::write(dir2.join("b.rs"), "0123456789")?; // size: 10
            std::fs::write(dir2.join("c.rs"), "abcde")?; // size: 5
            Ok(())
        },
        modify: Some(|fm, dir| {
            let file_path = dir.join("dir1").join("a.rs");
            fm.tag_item(&file_path.to_string_lossy(), "width:10")?;
            fm.tag_item(&file_path.to_string_lossy(), "height:20")?;

            let file_path2 = dir.join("dir2").join("b.rs");
            fm.tag_item(&file_path2.to_string_lossy(), "width:15")?;
            fm.tag_item(&file_path2.to_string_lossy(), "height:30")?;

            let file_path3 = dir.join("dir2").join("c.rs");
            fm.tag_item(&file_path3.to_string_lossy(), "width:5")?;
            fm.tag_item(&file_path3.to_string_lossy(), "height:6")?;
            Ok(())
        }),
        format_query: default_scope,
        query: "parentdir: &: (width: * height:)",
        assert: |res, _dir| {
            res.results
                .iter()
                .find(|r| r.name.contains("dir1") && r.name.contains("200"))
                .expect("Should find dir1 200.0 result");
            res.results
                .iter()
                .find(|r| r.name.contains("dir2") && r.name.contains("450"))
                .expect("Should find dir2 450.0 result");
            res.results
                .iter()
                .find(|r| r.name.contains("dir2") && r.name.contains("30"))
                .expect("Should find dir2 30.0 result");
            Ok(())
        },
    },
    // ── Phase 0: ラベル集合演算 ───────────────────────────────────────────────
    // ケース① Proj & Proj → ラベル値積集合
    // 両タグに共通するラベル値 "one" のみが label_results に返る
    label_set_intersect_proj_proj: {
        setup: |dir| {
            // a.txt : cat:one, flavor:one → 積集合に入る
            // b.txt : cat:two            → cat のみ
            // c.txt : flavor:three       → flavor のみ
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            std::fs::write(dir.join("c.txt"), "c")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "cat:one")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "flavor:one")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "cat:two")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "flavor:three")?;
            Ok(())
        }),
        format_query: default_scope,
        query: "cat: & flavor:",
        assert: |res, _dir| {
            // 仕様: 共通プレフィックスなし → Lv.2 Projection (type_for_projection = Some)
            assert!(
                res.type_for_projection.is_some(),
                "Proj & Proj (different keys) should set type_for_projection (Lv.2 by spec)"
            );
            assert_eq!(
                res.results.len(),
                1,
                "Intersection should yield exactly 1 group ('one'), got {:?}",
                res.results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>()
            );
            assert_eq!(res.results[0].name.as_str(), "one");
            assert!(res.results[0].tags.entries.iter().any(|r| r.label.as_str().contains("a.txt")));
            let all_names: Vec<String> = res
                .results
                .iter()
                .flat_map(|r| r.tags.entries.iter().map(|e| e.label.as_str().to_string()))
                .collect();
            assert!(!all_names.iter().any(|n| n.contains("b.txt")));
            assert!(!all_names.iter().any(|n| n.contains("c.txt")));
            Ok(())
        },
    },
    // ケース② Proj | Proj → ラベル値和集合
    // 異なるタグ型のラベル値が混合した Projection が返る
    label_set_union_proj_proj: {
        setup: |dir| {
            // a.txt : tagX:alpha
            // b.txt : tagY:beta
            // c.txt : tagX:alpha, tagY:beta → 両グループに属する
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            std::fs::write(dir.join("c.txt"), "c")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagX:alpha")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagY:beta")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagX:alpha")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagY:beta")?;
            Ok(())
        }),
        format_query: default_scope,
        query: "tagX: | tagY:",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "Proj | Proj (異なるキー) は Lv.2 混合 Projection → type_for_projection = Some"
            );
            assert_eq!(
                res.results.len(),
                2,
                "Union should yield 2 groups ('alpha' and 'beta'), got {:?}",
                res.results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>()
            );
            let alpha = res.results.iter().find(|r| r.name.as_str() == "alpha")
                .expect("Should have 'alpha' group");
            let beta  = res.results.iter().find(|r| r.name.as_str() == "beta")
                .expect("Should have 'beta' group");
            assert!(alpha.tags.entries.iter().any(|e| e.label.as_str().contains("a.txt")));
            assert!(alpha.tags.entries.iter().any(|e| e.label.as_str().contains("c.txt")));
            assert!(beta.tags.entries.iter().any(|e| e.label.as_str().contains("b.txt")));
            assert!(beta.tags.entries.iter().any(|e| e.label.as_str().contains("c.txt")));
            Ok(())
        },
    },
    // ケース③ Proj -: Proj → ラベル値差集合
    // 右辺 veggie: に存在する "apple" が除かれ、"banana" と "cherry" のみ返る
    label_set_except_proj_proj: {
        setup: |dir| {
            std::fs::write(dir.join("apple.txt"), "a")?;
            std::fs::write(dir.join("banana.txt"), "b")?;
            std::fs::write(dir.join("cherry.txt"), "c")?;
            std::fs::write(dir.join("also_apple.txt"), "d")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("apple.txt").to_string_lossy(), "fruit:apple")?;
            fm.tag_item(&dir.join("banana.txt").to_string_lossy(), "fruit:banana")?;
            fm.tag_item(&dir.join("cherry.txt").to_string_lossy(), "fruit:cherry")?;
            fm.tag_item(&dir.join("also_apple.txt").to_string_lossy(), "veggie:apple")?;
            Ok(())
        }),
        format_query: default_scope,
        query: "fruit: -: veggie:",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "Proj -: Proj (左辺 Lv.2) は Lv.2 Projection → type_for_projection = Some"
            );
            assert_eq!(
                res.results.len(),
                2,
                "Except should yield 2 groups ('banana' and 'cherry'), got {:?}",
                res.results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>()
            );
            assert!(!res.results.iter().any(|g| g.name.as_str() == "apple"),
                "'apple' must be excluded");
            assert!(res.results.iter().any(|g| g.name.as_str() == "banana"));
            assert!(res.results.iter().any(|g| g.name.as_str() == "cherry"));
            Ok(())
        },
    },
    // ケース⑤ Nest 右辺への LabelSetOp 適用
    // parentdir: &: (tagA: | tagB:) → parentdir グループ内に tagA/tagB 混合サブラベル
    nest_right_side_label_set_op_union: {
        setup: |dir| {
            let dir_a = dir.join("dir_a");
            let dir_b = dir.join("dir_b");
            std::fs::create_dir_all(&dir_a)?;
            std::fs::create_dir_all(&dir_b)?;
            std::fs::write(dir_a.join("file1.txt"), "content")?;
            std::fs::write(dir_b.join("file2.txt"), "content")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("dir_a").join("file1.txt").to_string_lossy(), "tagA:x")?;
            fm.tag_item(&dir.join("dir_a").join("file1.txt").to_string_lossy(), "tagB:y")?;
            fm.tag_item(&dir.join("dir_b").join("file2.txt").to_string_lossy(), "tagA:p")?;
            Ok(())
        }),
        format_query: default_scope,
        query: "parentdir: &: (tagA: | tagB:)",
        assert: |res, dir| {
            assert!(res.type_for_projection.is_some(), "Should return Projection result");
            // 複合ラベル形式: "parentdir_path &: tag_value"
            let dir_a = dir.join("dir_a").to_string_lossy().into_owned();
            let dir_b = dir.join("dir_b").to_string_lossy().into_owned();
            let mut names: Vec<String> = res.results.iter().map(|r| r.name.clone()).collect();
            names.sort_unstable();
            let mut expected = vec![
                format!("{dir_a} &: x"),
                format!("{dir_a} &: y"),
                format!("{dir_b} &: p"),
            ];
            expected.sort_unstable();
            assert_eq!(names, expected);
            Ok(())
        },
    },
    // ケース⑥-A Unnest の And 透過（Issue #4）
    // sum(parentdir: &: size:) に inject_path_scope が適用されると
    // sum((parentdir: &: size:) & path:dir/*) になり、Unnest が機能するべき
    unnest_transparent_and_filter: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir_all(&dir1)?;
            std::fs::create_dir_all(&dir2)?;
            std::fs::write(dir1.join("a.txt"), "0123456789")?;           // 10 bytes
            std::fs::write(dir1.join("b.txt"), "01234567890123456789")?;  // 20 bytes
            std::fs::write(dir2.join("c.txt"), "01234")?;                 // 5 bytes
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "sum(parentdir: &: size:)",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "sum(Nest & path_filter) must Unnest and return Projection, not scalar"
            );
            let dir1_group = res.results.iter()
                .find(|r| r.name.contains("dir1"))
                .expect("Should have dir1 group");
            let dir2_group = res.results.iter()
                .find(|r| r.name.contains("dir2"))
                .expect("Should have dir2 group");
            assert_eq!(get_nvalue(dir1_group).as_deref(), Some("30"), "dir1 sum=30");
            assert_eq!(get_nvalue(dir2_group).as_deref(), Some("5"),  "dir2 sum=5");
            Ok(())
        },
    },
    // ケース⑧ Proj & Proj → SearchResponse.warnings に警告を生成する
    // NOTE: warnings フィールドは Phase 1 で追加。Phase 0 ではコンパイルエラー（意図的 Red）。
    label_set_intersect_warns: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "cat:one")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "flavor:one")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "cat:two")?;
            Ok(())
        }),
        format_query: default_scope,
        query: "cat: & flavor:",
        assert: |res, _dir| {
            assert!(
                !res.warnings.is_empty(),
                "Proj & Proj should generate a warning"
            );
            assert!(
                res.warnings.iter().any(|w| w.contains("&:")),
                "Warning should suggest using '&:'"
            );
            Ok(())
        },
    },
    // ケース④-A Proj | TypedTag → Lv.1 フラットリスト
    // format_query で "dir_a" を絶対パスに置換してから隔離フィルタを付ける
    proj_or_typedtag_flat: {
        setup: |dir| {
            let dir_a = dir.join("dir_a");
            let dir_b = dir.join("dir_b");
            std::fs::create_dir_all(&dir_a)?;
            std::fs::create_dir_all(&dir_b)?;
            std::fs::write(dir_a.join("a.rs"), "x")?;
            std::fs::write(dir_a.join("b.txt"), "y")?;
            std::fs::write(dir_b.join("c.rs"), "z")?;
            Ok(())
        },
        modify: None,
        format_query: scope_path_from_dir,
        query: "extension: | path:dir_a/*",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_none(),
                "Proj | TypedTag should return Lv.1 (flat list), not Lv.2 Projection"
            );
            // extension: が全3ファイルをカバーするため Union 結果は過不足なく3ファイル
            let mut filenames: Vec<_> = res.results.iter()
                .map(|r| std::path::Path::new(&r.name).file_name().unwrap_or_default().to_str().unwrap_or(""))
                .collect();
            filenames.sort_unstable();
            assert_eq!(
                filenames,
                vec!["a.rs", "b.txt", "c.rs"],
                "Proj | TypedTag flat results should contain exactly all 3 files"
            );
            Ok(())
        },
    },
    // ケース④-B Proj - TypedTag → Projection（アイテム除外後も Lv.2 維持）
    // format_query で "exclude" を絶対パスに置換してから隔離フィルタを付ける
    proj_minus_typedtag_keeps_projection: {
        setup: |dir| {
            let dir_include = dir.join("include");
            let dir_exclude = dir.join("exclude");
            std::fs::create_dir_all(&dir_include)?;
            std::fs::create_dir_all(&dir_exclude)?;
            std::fs::write(dir_include.join("a.rs"), "x")?;
            std::fs::write(dir_include.join("b.txt"), "y")?;
            std::fs::write(dir_exclude.join("c.rs"), "z")?;
            std::fs::write(dir_exclude.join("d.txt"), "w")?;
            Ok(())
        },
        modify: None,
        format_query: scope_path_from_dir,
        query: "extension: - path:exclude/*",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "Proj - TypedTag should still return Projection (Lv.2)"
            );
            // rs グループ: include/a.rs のみ（exclude/c.rs は除外済み）
            let rs_group = res.results.iter().find(|r| r.name == "rs").expect("'rs' group");
            assert!(
                rs_group.tags.entries.len() == 1
                    && rs_group.tags.entries.iter().any(|e| e.label.as_str().starts_with("a.rs")),
                "rs group must contain only a.rs, got: {:?}",
                rs_group.tags.entries.iter().map(|e| e.label.as_str()).collect::<Vec<_>>()
            );
            // txt グループ: include/b.txt のみ（exclude/d.txt は除外済み）
            let txt_group = res.results.iter().find(|r| r.name == "txt").expect("'txt' group");
            assert!(
                txt_group.tags.entries.len() == 1
                    && txt_group.tags.entries.iter().any(|e| e.label.as_str().starts_with("b.txt")),
                "txt group must contain only b.txt, got: {:?}",
                txt_group.tags.entries.iter().map(|e| e.label.as_str()).collect::<Vec<_>>()
            );
            Ok(())
        },
    },
    // ケース⑦ LabelSetOp と TypedTag フィルタの And（build_pick_sql 経由）
    // (tagA: & tagB:) & grade:A → grade:A を持つアイテムのみ積集合に含まれる
    label_set_op_filter_context: {
        setup: |dir| {
            // a.txt : tagA:one, tagB:one, grade:A → 積集合かつフィルタ通過
            // b.txt : tagA:one, tagB:one          → 積集合だが grade:A なし → 除外
            // c.txt : tagA:two, grade:A           → 積集合に入らない（tagB なし）
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            std::fs::write(dir.join("c.txt"), "c")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagA:one")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagB:one")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "grade:A")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagA:one")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagB:one")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagA:two")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "grade:A")?;
            Ok(())
        }),
        format_query: default_scope,
        query: "(tagA: & tagB:) & grade:A",
        assert: |res, _dir| {
            // grade:A で絞ると積集合のラベル "one" は a.txt のみになるべき
            assert_eq!(
                res.results.len(),
                1,
                "LabelSetOp & TypedTag should return 1 group ('one'), got {:?}",
                res.results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>()
            );
            assert_eq!(res.results[0].name.as_str(), "one");
            // a.txt が含まれる（grade:A あり）
            assert!(
                res.results[0].tags.entries.iter().any(|r| r.label.as_str().contains("a.txt")),
                "a.txt (grade:A) should be in the result"
            );
            // b.txt は除外される（grade:A なし）
            assert!(
                !res.results[0].tags.entries.iter().any(|r| r.label.as_str().contains("b.txt")),
                "b.txt (no grade:A) must not appear"
            );
            Ok(())
        },
    },
    // ケース⑥-B Unnest の And 透過（タグごとにフィルタ注入）
    // sum((parentdir: & path:filter) &: (size: & path:filter)) が Unnest される
    unnest_transparent_and_filter_per_tag: {
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir_all(&dir1)?;
            std::fs::create_dir_all(&dir2)?;
            std::fs::write(dir1.join("a.txt"), "0123456789")?;           // 10 bytes
            std::fs::write(dir1.join("b.txt"), "01234567890123456789")?;  // 20 bytes
            std::fs::write(dir2.join("c.txt"), "01234")?;                 // 5 bytes
            Ok(())
        },
        modify: None,
        format_query: |_q, dir| {
            let p = dir.to_string_lossy();
            format!("sum((parentdir: & path:{p}/*) &: (size: & path:{p}/*))")
        },
        query: "sum((parentdir: & path:dir/*) &: (size: & path:dir/*))",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "sum((parentdir: & filter) &: (size: & filter)) must return Projection"
            );
            let dir1_group = res.results.iter()
                .find(|r| r.name.contains("dir1"))
                .expect("Should have dir1 group");
            assert_eq!(
                get_nvalue(dir1_group).as_deref(),
                Some("30"),
                "dir1 size sum should be 30"
            );
            Ok(())
        },
    },
    // ── Nest × TypedTag 集合演算 ─────────────────────────────────────
    // Nest & TypedTag → Lv.3 維持（現状正しい、リグレッション確認）
    nest_and_typedtag_regression: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "cat:one")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "flavor:sweet")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "grade:A")?;
            // a.txt: grade:A あり → Nest 結果に残る
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "cat:two")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "flavor:bitter")?;
            // b.txt: grade:A なし → 除外される
            Ok(())
        }),
        format_query: default_scope,
        query: "(cat: &: flavor:) & grade:A",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "Nest & TypedTag should maintain Nest structure (Lv.3)"
            );
            // a.txt の cat:one/flavor:sweet グループが存在する
            assert!(
                res.results.iter().any(|r| r.name.contains("one") || r.name.contains("sweet")),
                "cat:one/flavor:sweet group should exist, got: {:?}",
                res.results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>()
            );
            // b.txt (grade:A なし) の cat:two/flavor:bitter グループは除外される
            assert!(
                !res.results.iter().any(|r| r.name.contains("two") || r.name.contains("bitter")),
                "cat:two/flavor:bitter group must be absent (no grade:A)"
            );
            Ok(())
        },
    },
    // Nest | TypedTag → Lv.1 フラット（Phase 5）
    nest_or_typedtag_flat: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            std::fs::write(dir.join("c.txt"), "c")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "cat:one")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "flavor:sweet")?;
            // a.txt: cat/flavor のみ
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "grade:A")?;
            // b.txt: grade:A のみ
            // c.txt: タグなし → どちらにも属さない
            Ok(())
        }),
        format_query: default_scope,
        query: "(cat: &: flavor:) | grade:A",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_none(),
                "Nest | TypedTag should flatten to Lv.1"
            );
            assert!(
                res.results.iter().any(|r| r.name.contains("a.txt")),
                "a.txt (cat/flavor) should appear in flat results"
            );
            assert!(
                res.results.iter().any(|r| r.name.contains("b.txt")),
                "b.txt (grade:A) should appear in flat results"
            );
            assert!(
                !res.results.iter().any(|r| r.name.contains("c.txt")),
                "c.txt (no tags) must NOT appear"
            );
            Ok(())
        },
    },
    // Nest - TypedTag → Lv.3 維持・grade:A アイテム除外（Phase 5）
    nest_minus_typedtag_filter: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            std::fs::write(dir.join("c.txt"), "c")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "cat:one")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "flavor:sweet")?;
            // a.txt: grade:A なし → 除外されない
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "cat:one")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "flavor:sour")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "grade:A")?;
            // b.txt: grade:A あり → 除外される
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "cat:two")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "flavor:bitter")?;
            // c.txt: grade:A なし → 除外されない
            Ok(())
        }),
        format_query: default_scope,
        query: "(cat: &: flavor:) - grade:A",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "Nest - TypedTag should maintain Nest structure (Lv.3)"
            );
            // b.txt (grade:A) の "one &: sour" グループが消える
            assert!(
                !res.results.iter().any(|r| r.name.contains("sour")),
                "flavor:sour group (b.txt, grade:A) must be excluded"
            );
            // a.txt の "one &: sweet" グループが残る
            assert!(
                res.results.iter().any(|r| r.name.contains("sweet")),
                "flavor:sweet group (a.txt, no grade:A) should remain"
            );
            // c.txt の "two &: bitter" グループが残る
            assert!(
                res.results.iter().any(|r| r.name.contains("bitter")),
                "flavor:bitter group (c.txt, no grade:A) should remain"
            );
            Ok(())
        },
    },
    // ── Nest × Projection 集合演算 ──────────────────────────────────
    // Proj & Nest → LabelSetOp Intersect（Phase 2）
    // tagA: と tagA:&:tagB: の積集合: tagB: を持つファイルのみ残る
    proj_and_nest_intersect: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagA:x")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagB:1")?;
            // a.txt: tagA: & Nest 両方に属す
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagA:y")?;
            // b.txt: tagA: のみ（tagB: なし）→ Nest に非存在 → 除外
            Ok(())
        }),
        format_query: default_scope,
        query: "tagA: & (tagA: &: tagB:)",
        assert: |res, _dir| {
            // 仕様: & 演算の結果は常に Projection → is_some()
            assert!(
                res.type_for_projection.is_some(),
                "Proj & Nest should produce LabelSetOp (with type_for_projection)"
            );
            let all_items: Vec<String> = res.results.iter().flat_map(|r| r.tags.entries.iter().map(|e| e.label.as_str().to_string()))
                .collect();
            assert!(
                !all_items.iter().any(|n| n.contains("b.txt")),
                "b.txt (tagA only, no tagB) must NOT appear in intersection"
            );
            Ok(())
        },
    },
    // Nest & Nest → LabelSetOp Intersect（Phase 2）
    // (tagA:&:tagB:) & (tagA:&:tagC:): 両 Nest に属すファイルのみ残る
    nest_and_nest_intersect: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            std::fs::write(dir.join("c.txt"), "c")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagA:x")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagB:1")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagC:red")?;
            // a.txt: 両 Nest に存在 → 積集合に残る
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagA:x")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagB:2")?;
            // b.txt: tagC なし → 第2 Nest に非存在 → 除外
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagA:y")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagC:blue")?;
            // c.txt: tagB なし → 第1 Nest に非存在 → 除外
            Ok(())
        }),
        format_query: default_scope,
        query: "(tagA: &: tagB:) & (tagA: &: tagC:)",
        assert: |res, _dir| {
            // 仕様: & 演算の結果は常に Projection → is_some()
            assert!(
                res.type_for_projection.is_some(),
                "Nest & Nest should produce LabelSetOp (with type_for_projection)"
            );
            let all_items: Vec<String> = res.results.iter().flat_map(|r| r.tags.entries.iter().map(|e| e.label.as_str().to_string()))
                .collect();
            assert!(
                all_items.iter().any(|n| n.contains("a.txt")),
                "a.txt (in both nests) should appear in intersection result"
            );
            // b.txt は tagA:x を持ち、ラベル値 "x" は積集合に含まれる → 含まれる
            assert!(
                all_items.iter().any(|n| n.contains("b.txt")),
                "b.txt (has tagA:x in first Nest, label 'x' in intersection) should appear"
            );
            // c.txt は tagA:y を持ち、ラベル値 "y" は積集合に含まれない → 除外
            assert!(
                !all_items.iter().any(|n| n.contains("c.txt")),
                "c.txt (tagA:y not in label intersection) must NOT appear"
            );
            Ok(())
        },
    },
    // Nest{2keys} & Nest{3keys} → LabelSetOp Intersect
    // (tagA:&:tagB:) & (tagA:&:tagC:&:tagD:): 深さの異なる Nest の積集合
    nest2_and_nest3_intersect: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            std::fs::write(dir.join("c.txt"), "c")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagA:x")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagB:1")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagC:red")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagD:alpha")?;
            // a.txt: 両 Nest に存在 → 積集合に残る
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagA:x")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagB:2")?;
            // b.txt: tagC/tagD なし → 第2 Nest に非存在 → 除外
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagA:y")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagC:blue")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagD:beta")?;
            // c.txt: tagB なし → 第1 Nest に非存在 → 除外
            Ok(())
        }),
        format_query: default_scope,
        query: "(tagA: &: tagB:) & (tagA: &: tagC: &: tagD:)",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "Nest2 & Nest3 should produce LabelSetOp (with type_for_projection)"
            );
            let all_items: Vec<String> = res.results.iter()
                .flat_map(|r| r.tags.entries.iter().map(|e| e.label.as_str().to_string()))
                .collect();
            assert!(
                all_items.iter().any(|n| n.contains("a.txt")),
                "a.txt (in both nests) should appear in intersection, got: {:?}", all_items
            );
            // b.txt は tagA:x を持ち、ラベル値 "x" は積集合に含まれる → 含まれる
            assert!(
                all_items.iter().any(|n| n.contains("b.txt")),
                "b.txt (has tagA:x, label 'x' in intersection) should appear, got: {:?}", all_items
            );
            // c.txt は tagA:y を持ち、ラベル値 "y" は積集合に含まれない → 除外
            assert!(
                !all_items.iter().any(|n| n.contains("c.txt")),
                "c.txt (tagA:y not in label intersection) must NOT appear"
            );
            Ok(())
        },
    },
    // extension: & size: → LabelSetOp Intersect（Phase 2 regression）
    // extension: が And([is_dir:false, Nest{ext}]) に展開されても積集合になることを確認
    // 仕様: extension ラベル値集合 {"rs","txt"} と size ラベル値集合 {100,200} は型違いで完全不一致
    //       → ラベル値積集合 = 空 → 空 Projection
    extension_and_size_intersect: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), vec![0u8; 100])?;
            std::fs::write(dir.join("b.txt"), vec![0u8; 200])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "extension: & size:",
        assert: |res, _dir| {
            // 仕様: 共通プレフィックスなし、かつラベル値型不一致 → 空 Projection
            assert!(
                res.type_for_projection.is_some(),
                "extension: & size: should produce LabelSetOp (with type_for_projection), got: {:?}",
                res.type_for_projection
            );
            assert!(
                res.results.is_empty(),
                "extension: & size: label value sets are disjoint (string vs int) → empty Projection, got: {:?}",
                res.results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>()
            );
            Ok(())
        },
    },
    // size: & size: → 同一キー Lv.2 & Lv.2 積集合（同一集合との積 = 全体）
    // 整数型ラベル (label_int) でも type_for_projection が Some になることを確認
    size_and_size_intersect: {
        setup: |dir| {
            std::fs::write(dir.join("small.txt"), vec![0u8; 10])?;
            std::fs::write(dir.join("large.txt"), vec![0u8; 200])?;
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "size: & size:",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "size: & size: should produce Projection (type_for_projection = Some), got: {:?}",
                res.type_for_projection
            );
            let all_items: Vec<String> = res.results.iter()
                .flat_map(|r| r.tags.entries.iter().map(|e| e.label.as_str().to_string()))
                .collect();
            assert!(
                all_items.iter().any(|n| n.contains("small.txt")),
                "small.txt should appear in size: & size: result, got: {:?}", all_items
            );
            assert!(
                all_items.iter().any(|n| n.contains("large.txt")),
                "large.txt should appear in size: & size: result, got: {:?}", all_items
            );
            Ok(())
        },
    },
    // Proj | Nest → LabelSetOp Union（Phase 3）
    // tagA: | (tagA:&:tagB:): 両方の結果を合わせた和集合
    proj_or_nest: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagA:x")?;
            // a.txt: tagA のみ（tagB なし）→ tagA: に存在するが Nest には非存在
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagA:y")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagB:1")?;
            // b.txt: tagA + tagB → 両方に存在
            Ok(())
        }),
        format_query: default_scope,
        query: "tagA: | (tagA: &: tagB:)",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "Proj | Nest should produce LabelSetOp (with type_for_projection)"
            );
            let all_items: Vec<String> = res.results.iter()
                .flat_map(|r| r.tags.entries.iter().map(|e| e.label.as_str().to_string()))
                .collect();
            assert!(
                all_items.iter().any(|n| n.contains("a.txt")),
                "a.txt (tagA only) should appear in union"
            );
            assert!(
                all_items.iter().any(|n| n.contains("b.txt")),
                "b.txt (tagA+tagB) should appear in union"
            );
            Ok(())
        },
    },
    // Nest | Nest（異なるキー）→ Lv.1 フラット（Phase 3）
    // (cat: &: flavor:) | (shape: &: color:): キー構造が異なる → Lv.1 平坦化
    nest_or_nest_flat: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            std::fs::write(dir.join("c.txt"), "c")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "cat:one")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "flavor:sweet")?;
            // a.txt: 第1 Nest のみ
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "shape:round")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "color:red")?;
            // b.txt: 第2 Nest のみ
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "cat:two")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "flavor:bitter")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "shape:square")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "color:blue")?;
            // c.txt: 両 Nest に存在
            Ok(())
        }),
        format_query: default_scope,
        query: "(cat: &: flavor:) | (shape: &: color:)",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_none(),
                "Nest | Nest (Lv.3 異なるキー、共通プレフィックスなし) → Lv.1 フラット"
            );
            // LabelSetOp SQL はラベル値グループを返すため r.name はラベル値
            // ファイルパスは各グループの entries に格納される
            let all_items: Vec<String> = res.results.iter()
                .flat_map(|r| r.tags.entries.iter().map(|e| e.label.as_str().to_string()))
                .collect();
            assert!(
                all_items.iter().any(|n| n.contains("a.txt")),
                "a.txt should appear in label group entries, got: {:?}", all_items
            );
            assert!(
                all_items.iter().any(|n| n.contains("b.txt")),
                "b.txt should appear in label group entries, got: {:?}", all_items
            );
            assert!(
                all_items.iter().any(|n| n.contains("c.txt")),
                "c.txt should appear in label group entries, got: {:?}", all_items
            );
            Ok(())
        },
    },
    // Proj -: Nest → LabelSetOp Except（Phase 4）
    // tagA: -: (tagA:&:tagB:): Nest に属すアイテムを Proj 結果から除外
    proj_minus_nest_except: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagA:x")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagB:1")?;
            // a.txt: Nest に存在 → tagA: 結果から除外
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagA:y")?;
            // b.txt: tagB なし → Nest に非存在 → tagA: 結果に残る
            Ok(())
        }),
        format_query: default_scope,
        query: "tagA: -: (tagA: &: tagB:)",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "Proj -: Nest should produce LabelSetOp Except (with type_for_projection)"
            );
            // ラベルの確認（メイン）: "y" グループのみ残り "x" は除外される
            assert_eq!(
                res.results.len(), 1,
                "should have exactly 1 label group, got {:?}",
                res.results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>()
            );
            assert!(
                res.results.iter().any(|r| r.name == "y"),
                "'y' label group must remain (b.txt has tagA:y, not in Nest)"
            );
            assert!(
                !res.results.iter().any(|r| r.name == "x"),
                "'x' label group must be excluded (a.txt has tagA:x and is in Nest)"
            );
            // アイテムの確認（サブ）
            let all_items: Vec<String> = res.results.iter()
                .flat_map(|r| r.tags.entries.iter().map(|e| e.label.as_str().to_string()))
                .collect();
            assert!(all_items.iter().any(|n| n.contains("b.txt")), "b.txt must be in result");
            assert!(!all_items.iter().any(|n| n.contains("a.txt")), "a.txt must be excluded");
            Ok(())
        },
    },
    // Nest -: Proj → LabelSetOp Except（Phase 4）
    // (cat: &: flavor:) -: grade:: grade: を持つアイテムを Nest から除外
    nest_minus_proj_except: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "cat:one")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "flavor:sweet")?;
            // a.txt: grade: なし → Nest に残る
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "cat:two")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "flavor:bitter")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "grade:A")?;
            // b.txt: grade: あり → grade: Proj に存在 → Nest から除外
            Ok(())
        }),
        format_query: default_scope,
        query: "(cat: &: flavor:) -: grade:",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "Nest -: Proj should produce LabelSetOp Except (with type_for_projection)"
            );
            // ラベルの確認（メイン）: "one" グループのみ残り "two" は除外される
            assert_eq!(
                res.results.len(), 1,
                "should have exactly 1 label group, got {:?}",
                res.results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>()
            );
            assert!(
                res.results.iter().any(|r| r.name == "one"),
                "'one' label group must remain (a.txt has no grade:)"
            );
            assert!(
                !res.results.iter().any(|r| r.name == "two"),
                "'two' label group must be excluded (b.txt has grade:A)"
            );
            // アイテムの確認（サブ）
            let all_items: Vec<String> = res.results.iter()
                .flat_map(|r| r.tags.entries.iter().map(|e| e.label.as_str().to_string()))
                .collect();
            assert!(all_items.iter().any(|n| n.contains("a.txt")), "a.txt must be in result");
            assert!(!all_items.iter().any(|n| n.contains("b.txt")), "b.txt must be excluded");
            Ok(())
        },
    },
    // Nest -: Nest → LabelSetOp Except（Phase 4）
    // (tagA:&:tagB:) -: (tagA:&:tagC:): 第2 Nest に属すアイテムを第1 Nest から除外
    nest_minus_nest_except: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            std::fs::write(dir.join("c.txt"), "c")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagA:x")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagB:1")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagC:red")?;
            // a.txt: 両 Nest に存在 → 第1 Nest から除外
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagA:x")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagB:2")?;
            // b.txt: tagC なし → 第2 Nest に非存在 → 第1 Nest に残る
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagA:y")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagC:blue")?;
            // c.txt: tagB なし → 第1 Nest に非存在 → 結果に影響なし
            Ok(())
        }),
        format_query: default_scope,
        query: "(tagA: &: tagB:) -: (tagA: &: tagC:)",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "Nest -: Nest should produce LabelSetOp Except (with type_for_projection)"
            );
            // ラベルの確認（メイン）: "x" グループのみ残る（a.txt が除外されても b.txt が "x" に残る）
            assert_eq!(
                res.results.len(), 1,
                "should have exactly 1 label group, got {:?}",
                res.results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>()
            );
            assert!(
                res.results.iter().any(|r| r.name == "x"),
                "'x' label group must remain (b.txt has tagA:x and is only in Nest1)"
            );
            // アイテムの確認（サブ）
            let all_items: Vec<String> = res.results.iter()
                .flat_map(|r| r.tags.entries.iter().map(|e| e.label.as_str().to_string()))
                .collect();
            assert!(all_items.iter().any(|n| n.contains("b.txt")), "b.txt must be in result");
            assert!(!all_items.iter().any(|n| n.contains("a.txt")), "a.txt must be excluded");
            Ok(())
        },
    },
    // Lv.4 Nest -: Proj → LabelSetOp Except（Phase 4 深いネスト）
    // (tagA: &: tagB: &: tagC:) -: grade:: grade: を持つアイテムを Lv.4 Nest から除外
    nest_lv4_minus_proj_except: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            std::fs::write(dir.join("c.txt"), "c")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagA:x")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagB:1")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagC:red")?;
            // a.txt: grade: なし → 残る
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagA:x")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagB:2")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagC:blue")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "grade:A")?;
            // b.txt: grade: あり → 除外
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagA:y")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagB:3")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagC:green")?;
            // c.txt: grade: なし → 残る
            Ok(())
        }),
        format_query: default_scope,
        query: "(tagA: &: tagB: &: tagC:) -: grade:",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "Lv.4 Nest -: Proj should produce LabelSetOp Except (with type_for_projection)"
            );
            // ラベルの確認（メイン）: "x"（a.txt のみ）と "y"（c.txt）が残る
            assert_eq!(
                res.results.len(), 2,
                "should have 2 label groups ('x' and 'y'), got {:?}",
                res.results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>()
            );
            assert!(res.results.iter().any(|r| r.name == "x"), "'x' group must remain");
            assert!(res.results.iter().any(|r| r.name == "y"), "'y' group must remain");
            // "x" グループに a.txt のみ（b.txt は grade: で除外）
            let x_group = res.results.iter().find(|r| r.name == "x").unwrap();
            let x_items: Vec<_> = x_group.tags.entries.iter()
                .map(|e| e.label.as_str())
                .collect();
            assert!(x_items.iter().any(|n| n.contains("a.txt")), "a.txt must be in 'x' group");
            assert!(!x_items.iter().any(|n| n.contains("b.txt")), "b.txt must not be in 'x' group");
            Ok(())
        },
    },
    // Lv.4 Nest -: Lv.4 Nest → LabelSetOp Except（Phase 4 深いネスト同士）
    // (tagA: &: tagB: &: tagC:) -: (tagA: &: tagB: &: tagD:)
    nest_lv4_minus_nest_lv4_except: {
        setup: |dir| {
            std::fs::write(dir.join("a.txt"), "a")?;
            std::fs::write(dir.join("b.txt"), "b")?;
            std::fs::write(dir.join("c.txt"), "c")?;
            Ok(())
        },
        modify: Some(|fm, dir| {
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagA:x")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagB:1")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagC:red")?;
            fm.tag_item(&dir.join("a.txt").to_string_lossy(), "tagD:alpha")?;
            // a.txt: 両 Nest に存在 → 除外
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagA:x")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagB:2")?;
            fm.tag_item(&dir.join("b.txt").to_string_lossy(), "tagC:blue")?;
            // b.txt: tagD なし → 第2 Nest に非存在 → 残る
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagA:y")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagB:3")?;
            fm.tag_item(&dir.join("c.txt").to_string_lossy(), "tagD:beta")?;
            // c.txt: tagC なし → 第1 Nest に非存在 → 結果に影響なし
            Ok(())
        }),
        format_query: default_scope,
        query: "(tagA: &: tagB: &: tagC:) -: (tagA: &: tagB: &: tagD:)",
        assert: |res, _dir| {
            assert!(
                res.type_for_projection.is_some(),
                "Lv.4 Nest -: Lv.4 Nest should produce LabelSetOp Except (with type_for_projection)"
            );
            // ラベルの確認（メイン）: "x" グループのみ（b.txt）
            assert_eq!(
                res.results.len(), 1,
                "should have exactly 1 label group, got {:?}",
                res.results.iter().map(|r| r.name.as_str()).collect::<Vec<_>>()
            );
            assert!(res.results.iter().any(|r| r.name == "x"), "'x' group must remain");
            // アイテムの確認（サブ）
            let all_items: Vec<String> = res.results.iter()
                .flat_map(|r| r.tags.entries.iter().map(|e| e.label.as_str().to_string()))
                .collect();
            assert!(all_items.iter().any(|n| n.contains("b.txt")), "b.txt must be in result");
            assert!(!all_items.iter().any(|n| n.contains("a.txt")), "a.txt must be excluded");
            Ok(())
        },
    },
}

// ──────────────────────────────────────────────
// Phase 1: パース
// ──────────────────────────────────────────────

#[test]
fn test_nest_parse_basic() {
    let node = ttfm::query::parse("extension: &: parentdir:").unwrap();
    if let ttfm::query::QueryNode::Nest(nest) = &node {
        assert!(
            matches!(*nest.left, ttfm::query::QueryNode::Projection(_)),
            "left=Projection"
        );
        assert!(
            matches!(*nest.right, ttfm::query::QueryNode::Projection(_)),
            "right=Projection"
        );
    } else {
        panic!("Expected Nest, got {:?}", node);
    }
}

#[test]
fn test_nest_parse_chain() {
    let node = ttfm::query::parse("extension: &: parentdir: &: name:").unwrap();
    if let ttfm::query::QueryNode::Nest(outer) = &node {
        assert!(
            matches!(*outer.left, ttfm::query::QueryNode::Nest(_)),
            "left=Nest"
        );
        assert!(
            matches!(*outer.right, ttfm::query::QueryNode::Projection(_)),
            "right=Projection"
        );
    } else {
        panic!("Expected Nest, got {:?}", node);
    }
}

#[test]
fn test_nest_priority_over_and() {
    let node =
        ttfm::query::parse("extension: &: parentdir: & extension:rs").unwrap();
    assert!(
        matches!(node, ttfm::query::QueryNode::And(_)),
        "top-level And, got {:?}",
        node
    );
}

#[test]
fn test_nest_parse_with_aggregation() {
    let node =
        ttfm::query::parse("parentdir: &: count(extension:jpg)").unwrap();
    if let ttfm::query::QueryNode::Nest(nest) = &node {
        assert!(
            matches!(*nest.right, ttfm::query::QueryNode::Aggregation(_)),
            "right=Aggregation"
        );
    } else {
        panic!("Expected Nest, got {:?}", node);
    }
}

// ──────────────────────────────────────────────
// Phase 2: 論理解決
// ──────────────────────────────────────────────

#[test]
fn test_nest_left_must_be_projection() {
    let result =
        ttfm::query::lens_resolver::Resolver::new("extension:rs &: name:");
    assert!(result.is_err(), "non-projection left should fail");
}

// ──────────────────────────────────────────────
// Phase 3: 物理解決
// ──────────────────────────────────────────────

#[test]
fn test_nest_resolves_to_projection_with_nvalue() {
    let resolver = ttfm::query::lens_resolver::Resolver::new(
        "parentdir: &: count(extension:jpg)",
    )
    .unwrap();
    assert!(resolver.get_projection().is_some());
    assert!(resolver.get_nvalue().is_some());
}

#[test]
fn test_nest_resolves_sum_nvalue() {
    let resolver =
        ttfm::query::lens_resolver::Resolver::new("parentdir: &: sum(size:)")
            .unwrap();
    assert!(resolver.get_projection().is_some());
    assert!(resolver.get_nvalue().is_some());
}

#[test]
fn test_plain_projection_no_nvalue() {
    let resolver =
        ttfm::query::lens_resolver::Resolver::new("extension:").unwrap();
    assert!(resolver.get_projection().is_some());
    assert!(
        resolver.get_nvalue().is_none(),
        "plain projection has no nvalue"
    );
}

// ──────────────────────────────────────────────
// エラーケース
// ──────────────────────────────────────────────

#[test]
fn test_nest_error_typed_tag_left() {
    assert!(ttfm::query::lens_resolver::Resolver::new(
        "extension:rs &: count(*:*)"
    )
    .is_err());
}

#[test]
fn test_nest_error_aggregation_left() {
    assert!(ttfm::query::lens_resolver::Resolver::new(
        "count(*:*) &: extension:"
    )
    .is_err());
}

#[test]
fn test_nest_error_comparison_left() {
    assert!(ttfm::query::lens_resolver::Resolver::new(
        "(size: > 100) &: extension:"
    )
    .is_err());
}

#[test]
fn test_nest_right_comparison_resolves() {
    let resolver = ttfm::query::lens_resolver::Resolver::new(
        "parentdir: &: (count(extension:jpg) > 1)",
    )
    .unwrap();
    assert!(resolver.get_projection().is_some());
    assert!(resolver.get_nvalue().is_some());
    assert!(resolver.get_nvalue_condition().is_some());
}

// ──────────────────────────────────────────────
// 解決レベルのパターン確認
// ──────────────────────────────────────────────

#[test]
fn test_nest_resolver_all_aggregations() {
    let queries = [
        "parentdir: &: count(extension:rs)",
        "parentdir: &: sum(size:)",
        "parentdir: &: max(size:)",
        "parentdir: &: min(size:)",
        "parentdir: &: avg(size:)",
        "parentdir: &: count(*:*)",
        "extension: &: count(*:*)",
        "extension: &: sum(size:)",
        "extension: &: max(size:)",
        "extension: &: min(size:)",
        "extension: &: avg(size:)",
        "filename: &: count(*:*)",
        "filename: &: sum(size:)",
    ];
    for query in &queries {
        let result = ttfm::query::lens_resolver::Resolver::new(query);
        assert!(
            result.is_ok(),
            "'{}' should resolve: {}",
            query,
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
        let resolver = result.unwrap();
        assert!(
            resolver.get_projection().is_some(),
            "'{}' has projection",
            query
        );
        assert!(resolver.get_nvalue().is_some(), "'{}' has nvalue", query);
    }
}

#[test]
fn test_nest_scalar_right_resolves() {
    let resolver =
        ttfm::query::lens_resolver::Resolver::new("parentdir: &: 100").unwrap();
    assert!(resolver.get_projection().is_some());
    assert!(resolver.get_nvalue().is_some());
}

#[test]
fn test_nest_comparison_resolver_patterns() {
    let queries = [
        "parentdir: &: (count(extension:rs) > 1)",
        "parentdir: &: (sum(size:) > 100)",
        "parentdir: &: (max(size:) > 500)",
        "parentdir: &: (min(size:) > 10)",
        "parentdir: &: (avg(size:) > 50)",
        "extension: &: (count(*:*) > 2)",
    ];
    for query in &queries {
        let result = ttfm::query::lens_resolver::Resolver::new(query);
        assert!(
            result.is_ok(),
            "'{}': {}",
            query,
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
        let resolver = result.unwrap();
        assert!(
            resolver.get_projection().is_some(),
            "'{}' has projection",
            query
        );
        assert!(resolver.get_nvalue().is_some(), "'{}' has nvalue", query);
        assert!(
            resolver.get_nvalue_condition().is_some(),
            "'{}' has nvalue_condition",
            query
        );
    }
}

#[test]
fn test_nest_query_vs_calc_resolves() {
    let queries = [
        "extension: &: (sum(size:) > (sum(size:) / 2))",
        "parentdir: &: (sum(size:) > (sum(size:) / 2))",
        "parentdir: &: (avg(size:) > (sum(size:) / count()))",
    ];
    for query in &queries {
        let result = ttfm::query::lens_resolver::Resolver::new(query);
        assert!(
            result.is_ok(),
            "'{}': {}",
            query,
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }
}

#[test]
fn test_unnest_with_filter() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;

    // a.rs(7), b.rs(10) → rs: sum=17
    // c.txt(5) → txt: sum=5
    std::fs::write(root_path.join("a.rs"), "content")?;
    std::fs::write(root_path.join("b.rs"), "0123456789")?;
    std::fs::write(root_path.join("c.txt"), "abcde")?;

    let fm = FileManager::new_with_db_dir(db_dir.path())?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    let res =
        fm.search("sum(extension: &: size:)", SearchOptions::default())?;

    assert!(
        res.type_for_projection.is_some(),
        "Should return projection result"
    );

    let get_nvalue = |group: &ttfm::response::SearchResult| -> f64 {
        let nvalue = group
            .tags
            .entries
            .iter()
            .find(|e| e.label.tag_type().as_str() == "nvalue")
            .expect("Should have nvalue tag");
        match nvalue.label.value() {
            ttfm::types::LabelValue::Double(d_bits) => f64::from_bits(d_bits),
            ttfm::types::LabelValue::Integer(i) => i as f64,
            _ => panic!("Unexpected nvalue type"),
        }
    };

    let rs_group = res
        .results
        .iter()
        .find(|r| r.name.contains("rs"))
        .expect("Should have rs group");
    assert_eq!(get_nvalue(rs_group), 17.0);

    let txt_group = res
        .results
        .iter()
        .find(|r| r.name.contains("txt"))
        .expect("Should have txt group");
    assert_eq!(get_nvalue(txt_group), 5.0);

    Ok(())
}
