/// ネスト演算子 (`&:`) の統合テスト
use std::path::Path;
use std::sync::OnceLock;
use rstest::rstest;
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
    query: &'static str,
    /// クエリを実行前に加工する関数。デフォルトは `default_scope`。
    /// outer-agg クエリ等、特殊なスコープ付与が必要なケースで上書きする。
    format_query: fn(&str, &Path) -> String,
    /// DBのインデックス完了後に、ファイルに対してタグ付け等の操作を行うためのオプションのフック
    modify: Option<fn(&FileManager, &Path) -> anyhow::Result<()>>,
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
            std::fs::create_dir_all(&case_dir)
                .unwrap_or_else(|e| panic!("Failed to create dir for '{}': {}", case.name, e));
            (case.setup)(&case_dir)
                .unwrap_or_else(|e| panic!("Setup failed for '{}': {}", case.name, e));
        }
        {
            let fm = FileManager::new_with_db_dir(&db_dir).expect("FM create");
            fm.index_directory(root.path(), None::<&fn(usize)>, false)
                .expect("index_directory");
            for case in CASES {
                if let Some(modify) = case.modify {
                    let case_dir = root.path().join(case.name);
                    modify(&fm, &case_dir)
                        .unwrap_or_else(|e| panic!("Modify failed for '{}': {}", case.name, e));
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

/// 各Projectionなどへの個別注入を避け、可能な限り「Nest 全体」をパスフィルタで包みます。
/// これにより、Nestの論理構造を保護しつつ、実行範囲を対象ディレクトリに制限します。
fn inject_path_scope(query: &str, dir: &Path) -> String {
    let p = dir.to_string_lossy();
    let filter = format!("& path:{}/*", p);

    // 2段階集約: sum(sum(INNER))
    if query.starts_with("sum(sum(") && query.ends_with("))") {
        let inner = &query[8..query.len()-2];
        let res = format!("sum(sum(({}) {}))", inner, filter);
        println!("DEBUG: [inject_path_scope] original='{}' -> transformed='{}'", query, res);
        return res;
    }

    // 集計関数: agg(INNER)
    for agg in &["sum(", "count(", "avg(", "max(", "min("] {
        if query.starts_with(agg) && query.ends_with(')') {
            let inner = &query[agg.len()..query.len()-1];
            let res = format!("{}(({}) {})", &agg[..agg.len()-1], inner, filter);
            println!("DEBUG: [inject_path_scope] original='{}' -> transformed='{}'", query, res);
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
                let res = format!("{}(({}) {}){}", prefix, inner, filter, suffix);
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
                let res = format!("{}(({}) {}){}", prefix, inner, filter, suffix);
                println!("DEBUG: [inject_path_scope] original='{}' -> transformed='{}'", query, res);
                return res;
            }
        }
    }

    // 算術演算: (A) + (B)
    if query.contains(") + (") {
        if let Some(mid) = query.find(") + (") {
            let left = query[..mid + 1].trim_matches(|c| c == '(' || c == ')');
            let right = query[mid + 4..].trim_matches(|c| c == '(' || c == ')');
            let res = format!("(({}) {}) + (({}) {})", left, filter, right, filter);
            println!("DEBUG: [inject_path_scope] original='{}' -> transformed='{}'", query, res);
            return res;
        }
    }

    // その他 (通常の Nest 等): 全体を包む
    let res = format!("({}) {}", query, filter);
    println!("DEBUG: [inject_path_scope] original='{}' -> transformed='{}'", query, res);
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
// 全E2Eテストケースの定義
// ──────────────────────────────────────────────

static CASES: &[NestTestCase] = &[
    // ── 基本 nest クエリ（default_scope） ─────────────────────
    NestTestCase {
        name: "count_e2e",
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
        query: "parentdir: &: count(extension:jpg)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some(), "Should be projection");
            let src = res.results.iter().find(|r| r.name.contains("src")).expect("src");
            let docs = res.results.iter().find(|r| r.name.contains("docs")).expect("docs");
            assert_eq!(get_nvalue(src).as_deref(), Some("2"), "src: 2 jpg");
            assert_eq!(get_nvalue(docs).as_deref(), Some("1"), "docs: 1 jpg");
            Ok(())
        },
    },
    NestTestCase {
        name: "sum_e2e",
        setup: |dir| {
            let sub = dir.join("sub");
            std::fs::create_dir_all(&sub)?;
            std::fs::write(sub.join("a.txt"), vec![0u8; 100])?;
            std::fs::write(sub.join("b.txt"), vec![0u8; 200])?;
            Ok(())
        },
        query: "parentdir: &: sum(size:)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let sub = res.results.iter().find(|r| r.name.contains("sub")).expect("sub");
            assert_eq!(get_nvalue(sub).as_deref(), Some("300"), "sub sum=300");
            Ok(())
        },
    },
    NestTestCase {
        name: "no_regression_plain_projection",
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "")?;
            std::fs::write(dir.join("b.txt"), "")?;
            Ok(())
        },
        query: "extension:",
        format_query: default_scope,
        modify: None,
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
    NestTestCase {
        name: "extension_left_count",
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "a")?;
            std::fs::write(dir.join("b.rs"), "bb")?;
            std::fs::write(dir.join("c.txt"), "ccc")?;
            Ok(())
        },
        query: "extension: &: count(*:*)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let rs = res.results.iter().find(|r| r.name == "rs").expect("rs");
            let txt = res.results.iter().find(|r| r.name == "txt").expect("txt");
            assert_eq!(get_nvalue(rs).as_deref(), Some("2"), "rs count=2");
            assert_eq!(get_nvalue(txt).as_deref(), Some("1"), "txt count=1");
            Ok(())
        },
    },
    NestTestCase {
        name: "extension_left_sum_size",
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), vec![0u8; 100])?;
            std::fs::write(dir.join("b.rs"), vec![0u8; 200])?;
            std::fs::write(dir.join("c.txt"), vec![0u8; 50])?;
            Ok(())
        },
        query: "extension: &: sum(size:)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let rs = res.results.iter().find(|r| r.name == "rs").expect("rs");
            let txt = res.results.iter().find(|r| r.name == "txt").expect("txt");
            assert_eq!(get_nvalue(rs).as_deref(), Some("300"), "rs sum=300");
            assert_eq!(get_nvalue(txt).as_deref(), Some("50"), "txt sum=50");
            Ok(())
        },
    },
    NestTestCase {
        name: "max_size",
        setup: |dir| {
            let sub = dir.join("sub");
            std::fs::create_dir_all(&sub)?;
            std::fs::write(sub.join("small.txt"), vec![0u8; 10])?;
            std::fs::write(sub.join("large.txt"), vec![0u8; 500])?;
            Ok(())
        },
        query: "parentdir: &: max(size:)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let sub = res.results.iter().find(|r| r.name.contains("sub")).expect("sub");
            assert_eq!(get_nvalue(sub).as_deref(), Some("500"), "max=500");
            Ok(())
        },
    },
    NestTestCase {
        name: "min_size",
        setup: |dir| {
            let sub = dir.join("sub");
            std::fs::create_dir_all(&sub)?;
            std::fs::write(sub.join("small.txt"), vec![0u8; 10])?;
            std::fs::write(sub.join("large.txt"), vec![0u8; 500])?;
            Ok(())
        },
        query: "parentdir: &: min(size:)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let sub = res.results.iter().find(|r| r.name.contains("sub")).expect("sub");
            assert_eq!(get_nvalue(sub).as_deref(), Some("10"), "min=10");
            Ok(())
        },
    },
    NestTestCase {
        name: "avg_size",
        setup: |dir| {
            let sub = dir.join("sub");
            std::fs::create_dir_all(&sub)?;
            std::fs::write(sub.join("a.txt"), vec![0u8; 100])?;
            std::fs::write(sub.join("b.txt"), vec![0u8; 200])?;
            Ok(())
        },
        query: "parentdir: &: avg(size:)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let sub = res.results.iter().find(|r| r.name.contains("sub")).expect("sub");
            let nv: f64 = get_nvalue(sub).expect("nvalue").parse().expect("numeric");
            assert!((nv - 150.0).abs() < 1.0, "avg~150, got {}", nv);
            Ok(())
        },
    },
    NestTestCase {
        name: "count_all",
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
        query: "parentdir: &: count(*:*)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let alpha = res.results.iter().find(|r| r.name.contains("alpha")).expect("alpha");
            let beta = res.results.iter().find(|r| r.name.contains("beta")).expect("beta");
            assert_eq!(get_nvalue(alpha).as_deref(), Some("3"), "alpha=3");
            assert_eq!(get_nvalue(beta).as_deref(), Some("1"), "beta=1");
            Ok(())
        },
    },
    NestTestCase {
        name: "filename_left",
        setup: |dir| {
            std::fs::write(dir.join("hello.txt"), vec![0u8; 100])?;
            Ok(())
        },
        query: "filename: &: sum(size:)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let hello = res.results.iter().find(|r| r.name == "hello.txt").expect("hello.txt");
            assert_eq!(get_nvalue(hello).as_deref(), Some("100"), "sum=100");
            Ok(())
        },
    },
    NestTestCase {
        name: "comparison_count_gt",
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
        query: "parentdir: &: (count(extension:jpg) > 1)",
        format_query: default_scope,
        modify: None,
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
    NestTestCase {
        name: "comparison_sum_gt",
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), vec![0u8; 50])?;
            std::fs::write(dir.join("b.rs"), vec![0u8; 60])?;
            std::fs::write(dir.join("c.txt"), vec![0u8; 30])?;
            Ok(())
        },
        query: "extension: &: (sum(size:) > 100)",
        format_query: default_scope,
        modify: None,
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
    NestTestCase {
        name: "context_propagation",
        setup: |dir| {
            std::fs::write(dir.join("a.html"), vec![0u8; 100])?;
            std::fs::write(dir.join("b.html"), vec![0u8; 200])?;
            std::fs::write(dir.join("c.txt"), vec![0u8; 50])?;
            Ok(())
        },
        query: "stem:a & extension: &: sum(size:)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let html = res.results.iter().find(|r| r.name == "html").expect("html");
            assert_eq!(get_nvalue(html).as_deref(), Some("100"), "html=100");
            assert!(res.results.iter().find(|r| r.name == "txt").is_none(), "txt filtered");
            Ok(())
        },
    },
    NestTestCase {
        name: "pick_filter",
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
        query: "parentdir: &: (count(extension:jpg) > 10)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(!names.iter().any(|&n| n == "f1.jpg" || n == "f2.jpg"),
                "dirA excluded: {:?}", names);
            assert!(names.iter().any(|n| n.starts_with('g')),
                "dirB included: {:?}", names);
            Ok(())
        },
    },
    NestTestCase {
        name: "scenario_a",
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
        query: "extension:html & parentdir: &: count(extension:html) > 0",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|&n| n == "f1.html" || n == "f3.html"),
                "html files should appear: {:?}", names);
            Ok(())
        },
    },
    NestTestCase {
        name: "scenario_b",
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
        query: "parentdir: &: (avg(size:) == sum(size:))",
        format_query: default_scope,
        modify: None,
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
    NestTestCase {
        name: "scenario_stem_wildcard",
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
        query: "extension:html & parentdir: &: count(stem:*a*) == 2",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|&n| n == "apple.html" || n == "banana.html"),
                "dirA html expected: {:?}", names);
            assert_eq!(names.len(), 2, "Only 2 items: {:?}", names);
            Ok(())
        },
    },
    NestTestCase {
        name: "chained_comparison",
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
        query: "parentdir: &: (200 > sum(size:) > 50)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|&n| n == "a.txt" || n == "b.txt"), "dirA included: {:?}", names);
            assert!(!names.iter().any(|&n| n == "c.txt" || n == "d.txt"), "dirB excluded: {:?}", names);
            assert!(!names.iter().any(|&n| n == "e.txt" || n == "f.txt"), "dirC excluded: {:?}", names);
            Ok(())
        },
    },
    NestTestCase {
        name: "arithmetic_mul",
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
        query: "parentdir: &: (sum(size:) * count(size:))",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("60"), "dir1: 30*2=60");
            let d2 = res.results.iter().find(|r| r.name.contains("dir2")).expect("dir2");
            assert_eq!(get_nvalue(d2).as_deref(), Some("100"), "dir2: 100*1=100");
            Ok(())
        },
    },
    NestTestCase {
        name: "arithmetic_add",
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
        query: "parentdir: &: (sum(size:) + count(size:))",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("32"), "dir1: 30+2=32");
            Ok(())
        },
    },
    NestTestCase {
        name: "arithmetic_sub",
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
        query: "parentdir: &: (sum(size:) - count(size:))",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("28"), "dir1: 30-2=28");
            Ok(())
        },
    },
    NestTestCase {
        name: "arithmetic_div",
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
        query: "parentdir: &: (sum(size:) / count(size:))",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("15"), "dir1: 30/2=15");
            Ok(())
        },
    },
    NestTestCase {
        name: "arithmetic_avg_sum",
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
        query: "parentdir: &: (avg(size:) + sum(size:))",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("45"), "dir1: avg(15)+sum(30)=45");
            Ok(())
        },
    },
    NestTestCase {
        name: "arithmetic_max_lit",
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
        query: "parentdir: &: (max(size:) * 2)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("40"), "dir1: max(20)*2=40");
            Ok(())
        },
    },
    NestTestCase {
        name: "arithmetic_lit_min",
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
        query: "parentdir: &: (1000 / min(size:))",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("100"), "dir1: 1000/min(10)=100");
            Ok(())
        },
    },
    NestTestCase {
        name: "arithmetic_nested",
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
        query: "parentdir: &: ((sum(size:) + 10) * count(size:))",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let d1 = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            assert_eq!(get_nvalue(d1).as_deref(), Some("80"), "dir1: (30+10)*2=80");
            Ok(())
        },
    },
    NestTestCase {
        name: "or_merged_projection",
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
        query: "parentdir: &: (count(extension:rs) > 0) | parentdir: &: (count(*:*) > 1)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|&n| n == "main.rs"), "dirA included: {:?}", names);
            assert!(names.iter().any(|&n| n == "a.txt" || n == "b.txt"), "dirB included: {:?}", names);
            assert!(!names.iter().any(|&n| n == "c.txt"), "dirC excluded: {:?}", names);
            Ok(())
        },
    },
    NestTestCase {
        name: "arithmetic_null_propagation",
        setup: |dir| {
            let dir_rs = dir.join("dir_rs");
            let dir_txt = dir.join("dir_txt");
            std::fs::create_dir(&dir_rs)?;
            std::fs::create_dir(&dir_txt)?;
            std::fs::write(dir_rs.join("main.rs"), vec![0u8; 10])?;
            std::fs::write(dir_txt.join("readme.txt"), vec![0u8; 50])?;
            Ok(())
        },
        query: "parentdir: &: (sum(size:) + count(extension:rs))",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let dir_rs = res.results.iter().find(|r| r.name.contains("dir_rs")).expect("dir_rs");
            assert_eq!(get_nvalue(dir_rs).as_deref(), Some("11"), "10+1=11");
            let dir_txt = res.results.iter().find(|r| r.name.contains("dir_txt")).expect("dir_txt");
            assert_eq!(get_nvalue(dir_txt).as_deref(), Some("50"), "50+0=50");
            Ok(())
        },
    },
    NestTestCase {
        name: "filter_empty_groups",
        setup: |dir| {
            let dir1 = dir.join("dir1");
            let dir2 = dir.join("dir2");
            std::fs::create_dir_all(&dir1)?;
            std::fs::create_dir_all(&dir2)?;
            std::fs::write(dir1.join("a.rs"), "code")?;
            std::fs::write(dir2.join("b.txt"), "text")?;
            Ok(())
        },
        query: "parentdir: &: count(extension:rs)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some());
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.iter().any(|&n| n.contains("dir1")), "dir1 included");
            assert!(!names.iter().any(|&n| n.contains("dir2")), "dir2 excluded");
            Ok(())
        },
    },
    NestTestCase {
        name: "dedup_keys",
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "content")?;
            Ok(())
        },
        query: "parentdir: &: parentdir: &: count()",
        format_query: default_scope,
        modify: None,
        assert: |_res, _dir| Ok(()),
    },
    NestTestCase {
        name: "level3_projection",
        setup: |dir| {
            let work = dir.join("work");
            std::fs::create_dir(&work)?;
            std::fs::write(work.join("a.rs"), "content")?;
            Ok(())
        },
        query: "parentdir: &: filename:",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.results.iter().any(|r| r.name.contains("work") && r.name.contains("a.rs")),
                "work/a.rs expected: {:?}", res.results.iter().map(|r| &r.name).collect::<Vec<_>>());
            Ok(())
        },
    },
    NestTestCase {
        name: "level3_projection_with_agg",
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
        query: "parentdir: &: extension: &: sum(size:)",
        format_query: default_scope,
        modify: None,
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
    NestTestCase {
        name: "level3_projection_with_agg_filter",
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
        query: "parentdir: &: extension: &: (sum(size:) > 2)",
        format_query: default_scope,
        modify: None,
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
    NestTestCase {
        name: "level4_nest",
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
        query: "parentdir: &: extension: &: size:",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let dir1_rs = res.results.iter().filter(|r| r.name.contains("dir1") && r.name.contains("rs")).count();
            let dir2_txt = res.results.iter().filter(|r| r.name.contains("dir2") && r.name.contains("txt")).count();
            assert_eq!(dir1_rs, 1);
            assert_eq!(dir2_txt, 1);
            Ok(())
        },
    },
    NestTestCase {
        name: "level3_agg_internal_filter",
        setup: |dir| {
            let dir1 = dir.join("dir1");
            std::fs::create_dir_all(&dir1)?;
            std::fs::write(dir1.join("small.rs"), "12345")?;           // 5
            std::fs::write(dir1.join("large.rs"), "0123456789ABCDE")?; // 15
            Ok(())
        },
        query: "parentdir: &: extension: &: sum(size: :> 10 & size:)",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            let dir1_rs = res.results.iter()
                .find(|r| r.name.contains("dir1") && r.name.contains("rs"))
                .expect("dir1/rs");
            let val = get_nvalue_f64(dir1_rs).expect("nvalue");
            assert_eq!(val, 15.0, "only >10 bytes included: {}", val);
            Ok(())
        },
    },
    NestTestCase {
        name: "query_vs_calc_e2e",
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), vec![0u8; 10])?;
            std::fs::write(dir.join("b.rs"), vec![0u8; 20])?;
            std::fs::write(dir.join("c.rs"), vec![0u8; 30])?;
            std::fs::write(dir.join("d.txt"), vec![0u8; 5])?;
            Ok(())
        },
        query: "extension: &: (sum(size:) > (sum(size:) / 2))",
        format_query: default_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none(), "flat list");
            assert!(!res.results.is_empty(), "has results");
            Ok(())
        },
    },
    // ── outer-agg クエリ（scope_with_sum / scope_with_count） ──
    NestTestCase {
        name: "agg_over_nvalue_sum_count",
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
        query: "sum(parentdir: &: count(extension:jpg))",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            assert_eq!(res.results.len(), 1);
            let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
            assert_eq!(val, 3.0, "scalar 3.0, got {}", val);
            Ok(())
        },
    },
    NestTestCase {
        name: "agg_over_nvalue_count",
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
        query: "count(parentdir: &: count(extension:jpg))",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            assert_eq!(res.results.len(), 1);
            let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
            assert_eq!(val, 2.0, "scalar 2.0, got {}", val);
            Ok(())
        },
    },
    NestTestCase {
        name: "agg_over_nvalue_with_comparison",
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
        query: "count(parentdir: &: (count(extension:jpg) > 1))",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            assert_eq!(res.results.len(), 1);
            let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
            assert_eq!(val, 1.0, "scalar 1.0, got {}", val);
            Ok(())
        },
    },
    NestTestCase {
        name: "agg_over_nvalue_sum_with_comparison",
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
        query: "sum(parentdir: &: (count(extension:jpg) > 1))",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            assert_eq!(res.results.len(), 1);
            let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
            assert_eq!(val, 3.0, "scalar 3.0, got {}", val);
            Ok(())
        },
    },
    NestTestCase {
        name: "agg_calc_wrap",
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
        query: "100 - count(parentdir: &: (count(extension:jpg) > 1))",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            assert_eq!(res.results.len(), 1);
            let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
            assert_eq!(val, 99.0, "99.0, got {}", val);
            Ok(())
        },
    },
    NestTestCase {
        name: "agg_sum_calc_wrap",
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
        query: "sum(parentdir: &: (count(extension:jpg) > 1)) * 2",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none());
            assert_eq!(res.results.len(), 1);
            let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
            assert_eq!(val, 6.0, "6.0, got {}", val);
            Ok(())
        },
    },
    // ── mixed-key（各オペランドに個別でpath注入） ──────────────
/*
    NestTestCase {
        name: "mixed_key_calculation",
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
        query: "(parentdir: &: count()) + (extension: &: count())",
        format_query: inject_path_scope,
        modify: None,
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
    NestTestCase {
        name: "mixed_key_arithmetic_deepens_nest",
        setup: |dir| {
            let dir1 = dir.join("dir1");
            std::fs::create_dir_all(&dir1)?;
            std::fs::write(dir1.join("a.rs"), "a")?; // 1 byte
            Ok(())
        },
        query: "(parentdir: &: sum(size:)) + (extension: &: sum(size:))",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert_eq!(res.results.len(), 1, "1 merged group");
            let group = &res.results[0];
            assert!(group.name.contains("rs"), "key has rs: {}", group.name);
            assert_eq!(get_nvalue_f64(group), Some(2.0), "1+1=2");
            Ok(())
        },
    },
    // ── unnest クエリ（scope_with_sum / scope_with_count） ────────
    NestTestCase {
        name: "unnest_sum_basic",
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
        query: "sum(parentdir: &: size:)",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some(), "projection");
            let dir1_r = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            let dir2_r = res.results.iter().find(|r| r.name.contains("dir2")).expect("dir2");
            assert_eq!(get_nvalue_f64(dir1_r), Some(17.0), "dir1 sum=17");
            assert_eq!(get_nvalue_f64(dir2_r), Some(5.0), "dir2 sum=5");
            Ok(())
        },
    },
    NestTestCase {
        name: "unnest_count_basic",
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
        query: "count(parentdir: &: extension:)",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_some(), "projection");
            let dir1_r = res.results.iter().find(|r| r.name.contains("dir1")).expect("dir1");
            let dir2_r = res.results.iter().find(|r| r.name.contains("dir2")).expect("dir2");
            assert_eq!(get_nvalue_f64(dir1_r), Some(2.0), "dir1 count=2");
            assert_eq!(get_nvalue_f64(dir2_r), Some(1.0), "dir2 count=1");
            Ok(())
        },
    },
    NestTestCase {
        name: "unnest_deep",
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
        query: "sum(parentdir: &: extension: &: size:)",
        format_query: inject_path_scope,
        modify: None,
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
    },
*/
    NestTestCase {
        name: "unnest_regression_plain_agg",
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "content")?;    // 7
            std::fs::write(dir.join("b.rs"), "0123456789")?; // 10
            Ok(())
        },
        query: "sum(extension:rs & size:)",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none(), "scalar");
            assert_eq!(res.results[0].name, "17", "sum=17");
            Ok(())
        },
    },
/*
    NestTestCase {
        name: "unnest_regression_nvalue_agg",
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
        query: "sum(parentdir: &: count())",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none(), "scalar");
            let val: f64 = res.results[0].name.parse().expect("numeric");
            assert_eq!(val, 6.0, "sum of counts=6");
            Ok(())
        },
    },
    NestTestCase {
        name: "unnest_depth4_to_3",
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
        query: "sum(parentdir: &: extension: &: filename: &: size:)",
        format_query: inject_path_scope,
        modify: None,
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
    NestTestCase {
        name: "unnest_multistage_4_to_0",
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
        query: "sum(sum(parentdir: &: extension: &: size:))",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none(), "scalar");
            let val: f64 = res.results[0].name.parse().expect("numeric");
            assert_eq!(val, 22.0, "22.0, got {}", val);
            Ok(())
        },
    },
    NestTestCase {
        name: "unnest_multistage_3_to_0",
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
        query: "sum(count(parentdir: &: extension:))",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none(), "scalar");
            let val: f64 = res.results[0].name.parse().expect("numeric");
            assert_eq!(val, 3.0, "3.0, got {}", val);
            Ok(())
        },
    },
    NestTestCase {
        name: "unnest_multistage_with_context",
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "12345")?;            // 5
            std::fs::write(dir.join("b.txt"), "123456789012345")?; // 15
            Ok(())
        },
        query: "count(extension: &: parentdir: &: (sum(size:) > 10))",
        format_query: inject_path_scope,
        modify: None,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none(), "scalar");
            let val: f64 = res.results[0].name.parse().expect("numeric");
            assert_eq!(val, 1.0, "1 valid group, got {}", val);
            Ok(())
        },
    },
*/
    NestTestCase {
        name: "level3_arithmetic_add",
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
        query: "parentdir: &: (size: + 1)",
        format_query: default_scope,
        modify: None,
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
    NestTestCase {
        name: "level3_arithmetic_mul_size",
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
        query: "parentdir: &: (size: * 2)",
        format_query: default_scope,
        modify: None,
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
    NestTestCase {
        name: "level3_arithmetic_width_height",
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
        query: "parentdir: &: (width: * height:)",
        format_query: default_scope,
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
];

// ──────────────────────────────────────────────
// E2E ディスパッチャー
// ──────────────────────────────────────────────

#[rstest]
#[case::count_e2e("count_e2e")]
#[case::sum_e2e("sum_e2e")]
#[case::no_regression_plain_projection("no_regression_plain_projection")]
#[case::extension_left_count("extension_left_count")]
#[case::extension_left_sum_size("extension_left_sum_size")]
#[case::max_size("max_size")]
#[case::min_size("min_size")]
#[case::avg_size("avg_size")]
#[case::count_all("count_all")]
#[case::filename_left("filename_left")]
#[case::comparison_count_gt("comparison_count_gt")]
#[case::comparison_sum_gt("comparison_sum_gt")]
#[case::context_propagation("context_propagation")]
#[case::pick_filter("pick_filter")]
#[case::scenario_a("scenario_a")]
#[case::scenario_b("scenario_b")]
#[case::scenario_stem_wildcard("scenario_stem_wildcard")]
#[case::chained_comparison("chained_comparison")]
#[case::arithmetic_mul("arithmetic_mul")]
#[case::arithmetic_add("arithmetic_add")]
#[case::arithmetic_sub("arithmetic_sub")]
#[case::arithmetic_div("arithmetic_div")]
#[case::arithmetic_avg_sum("arithmetic_avg_sum")]
#[case::arithmetic_max_lit("arithmetic_max_lit")]
#[case::arithmetic_lit_min("arithmetic_lit_min")]
#[case::arithmetic_nested("arithmetic_nested")]
#[case::or_merged_projection("or_merged_projection")]
#[case::arithmetic_null_propagation("arithmetic_null_propagation")]
#[case::filter_empty_groups("filter_empty_groups")]
#[case::dedup_keys("dedup_keys")]
#[case::level3_projection("level3_projection")]
#[case::level3_projection_with_agg("level3_projection_with_agg")]
#[case::level3_projection_with_agg_filter("level3_projection_with_agg_filter")]
#[case::level4_nest("level4_nest")]
#[case::level3_agg_internal_filter("level3_agg_internal_filter")]
#[case::query_vs_calc_e2e("query_vs_calc_e2e")]
#[case::agg_over_nvalue_sum_count("agg_over_nvalue_sum_count")]
#[case::agg_over_nvalue_count("agg_over_nvalue_count")]
#[case::agg_over_nvalue_with_comparison("agg_over_nvalue_with_comparison")]
#[case::agg_over_nvalue_sum_with_comparison("agg_over_nvalue_sum_with_comparison")]
#[case::agg_calc_wrap("agg_calc_wrap")]
#[case::agg_sum_calc_wrap("agg_sum_calc_wrap")]
// #[case::mixed_key_calculation("mixed_key_calculation")]
// #[case::mixed_key_arithmetic_deepens_nest("mixed_key_arithmetic_deepens_nest")]
// #[case::unnest_sum_basic("unnest_sum_basic")]
// #[case::unnest_count_basic("unnest_count_basic")]
// #[case::unnest_deep("unnest_deep")]
// #[case::unnest_with_filter("unnest_with_filter")]
#[case::unnest_regression_plain_agg("unnest_regression_plain_agg")]
// #[case::unnest_regression_nvalue_agg("unnest_regression_nvalue_agg")]
// #[case::unnest_depth4_to_3("unnest_depth4_to_3")]
// #[case::unnest_multistage_4_to_0("unnest_multistage_4_to_0")]
// #[case::unnest_multistage_3_to_0("unnest_multistage_3_to_0")]
// #[case::unnest_multistage_with_context("unnest_multistage_with_context")]
#[case::level3_arithmetic_add("level3_arithmetic_add")]
#[case::level3_arithmetic_mul_size("level3_arithmetic_mul_size")]
#[case::level3_arithmetic_width_height("level3_arithmetic_width_height")]
fn test_nest_e2e(#[case] name: &'static str) -> anyhow::Result<()> {
    let fix = get_fixture();
    let fm = FileManager::new_with_db_dir(&fix.db_dir)?;
    let case = CASES.iter().find(|c| c.name == name).unwrap();
    let case_dir = fix.root.path().join(case.name);
    let query = (case.format_query)(case.query, &case_dir);
    let res = fm.search(&query, SearchOptions::default())?;
    (case.assert)(&res, &case_dir)
}

// ──────────────────────────────────────────────
// Phase 1: パース
// ──────────────────────────────────────────────

#[test]
fn test_nest_parse_basic() {
    let node = ttfm::query::parse("extension: &: parentdir:").unwrap();
    if let ttfm::query::QueryNode::Nest(nest) = &node {
        assert!(matches!(*nest.left, ttfm::query::QueryNode::Projection(_)), "left=Projection");
        assert!(matches!(*nest.right, ttfm::query::QueryNode::Projection(_)), "right=Projection");
    } else {
        panic!("Expected Nest, got {:?}", node);
    }
}

#[test]
fn test_nest_parse_chain() {
    let node = ttfm::query::parse("extension: &: parentdir: &: name:").unwrap();
    if let ttfm::query::QueryNode::Nest(outer) = &node {
        assert!(matches!(*outer.left, ttfm::query::QueryNode::Nest(_)), "left=Nest");
        assert!(matches!(*outer.right, ttfm::query::QueryNode::Projection(_)), "right=Projection");
    } else {
        panic!("Expected Nest, got {:?}", node);
    }
}

#[test]
fn test_nest_priority_over_and() {
    let node = ttfm::query::parse("extension: &: parentdir: & extension:rs").unwrap();
    assert!(matches!(node, ttfm::query::QueryNode::And(_)), "top-level And, got {:?}", node);
}

#[test]
fn test_nest_parse_with_aggregation() {
    let node = ttfm::query::parse("parentdir: &: count(extension:jpg)").unwrap();
    if let ttfm::query::QueryNode::Nest(nest) = &node {
        assert!(matches!(*nest.right, ttfm::query::QueryNode::Aggregation(_)), "right=Aggregation");
    } else {
        panic!("Expected Nest, got {:?}", node);
    }
}

// ──────────────────────────────────────────────
// Phase 2: 論理解決
// ──────────────────────────────────────────────

#[test]
fn test_nest_left_must_be_projection() {
    let result = ttfm::query::lens_resolver::Resolver::new("extension:rs &: name:");
    assert!(result.is_err(), "non-projection left should fail");
}

// ──────────────────────────────────────────────
// Phase 3: 物理解決
// ──────────────────────────────────────────────

#[test]
fn test_nest_resolves_to_projection_with_nvalue() {
    let resolver = ttfm::query::lens_resolver::Resolver::new("parentdir: &: count(extension:jpg)").unwrap();
    assert!(resolver.get_projection().is_some());
    assert!(resolver.get_nvalue().is_some());
}

#[test]
fn test_nest_resolves_sum_nvalue() {
    let resolver = ttfm::query::lens_resolver::Resolver::new("parentdir: &: sum(size:)").unwrap();
    assert!(resolver.get_projection().is_some());
    assert!(resolver.get_nvalue().is_some());
}

#[test]
fn test_plain_projection_no_nvalue() {
    let resolver = ttfm::query::lens_resolver::Resolver::new("extension:").unwrap();
    assert!(resolver.get_projection().is_some());
    assert!(resolver.get_nvalue().is_none(), "plain projection has no nvalue");
}

// ──────────────────────────────────────────────
// エラーケース
// ──────────────────────────────────────────────

#[test]
fn test_nest_error_typed_tag_left() {
    assert!(ttfm::query::lens_resolver::Resolver::new("extension:rs &: count(*:*)").is_err());
}

#[test]
fn test_nest_error_aggregation_left() {
    assert!(ttfm::query::lens_resolver::Resolver::new("count(*:*) &: extension:").is_err());
}

#[test]
fn test_nest_error_comparison_left() {
    assert!(ttfm::query::lens_resolver::Resolver::new("(size: > 100) &: extension:").is_err());
}

#[test]
fn test_nest_right_comparison_resolves() {
    let resolver = ttfm::query::lens_resolver::Resolver::new("parentdir: &: (count(extension:jpg) > 1)").unwrap();
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
        assert!(result.is_ok(), "'{}' should resolve: {}",
            query, result.err().map(|e| e.to_string()).unwrap_or_default());
        let resolver = result.unwrap();
        assert!(resolver.get_projection().is_some(), "'{}' has projection", query);
        assert!(resolver.get_nvalue().is_some(), "'{}' has nvalue", query);
    }
}

#[test]
fn test_nest_scalar_right_resolves() {
    let resolver = ttfm::query::lens_resolver::Resolver::new("parentdir: &: 100").unwrap();
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
        assert!(result.is_ok(), "'{}': {}", query, result.err().map(|e| e.to_string()).unwrap_or_default());
        let resolver = result.unwrap();
        assert!(resolver.get_projection().is_some(), "'{}' has projection", query);
        assert!(resolver.get_nvalue().is_some(), "'{}' has nvalue", query);
        assert!(resolver.get_nvalue_condition().is_some(), "'{}' has nvalue_condition", query);
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
        assert!(result.is_ok(), "'{}': {}", query, result.err().map(|e| e.to_string()).unwrap_or_default());
    }
}


// ──────────────────────────────────────────────
// RESTORED STANDALONE TESTS (DUE TO SHARING ENVIRONMENT ISSUES)
// ──────────────────────────────────────────────

#[test]
fn test_mixed_key_calculation() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;
    let db_dir_path = db_dir.path();

    // dir1 に rs ファイル2件、dir2 に rs ファイル1件
    // → count(parentdir:dir1)=2, count(parentdir:dir2)=1, count(extension:rs)=3
    let dir1 = root_path.join("dir1");
    std::fs::create_dir(&dir1)?;
    std::fs::write(dir1.join("a.rs"), "rust")?;
    std::fs::write(dir1.join("b.rs"), "rust")?;

    let dir2 = root_path.join("dir2");
    std::fs::create_dir(&dir2)?;
    std::fs::write(dir2.join("c.rs"), "rust")?;

    let fm = FileManager::new_with_db_dir(db_dir_path)?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    // (parentdir: &: count()) + (extension: &: count())
    // dir1 グループ: count(dir1)=2, count(rs)=3 → nvalue = 2+3 = 5
    // dir2 グループ: count(dir2)=1, count(rs)=3 → nvalue = 1+3 = 4
    let query = "(parentdir: &: count()) + (extension: &: count())";

    let results = fm.search(query, SearchOptions::default())?;

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

    let dir1_group = results
        .results
        .iter()
        .find(|r| r.name.contains("dir1") && r.name.contains("rs"))
        .expect("Should have (dir1, rs) group");
    assert_eq!(
        get_nvalue(dir1_group),
        5.0,
        "dir1 group: count(dir1)=2 + count(rs)=3 should be 5"
    );

    let dir2_group = results
        .results
        .iter()
        .find(|r| r.name.contains("dir2") && r.name.contains("rs"))
        .expect("Should have (dir2, rs) group");
    assert_eq!(
        get_nvalue(dir2_group),
        4.0,
        "dir2 group: count(dir2)=1 + count(rs)=3 should be 4"
    );

    Ok(())
}

#[test]
fn test_mixed_key_arithmetic_deepens_nest() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;
    let db_dir_path = db_dir.path();
    std::fs::create_dir_all(root_path.join("dir1"))?;
    std::fs::write(root_path.join("dir1/a.rs"), "a")?;
    let fm = FileManager::new_with_db_dir(db_dir_path)?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    // dir1/a.rs は "a" = 1byte: sum(size: dir1)=1, sum(size: rs)=1 → nvalue = 1+1 = 2
    let query = "(parentdir: &: sum(size:)) + (extension: &: sum(size:))";
    let res = fm.search(query, SearchOptions::default())?;

    // (dir1, rs) の 1グループ
    assert_eq!(res.results.len(), 1, "Should have 1 merged group");
    let group = &res.results[0];
    assert!(
        group.name.contains("rs"),
        "Group key should contain rs, got: {}",
        group.name
    );

    // nvalue = sum(size: dir1) + sum(size: rs) = 1 + 1 = 2
    let nvalue = group
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type().as_str() == "nvalue")
        .expect("Should have nvalue tag");
    let val = match nvalue.label.value() {
        ttfm::types::LabelValue::Double(d_bits) => f64::from_bits(d_bits),
        ttfm::types::LabelValue::Integer(i) => i as f64,
        _ => panic!("Unexpected nvalue type"),
    };
    assert_eq!(
        val, 2.0,
        "nvalue should be sum(size:dir1)+sum(size:rs)=1+1=2, got: {}",
        val
    );
    Ok(())
}

#[test]
fn test_unnest_sum_basic() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;

    // dir1: a.rs(7), b.rs(10) → sum=17
    // dir2: c.rs(5) → sum=5
    let dir1 = root_path.join("dir1");
    std::fs::create_dir(&dir1)?;
    std::fs::write(dir1.join("a.rs"), "content")?; // 7 bytes
    std::fs::write(dir1.join("b.rs"), "0123456789")?; // 10 bytes

    let dir2 = root_path.join("dir2");
    std::fs::create_dir(&dir2)?;
    std::fs::write(dir2.join("c.rs"), "abcde")?; // 5 bytes

    let fm = FileManager::new_with_db_dir(db_dir.path())?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    let res =
        fm.search("sum(parentdir: &: size:)", SearchOptions::default())?;

    // Projection結果（各parentdirがラベルとして表示される）
    assert!(
        res.type_for_projection.is_some(),
        "Should return projection result, not scalar"
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

    let dir1_group = res
        .results
        .iter()
        .find(|r| r.name.contains("dir1"))
        .expect("Should have dir1 group");
    assert_eq!(get_nvalue(dir1_group), 17.0);

    let dir2_group = res
        .results
        .iter()
        .find(|r| r.name.contains("dir2"))
        .expect("Should have dir2 group");
    assert_eq!(get_nvalue(dir2_group), 5.0);

    Ok(())
}

#[test]
fn test_unnest_count_basic() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;

    // dir1: a.rs, b.txt → extension count=2
    // dir2: c.rs → extension count=1
    let dir1 = root_path.join("dir1");
    std::fs::create_dir(&dir1)?;
    std::fs::write(dir1.join("a.rs"), "x")?;
    std::fs::write(dir1.join("b.txt"), "y")?;

    let dir2 = root_path.join("dir2");
    std::fs::create_dir(&dir2)?;
    std::fs::write(dir2.join("c.rs"), "z")?;

    let fm = FileManager::new_with_db_dir(db_dir.path())?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    let res =
        fm.search("count(parentdir: &: extension:)", SearchOptions::default())?;

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

    let dir1_group = res
        .results
        .iter()
        .find(|r| r.name.contains("dir1"))
        .expect("Should have dir1 group");
    assert_eq!(get_nvalue(dir1_group), 2.0);

    let dir2_group = res
        .results
        .iter()
        .find(|r| r.name.contains("dir2"))
        .expect("Should have dir2 group");
    assert_eq!(get_nvalue(dir2_group), 1.0);

    Ok(())
}

#[test]
fn test_unnest_deep() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;

    // dir1: a.rs(7), b.txt(10)
    // dir2: c.rs(5), d.txt(3)
    let dir1 = root_path.join("dir1");
    std::fs::create_dir(&dir1)?;
    std::fs::write(dir1.join("a.rs"), "content")?; // 7 bytes
    std::fs::write(dir1.join("b.txt"), "0123456789")?; // 10 bytes

    let dir2 = root_path.join("dir2");
    std::fs::create_dir(&dir2)?;
    std::fs::write(dir2.join("c.rs"), "abcde")?; // 5 bytes
    std::fs::write(dir2.join("d.txt"), "xyz")?; // 3 bytes

    let fm = FileManager::new_with_db_dir(db_dir.path())?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    let res = fm.search(
        "sum(parentdir: &: extension: &: size:)",
        SearchOptions::default(),
    )?;

    // Projection結果: (parentdir, extension) の組み合わせ4つ
    assert!(
        res.type_for_projection.is_some(),
        "Should return projection result"
    );
    assert_eq!(
        res.results.len(),
        4,
        "Should have 4 groups: {:?}",
        res.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );

    let find_group = |pdir: &str, ext: &str| -> f64 {
        let group = res
            .results
            .iter()
            .find(|r| r.name.contains(pdir) && r.name.contains(ext))
            .unwrap_or_else(|| panic!("Should find group {}/{}", pdir, ext));
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

    assert_eq!(find_group("dir1", "rs"), 7.0);
    assert_eq!(find_group("dir1", "txt"), 10.0);
    assert_eq!(find_group("dir2", "rs"), 5.0);
    assert_eq!(find_group("dir2", "txt"), 3.0);

    Ok(())
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

#[test]
fn test_unnest_regression_nvalue_agg() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;

    // dir1: 2 files, dir2: 1 file → sum of counts = 3
    let dir1 = root_path.join("dir1");
    std::fs::create_dir(&dir1)?;
    std::fs::write(dir1.join("a.rs"), "x")?;
    std::fs::write(dir1.join("b.rs"), "y")?;

    let dir2 = root_path.join("dir2");
    std::fs::create_dir(&dir2)?;
    std::fs::write(dir2.join("c.rs"), "z")?;

    let fm = FileManager::new_with_db_dir(db_dir.path())?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    let res =
        fm.search("sum(parentdir: &: count())", SearchOptions::default())?;

    // スカラー結果
    assert!(
        res.type_for_projection.is_none(),
        "Should return scalar result"
    );
    let val: f64 = res.results[0].name.parse().expect("Should be a number");
    assert_eq!(val, 6.0, "sum of per-parentdir counts should be 6");

    Ok(())
}

#[test]
fn test_unnest_depth4_to_3() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;

    // dir1/a.rs(7), dir1/b.rs(10), dir2/c.txt(5)
    let dir1 = root_path.join("dir1");
    std::fs::create_dir(&dir1)?;
    std::fs::write(dir1.join("a.rs"), "content")?; // 7 bytes
    std::fs::write(dir1.join("b.rs"), "0123456789")?; // 10 bytes

    let dir2 = root_path.join("dir2");
    std::fs::create_dir(&dir2)?;
    std::fs::write(dir2.join("c.txt"), "abcde")?; // 5 bytes

    let fm = FileManager::new_with_db_dir(db_dir.path())?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    // 4キー(parentdir, extension, filename, size) → unnest → 3キー + nvalue=sum(size)
    // 各(parentdir, extension, filename)グループには1ファイルしかないため nvalue=そのファイルのサイズ
    let res = fm.search(
        "sum(parentdir: &: extension: &: filename: &: size:)",
        SearchOptions::default(),
    )?;

    assert!(
        res.type_for_projection.is_some(),
        "Should return projection result"
    );
    assert_eq!(
        res.results.len(),
        3,
        "Should have 3 groups (dir1/rs/a.rs, dir1/rs/b.rs, dir2/txt/c.txt): {:?}",
        res.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );

    let find_group = |pdir: &str, ext: &str, fname: &str| -> f64 {
        let group = res
            .results
            .iter()
            .find(|r| {
                r.name.contains(pdir)
                    && r.name.contains(ext)
                    && r.name.contains(fname)
            })
            .unwrap_or_else(|| {
                panic!("Should find group {}/{}/{}", pdir, ext, fname)
            });
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

    assert_eq!(find_group("dir1", "rs", "a.rs"), 7.0);
    assert_eq!(find_group("dir1", "rs", "b.rs"), 10.0);
    assert_eq!(find_group("dir2", "txt", "c.txt"), 5.0);

    Ok(())
}

#[test]
fn test_unnest_multistage_4_to_0() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;

    // dir1: a.rs(7), b.rs(10) → (dir1, rs): sum(size)=17
    // dir2: c.rs(5)           → (dir2, rs): sum(size)=5
    // 全体の合計: 17 + 5 = 22
    let dir1 = root_path.join("dir1");
    std::fs::create_dir(&dir1)?;
    std::fs::write(dir1.join("a.rs"), "content")?;
    std::fs::write(dir1.join("b.rs"), "0123456789")?;

    let dir2 = root_path.join("dir2");
    std::fs::create_dir(&dir2)?;
    std::fs::write(dir2.join("c.rs"), "abcde")?;

    let fm = FileManager::new_with_db_dir(db_dir.path())?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    let res = fm.search(
        "sum(sum(parentdir: &: extension: &: size:))",
        SearchOptions::default(),
    )?;

    // スカラー結果
    assert!(
        res.type_for_projection.is_none(),
        "Should return scalar result"
    );
    let val: f64 = res.results[0].name.parse().expect("Should be a number");
    assert_eq!(val, 22.0, "sum of per-(parentdir,ext) sums should be 22");

    Ok(())
}

#[test]
fn test_unnest_multistage_3_to_0() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;

    // dir1: a.rs, b.txt → 2 extension types
    // dir2: c.rs         → 1 extension type
    // 合計: 2 + 1 = 3
    let dir1 = root_path.join("dir1");
    std::fs::create_dir(&dir1)?;
    std::fs::write(dir1.join("a.rs"), "x")?;
    std::fs::write(dir1.join("b.txt"), "y")?;

    let dir2 = root_path.join("dir2");
    std::fs::create_dir(&dir2)?;
    std::fs::write(dir2.join("c.rs"), "z")?;

    let fm = FileManager::new_with_db_dir(db_dir.path())?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    let res = fm.search(
        "sum(count(parentdir: &: extension:))",
        SearchOptions::default(),
    )?;

    // スカラー結果
    assert!(
        res.type_for_projection.is_none(),
        "Should return scalar result"
    );
    let val: f64 = res.results[0].name.parse().expect("Should be a number");
    assert_eq!(
        val, 3.0,
        "sum of per-parentdir extension counts should be 3"
    );

    Ok(())
}

#[test]
fn test_unnest_multistage_with_context() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;

    // a.rs: size 5 (sum < 10) -> false
    // b.txt: size 15 (sum > 10) -> true
    std::fs::write(root_path.join("a.rs"), "12345")?;
    std::fs::write(root_path.join("b.txt"), "123456789012345")?;

    let fm = FileManager::new_with_db_dir(db_dir.path())?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    let res = fm.search(
        "count(extension: &: parentdir: &: (sum(size:) > 10))",
        SearchOptions::default(),
    )?;

    // スカラー結果 (条件を満たすグループの数: 1)
    assert!(
        res.type_for_projection.is_none(),
        "Should return scalar result"
    );
    let val: f64 = res.results[0].name.parse().expect("Should be a number");
    assert_eq!(val, 1.0, "Should count 1 valid group");

    Ok(())
}
