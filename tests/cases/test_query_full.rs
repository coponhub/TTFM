use std::collections::HashSet;
use std::fs::File;
use tempfile::tempdir;
use ttfm::query::lens_resolver::Resolver;

use ttfm::types::TagType;
use ttfm::FileManager;

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

/// Binder Error 再現テスト:
/// Calculation nvalue を含む NestMatch の表示用SQL
/// (build_fetch_label_groups_sql) で HAVING が不正に適用され
/// Binder Error が発生する問題を検証する。
///
/// クエリ:
///   (((parentdir: &: count(extension:rs))
///     / (parentdir: &: count())) * 10) :> 5
///
/// 意味: parentdir ごとの .rs 比率 × 10 が 5 を超える
///       parentdir のアイテム一覧を返す
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

    // src/ : a.rs, b.rs, c.rs, d.txt → rs比率 3/4 = 0.75
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
            conn.execute(
                &format!(
                    "INSERT INTO oneview VALUES \
                     ({id}, 10, 'file', 'user', \
                      '{tag_type}', '{label_str}', \
                      NULL, NULL, {label_bool})"
                ),
                [],
            )
            .unwrap();
        }
    }

    // doc/ : e.rs, f.txt, g.txt, h.txt → rs比率 1/4 = 0.25
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
            conn.execute(
                &format!(
                    "INSERT INTO oneview VALUES \
                     ({id}, 5, 'file', 'user', \
                      '{tag_type}', '{label_str}', \
                      NULL, NULL, {label_bool})"
                ),
                [],
            )
            .unwrap();
        }
    }

    std::env::set_var("TTFM_DEBUG", "1");

    let q = r#"(((parentdir: &: count(extension:rs)) / (parentdir: &: count())) * 10) :> 5"#;
    let resolver = Resolver::new(q).unwrap();

    // 1. アイテム取得パスの検証
    //    (build_merged_nest_match_sql 経由)
    let fetcher = ttfm::query::fetcher::Fetcher::new(&resolver, &conn);
    let plan = fetcher.pick(None, None).unwrap();

    // src/ 配下の 4 アイテム (id 1-4) のみが返る
    // doc/ 配下 (id 5-8) は比率 0.25×10=2.5 < 5 で除外
    assert_eq!(
        plan.candidate_ids,
        vec![1, 2, 3, 4],
        "Only items from src/ (ratio 0.75*10=7.5 > 5) \
         should be picked"
    );

    // 2. 表示用パスの検証
    //    (build_fetch_label_groups_sql 経由 → Binder Error の再現箇所)
    let proj_type = resolver
        .get_projection()
        .expect("Should have projection type");
    let label_results = fetcher.fetch_label_groups(&proj_type, 100, 0).expect(
        "fetch_label_groups should not fail \
             with Binder Error",
    );

    // src/ のみが返る
    assert_eq!(
        label_results.len(),
        1,
        "Only 'src' group should appear (ratio > 0.5)"
    );
    assert_eq!(label_results[0].name, "src");
}

/// :> 0 の場合: rs ファイルが存在するディレクトリのアイテムが返る
/// (src/ と doc/ の両方に rs があるため、全8アイテムが返る)
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

    // src/ : a.rs, b.rs, c.rs, d.txt → rs比率 3/4 > 0
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
            conn.execute(
                &format!(
                    "INSERT INTO oneview VALUES \
                     ({id}, 10, 'file', 'user', \
                      '{tag_type}', '{label_str}', \
                      NULL, NULL, {label_bool})"
                ),
                [],
            )
            .unwrap();
        }
    }

    // doc/ : e.rs, f.txt, g.txt, h.txt → rs比率 1/4 > 0
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
            conn.execute(
                &format!(
                    "INSERT INTO oneview VALUES \
                     ({id}, 5, 'file', 'user', \
                      '{tag_type}', '{label_str}', \
                      NULL, NULL, {label_bool})"
                ),
                [],
            )
            .unwrap();
        }
    }

    std::env::set_var("TTFM_DEBUG", "1");

    let q = r#"((parentdir: &: count(extension:rs)) / (parentdir: &: count())) :> 0"#;
    let resolver = Resolver::new(q).unwrap();

    let fetcher = ttfm::query::fetcher::Fetcher::new(&resolver, &conn);
    let plan = fetcher.pick(None, None).unwrap();

    // 両ディレクトリとも rs 比率 > 0 なので全アイテムが返る
    assert_eq!(
        plan.candidate_ids,
        vec![1, 2, 3, 4, 5, 6, 7, 8],
        "All items should be picked (both dirs have rs ratio > 0)"
    );

    let proj_type = resolver
        .get_projection()
        .expect("Should have projection type");
    let label_results = fetcher.fetch_label_groups(&proj_type, 100, 0).expect(
        "fetch_label_groups should not fail \
             with Binder Error",
    );

    // doc と src の両方が返る
    assert_eq!(label_results.len(), 2);
    let names: Vec<&str> =
        label_results.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"src"));
    assert!(names.contains(&"doc"));
}

/// QUERY.md L77: 「ラベル比較に対しては、通常の演算と同様アイテムリストを返す」
/// Calculation nvalue 付き比較がアイテム一覧を返すことを検証する e2e テスト。
#[test]
fn test_calculation_nvalue_returns_items_e2e() -> anyhow::Result<()> {
    use ttfm::FileManager;

    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // src/ に rs 3件, txt 1件 → rs比率 3/4 = 0.75
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::write(src_dir.join("a.rs"), "a")?;
    std::fs::write(src_dir.join("b.rs"), "b")?;
    std::fs::write(src_dir.join("c.rs"), "c")?;
    std::fs::write(src_dir.join("d.txt"), "d")?;

    // doc/ に rs 1件, txt 3件 → rs比率 1/4 = 0.25
    let doc_dir = root.join("doc");
    std::fs::create_dir_all(&doc_dir)?;
    std::fs::write(doc_dir.join("e.rs"), "e")?;
    std::fs::write(doc_dir.join("f.txt"), "f")?;
    std::fs::write(doc_dir.join("g.txt"), "g")?;
    std::fs::write(doc_dir.join("h.txt"), "h")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    std::env::set_var("TTFM_DEBUG", "1");

    let res = fm.search(
        r#"(((parentdir: &: count(extension:rs)) / (parentdir: &: count())) * 10) :> 5"#,
        Default::default(),
    )?;

    // QUERY.md L77: ラベル比較はアイテムリストを返す
    assert!(
        res.type_for_projection.is_none(),
        "Should return items, not projection. Got projection: {:?}",
        res.type_for_projection
    );

    // src/ 配下のアイテムのみが返る (rs比率 0.75*10=7.5 > 5)
    // doc/ は rs比率 0.25*10=2.5 で除外
    assert!(!res.results.is_empty(), "Should have items from src/");
    for item in &res.results {
        assert!(
            !item.name.contains("doc"),
            "doc/ items should be excluded, but got: {}",
            item.name
        );
    }

    Ok(())
}

/// extension: は is_dir:false 付きで展開されるため、
/// Calculation のオペランドに Query(And(...)) が残る問題を検証
#[test]
fn test_expanded_projection_calculation_e2e() -> anyhow::Result<()> {
    use ttfm::FileManager;

    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // rs 3件, txt 1件
    std::fs::write(root.join("a.rs"), "a")?;
    std::fs::write(root.join("b.rs"), "b")?;
    std::fs::write(root.join("c.rs"), "c")?;
    std::fs::write(root.join("d.txt"), "d")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // extension ごとの rs 個数 * 10 > 0
    // rs ファイル: extension は "rs" で、rs ファイルは3つ → 3 * 10 = 30 > 0 ✓
    // txt ファイル: extension は "txt" で、rs ファイルは0こ → 0 * 10 = 0 ✗
    let res = fm.search(
        r#"((extension: &: count(extension:rs)) * 10) :> 0"#,
        Default::default(),
    )?;

    // エラーなしで正常なアイテム群が返る
    assert!(!res.results.is_empty(), "Should return items without error");

    // rs を含む extension (つまり rs や txt 以外の rs ファイル自体)
    // このテストデータでは a.rs, b.rs, c.rs の extension は "rs"
    // "rs" extension グループには a.rs, b.rs, c.rs が属し、count(extension:rs) は 3 > 0
    // d.txt の extension は "txt" で "txt" グループには d.txt のみ属し、count(extension:rs) は 0
    let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();

    // extension: はアイテム一覧を返すため、条件を満たした extension ("rs") に属するアイテム一覧が返る
    assert!(
        names.contains(&"a.rs")
            && names.contains(&"b.rs")
            && names.contains(&"c.rs"),
        "rs files should be included. Got: {:?}",
        names
    );
    assert!(
        !names.contains(&"d.txt"),
        "txt files should be excluded. Got: {:?}",
        names
    );

    Ok(())
}

#[test]
fn test_expanded_projection_complex_division_e2e() -> anyhow::Result<()> {
    let db_dir = tempdir()?;
    let root = tempdir()?;

    // a.rs, b.rs, c.rs (extension: rs) -> 3 files
    // d.html, e.html (extension: html) -> 2 files
    for name in &["a.rs", "b.rs", "c.rs", "d.html", "e.html"] {
        File::create(root.path().join(name))?;
    }

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let query = r#"((extension: &: count(extension:rs)) / (extension: &: count(extension:html))) :> 0"#;

    // We expect it to NOT fail (it should parse, bind, and execute).
    let res = fm.search(query, Default::default());

    assert!(
        res.is_ok(),
        "Complex calculation with expanded projection shouldn't fail: {:?}",
        res.err()
    );

    Ok(())
}

/// 3要素の Calculation: (A * B + C) :> 0 で nvalue サブクエリの再帰的な
/// calc_sub 包装が正しく機能することを検証
#[test]
fn test_expanded_projection_three_operand_calc_e2e() -> anyhow::Result<()> {
    let db_dir = tempdir()?;
    let root = tempdir()?;

    for name in &["a.rs", "b.rs", "c.rs", "d.html", "e.html"] {
        File::create(root.path().join(name))?;
    }

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 3要素: (count(rs) * 10 + count(html)) :> 0
    // rs グループ: (3 * 10 + 2) = 32 > 0 ✓
    // html グループ: (0 * 10 + 2) = 2 > 0 ✓
    let query = r#"((extension: &: count(extension:rs)) * 10 + (extension: &: count(extension:html))) :> 0"#;

    let res = fm.search(query, Default::default());
    assert!(
        res.is_ok(),
        "Three-operand calculation with expanded projection shouldn't fail: {:?}",
        res.err()
    );

    let results = res.unwrap();
    assert!(
        !results.results.is_empty(),
        "Should return results for three-operand calculation"
    );

    Ok(())
}
