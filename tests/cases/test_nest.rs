/// ネスト演算子 (`&:`) の統合テスト
///
/// テスト対象:
/// - Phase 1: パース — `&:` が正しくパースされる
/// - Phase 2: 論理解決 — 左辺検証、比較正規化
/// - Phase 3: 物理解決 — nvalue付きProjectionの生成
/// - Phase 4: SQL生成・Fetch — nvalue付きProjectionの検索結果
use tempfile::tempdir;
use ttfm::FileManager;

// ──────────────────────────────────────────────
// Phase 1: パース
// ──────────────────────────────────────────────

/// `&:` が文法エラーにならずパースできることを確認
#[test]
fn test_nest_parse_basic() {
    let node = ttfm::query::parse("extension: &: parentdir:").unwrap();
    // Nest(Projection(extension), Projection(parentdir))
    if let ttfm::query::QueryNode::Nest(nest) = &node {
        assert!(
            matches!(*nest.left, ttfm::query::QueryNode::Projection(_)),
            "left should be Projection"
        );
        assert!(
            matches!(*nest.right, ttfm::query::QueryNode::Projection(_)),
            "right should be Projection"
        );
    } else {
        panic!("Expected Nest node, got {:?}", node);
    }
}

/// チェーン: `a: &: b: &: c:` → Nest(Nest(a, b), c) (左結合)
#[test]
fn test_nest_parse_chain() {
    let node = ttfm::query::parse("extension: &: parentdir: &: name:").unwrap();
    if let ttfm::query::QueryNode::Nest(outer) = &node {
        assert!(
            matches!(*outer.left, ttfm::query::QueryNode::Nest(_)),
            "left should be Nest (chained)"
        );
        assert!(
            matches!(*outer.right, ttfm::query::QueryNode::Projection(_)),
            "right should be Projection"
        );
    } else {
        panic!("Expected Nest node, got {:?}", node);
    }
}

/// `&:` は `&` より優先度が高い: `a: &: b: & c:d` → And(Nest(a,b), c:d)
#[test]
fn test_nest_priority_over_and() {
    let node =
        ttfm::query::parse("extension: &: parentdir: & extension:rs").unwrap();
    assert!(
        matches!(node, ttfm::query::QueryNode::And(_)),
        "Top-level should be And, got {:?}",
        node
    );
}

/// Nest 右辺に集約: `proj: &: count(query)` がパースできる
#[test]
fn test_nest_parse_with_aggregation() {
    let node =
        ttfm::query::parse("parentdir: &: count(extension:jpg)").unwrap();
    if let ttfm::query::QueryNode::Nest(nest) = &node {
        assert!(
            matches!(*nest.right, ttfm::query::QueryNode::Aggregation(_)),
            "right should be Aggregation"
        );
    } else {
        panic!("Expected Nest node, got {:?}", node);
    }
}

// ──────────────────────────────────────────────
// Phase 2: 論理解決
// ──────────────────────────────────────────────

/// 左辺が Projection でも Nest でもない場合はエラーになることを確認
#[test]
fn test_nest_left_must_be_projection() {
    // extension:rs (TypedTag) は Projection ではないため失敗するはず
    let result =
        ttfm::query::lens_resolver::Resolver::new("extension:rs &: name:");
    assert!(result.is_err(), "Nest with non-projection left should fail");
}

// ──────────────────────────────────────────────
// Phase 3: 物理解決 (nvalue 付き Projection)
// ──────────────────────────────────────────────

/// `parentdir: &: count(extension:jpg)` が nvalue 付き Projection に解決される
#[test]
fn test_nest_resolves_to_projection_with_nvalue() {
    let resolver = ttfm::query::lens_resolver::Resolver::new(
        "parentdir: &: count(extension:jpg)",
    )
    .unwrap();

    // Projection として認識される
    assert!(
        resolver.get_projection().is_some(),
        "Nest depth-1 should resolve to Projection"
    );

    // nvalue が設定されている
    assert!(
        resolver.get_nvalue().is_some(),
        "Nest with aggregation right should have nvalue"
    );
}

/// `parentdir: &: sum(size:)` が nvalue 付き Projection に解決される
#[test]
fn test_nest_resolves_sum_nvalue() {
    let resolver =
        ttfm::query::lens_resolver::Resolver::new("parentdir: &: sum(size:)")
            .unwrap();

    assert!(resolver.get_projection().is_some());
    assert!(resolver.get_nvalue().is_some());
}

/// 通常の Projection には nvalue がない (リグレッション確認)
#[test]
fn test_plain_projection_no_nvalue() {
    let resolver =
        ttfm::query::lens_resolver::Resolver::new("extension:").unwrap();

    assert!(resolver.get_projection().is_some());
    assert!(
        resolver.get_nvalue().is_none(),
        "Plain projection should NOT have nvalue"
    );
}

// ──────────────────────────────────────────────
// Phase 4: E2E — nvalue 付き検索結果
// ──────────────────────────────────────────────

/// `parentdir: &: count(extension:jpg)` の E2E テスト
/// 実際にファイルをインデックスし、検索結果に nvalue タグが含まれることを確認
#[test]
fn test_nest_count_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // ディレクトリ構造を作成
    let src_dir = root.join("src");
    let docs_dir = root.join("docs");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::create_dir_all(&docs_dir)?;

    // src/ に jpg と png
    std::fs::write(src_dir.join("photo1.jpg"), "jpg1")?;
    std::fs::write(src_dir.join("photo2.jpg"), "jpg2")?;
    std::fs::write(src_dir.join("image.png"), "png1")?;

    // docs/ に jpg
    std::fs::write(docs_dir.join("scan.jpg"), "jpg3")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // parentdir: &: count(extension:jpg) で検索
    let res =
        fm.search("parentdir: &: count(extension:jpg)", Default::default())?;

    // Projection として認識されること
    assert!(
        res.type_for_projection.is_some(),
        "Should be treated as projection"
    );

    // 少なくとも2つの parentdir グループ (src, docs) があるはず
    // (root 直下にファイルがないため、src と docs のみ)
    let parentdir_names: Vec<&str> =
        res.results.iter().map(|r| r.name.as_str()).collect();
    println!("parentdir groups: {:?}", parentdir_names);

    // src と docs が含まれる (パスの形式は環境依存なので部分一致で確認)
    let has_src = res.results.iter().any(|r| r.name.contains("src"));
    let has_docs = res.results.iter().any(|r| r.name.contains("docs"));
    assert!(
        has_src,
        "Should find src group in results: {:?}",
        parentdir_names
    );
    assert!(
        has_docs,
        "Should find docs group in results: {:?}",
        parentdir_names
    );

    // 各グループに nvalue タグがあること
    for item in &res.results {
        if !item.name.contains("src") && !item.name.contains("docs") {
            continue; // テスト対象外のグループはスキップ
        }

        let nvalue_tag = item.tags.entries.iter().find(|e| {
            e.label.tag_type() == ttfm::types::TagType::from("nvalue")
        });
        assert!(
            nvalue_tag.is_some(),
            "Label '{}' should have nvalue tag. Tags: {:?}",
            item.name,
            item.tags
                .entries
                .iter()
                .map(|e| format!(
                    "{}:{}",
                    e.label.tag_type().as_str(),
                    e.label.as_str()
                ))
                .collect::<Vec<_>>()
        );
    }

    // src: jpg 2件
    let src_item = res.results.iter().find(|r| r.name.contains("src")).unwrap();
    let src_nv = src_item
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .unwrap();
    assert_eq!(
        src_nv.label.as_str(),
        "2",
        "src should have 2 jpg files, got '{}'",
        src_nv.label.as_str()
    );

    // docs: jpg 1件
    let docs_item = res
        .results
        .iter()
        .find(|r| r.name.contains("docs"))
        .unwrap();
    let docs_nv = docs_item
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .unwrap();
    assert_eq!(
        docs_nv.label.as_str(),
        "1",
        "docs should have 1 jpg file, got '{}'",
        docs_nv.label.as_str()
    );

    Ok(())
}

/// `parentdir: &: sum(size:)` の E2E テスト
/// 実際にファイルをインデックスし、各 parentdir グループのサイズ合計が nvalue に含まれることを確認
#[test]
fn test_nest_sum_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let sub = root.join("sub");
    std::fs::create_dir_all(&sub)?;

    // sub/ にサイズの異なるファイルを作成
    std::fs::write(sub.join("a.txt"), vec![0u8; 100])?;
    std::fs::write(sub.join("b.txt"), vec![0u8; 200])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // parentdir: &: sum(size:)
    let res = fm.search("parentdir: &: sum(size:)", Default::default())?;

    assert!(
        res.type_for_projection.is_some(),
        "Should be treated as projection"
    );

    // sub ディレクトリが結果に含まれること
    let sub_item = res.results.iter().find(|r| r.name.contains("sub"));
    assert!(
        sub_item.is_some(),
        "Should find sub group. Groups: {:?}",
        res.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );

    // nvalue タグが存在すること
    let sub_item = sub_item.unwrap();
    let nvalue_tag =
        sub_item.tags.entries.iter().find(|e| {
            e.label.tag_type() == ttfm::types::TagType::from("nvalue")
        });
    assert!(
        nvalue_tag.is_some(),
        "sub group should have nvalue tag for sum(size:)"
    );

    // nvalue が 300 (100 + 200) であること
    let nv = nvalue_tag.unwrap();
    assert_eq!(
        nv.label.as_str(),
        "300",
        "sub group sum(size:) should be 300 (100+200), got '{}'",
        nv.label.as_str()
    );

    Ok(())
}

/// 通常の Projection (nvalue なし) にリグレッションがないことを確認
#[test]
fn test_nest_no_regression_plain_projection() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("a.rs"), "")?;
    std::fs::write(root.join("b.txt"), "")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res = fm.search("extension:", Default::default())?;

    assert_eq!(
        res.type_for_projection,
        Some(ttfm::types::TagType::from("extension"))
    );
    assert!(res.results.iter().any(|r| r.name == "rs"));
    assert!(res.results.iter().any(|r| r.name == "txt"));

    // nvalue タグがどのラベルにも含まれていないこと
    for item in &res.results {
        let has_nvalue = item.tags.entries.iter().any(|e| {
            e.label.tag_type() == ttfm::types::TagType::from("nvalue")
        });
        assert!(
            !has_nvalue,
            "Plain projection label '{}' should NOT have nvalue tag",
            item.name
        );
    }

    Ok(())
}

// ──────────────────────────────────────────────
// 左辺が And に展開されるケース
// ──────────────────────────────────────────────

/// nvalue タグを取得するヘルパー
fn get_nvalue(item: &ttfm::SearchResult) -> Option<String> {
    item.tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string())
}

/// `extension: &: count(name:)` — extension: は And([is_dir:false, Proj]) に展開される
#[test]
fn test_nest_extension_left_count() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("a.rs"), "a")?;
    std::fs::write(root.join("b.rs"), "bb")?;
    std::fs::write(root.join("c.txt"), "ccc")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // extension: &: count(*:*) — 各拡張子グループのアイテム数
    let res = fm.search("extension: &: count(*:*)", Default::default())?;
    assert!(res.type_for_projection.is_some(), "Should be projection");

    let rs = res.results.iter().find(|r| r.name == "rs");
    assert!(
        rs.is_some(),
        "Should have 'rs' group. Got: {:?}",
        res.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );

    let rs_nv = get_nvalue(rs.unwrap());
    assert_eq!(rs_nv.as_deref(), Some("2"), "rs should have 2 items");

    let txt = res.results.iter().find(|r| r.name == "txt");
    assert!(txt.is_some(), "Should have 'txt' group");
    let txt_nv = get_nvalue(txt.unwrap());
    assert_eq!(txt_nv.as_deref(), Some("1"), "txt should have 1 item");

    Ok(())
}

/// `extension: &: sum(size:)` — 拡張子ごとのサイズ合計
#[test]
fn test_nest_extension_left_sum_size() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("a.rs"), vec![0u8; 100])?;
    std::fs::write(root.join("b.rs"), vec![0u8; 200])?;
    std::fs::write(root.join("c.txt"), vec![0u8; 50])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res = fm.search("extension: &: sum(size:)", Default::default())?;
    assert!(res.type_for_projection.is_some());

    let rs = res.results.iter().find(|r| r.name == "rs");
    assert!(rs.is_some(), "Should have 'rs' group");
    let rs_nv = get_nvalue(rs.unwrap());
    assert_eq!(
        rs_nv.as_deref(),
        Some("300"),
        "rs sum(size:) should be 300 (100+200)"
    );

    let txt = res.results.iter().find(|r| r.name == "txt");
    assert!(txt.is_some(), "Should have 'txt' group");
    let txt_nv = get_nvalue(txt.unwrap());
    assert_eq!(txt_nv.as_deref(), Some("50"), "txt sum(size:) should be 50");

    Ok(())
}

// ──────────────────────────────────────────────
// 全集約関数のカバー (max, min, avg)
// ──────────────────────────────────────────────

/// `parentdir: &: max(size:)` — max 集約
#[test]
fn test_nest_max_size() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let sub = root.join("sub");
    std::fs::create_dir_all(&sub)?;
    std::fs::write(sub.join("small.txt"), vec![0u8; 10])?;
    std::fs::write(sub.join("large.txt"), vec![0u8; 500])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res = fm.search("parentdir: &: max(size:)", Default::default())?;
    assert!(res.type_for_projection.is_some());

    let sub_item = res.results.iter().find(|r| r.name.contains("sub"));
    assert!(sub_item.is_some(), "Should have sub group");
    let nv = get_nvalue(sub_item.unwrap());
    assert_eq!(nv.as_deref(), Some("500"), "sub max(size:) should be 500");

    Ok(())
}

/// `parentdir: &: min(size:)` — min 集約
#[test]
fn test_nest_min_size() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let sub = root.join("sub");
    std::fs::create_dir_all(&sub)?;
    std::fs::write(sub.join("small.txt"), vec![0u8; 10])?;
    std::fs::write(sub.join("large.txt"), vec![0u8; 500])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res = fm.search("parentdir: &: min(size:)", Default::default())?;
    assert!(res.type_for_projection.is_some());

    let sub_item = res.results.iter().find(|r| r.name.contains("sub"));
    assert!(sub_item.is_some(), "Should have sub group");
    let nv = get_nvalue(sub_item.unwrap());
    assert_eq!(nv.as_deref(), Some("10"), "sub min(size:) should be 10");

    Ok(())
}

/// `parentdir: &: avg(size:)` — avg 集約
#[test]
fn test_nest_avg_size() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let sub = root.join("sub");
    std::fs::create_dir_all(&sub)?;
    std::fs::write(sub.join("a.txt"), vec![0u8; 100])?;
    std::fs::write(sub.join("b.txt"), vec![0u8; 200])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res = fm.search("parentdir: &: avg(size:)", Default::default())?;
    assert!(res.type_for_projection.is_some());

    let sub_item = res.results.iter().find(|r| r.name.contains("sub"));
    assert!(sub_item.is_some(), "Should have sub group");

    let nv_str = get_nvalue(sub_item.unwrap());
    assert!(nv_str.is_some(), "sub should have nvalue for avg");
    // avg(100, 200) = 150.0 (浮動小数点で返る可能性)
    let nv: f64 = nv_str.unwrap().parse().expect("nvalue should be numeric");
    assert!(
        (nv - 150.0).abs() < 1.0,
        "sub avg(size:) should be ~150, got {}",
        nv
    );

    Ok(())
}

// ──────────────────────────────────────────────
// count(*:*) — 全アイテムカウント
// ──────────────────────────────────────────────

/// `parentdir: &: count(*:*)` — 各 parentdir のアイテム総数
#[test]
fn test_nest_count_all() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let alpha = root.join("alpha");
    let beta = root.join("beta");
    std::fs::create_dir_all(&alpha)?;
    std::fs::create_dir_all(&beta)?;

    std::fs::write(alpha.join("x.txt"), "x")?;
    std::fs::write(alpha.join("y.txt"), "y")?;
    std::fs::write(alpha.join("z.txt"), "z")?;
    std::fs::write(beta.join("w.txt"), "w")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res = fm.search("parentdir: &: count(*:*)", Default::default())?;
    assert!(res.type_for_projection.is_some());

    let alpha_item = res.results.iter().find(|r| r.name.contains("alpha"));
    assert!(alpha_item.is_some(), "Should have alpha group");
    let nv = get_nvalue(alpha_item.unwrap());
    assert_eq!(nv.as_deref(), Some("3"), "alpha should have 3 items");

    let beta_item = res.results.iter().find(|r| r.name.contains("beta"));
    assert!(beta_item.is_some(), "Should have beta group");
    let nv = get_nvalue(beta_item.unwrap());
    assert_eq!(nv.as_deref(), Some("1"), "beta should have 1 item");

    Ok(())
}

// ──────────────────────────────────────────────
// filename: 左辺 — And に展開されるもう一つのパターン
// ──────────────────────────────────────────────

/// `filename: &: sum(size:)` — filename も is_dir:false フィルタ付きで展開される
#[test]
fn test_nest_filename_left() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("hello.txt"), vec![0u8; 100])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res = fm.search("filename: &: sum(size:)", Default::default())?;
    assert!(
        res.type_for_projection.is_some(),
        "filename: &: sum(size:) should be projection"
    );

    let hello = res.results.iter().find(|r| r.name == "hello.txt");
    assert!(
        hello.is_some(),
        "Should have 'hello.txt' group. Got: {:?}",
        res.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    let nv = get_nvalue(hello.unwrap());
    assert_eq!(
        nv.as_deref(),
        Some("100"),
        "hello.txt sum(size:) should be 100"
    );

    Ok(())
}

// ──────────────────────────────────────────────
// エラーケース
// ──────────────────────────────────────────────

/// 左辺が TypedTag (値指定あり) の場合はエラー
#[test]
fn test_nest_error_typed_tag_left() {
    let result =
        ttfm::query::lens_resolver::Resolver::new("extension:rs &: count(*:*)");
    assert!(result.is_err(), "TypedTag left should fail");
}

/// 左辺が Aggregation の場合はエラー
#[test]
fn test_nest_error_aggregation_left() {
    let result =
        ttfm::query::lens_resolver::Resolver::new("count(*:*) &: extension:");
    assert!(result.is_err(), "Aggregation left should fail");
}

/// 左辺が Comparison の場合はエラー
#[test]
fn test_nest_error_comparison_left() {
    let result = ttfm::query::lens_resolver::Resolver::new(
        "(size: > 100) &: extension:",
    );
    assert!(result.is_err(), "Comparison left should fail");
}

/// 右辺 Comparison: `parentdir: &: (count(ext:jpg) > 1)`
/// logical_resolver で分配され、nvalue 付き Projection として解決される。
#[test]
fn test_nest_right_comparison_resolves() {
    let resolver = ttfm::query::lens_resolver::Resolver::new(
        "parentdir: &: (count(extension:jpg) > 1)",
    )
    .expect("Nest with comparison should resolve");

    assert!(
        resolver.get_projection().is_some(),
        "Should return Projection"
    );
    assert!(resolver.get_nvalue().is_some(), "Should have nvalue");
    assert!(
        resolver.get_nvalue_condition().is_some(),
        "Should have nvalue_condition"
    );
}

// ──────────────────────────────────────────────
// 解決レベルのパターン確認
// ──────────────────────────────────────────────

/// 全集約関数で Resolver が正常に作成できること
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
            "Query '{}' should resolve successfully, got error: {}",
            query,
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );

        let resolver = result.unwrap();
        assert!(
            resolver.get_projection().is_some(),
            "Query '{}' should have projection",
            query
        );
        assert!(
            resolver.get_nvalue().is_some(),
            "Query '{}' should have nvalue",
            query
        );
    }
}

// ──────────────────────────────────────────────
// 右辺 Comparison (agg vs literal) の E2E テスト
// ──────────────────────────────────────────────

/// `parentdir: &: (count(extension:jpg) > 1)` — jpg が2件以上のグループのみ返す
#[test]
fn test_nest_comparison_count_gt_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // src/ に jpg 3件 → count > 1 を満たす
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::write(src_dir.join("a.jpg"), "a")?;
    std::fs::write(src_dir.join("b.jpg"), "b")?;
    std::fs::write(src_dir.join("c.jpg"), "c")?;

    // docs/ に jpg 1件 → count > 1 を満たさない
    let docs_dir = root.join("docs");
    std::fs::create_dir_all(&docs_dir)?;
    std::fs::write(docs_dir.join("d.jpg"), "d")?;
    std::fs::write(docs_dir.join("e.txt"), "e")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res = fm.search(
        "parentdir: &: (count(extension:jpg) > 1)",
        Default::default(),
    )?;

    // QUERY.md L77: ラベル比較はアイテムリストを返す
    assert!(
        res.type_for_projection.is_none(),
        "Should return items, not projection"
    );

    // src/ 配下のアイテム (a.jpg, b.jpg, c.jpg) が含まれる
    let names: Vec<_> = res.results.iter().map(|r| &r.name).collect();
    assert!(
        names
            .iter()
            .any(|n| *n == "a.jpg" || *n == "b.jpg" || *n == "c.jpg"),
        "src items should be included. Got: {:?}",
        names
    );

    // docs/ の d.jpg は除外 (count=1, not > 1)
    assert!(
        !names.iter().any(|n| *n == "d.jpg"),
        "docs items should be excluded (count=1). Got: {:?}",
        names
    );

    Ok(())
}

/// `extension: &: (sum(size:) > 100)` — サイズ合計が100超のグループのみ
#[test]
fn test_nest_comparison_sum_gt_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // rs: 50 + 60 = 110 → > 100 を満たす
    std::fs::write(root.join("a.rs"), vec![0u8; 50])?;
    std::fs::write(root.join("b.rs"), vec![0u8; 60])?;
    // txt: 30 → > 100 を満たさない
    std::fs::write(root.join("c.txt"), vec![0u8; 30])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res =
        fm.search("extension: &: (sum(size:) > 100)", Default::default())?;

    // QUERY.md L77: ラベル比較はアイテムリストを返す
    assert!(
        res.type_for_projection.is_none(),
        "Should return items, not projection"
    );

    let names: Vec<_> = res.results.iter().map(|r| &r.name).collect();
    // rs ファイル (a.rs, b.rs) は sum=110 > 100 → 含まれる
    assert!(
        names.iter().any(|n| *n == "a.rs" || *n == "b.rs"),
        "rs items (sum=110) should be included. Got: {:?}",
        names
    );

    // txt ファイル (c.txt) は sum=30 → 除外
    assert!(
        !names.iter().any(|n| *n == "c.txt"),
        "txt items (sum=30) should be excluded. Got: {:?}",
        names
    );

    Ok(())
}

// ──────────────────────────────────────────────
// 右辺 Scalar (Projection(Literal)) の解決テスト
// ──────────────────────────────────────────────

/// `parentdir: &: 100` — 右辺スカラーは nvalue: Some(Literal(100)) として解決される
#[test]
fn test_nest_scalar_right_resolves() {
    let resolver =
        ttfm::query::lens_resolver::Resolver::new("parentdir: &: 100")
            .expect("Nest with scalar right should resolve");

    assert!(
        resolver.get_projection().is_some(),
        "Should have projection"
    );
    assert!(
        resolver.get_nvalue().is_some(),
        "Scalar right should have nvalue"
    );
}

/// 右辺 Comparison の解決パターン確認（各集約関数）
/// Phase 4 まではエラーが期待値
#[test]
fn test_nest_agg_over_nvalue_sum_count_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // src/ に jpg 2件, docs/ に jpg 1件
    let src_dir = root.join("src");
    let docs_dir = root.join("docs");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::create_dir_all(&docs_dir)?;
    std::fs::write(src_dir.join("a.jpg"), "a")?;
    std::fs::write(src_dir.join("b.jpg"), "b")?;
    std::fs::write(docs_dir.join("c.jpg"), "c")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(parentdir: &: count(extension:jpg)) → 2 + 1 = 3
    let res = fm.search(
        "sum(parentdir: &: count(extension:jpg))",
        Default::default(),
    )?;

    // スカラー結果（Projectionではない）
    assert!(
        res.type_for_projection.is_none(),
        "Should be scalar, not projection"
    );
    assert_eq!(res.results.len(), 1, "Scalar should return 1 result");

    let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
    assert_eq!(val, 3.0, "sum of nvalues: 2 + 1 = 3");

    Ok(())
}

/// `count(parentdir: &: count(extension:jpg))` — nvalue付きProjectionのラベル数を数える
#[test]
fn test_nest_agg_over_nvalue_count_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // src/ に jpg 2件, docs/ に jpg 1件
    let src_dir = root.join("src");
    let docs_dir = root.join("docs");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::create_dir_all(&docs_dir)?;
    std::fs::write(src_dir.join("a.jpg"), "a")?;
    std::fs::write(src_dir.join("b.jpg"), "b")?;
    std::fs::write(docs_dir.join("c.jpg"), "c")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // count(parentdir: &: count(extension:jpg)) → 2グループ (src, docs)
    let res = fm.search(
        "count(parentdir: &: count(extension:jpg))",
        Default::default(),
    )?;

    assert!(
        res.type_for_projection.is_none(),
        "Should be scalar, not projection"
    );
    assert_eq!(res.results.len(), 1);

    let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
    assert_eq!(val, 2.0, "2 parentdir groups with jpg");

    Ok(())
}

/// `count(parentdir: &: (count(extension:jpg) > 1))` — nvalue比較付きProjectionに対する外側集約
/// src/ に jpg 3件 (count > 1 を満たす)、docs/ に jpg 1件 (満たさない) → 条件を満たすグループは1つ
#[test]
fn test_nest_agg_over_nvalue_with_comparison_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // src/ に jpg 3件 → count(ext:jpg) = 3 > 1 ✓
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::write(src_dir.join("a.jpg"), "a")?;
    std::fs::write(src_dir.join("b.jpg"), "b")?;
    std::fs::write(src_dir.join("c.jpg"), "c")?;

    // docs/ に jpg 1件 → count(ext:jpg) = 1, > 1 を満たさない ✗
    let docs_dir = root.join("docs");
    std::fs::create_dir_all(&docs_dir)?;
    std::fs::write(docs_dir.join("d.jpg"), "d")?;
    std::fs::write(docs_dir.join("e.txt"), "e")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // count(parentdir: &: (count(extension:jpg) > 1)) → 1 (src のみ)
    let res = fm.search(
        "count(parentdir: &: (count(extension:jpg) > 1))",
        Default::default(),
    )?;

    assert!(
        res.type_for_projection.is_none(),
        "Should be scalar, not projection"
    );
    assert_eq!(res.results.len(), 1, "Scalar should return 1 result");

    let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
    assert_eq!(
        val, 1.0,
        "Only src (count=3 > 1) should pass filter, so count = 1"
    );

    Ok(())
}

/// `sum(parentdir: &: (count(extension:jpg) > 1))` — nvalue比較付きProjectionのnvalueを合算
/// src/ に jpg 3件 (count > 1 ✓, nvalue=3)、docs/ に jpg 1件 (✗) → sum = 3
#[test]
fn test_nest_agg_over_nvalue_sum_with_comparison_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // src/ に jpg 3件 → count(ext:jpg) = 3 > 1 ✓
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::write(src_dir.join("a.jpg"), "a")?;
    std::fs::write(src_dir.join("b.jpg"), "b")?;
    std::fs::write(src_dir.join("c.jpg"), "c")?;

    // docs/ に jpg 1件 → count(ext:jpg) = 1, > 1 を満たさない ✗
    let docs_dir = root.join("docs");
    std::fs::create_dir_all(&docs_dir)?;
    std::fs::write(docs_dir.join("d.jpg"), "d")?;
    std::fs::write(docs_dir.join("e.txt"), "e")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(parentdir: &: (count(extension:jpg) > 1)) → 3 (src のみ, nvalue=3)
    let res = fm.search(
        "sum(parentdir: &: (count(extension:jpg) > 1))",
        Default::default(),
    )?;

    assert!(
        res.type_for_projection.is_none(),
        "Should be scalar, not projection"
    );
    assert_eq!(res.results.len(), 1);

    let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
    assert_eq!(
        val, 3.0,
        "Only src (count=3 > 1) passes, so sum of nvalues = 3"
    );

    Ok(())
}

/// Calculation が nvalue 条件付き集約をラップするケース。
/// ProjectionMatch が inner に埋め込まれた条件を正しく伝播するかを検証。
/// 例: `100 - count(parentdir: &: (count(extension:jpg) > 1))`
#[test]
fn test_nest_agg_calc_wrap_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // src/ に jpg 3件 → count(ext:jpg) = 3 > 1 ✓
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::write(src_dir.join("a.jpg"), "a")?;
    std::fs::write(src_dir.join("b.jpg"), "b")?;
    std::fs::write(src_dir.join("c.jpg"), "c")?;

    // docs/ に jpg 1件 → count(ext:jpg) = 1, > 1 を満たさない ✗
    let docs_dir = root.join("docs");
    std::fs::create_dir_all(&docs_dir)?;
    std::fs::write(docs_dir.join("d.jpg"), "d")?;
    std::fs::write(docs_dir.join("e.txt"), "e")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // count(parentdir: &: (count(extension:jpg) > 1)) → 1 (src のみ)
    // 100 - 1 = 99
    let res = fm.search(
        "100 - count(parentdir: &: (count(extension:jpg) > 1))",
        Default::default(),
    )?;

    assert!(
        res.type_for_projection.is_none(),
        "Should be scalar, not projection"
    );
    assert_eq!(res.results.len(), 1);

    let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
    assert_eq!(val, 99.0, "100 - count(matching dirs) = 100 - 1 = 99");

    Ok(())
}

/// sum() で nvalue 付き ProjectionMatch をラップし、
/// さらに算術演算を行うケース。
/// 例: `sum(parentdir: &: (count(extension:jpg) > 1)) * 2`
#[test]
fn test_nest_agg_sum_calc_wrap_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // src/ に jpg 3件 → count(ext:jpg) = 3 > 1 ✓ → nvalue = 3
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::write(src_dir.join("a.jpg"), "a")?;
    std::fs::write(src_dir.join("b.jpg"), "b")?;
    std::fs::write(src_dir.join("c.jpg"), "c")?;

    // docs/ に jpg 1件 → count(ext:jpg) = 1, > 1 を満たさない ✗
    let docs_dir = root.join("docs");
    std::fs::create_dir_all(&docs_dir)?;
    std::fs::write(docs_dir.join("d.jpg"), "d")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(parentdir: &: (count(extension:jpg) > 1)) → 3 (src のみ)
    // 3 * 2 = 6
    let res = fm.search(
        "sum(parentdir: &: (count(extension:jpg) > 1)) * 2",
        Default::default(),
    )?;

    assert!(
        res.type_for_projection.is_none(),
        "Should be scalar, not projection"
    );
    assert_eq!(res.results.len(), 1);

    let val: f64 = res.results[0].name.parse().unwrap_or(-1.0);
    assert_eq!(val, 6.0, "sum(nvalues of matching dirs) * 2 = 3 * 2 = 6");

    Ok(())
}

/// 右辺 Comparison の解決パターン確認（各集約関数）
/// Phase 4 まではエラーが期待値
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
            "Query '{}' should resolve, got: {}",
            query,
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );

        let resolver = result.unwrap();
        assert!(
            resolver.get_projection().is_some(),
            "Query '{}' should have projection",
            query
        );
        assert!(
            resolver.get_nvalue().is_some(),
            "Query '{}' should have nvalue",
            query
        );
        assert!(
            resolver.get_nvalue_condition().is_some(),
            "Query '{}' should have nvalue_condition",
            query
        );
    }
}

#[test]
fn test_nest_context_propagation_repro() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // item 1: name:a, ext:html, size: 100
    std::fs::write(root.join("a.html"), vec![0u8; 100])?;
    // item 2: name:b, ext:html, size: 200
    std::fs::write(root.join("b.html"), vec![0u8; 200])?;
    // item 3: name:c, ext:txt, size: 50
    std::fs::write(root.join("c.txt"), vec![0u8; 50])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: stem:a & extension: &: sum(size:)
    // stem:a でフィルタされるため、item 1 のみが残り、結果は html(100) になるはず
    let res =
        fm.search("stem:a & extension: &: sum(size:)", Default::default())?;

    println!(
        "Results: {:?}",
        res.results
            .iter()
            .map(|r| (&r.name, &r.tags))
            .collect::<Vec<_>>()
    );

    let html = res.results.iter().find(|r| r.name == "html");
    assert!(html.is_some(), "Should have 'html' group");

    let nv = html
        .unwrap()
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string());

    assert_eq!(
        nv.as_deref(),
        Some("100"),
        "nvalue should be 100 (filtered by name:a), NOT 300"
    );

    // txt グループはフィルタされているはず
    let txt = res.results.iter().find(|r| r.name == "txt");
    assert!(txt.is_none(), "txt group should be filtered out by name:a");

    Ok(())
}

#[test]
fn test_nest_pick_filter_repro() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // dirA: 2 jpg files
    let dir_a = root.join("dirA");
    std::fs::create_dir(&dir_a)?;
    std::fs::write(dir_a.join("f1.jpg"), "1")?;
    std::fs::write(dir_a.join("f2.jpg"), "2")?;

    // dirB: 11 jpg files
    let dir_b = root.join("dirB");
    std::fs::create_dir(&dir_b)?;
    for i in 0..11 {
        std::fs::write(dir_b.join(format!("g{}.jpg", i)), "content")?;
    }

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: parentdir: &: (count(extension:jpg) > 10)
    // dirB のアイテムのみが返るはず (dirA は 2 < 10 なので除外)
    let res = fm.search(
        "parentdir: &: (count(extension:jpg) > 10)",
        Default::default(),
    )?;

    let item_names: Vec<String> =
        res.results.iter().map(|r| r.name.clone()).collect();
    println!("Item names: {:?}", item_names);

    // dirA のアイテム (f1.jpg, f2.jpg) が含まれていないことを確認
    assert!(
        !item_names.iter().any(|n| n == "f1.jpg" || n == "f2.jpg"),
        "Items from dirA should be filtered out. Got: {:?}",
        item_names
    );
    // dirB のアイテム (g0.jpg..g10.jpg) が含まれていることを確認
    assert!(
        item_names.iter().any(|n| n.starts_with('g')),
        "Items from dirB should be included. Got: {:?}",
        item_names
    );

    assert!(!item_names.is_empty(), "Should have items from dirB");

    Ok(())
}

#[test]
fn test_nest_scenario_a_context_propagation() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // dirA: html と jpg が両方ある -> 条件を満たす
    let dira = root.join("dirA");
    std::fs::create_dir(&dira)?;
    std::fs::write(dira.join("f1.html"), "html")?;
    std::fs::write(dira.join("f2.jpg"), "jpg")?;

    // dirB: html のみある -> jpg がないので条件(count > 0)を満たさない
    let dirb = root.join("dirB");
    std::fs::create_dir(&dirb)?;
    std::fs::write(dirb.join("f3.html"), "html")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: extension:html & parentdir: &: count(extension:html) > 0
    // extension:html フィルタにより、html ファイルを持つアイテムのみが集計・表示対象になる。
    let res = fm.search(
        "extension:html & parentdir: &: count(extension:html) > 0",
        Default::default(),
    )?;

    let item_names: Vec<_> = res.results.iter().map(|r| &r.name).collect();
    // dirA の html ファイルが含まれること
    assert!(
        item_names
            .iter()
            .any(|n| *n == "f1.html" || *n == "f2.html"),
        "Results should contain dirA html files, got: {:?}",
        item_names
    );
    // dirB の html ファイルが含まれること
    assert!(
        item_names
            .iter()
            .any(|n| *n == "apple.html" || *n == "f3.html"),
        "Results should contain dirB html files, got: {:?}",
        item_names
    );

    Ok(())
}

#[test]
fn test_nest_scenario_b_query_vs_query() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // dirA: 1ファイルのみ -> avg == sum
    let dira = root.join("dirA");
    std::fs::create_dir(&dira)?;
    std::fs::write(dira.join("f1.txt"), vec![0u8; 100])?;

    // dirB: 2ファイル -> avg != sum (150 != 300)
    let dirb = root.join("dirB");
    std::fs::create_dir(&dirb)?;
    std::fs::write(dirb.join("f2.txt"), vec![0u8; 100])?;
    std::fs::write(dirb.join("f3.txt"), vec![0u8; 200])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: parentdir: &: (avg(size:) == sum(size:))
    // avg == sum となるのは count=1 の場合のみ。
    let res = fm.search(
        "parentdir: &: (avg(size:) == sum(size:))",
        Default::default(),
    )?;

    let item_names: Vec<_> = res.results.iter().map(|r| &r.name).collect();
    assert!(
        item_names.iter().any(|n| n.contains("dirA")),
        "Results should contain dirA, got: {:?}",
        item_names
    );
    assert!(
        !item_names.iter().any(|n| n.contains("dirB")),
        "Results should NOT contain dirB, got: {:?}",
        item_names
    );

    Ok(())
}

#[test]
fn test_nest_scenario_stem_wildcard_context() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // dirA: html かつ stem に 'a' を含むものが2つ -> 条件一致
    let dira = root.join("dirA");
    std::fs::create_dir(&dira)?;
    std::fs::write(dira.join("apple.html"), "h")?; // html, stem:"apple" has 'a'
    std::fs::write(dira.join("banana.html"), "h")?; // html, stem:"banana" has 'a'
    std::fs::write(dira.join("cherry.jpg"), "j")?; // has 'a', but NOT html (context ensures it's excluded)

    // dirB: html かつ stem に 'a' を含むものが1つのみ
    let dirb = root.join("dirB");
    std::fs::create_dir(&dirb)?;
    std::fs::write(dirb.join("apple.html"), "h")?; // html, has 'a'
    std::fs::write(dirb.join("grape.txt"), "t")?; // has 'a', but NOT html
    std::fs::write(dirb.join("berry.html"), "h")?; // html, but NO 'a' ("berry")

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: extension:html & parentdir: &: count(stem:*a*) == 2
    // コンテキスト伝播により、count(stem:*a*) は実質的に count(extension:html & stem:*a*) として機能するはず。
    let res = fm.search(
        "extension:html & parentdir: &: count(stem:*a*) == 2",
        Default::default(),
    )?;

    let item_names: Vec<_> = res.results.iter().map(|r| &r.name).collect();
    // dirA は (apple.html, banana.html) の2つが条件に合うため含まれる。
    // dirA の html ファイルが含まれること (count(stem:*a*)=2: apple, banana)
    assert!(
        item_names
            .iter()
            .any(|n| n.as_str() == "apple.html" || n.as_str() == "banana.html"),
        "Results should contain dirA html files, got: {:?}",
        item_names
    );
    // dirA の html は apple.html, banana.html の2件
    // (cherry.jpg は extension:html コンテキストで除外)
    // dirB は count(stem:*a*)=1 なので除外
    assert_eq!(
        item_names.len(),
        2,
        "Only dirA's 2 html files should be returned, got: {:?}",
        item_names
    );

    Ok(())
}

// ──────────────────────────────────────────────
// 連鎖比較 (chained comparison) + Nest
// ──────────────────────────────────────────────

/// `parentdir: &: (200 > sum(size:) > 50)` — 範囲フィルタ
/// 連鎖比較が And に展開された後も、各 Comparison に Nest コンテキストが分配されることを検証
#[test]
fn test_nest_chained_comparison_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // dirA: sum(size:) = 60 + 40 = 100 → 50 < 100 < 200 ✓
    let dira = root.join("dirA");
    std::fs::create_dir_all(&dira)?;
    std::fs::write(dira.join("a.txt"), vec![0u8; 60])?;
    std::fs::write(dira.join("b.txt"), vec![0u8; 40])?;

    // dirB: sum(size:) = 150 + 200 = 350 → 350 > 200 ✗
    let dirb = root.join("dirB");
    std::fs::create_dir_all(&dirb)?;
    std::fs::write(dirb.join("c.txt"), vec![0u8; 150])?;
    std::fs::write(dirb.join("d.txt"), vec![0u8; 200])?;

    // dirC: sum(size:) = 10 + 20 = 30 → 30 < 50 ✗
    let dirc = root.join("dirC");
    std::fs::create_dir_all(&dirc)?;
    std::fs::write(dirc.join("e.txt"), vec![0u8; 10])?;
    std::fs::write(dirc.join("f.txt"), vec![0u8; 20])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res =
        fm.search("parentdir: &: (200 > sum(size:) > 50)", Default::default())?;

    // QUERY.md L77: ラベル比較はアイテムリストを返す
    assert!(
        res.type_for_projection.is_none(),
        "Should return items, not projection"
    );

    let names: Vec<_> = res.results.iter().map(|r| &r.name).collect();

    // dirA (sum=100): 50 < 100 < 200 → a.txt, b.txt が返る
    assert!(
        names.iter().any(|n| *n == "a.txt" || *n == "b.txt"),
        "dirA items (sum=100) should be included. Got: {:?}",
        names
    );

    // dirB (sum=350): 350 > 200 → c.txt, d.txt は除外
    assert!(
        !names.iter().any(|n| *n == "c.txt" || *n == "d.txt"),
        "dirB items (sum=350) should be excluded. Got: {:?}",
        names
    );

    // dirC (sum=30): 30 < 50 → e.txt, f.txt は除外
    assert!(
        !names.iter().any(|n| *n == "e.txt" || *n == "f.txt"),
        "dirC items (sum=30) should be excluded. Got: {:?}",
        names
    );

    Ok(())
}

#[test]
fn test_nest_arithmetic_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // dir1: 2 files, 10 bytes and 20 bytes (sum=30, count=2 -> nvalue=60)
    let dir1 = root.join("dir1");
    std::fs::create_dir(&dir1)?;
    std::fs::write(dir1.join("file1"), vec![0u8; 10])?;
    std::fs::write(dir1.join("file2"), vec![0u8; 20])?;

    // dir2: 1 file, 100 bytes (sum=100, count=1 -> nvalue=100)
    let dir2 = root.join("dir2");
    std::fs::create_dir(&dir2)?;
    std::fs::write(dir2.join("file3"), vec![0u8; 100])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: parentdir: &: (sum(size:) * count(size:))
    let res = fm.search(
        "parentdir: &: (sum(size:) * count(size:))",
        Default::default(),
    )?;

    let d1_res = res.results.iter().find(|r| r.name.contains("dir1"));
    assert!(d1_res.is_some(), "Should have 'dir1' group");
    let nv1 = d1_res
        .unwrap()
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string());
    assert_eq!(nv1.as_deref(), Some("60"), "dir1 (*) nvalue should be 60");

    let d2_res = res.results.iter().find(|r| r.name.contains("dir2"));
    assert!(d2_res.is_some(), "Should have 'dir2' group");
    let nv2 = d2_res
        .unwrap()
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string());
    assert_eq!(nv2.as_deref(), Some("100"), "dir2 (*) nvalue should be 100");

    // クエリ: parentdir: &: (sum(size:) + count(size:))
    let res = fm.search(
        "parentdir: &: (sum(size:) + count(size:))",
        Default::default(),
    )?;
    let nv1 = res
        .results
        .iter()
        .find(|r| r.name.contains("dir1"))
        .unwrap()
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string());
    assert_eq!(nv1.as_deref(), Some("32"), "dir1 (+) nvalue should be 32");

    // クエリ: parentdir: &: (sum(size:) - count(size:))
    let res = fm.search(
        "parentdir: &: (sum(size:) - count(size:))",
        Default::default(),
    )?;
    let nv1 = res
        .results
        .iter()
        .find(|r| r.name.contains("dir1"))
        .unwrap()
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string());
    assert_eq!(nv1.as_deref(), Some("28"), "dir1 (-) nvalue should be 28");

    // クエリ: parentdir: &: (sum(size:) / count(size:))
    let res = fm.search(
        "parentdir: &: (sum(size:) / count(size:))",
        Default::default(),
    )?;
    let nv1 = res
        .results
        .iter()
        .find(|r| r.name.contains("dir1"))
        .unwrap()
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string());
    assert_eq!(nv1.as_deref(), Some("15"), "dir1 (/) nvalue should be 15");

    // --- 検証範囲拡大 (Phase 4) ---

    // 1. 異なる集計の組み合わせ: avg(size:) + sum(size:)
    // dir1: avg=15, sum=30 => 45
    let res = fm.search(
        "parentdir: &: (avg(size:) + sum(size:))",
        Default::default(),
    )?;
    let nv1 = res
        .results
        .iter()
        .find(|r| r.name.contains("dir1"))
        .unwrap()
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string());
    assert_eq!(
        nv1.as_deref(),
        Some("45"),
        "dir1 (avg+sum) nvalue should be 45"
    );

    // 2. 集計 vs リテラル: max(size:) * 2
    // dir1: max=20 => 40
    let res =
        fm.search("parentdir: &: (max(size:) * 2)", Default::default())?;
    let nv1 = res
        .results
        .iter()
        .find(|r| r.name.contains("dir1"))
        .unwrap()
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string());
    assert_eq!(
        nv1.as_deref(),
        Some("40"),
        "dir1 (max*2) nvalue should be 40"
    );

    // 3. リテラル vs 集計: 1000 / min(size:)
    // dir1: min=10 => 100
    let res =
        fm.search("parentdir: &: (1000 / min(size:))", Default::default())?;
    let nv1 = res
        .results
        .iter()
        .find(|r| r.name.contains("dir1"))
        .unwrap()
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string());
    assert_eq!(
        nv1.as_deref(),
        Some("100"),
        "dir1 (1000/min) nvalue should be 100"
    );

    // 4. 入れ子になった算術演算: (sum(size:) + 10) * count(size:)
    // dir1: (30 + 10) * 2 = 80
    let res = fm.search(
        "parentdir: &: ((sum(size:) + 10) * count(size:))",
        Default::default(),
    )?;
    let nv1 = res
        .results
        .iter()
        .find(|r| r.name.contains("dir1"))
        .unwrap()
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string());
    assert_eq!(
        nv1.as_deref(),
        Some("80"),
        "dir1 (nested) nvalue should be 80"
    );

    Ok(())
}

// ──────────────────────────────────────────────
// バグ再現テスト（修正前に追加、修正後に pass する）
// ──────────────────────────────────────────────

/// Issue 1: OR演算子でMergedProjectionMatchが正しくユニオンを返すことを確認
///
/// バグ: `P &: C1 | P &: C2` がC1のみの結果を返す（ORが機能しない）
///
/// 期待値:
/// - dirA: count(extension:rs) = 1 > 0 ✓ → マッチ
/// - dirB: count(extension:rs) = 0, count(*:*) = 2 > 1 ✓ → マッチ
/// - dirC: count(*:*) = 1, どちらも不一致 → 除外
#[test]
fn test_nest_or_merged_projection_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // dirA: .rs file 1件 → count(ext:rs)=1 > 0 ✓, count(*:*)=1 (NOT > 1)
    let dira = root.join("dirA");
    std::fs::create_dir(&dira)?;
    std::fs::write(dira.join("main.rs"), vec![0u8; 10])?;

    // dirB: .txt files 2件 → count(ext:rs)=0 (NOT > 0), count(*:*)=2 > 1 ✓
    let dirb = root.join("dirB");
    std::fs::create_dir(&dirb)?;
    std::fs::write(dirb.join("a.txt"), vec![0u8; 20])?;
    std::fs::write(dirb.join("b.txt"), vec![0u8; 30])?;

    // dirC: .txt 1件 → どちらの条件にも一致しない
    let dirc = root.join("dirC");
    std::fs::create_dir(&dirc)?;
    std::fs::write(dirc.join("c.txt"), vec![0u8; 40])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res = fm.search(
        "parentdir: &: (count(extension:rs) > 0) | parentdir: &: (count(*:*) > 1)",
        Default::default(),
    )?;

    let names: Vec<_> = res.results.iter().map(|r| &r.name).collect();

    // dirA: main.rs が含まれる (count(rs)=1 > 0)
    assert!(
        names.iter().any(|n| *n == "main.rs"),
        "dirA items should be included (count(rs) > 0). Got: {:?}",
        names
    );
    // dirB: a.txt, b.txt が含まれる (count(*:*)=2 > 1)
    assert!(
        names.iter().any(|n| *n == "a.txt" || *n == "b.txt"),
        "dirB items should be included (count(*) > 1). Got: {:?}",
        names
    );
    // dirC: c.txt は除外
    assert!(
        !names.iter().any(|n| *n == "c.txt"),
        "dirC items should be excluded. Got: {:?}",
        names
    );

    Ok(())
}

/// Issue 2: 異なる集約次元の算術演算でNULL伝播が起きないことを確認
///
/// バグ: `parentdir: &: (sum(size:) + count(extension:rs))` において、
/// extension:rs を持たないディレクトリで count(extension:rs) が 0 でなく NULL を返し、
/// sum(size:) + NULL = NULL となり nvalue が消える。
///
/// 期待値:
/// - dir_rs: sum(size:)=10, count(ext:rs)=1 → nvalue=11
/// - dir_txt: sum(size:)=50, count(ext:rs)=0 → nvalue=50 (0でなくNULLになるとバグ)
#[test]
fn test_nest_arithmetic_mixed_agg_null_propagation_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // dir_rs: .rs file 1件 10バイト → sum(size:)=10, count(ext:rs)=1 → nvalue=11
    let dir_rs = root.join("dir_rs");
    std::fs::create_dir(&dir_rs)?;
    std::fs::write(dir_rs.join("main.rs"), vec![0u8; 10])?;

    // dir_txt: .txt file 1件 50バイト → sum(size:)=50, count(ext:rs)=0 → nvalue=50
    let dir_txt = root.join("dir_txt");
    std::fs::create_dir(&dir_txt)?;
    std::fs::write(dir_txt.join("readme.txt"), vec![0u8; 50])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res = fm.search(
        "parentdir: &: (sum(size:) + count(extension:rs))",
        Default::default(),
    )?;

    let names: Vec<_> = res.results.iter().map(|r| &r.name).collect();

    // dir_rs: sum=10 + count_rs=1 = 11
    let rs_res = res.results.iter().find(|r| r.name.contains("dir_rs"));
    assert!(
        rs_res.is_some(),
        "dir_rs should appear in results. Got: {:?}",
        names
    );
    let nv_rs = rs_res
        .unwrap()
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string());
    assert_eq!(
        nv_rs.as_deref(),
        Some("11"),
        "dir_rs nvalue should be 11 (10+1)"
    );

    // dir_txt: sum=50 + count_rs=0 = 50
    // このケースでバグが発生: count(extension:rs) が NULL を返し nvalue が消える
    let txt_res = res.results.iter().find(|r| r.name.contains("dir_txt"));
    assert!(
        txt_res.is_some(),
        "dir_txt should appear in results (count(rs)=0, sum+0=50). Got: {:?}",
        names
    );
    let nv_txt = txt_res
        .unwrap()
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type() == ttfm::types::TagType::from("nvalue"))
        .map(|e| e.label.as_str().to_string());
    assert_eq!(
        nv_txt.as_deref(),
        Some("50"),
        "dir_txt nvalue should be 50 (50+0), but got None due to NULL propagation bug"
    );

    Ok(())
}
