use std::fs::File;
use ttfm::query::lens_resolver::Resolver;

// ──────────────────────────────────────────────
// define_cases! 移行済み (FileManager e2e テスト)
// ──────────────────────────────────────────────

define_cases! {
    calc_nvalue_returns_items: {
        setup: |dir| {
            // src/ に rs 3件, txt 1件 → rs比率 0.75
            let src_dir = dir.join("src");
            std::fs::create_dir_all(&src_dir)?;
            std::fs::write(src_dir.join("a.rs"), "a")?;
            std::fs::write(src_dir.join("b.rs"), "b")?;
            std::fs::write(src_dir.join("c.rs"), "c")?;
            std::fs::write(src_dir.join("d.txt"), "d")?;
            // doc/ に rs 1件, txt 3件 → rs比率 0.25
            let doc_dir = dir.join("doc");
            std::fs::create_dir_all(&doc_dir)?;
            std::fs::write(doc_dir.join("e.rs"), "e")?;
            std::fs::write(doc_dir.join("f.txt"), "f")?;
            std::fs::write(doc_dir.join("g.txt"), "g")?;
            std::fs::write(doc_dir.join("h.txt"), "h")?;
            Ok(())
        },
        modify: None,
        format_query: super::default_scope,
        query: r#"(((parentdir: &: count(extension:rs)) / (parentdir: &: count())) * 10) :> 5"#,
        assert: |res, _dir| {
            assert!(res.type_for_projection.is_none(), "Should return items, not projection. Got: {:?}", res.type_for_projection);
            assert!(!res.results.is_empty(), "Should have items from src/");
            for item in &res.results {
                assert!(!item.name.contains("doc"), "doc/ items should be excluded, but got: {}", item.name);
            }
            Ok(())
        },
    },
    expanded_projection_calc: {
        setup: |dir| {
            std::fs::write(dir.join("a.rs"), "a")?;
            std::fs::write(dir.join("b.rs"), "b")?;
            std::fs::write(dir.join("c.rs"), "c")?;
            std::fs::write(dir.join("d.txt"), "d")?;
            Ok(())
        },
        modify: None,
        format_query: super::default_scope,
        query: r#"((extension: &: count(extension:rs)) * 10) :> 0"#,
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Should return items without error");
            let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();
            assert!(names.contains(&"a.rs") && names.contains(&"b.rs") && names.contains(&"c.rs"),
                "rs files should be included. Got: {:?}", names);
            assert!(!names.contains(&"d.txt"), "txt files should be excluded. Got: {:?}", names);
            Ok(())
        },
    },
    expanded_projection_complex_division: {
        setup: |dir| {
            for name in &["a.rs", "b.rs", "c.rs", "d.html", "e.html"] {
                File::create(dir.join(name))?;
            }
            Ok(())
        },
        modify: None,
        format_query: super::default_scope,
        query: r#"((extension: &: count(extension:rs)) / (extension: &: count(extension:html))) :> 0"#,
        assert: |res, _dir| {
            let _ = res;
            Ok(())
        },
    },
    expanded_projection_three_operand_calc: {
        setup: |dir| {
            for name in &["a.rs", "b.rs", "c.rs", "d.html", "e.html"] {
                File::create(dir.join(name))?;
            }
            Ok(())
        },
        modify: None,
        format_query: super::default_scope,
        query: r#"((extension: &: count(extension:rs)) * 10 + (extension: &: count(extension:html))) :> 0"#,
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Should return results for three-operand calculation");
            Ok(())
        },
    },
}

// ──────────────────────────────────────────────
// 移行不可: Resolver のみ / DuckDB 直接操作
// ──────────────────────────────────────────────

#[test]
fn test_full_resolution() {
    let q = r#"((parentdir: &: count(extension:rs)) / (parentdir: &: count())) :> 1"#;
    match Resolver::new(q) {
        Ok(resolver) => {
            eprintln!("SUCCESS: {:?}", resolver.get_projection())
        }
        Err(e) => panic!("Resolution failed: {}", e),
    }
}

#[test]
fn test_calculation_nvalue_label_groups() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE oneview (
            item_id BIGINT, rank BIGINT,
            item_kind TEXT, origin TEXT, type TEXT,
            label_str TEXT, label_int BIGINT,
            label_double DOUBLE, label_bool BOOLEAN
        )",
        [],
    )
    .unwrap();

    for (id, name, ext) in [
        (1, "a.rs", "rs"),
        (2, "b.rs", "rs"),
        (3, "c.rs", "rs"),
        (4, "d.txt", "txt"),
    ] {
        for (tag_type, label_str, label_bool) in [
            ("parentdir", "src", "NULL"),
            ("extension", ext, "NULL"),
            ("name", name, "NULL"),
            ("is_dir", "false", "FALSE"),
        ] {
            conn.execute(&format!("INSERT INTO oneview VALUES ({id}, 10, 'file', 'user', '{tag_type}', '{label_str}', NULL, NULL, {label_bool})"), []).unwrap();
        }
    }

    for (id, name, ext) in [
        (5, "e.rs", "rs"),
        (6, "f.txt", "txt"),
        (7, "g.txt", "txt"),
        (8, "h.txt", "txt"),
    ] {
        for (tag_type, label_str, label_bool) in [
            ("parentdir", "doc", "NULL"),
            ("extension", ext, "NULL"),
            ("name", name, "NULL"),
            ("is_dir", "false", "FALSE"),
        ] {
            conn.execute(&format!("INSERT INTO oneview VALUES ({id}, 5, 'file', 'user', '{tag_type}', '{label_str}', NULL, NULL, {label_bool})"), []).unwrap();
        }
    }

    std::env::set_var("TTFM_DEBUG", "1");

    let q = r#"(((parentdir: &: count(extension:rs)) / (parentdir: &: count())) * 10) :> 5"#;
    let resolver = Resolver::new(q).unwrap();
    let fetcher = ttfm::query::fetcher::Fetcher::new(&resolver, &conn);
    let plan = fetcher.pick(None, None).unwrap();
    assert_eq!(
        plan.candidate_ids,
        vec![1, 2, 3, 4],
        "Only items from src/ should be picked"
    );

    let proj_type = resolver
        .get_projection()
        .expect("Should have projection type");
    let label_results = fetcher
        .fetch_label_groups(&proj_type, 100, 0)
        .expect("fetch_label_groups should not fail");
    assert_eq!(label_results.len(), 1, "Only 'src' group should appear");
    assert_eq!(label_results[0].name, "src");
}

#[test]
fn test_calculation_nvalue_gt_zero() {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE oneview (
            item_id BIGINT, rank BIGINT,
            item_kind TEXT, origin TEXT, type TEXT,
            label_str TEXT, label_int BIGINT,
            label_double DOUBLE, label_bool BOOLEAN
        )",
        [],
    )
    .unwrap();

    for (id, name, ext) in [
        (1, "a.rs", "rs"),
        (2, "b.rs", "rs"),
        (3, "c.rs", "rs"),
        (4, "d.txt", "txt"),
    ] {
        for (tag_type, label_str, label_bool) in [
            ("parentdir", "src", "NULL"),
            ("extension", ext, "NULL"),
            ("name", name, "NULL"),
            ("is_dir", "false", "FALSE"),
        ] {
            conn.execute(&format!("INSERT INTO oneview VALUES ({id}, 10, 'file', 'user', '{tag_type}', '{label_str}', NULL, NULL, {label_bool})"), []).unwrap();
        }
    }

    for (id, name, ext) in [
        (5, "e.rs", "rs"),
        (6, "f.txt", "txt"),
        (7, "g.txt", "txt"),
        (8, "h.txt", "txt"),
    ] {
        for (tag_type, label_str, label_bool) in [
            ("parentdir", "doc", "NULL"),
            ("extension", ext, "NULL"),
            ("name", name, "NULL"),
            ("is_dir", "false", "FALSE"),
        ] {
            conn.execute(&format!("INSERT INTO oneview VALUES ({id}, 5, 'file', 'user', '{tag_type}', '{label_str}', NULL, NULL, {label_bool})"), []).unwrap();
        }
    }

    std::env::set_var("TTFM_DEBUG", "1");

    let q = r#"((parentdir: &: count(extension:rs)) / (parentdir: &: count())) :> 0"#;
    let resolver = Resolver::new(q).unwrap();
    let fetcher = ttfm::query::fetcher::Fetcher::new(&resolver, &conn);
    let plan = fetcher.pick(None, None).unwrap();
    assert_eq!(
        plan.candidate_ids,
        vec![1, 2, 3, 4, 5, 6, 7, 8],
        "All items should be picked"
    );

    let proj_type = resolver
        .get_projection()
        .expect("Should have projection type");
    let label_results = fetcher
        .fetch_label_groups(&proj_type, 100, 0)
        .expect("fetch_label_groups should not fail");
    assert_eq!(label_results.len(), 2);
    let names: Vec<&str> =
        label_results.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"src"));
    assert!(names.contains(&"doc"));
}
