/// ネスト演算子 (`&:`) の統合テスト
///
/// テスト対象:
/// - Phase 1: パース — `&:` が正しくパースされる
/// - Phase 2: 論理解決 — 左辺検証、比較正規化
/// - Phase 3: 物理解決 — nvalue付きProjectionの生成
/// - Phase 4: SQL生成・Fetch — nvalue付きProjectionの検索結果
use tempfile::tempdir;
use ttfm::FileManager;
use ttfm::SearchOptions;

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
/// NestMatch が inner に埋め込まれた条件を正しく伝播するかを検証。
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

/// sum() で nvalue 付き NestMatch をラップし、
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

    // Lv.1フラットリスト: type_for_projection は None
    assert!(
        res.type_for_projection.is_none(),
        "agg==agg comparison should return flat list (Lv.1), not projection"
    );

    // フラットリストではparentdir=dirA(path)のファイルが返る。
    // アイテムのタグから parentdir の値でdirA/dirBの所属を確認する。
    let result_parentdirs: Vec<String> = res
        .results
        .iter()
        .flat_map(|r| {
            r.tags
                .entries
                .iter()
                .filter(|e| e.label.tag_type().as_str() == "parentdir")
                .map(|e| e.label.as_str().to_string())
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        result_parentdirs.iter().any(|p| p.contains("dirA")),
        "Results should contain items from dirA, parentdirs: {:?}",
        result_parentdirs
    );
    assert!(
        !result_parentdirs.iter().any(|p| p.contains("dirB")),
        "Results should NOT contain items from dirB, parentdirs: {:?}",
        result_parentdirs
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

/// Issue 1: OR演算子でMergedNestMatchが正しくユニオンを返すことを確認
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

/// `parentdir: &: count(extension:rs)` 等で、該当アイテムがない親ディレクトリが結果から除外されることを確認
#[test]
fn test_nest_filter_empty_groups() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    let dir1 = root.join("dir1");
    let dir2 = root.join("dir2");
    std::fs::create_dir_all(&dir1)?;
    std::fs::create_dir_all(&dir2)?;

    // dir1 には rs ファイルが存在する
    std::fs::write(dir1.join("a.rs"), "code")?;
    // dir2 には txt ファイルしか存在しない
    std::fs::write(dir2.join("b.txt"), "text")?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // parentdir: &: count(extension:rs)
    // 期待結果: dir1 は結果に含まれるが、rs ファイルが存在しない dir2 は完全に除外される
    let res =
        fm.search("parentdir: &: count(extension:rs)", Default::default())?;

    assert!(res.type_for_projection.is_some(), "Should be projection");

    let names: Vec<_> = res.results.iter().map(|r| r.name.as_str()).collect();

    assert!(
        names.iter().any(|&n| n.contains("dir1")),
        "dir1 should be included"
    );
    assert!(
        !names.iter().any(|&n| n.contains("dir2")),
        "dir2 should be excluded because it has no rs files"
    );

    Ok(())
}

#[test]
fn test_resolve_nest_dedup_keys() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;
    let db_dir_path = db_dir.path();
    std::fs::write(root_path.join("a.rs"), "content")?;
    let fm = FileManager::new_with_db_dir(db_dir_path)?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    let query = "parentdir: &: parentdir: &: count()";
    let res = fm.search(query, SearchOptions::default());
    assert!(
        res.is_ok(),
        "Depth 2+ nest with same keys should succeed and dedup"
    );
    Ok(())
}

#[test]
fn test_level3_nest_projection() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let work_dir = root_path.join("work");
    std::fs::create_dir(&work_dir)?;
    let db_dir = tempdir()?;
    let db_dir_path = db_dir.path();
    std::fs::write(work_dir.join("a.rs"), "content")?;
    let fm = FileManager::new_with_db_dir(db_dir_path)?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    let query = "parentdir: &: filename:";
    let res = fm.search(query, SearchOptions::default())?;

    assert!(res.results.iter().any(|r| r.name.contains("work") && r.name.contains("a.rs")), 
        "At least one result should contain both parentdir(work) and filename(a.rs). Found: {:?}", 
        res.results.iter().map(|r| &r.name).collect::<Vec<_>>());
    Ok(())
}

#[test]
fn test_level3_nest_projection_with_agg() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;
    let db_dir_path = db_dir.path();

    // dir1: a.rs (7), a.txt (10)
    let dir1 = root_path.join("dir1");
    std::fs::create_dir_all(&dir1)?;
    std::fs::write(dir1.join("a.rs"), "content")?; // 7 bytes
    std::fs::write(dir1.join("a.txt"), "0123456789")?; // 10 bytes

    // dir2: a.rs (5), a.txt (3), b.txt (2)
    let dir2 = root_path.join("dir2");
    std::fs::create_dir_all(&dir2)?;
    std::fs::write(dir2.join("a.rs"), "abcde")?; // 5 bytes
    std::fs::write(dir2.join("a.txt"), "xyz")?; // 3 bytes
    std::fs::write(dir2.join("b.txt"), "ok")?; // 2 bytes

    let fm = FileManager::new_with_db_dir(db_dir_path)?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    // parentdir: &: extension: &: sum(size:)
    // このクエリは parentdir と extension でグループ化し、各グループのサイズ合計を nvalue として出す
    let query = "parentdir: &: extension: &: sum(size:)";
    let res = fm.search(query, SearchOptions::default())?;

    // 期待されるグループ:
    // 1. dir1, rs  -> sum=7
    // 2. dir1, txt -> sum=10
    // 3. dir2, rs  -> sum=5
    // 4. dir2, txt -> sum=3+2=5
    assert_eq!(
        res.results.len(),
        4,
        "Should have 4 groups, but got: {:?}",
        res.results
    );

    let find_group = |pdir: &str, ext: &str, expected_sum: f64| {
        res.results
            .iter()
            .find(|r| {
                let label = &r.name;
                label.contains(pdir)
                    && label.contains(ext)
                    && r.tags.entries.iter().any(|e| {
                        if e.label.tag_type().as_str() == "nvalue" {
                            let val = match e.label.value() {
                                ttfm::types::LabelValue::Double(d_bits) => {
                                    f64::from_bits(d_bits)
                                }
                                ttfm::types::LabelValue::Integer(i) => i as f64,
                                _ => 0.0,
                            };
                            return (val - expected_sum).abs() < 0.001;
                        }
                        false
                    })
            })
            .expect(&format!(
                "Should find group {}/{} with sum {}",
                pdir, ext, expected_sum
            ));
    };

    find_group("dir1", "rs", 7.0);
    find_group("dir1", "txt", 10.0);
    find_group("dir2", "rs", 5.0);
    find_group("dir2", "txt", 5.0);

    Ok(())
}

#[test]
fn test_level3_nest_arithmetic() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;
    let db_dir_path = db_dir.path();
    let target_dir = root_path.join("dir1");
    std::fs::create_dir_all(&target_dir)?;
    std::fs::write(target_dir.join("a.rs"), "content")?; // size: 7

    let target_dir2 = root_path.join("dir2");
    std::fs::create_dir_all(&target_dir2)?;
    std::fs::write(target_dir2.join("b.rs"), "0123456789")?; // size: 10
    std::fs::write(target_dir2.join("c.rs"), "abcde")?; // size: 5

    let fm = FileManager::new_with_db_dir(db_dir_path)?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    // size: は数値型タグなので算術が可能
    // 仕様により、集計なしの投影演算はネストを深化させる
    let query1 = "parentdir: &: (size: + 1)";
    let res1 = fm.search(query1, SearchOptions::default())?;

    // dir1 は 1 ファイルなので 1 結果
    res1.results
        .iter()
        .find(|r| r.name.contains("dir1") && r.name.contains("8"))
        .expect("Should find dir1 8.0 result");

    // dir2 は 2 ファイルなので、深化により 2 つの結果が返るはず
    res1.results
        .iter()
        .find(|r| r.name.contains("dir2") && r.name.contains("11"))
        .expect("Should find dir2 11.0 result (b.rs)");
    res1.results
        .iter()
        .find(|r| r.name.contains("dir2") && r.name.contains("6"))
        .expect("Should find dir2 6.0 result (c.rs)");

    // size: * 2 も数値型算術
    let query2 = "parentdir: &: (size: * 2)";
    let res2 = fm.search(query2, SearchOptions::default())?;

    res2.results
        .iter()
        .find(|r| r.name.contains("dir1") && r.name.contains("14"))
        .expect("Should find dir1 14.0 result");
    res2.results
        .iter()
        .find(|r| r.name.contains("dir2") && r.name.contains("20"))
        .expect("Should find dir2 20.0 result");
    res2.results
        .iter()
        .find(|r| r.name.contains("dir2") && r.name.contains("10"))
        .expect("Should find dir2 10.0 result");

    // parentdir: &: (width: * height:)
    let file_path = target_dir.join("a.rs");
    fm.tag_item(&file_path.to_string_lossy(), "width:10")?;
    fm.tag_item(&file_path.to_string_lossy(), "height:20")?;

    let file_path2 = target_dir2.join("b.rs");
    fm.tag_item(&file_path2.to_string_lossy(), "width:15")?;
    fm.tag_item(&file_path2.to_string_lossy(), "height:30")?;

    let file_path3 = target_dir2.join("c.rs");
    fm.tag_item(&file_path3.to_string_lossy(), "width:5")?;
    fm.tag_item(&file_path3.to_string_lossy(), "height:6")?;

    let query3 = "parentdir: &: (width: * height:)";
    let res3 = fm.search(query3, SearchOptions::default())?;

    res3.results
        .iter()
        .find(|r| r.name.contains("dir1") && r.name.contains("200"))
        .expect("Should find dir1 200.0 result");
    res3.results
        .iter()
        .find(|r| r.name.contains("dir2") && r.name.contains("450"))
        .expect("Should find dir2 450.0 result");
    res3.results
        .iter()
        .find(|r| r.name.contains("dir2") && r.name.contains("30"))
        .expect("Should find dir2 30.0 result");

    Ok(())
}

#[test]
fn test_level3_nest_projection_with_agg_filter() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;
    let db_dir_path = db_dir.path();

    // dir1/a.rs (7) -> Group dir1/rs Sum=7 (OK)
    let dir1 = root_path.join("dir1");
    std::fs::create_dir_all(&dir1)?;
    std::fs::write(dir1.join("a.rs"), "content")?;

    // dir1/b.txt (1) -> Group dir1/txt Sum=1 (FAIL)
    std::fs::write(dir1.join("b.txt"), "x")?;

    // dir2/a.rs (5) -> Group dir2/rs Sum=5 (OK)
    let dir2 = root_path.join("dir2");
    std::fs::create_dir_all(&dir2)?;
    std::fs::write(dir2.join("a.rs"), "abcde")?;

    // dir2/c.txt (2) + dir2/d.txt (1) -> Group dir2/txt Sum=3 (OK)
    std::fs::write(dir2.join("c.txt"), "ok")?;
    std::fs::write(dir2.join("d.txt"), "z")?;

    let fm = FileManager::new_with_db_dir(db_dir_path)?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    // parentdir: &: extension: &: (sum(size:) > 2)
    // 仕様により、比較が行われた時点でネストは解体され、フラットリストが返る
    let query = "parentdir: &: extension: &: (sum(size:) > 2)";
    let res = fm.search(query, SearchOptions::default())?;

    println!("Result Count: {}", res.results.len());
    for r in &res.results {
        println!("  RESULT: name='{}', tags={:?}", r.name, r.tags);
    }

    assert_eq!(
        res.type_for_projection, None,
        "Nest comparison should result in flat list"
    );

    // 期待されるファイル: dir1/a.rs, dir2/a.rs, dir2/c.txt, dir2/d.txt
    // 除外されるファイル: dir1/b.txt (sum=1), 各ディレクトリ (is_dir: true)
    let files: Vec<_> = res
        .results
        .iter()
        .filter(|r| {
            !r.tags.entries.iter().any(|e| {
                e.label.tag_type().to_string() == "is_dir"
                    && e.label.as_str() == "true"
            })
        })
        .collect();

    assert_eq!(
        files.len(),
        4,
        "Should return 4 files, but got: {:?}",
        files
    );

    let names: Vec<String> = files.iter().map(|r| r.name.clone()).collect();
    assert!(names.iter().any(|n| n.contains("a.rs")), "Missing a.rs"); // dir1, dir2 両方 a.rs なので ambiguity あるが、とりあえず存在確認
    assert!(names.iter().any(|n| n.contains("c.txt")), "Missing c.txt");
    assert!(names.iter().any(|n| n.contains("d.txt")), "Missing d.txt");
    assert!(
        !names.iter().any(|n| n.contains("b.txt")),
        "dir1/b.txt should have been filtered out"
    );

    Ok(())
}

#[test]
fn test_level4_nest() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;
    let db_dir_path = db_dir.path();
    let target_dir = root_path.join("dir1");
    std::fs::create_dir_all(&target_dir)?;
    std::fs::write(target_dir.join("a.rs"), "content")?;
    std::fs::write(target_dir.join("b.rs"), "content")?;

    let target_dir2 = root_path.join("dir2");
    std::fs::create_dir_all(&target_dir2)?;
    std::fs::write(target_dir2.join("c.txt"), "content2")?;

    let fm = FileManager::new_with_db_dir(db_dir_path)?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    let query = "parentdir: &: extension: &: size:";
    let res = fm.search(query, SearchOptions::default())?;
    for (i, r) in res.results.iter().enumerate() {
        println!("  result[{}] name = {}", i, r.name);
    }

    // We expect 3 distinct groupings at the file level:
    // dir1 &: rs &: 7 (covers a.rs and b.rs)
    // dir2 &: txt &: 8 (covers c.txt)
    let mut dir1_rs_count = 0;
    let mut dir2_txt_count = 0;

    for r in res.results.iter() {
        if r.name.contains("dir1") && r.name.contains("rs") {
            dir1_rs_count += 1;
        }
        if r.name.contains("dir2") && r.name.contains("txt") {
            dir2_txt_count += 1;
        }
    }

    assert_eq!(
        dir1_rs_count, 1,
        "Should find exactly 1 group for rs inside dir1"
    );
    assert_eq!(
        dir2_txt_count, 1,
        "Should find exactly 1 group for txt inside dir2"
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
fn test_level3_nest_agg_internal_filter_repro() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;
    let db_dir_path = db_dir.path();

    let fm = FileManager::new_with_db_dir(db_dir_path)?;

    // dir1/small.rs (size=5) -> sum(size: :> 10 & size:) では除外されるべき
    let dir1 = root_path.join("dir1");
    std::fs::create_dir_all(&dir1)?;
    std::fs::write(dir1.join("small.rs"), "12345")?; // 5 bytes

    // dir1/large.rs (size=15) -> sum(size: :> 10 & size:) でカウントされるべき
    std::fs::write(dir1.join("large.rs"), "0123456789ABCDE")?; // 15 bytes

    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    // parentdir: &: extension: &: sum(size: :> 10 & size:)
    // Level 3+ (parentdir, extension) であるため Pivot CTE パスを通る
    let query = "parentdir: &: extension: &: sum(size: :> 10 & size:)";
    let res = fm.search(query, SearchOptions::default())?;

    // 期待されるグループ:
    // dir1, rs -> nvalue=15.0 (large.rs のみ。small.rs は除外)

    let dir1_rs = res
        .results
        .iter()
        .find(|r| r.name.contains("dir1") && r.name.contains("rs"))
        .expect("Should find dir1/rs group");

    let nvalue = dir1_rs
        .tags
        .entries
        .iter()
        .find(|e| e.label.tag_type().as_str() == "nvalue")
        .expect("Should have nvalue tag");

    let val = match nvalue.label.value() {
        ttfm::types::LabelValue::Double(d_bits) => f64::from_bits(d_bits),
        ttfm::types::LabelValue::Integer(i) => i as f64,
        _ => 0.0,
    };

    // フィルタが無視されると 5 + 15 = 20 になる。正しく動作すれば 15.0。
    assert_eq!(
        val, 15.0,
        "Sum should only include files > 10 bytes, but got: {}",
        val
    );

    Ok(())
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
            "Query '{}' should resolve without error, got: {}",
            query,
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }
}

#[test]
fn test_nest_query_vs_calc_e2e() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 3 rs files of size 10, 20, 30 → sum(size) = 60, sum/2 = 30
    // sum > sum/2 is always true for positive sum, so rs extension should be included
    std::fs::write(root.join("a.rs"), vec![0u8; 10])?;
    std::fs::write(root.join("b.rs"), vec![0u8; 20])?;
    std::fs::write(root.join("c.rs"), vec![0u8; 30])?;
    // 1 txt file
    std::fs::write(root.join("d.txt"), vec![0u8; 5])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res = fm.search(
        "extension: &: (sum(size:) > (sum(size:) / 2))",
        Default::default(),
    )?;

    // Result should be flat list (not projection)
    assert!(
        res.type_for_projection.is_none(),
        "Query should return flat list, not projection"
    );

    // sum(rs) = 60, 60 > 60/2=30 → true → rs files included
    // sum(txt) = 5, 5 > 5/2=2.5 → true → txt files included
    // All files should be in the result (condition is always true for positive sizes)
    assert!(
        !res.results.is_empty(),
        "Should have results, got: {:?}",
        res.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );

    Ok(())
}

// ──────────────────────────────────────────────
// Phase 7: Unnest by Aggregation
// ──────────────────────────────────────────────

/// sum(parentdir: &: size:) → 各parentdirのサイズ合計がnvalueとして付与される
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

/// count(parentdir: &: extension:) → 各parentdirの拡張子種類数
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

/// sum(parentdir: &: extension: &: size:) → 深さ3→2 (2キーNest + nvalue)
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

/// sum(extension: &: size:) → is_dir:false フィルタが正常に適用される
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

/// sum(size:) → 通常集約がunnestの影響を受けないことの確認
#[test]
fn test_unnest_regression_plain_agg() -> anyhow::Result<()> {
    let root = tempdir()?;
    let root_path = root.path();
    let db_dir = tempdir()?;

    std::fs::write(root_path.join("a.rs"), "content")?; // 7 bytes
    std::fs::write(root_path.join("b.rs"), "0123456789")?; // 10 bytes

    let fm = FileManager::new_with_db_dir(db_dir.path())?;
    fm.index_directory(root_path, None::<&fn(usize)>, false)?;

    let res =
        fm.search("sum(extension:rs & size:)", SearchOptions::default())?;

    // スカラー結果
    assert!(
        res.type_for_projection.is_none(),
        "Should return scalar result, not projection"
    );
    assert_eq!(res.results[0].name, "17");

    Ok(())
}

/// sum(parentdir: &: count()) → 既存のnvalue付きNest集約に影響なし
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

    // スカラー結果: ディレクトリも indexing されるため以下のグループが存在する
    // - parent_of_root: root → count()=1
    // - root: dir1, dir2 → count()=2
    // - dir1: a.rs, b.rs → count()=2
    // - dir2: c.rs → count()=1
    // 合計: 1+2+2+1 = 6
    assert!(
        res.type_for_projection.is_none(),
        "Should return scalar result"
    );
    let val: f64 = res.results[0].name.parse().expect("Should be a number");
    assert_eq!(val, 6.0, "sum of per-parentdir counts should be 6");

    Ok(())
}

/// Lv5→Lv4: sum(parentdir: &: extension: &: filename: &: size:)
/// 4キー → 3キー + nvalue (単段unnest)
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

    find_group("dir1", "rs", "a.rs");
    assert_eq!(find_group("dir1", "rs", "a.rs"), 7.0);
    assert_eq!(find_group("dir1", "rs", "b.rs"), 10.0);
    assert_eq!(find_group("dir2", "txt", "c.txt"), 5.0);

    Ok(())
}

/// 多段unnest: Lv4→Lv2→Lv0
/// sum(sum(parentdir: &: extension: &: size:))
/// 内側sum: 3キー → 2キー + nvalue=sum(size)
/// 外側sum: Nest{2キー, nvalue} → スカラー (各グループのnvalueの合計)
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

/// 多段unnest: Lv3→Lv2→Lv0
/// sum(count(parentdir: &: extension:))
/// 内側count: 2キー → 1キー + nvalue=count(extension)
/// 外側sum: Nest{1キー, nvalue} → スカラー (各parentdirの拡張子種類数の合計)
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

    // スカラー結果: count(dir1のext種類)=2 + count(dir2のext種類)=1 = 3
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
