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
/// logical_resolver で分配され、ラベル比較に変換される。
/// Phase 4 で nvalue 付き Projection の比較を実装するため、
/// 現段階ではエラーを返すことを確認。
#[test]
fn test_nest_right_comparison_resolves() {
    let result = ttfm::query::lens_resolver::Resolver::new(
        "parentdir: &: (count(extension:jpg) > 1)",
    );
    assert!(
        result.is_err(),
        "Comparison on nvalue-bearing Projection should error until Phase 4"
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
/// Phase 4 で nvalue 付き Projection への比較を実装後に有効化
#[test]
#[ignore = "Phase 4: comparison on nvalue-bearing Projection not yet implemented"]
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

    assert!(
        res.type_for_projection.is_some(),
        "Should be treated as projection"
    );

    // src は count(ext:jpg) = 3 > 1 → 含まれる
    let has_src = res.results.iter().any(|r| r.name.contains("src"));
    assert!(
        has_src,
        "src (3 jpg) should be included. Got: {:?}",
        res.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );

    // docs は count(ext:jpg) = 1 で > 1 を満たさない → 含まれない
    let has_docs = res.results.iter().any(|r| r.name.contains("docs"));
    assert!(
        !has_docs,
        "docs (1 jpg) should be excluded by HAVING count > 1"
    );

    // src の nvalue は 3
    let src_item = res.results.iter().find(|r| r.name.contains("src")).unwrap();
    let nv = get_nvalue(src_item);
    assert_eq!(nv.as_deref(), Some("3"), "src count(ext:jpg) should be 3");

    Ok(())
}

/// `extension: &: (sum(size:) > 100)` — サイズ合計が100超のグループのみ
/// Phase 4 で nvalue 付き Projection への比較を実装後に有効化
#[test]
#[ignore = "Phase 4: comparison on nvalue-bearing Projection not yet implemented"]
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

    assert!(res.type_for_projection.is_some());

    let has_rs = res.results.iter().any(|r| r.name == "rs");
    assert!(has_rs, "rs (sum=110) should be included");

    let has_txt = res.results.iter().any(|r| r.name == "txt");
    assert!(
        !has_txt,
        "txt (sum=30) should be excluded by HAVING sum > 100"
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
            result.is_err(),
            "Query '{}' should error until Phase 4 (comparison on nvalue-bearing Projection)",
            query
        );
    }
}
