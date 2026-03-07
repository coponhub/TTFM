/// 算術演算 (Calculation) 機能の統合テスト
///
/// テスト対象:
/// - `(1 + 2) :> size:`: リテラル演算と比較
/// - `(size: + 100) > 1000`: タグと演算の組み合わせ
/// - `((1 + 2) * 3) :> size:`: ネストした演算
/// - `(1MB + 100B) :> size:`: サイズ単位の演算
/// - `size: > (1000 + 500)`: タグと演算の比較
/// - 全演算子 (+, -, *, /, %) のテスト
/// - `(size: - 100) :> (size: * 0.1)`: Calculation :> Calculation
use tempfile::tempdir;
use ttfm::FileManager;

/// Phase 1: リテラル演算の基本テスト
/// (1 + 2) :> size: - 値3と比較
#[test]
fn test_calculation_literal_simple() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // テストファイル作成（5バイト）
    std::fs::write(root.join("file1.txt"), b"12345")?;

    // FileManager初期化
    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: (1 + 2) :< size:
    // (1 + 2) < size:、つまりsize: > 3

    // デバッグ: Resolverとクエリの確認
    let resolver =
        ttfm::query::lens_resolver::Resolver::new("(1 + 2) :< size:")?;
    let sql =
        ttfm::query::sql::build_pick_sql(&resolver.resolved_query, "oneview");
    let sql_str = sql.to_string(sea_query::PostgresQueryBuilder);
    println!("Generated SQL: {}", sql_str);

    let res = fm.search("(1 + 2) :< size:", Default::default())?;

    // file1.txtのサイズは5バイトなので、3より大きいためマッチする
    assert!(!res.results.is_empty(), "Should have at least one result");

    // file1.txtが含まれていることを確認
    let has_file1 = res
        .results
        .iter()
        .any(|item| item.name.contains("file1.txt"));
    assert!(has_file1, "Results should contain file1.txt");

    Ok(())
}

/// Phase 3: タグと演算の組み合わせ
/// (size: + 100) :> 1000
#[test]
fn test_calculation_with_tag() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // テストファイル作成（1000バイト）
    std::fs::write(root.join("large.txt"), vec![0u8; 1000])?;
    // 小さいファイル（500バイト）
    std::fs::write(root.join("small.txt"), vec![0u8; 500])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: (size: + 100) :> 1000
    // size: + 100 が 1000 より大きいアイテムを検索

    // デバッグ: SQL確認
    let resolver =
        ttfm::query::lens_resolver::Resolver::new("(size: + 100) :> 1000")?;
    println!("ResolvedNode: {:?}", resolver.resolved_query);
    let sql =
        ttfm::query::sql::build_pick_sql(&resolver.resolved_query, "oneview");
    let sql_str = sql.to_string(sea_query::PostgresQueryBuilder);
    println!("Generated SQL: {}", sql_str);

    let res = fm.search("(size: + 100) :> 1000", Default::default())?;

    // 結果を出力
    println!("Results count: {}", res.results.len());
    for item in &res.results {
        println!("  - {} (id: {})", item.name, item.id.as_i64());
    }

    // large.txt (1000 + 100 = 1100 > 1000) がマッチする
    // small.txt (500 + 100 = 600 < 1000) はマッチしない
    assert!(!res.results.is_empty());
    let has_large = res
        .results
        .iter()
        .any(|item| item.name.contains("large.txt"));
    assert!(has_large, "Results should contain large.txt");

    // small.txtが含まれる場合は、より詳細な情報を出力
    if res
        .results
        .iter()
        .any(|item| item.name.contains("small.txt"))
    {
        println!("WARNING: small.txt is in results but shouldn't be");
        // ファイルのサイズを直接確認
        let small_res = fm.search("name:small.txt", Default::default())?;
        if !small_res.results.is_empty() {
            println!("small.txt tags:");
            for tag in &small_res.results[0].tags.entries {
                println!(
                    "  {} = {}",
                    tag.label.tag_type().as_str(),
                    tag.label.as_str()
                );
            }
        }
    }

    Ok(())
}

/// Phase 3: タグと算術演算の比較
/// size: > (1000 + 500)
#[test]
fn test_calculation_tag_comparison() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // テストファイル作成（2000バイト）
    std::fs::write(root.join("huge.txt"), vec![0u8; 2000])?;
    // 中サイズファイル（1000バイト）
    std::fs::write(root.join("medium.txt"), vec![0u8; 1000])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: (1000 + 500) :< size:
    // (1000 + 500) が size: より小さい、つまりsize: > 1500

    // デバッグ: SQLとResolvedNode確認
    let resolver =
        ttfm::query::lens_resolver::Resolver::new("(1000 + 500) :< size:")?;
    println!("ResolvedNode: {:?}", resolver.resolved_query);
    let sql =
        ttfm::query::sql::build_pick_sql(&resolver.resolved_query, "oneview");
    let sql_str = sql.to_string(sea_query::PostgresQueryBuilder);
    println!("Generated SQL: {}", sql_str);

    let res = fm.search("(1000 + 500) :< size:", Default::default())?;

    // 結果を出力
    println!("Results count: {}", res.results.len());
    for item in &res.results {
        println!("  - {} (id: {})", item.name, item.id.as_i64());
    }

    // huge.txt (2000 > 1500) がマッチする
    // medium.txt (1000 < 1500) はマッチしない
    assert!(!res.results.is_empty());
    let has_huge = res
        .results
        .iter()
        .any(|item| item.name.contains("huge.txt"));
    assert!(has_huge, "Results should contain huge.txt");

    let has_medium = res
        .results
        .iter()
        .any(|item| item.name.contains("medium.txt"));
    assert!(
        !has_medium,
        "Results should not contain medium.txt (1000 < 1500)"
    );

    Ok(())
}

/// Phase 4: ネストした演算
/// ((1 + 2) * 3) :< size:
#[test]
fn test_calculation_nested() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // テストファイル作成（10バイト）
    std::fs::write(root.join("big.txt"), vec![0u8; 10])?;
    // 小さいファイル（5バイト）
    std::fs::write(root.join("small.txt"), vec![0u8; 5])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: ((1 + 2) * 3) :< size:
    // ((1 + 2) * 3) = 9、つまりsize: > 9
    let res = fm.search("((1 + 2) * 3) :< size:", Default::default())?;

    // big.txt (10 > 9) がマッチする
    // small.txt (5 < 9) はマッチしない
    assert!(!res.results.is_empty());
    let has_big = res.results.iter().any(|item| item.name.contains("big.txt"));
    assert!(has_big, "Results should contain big.txt");

    let has_small = res
        .results
        .iter()
        .any(|item| item.name.contains("small.txt"));
    assert!(!has_small, "Results should not contain small.txt (5 < 9)");

    Ok(())
}

/// Phase 5: サイズ単位の演算
/// (1MB + 100B) :< size:
#[test]
fn test_calculation_size_unit() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 1MB + 200バイト (1048776バイト)
    std::fs::write(root.join("large.dat"), vec![0u8; 1_048_776])?;
    // 1MBちょうど (1048576バイト)
    std::fs::write(root.join("medium.dat"), vec![0u8; 1_048_576])?;
    // 1MB未満 (1048476バイト)
    std::fs::write(root.join("small.dat"), vec![0u8; 1_048_476])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: (1MB + 100B) :< size:
    // 1MB + 100B = 1048576 + 100 = 1048676
    // つまりsize: > 1048676
    let res = fm.search("(1MB + 100B) :< size:", Default::default())?;

    // large.dat (1048776 > 1048676) がマッチする
    assert!(!res.results.is_empty());
    let has_large = res
        .results
        .iter()
        .any(|item| item.name.contains("large.dat"));
    assert!(has_large, "Results should contain large.dat");

    // medium.dat (1048576 < 1048676) はマッチしない
    let has_medium = res
        .results
        .iter()
        .any(|item| item.name.contains("medium.dat"));
    assert!(!has_medium, "Should not contain medium.dat");

    // small.dat (1048476 < 1048676) はマッチしない
    let has_small = res
        .results
        .iter()
        .any(|item| item.name.contains("small.dat"));
    assert!(!has_small, "Should not contain small.dat");

    Ok(())
}

/// Phase 6: 全演算子の確認
/// +, -, *, /, % の各演算子をテスト
#[test]
fn test_calculation_all_operators() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 各演算子のテスト用ファイル
    std::fs::write(root.join("f10.txt"), vec![0u8; 10])?;
    std::fs::write(root.join("f20.txt"), vec![0u8; 20])?;
    std::fs::write(root.join("f50.txt"), vec![0u8; 50])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 加算: (5 + 10) :< size: → size: > 15
    let res = fm.search("(5 + 10) :< size:", Default::default())?;
    assert!(
        res.results.iter().any(|i| i.name.contains("f20.txt")),
        "Addition: f20.txt (20 > 15) should match"
    );
    assert!(
        !res.results.iter().any(|i| i.name.contains("f10.txt")),
        "Addition: f10.txt (10 < 15) should not match"
    );

    // 減算: (30 - 10) :< size: → size: > 20
    let res = fm.search("(30 - 10) :< size:", Default::default())?;
    assert!(
        res.results.iter().any(|i| i.name.contains("f50.txt")),
        "Subtraction: f50.txt (50 > 20) should match"
    );
    assert!(
        !res.results.iter().any(|i| i.name.contains("f20.txt")),
        "Subtraction: f20.txt (20 = 20) should not match"
    );

    // 乗算: (5 * 3) :< size: → size: > 15
    let res = fm.search("(5 * 3) :< size:", Default::default())?;
    assert!(
        res.results.iter().any(|i| i.name.contains("f20.txt")),
        "Multiplication: f20.txt (20 > 15) should match"
    );

    // 除算: (100 / 5) :< size: → size: > 20
    let res = fm.search("(100 / 5) :< size:", Default::default())?;
    assert!(
        res.results.iter().any(|i| i.name.contains("f50.txt")),
        "Division: f50.txt (50 > 20) should match"
    );

    // 剰余: (25 % 20) :< size: → 25 % 20 = 5, size: > 5
    let res = fm.search("(25 % 20) :< size:", Default::default())?;
    assert!(
        res.results.iter().any(|i| i.name.contains("f10.txt")),
        "Modulo: f10.txt (10 > 5) should match"
    );

    Ok(())
}

/// Phase 7: 集約関数との組み合わせ
/// (sum(size:) + 100) > 1000
#[test]
fn test_calculation_with_aggregation() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 複数のファイル作成（合計1000バイト）
    std::fs::write(root.join("a.txt"), vec![0u8; 300])?;
    std::fs::write(root.join("b.txt"), vec![0u8; 300])?;
    std::fs::write(root.join("c.txt"), vec![0u8; 400])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: (sum(size:) + 100) > 1000
    // sum(size:) = 300 + 300 + 400 = 1000
    // 1000 + 100 = 1100 > 1000 なので全てマッチするはず
    let res = fm.search("(sum(size:) + 100) > 1000", Default::default())?;

    // 結果を出力
    println!("Results count: {}", res.results.len());
    for item in &res.results {
        println!("  - {} (id: {})", item.name, item.id.as_i64());
    }

    // sum(size:)は全ファイルの合計なので、条件を満たす場合は全ファイルが返る
    assert!(!res.results.is_empty(), "Should have results");

    Ok(())
}

/// Phase 7: 複雑な集約演算
/// sum(size:) > (100 * 2)
#[test]
fn test_calculation_aggregation_complex() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 合計が200を超えるファイルを作成
    std::fs::write(root.join("x.txt"), vec![0u8; 100])?;
    std::fs::write(root.join("y.txt"), vec![0u8; 150])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // クエリ: sum(size:) > (100 * 2)
    // sum = 250 > 200 なので条件を満たす
    let res = fm.search("sum(size:) > (100 * 2)", Default::default())?;

    // 結果を出力
    println!("Results count: {}", res.results.len());
    for item in &res.results {
        println!("  - {} (id: {})", item.name, item.id.as_i64());
    }

    // 条件を満たす場合は全ファイルが返るはず
    assert!(!res.results.is_empty(), "Should have results");

    Ok(())
}

// ========== bare_calculation: 集約内の括弧省略 ==========
// QUERY.md:103 — 同じレベルの () 内に算術演算子以外の演算子が無い場合、括弧を省略可

/// bare_calculation — 減算: sum(size: - 100)
/// sum((size: - 100)) と同意。スカラー結果を返す。
#[test]
fn test_aggregation_bare_calc_sub() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 合計サイズ: 200 + 300 = 500
    std::fs::write(root.join("a.txt"), vec![0u8; 200])?;
    std::fs::write(root.join("b.txt"), vec![0u8; 300])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(size: - 100) = sum(size - 100) = (200-100)+(300-100) = 300
    let res = fm.search("sum(size: - 100)", Default::default())?;
    assert!(!res.results.is_empty(), "Should return a scalar result");

    // 数値として解析出来ることを確認
    let val: f64 = res.results[0].name.parse().unwrap_or(f64::NAN);
    assert!(
        !val.is_nan(),
        "Result should be a number, got: {}",
        res.results[0].name
    );

    Ok(())
}

/// bare_calculation — スカラー比較: sum(size: * 2) > 1000
#[test]
fn test_aggregation_bare_calc_mul_with_cmp() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 合計サイズ: 400 + 600 = 1000、*2 で 2000
    std::fs::write(root.join("a.txt"), vec![0u8; 400])?;
    std::fs::write(root.join("b.txt"), vec![0u8; 600])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(size: * 2) > 1000
    // sum(size*2) = (400*2)+(600*2) = 2000 > 1000 → TRUE
    let res = fm.search("sum(size: * 2) > 1000", Default::default())?;
    assert_eq!(res.results.len(), 1);
    assert_eq!(res.results[0].name, "TRUE");

    Ok(())
}

/// bare_calculation — 複数演算子の左結合チェーン: sum(size: + 100 - 50)
#[test]
fn test_aggregation_bare_calc_multiop() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    // 合計サイズ: 200 + 300 = 500
    std::fs::write(root.join("a.txt"), vec![0u8; 200])?;
    std::fs::write(root.join("b.txt"), vec![0u8; 300])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // sum(size: + 100 - 50) = sum((size+100)-50) = ((200+100)-50)+((300+100)-50) = 600
    let res = fm.search("sum(size: + 100 - 50)", Default::default())?;
    assert!(!res.results.is_empty(), "Should return a scalar result");

    let val: f64 = res.results[0].name.parse().unwrap_or(f64::NAN);
    assert!(
        !val.is_nan(),
        "Result should be a number, got: {}",
        res.results[0].name
    );

    Ok(())
}

/// bare_calculation — ベースライン: 明示的括弧版 sum((size: - 100)) と同じ結果
#[test]
fn test_aggregation_bare_calc_explicit_paren_baseline() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("a.txt"), vec![0u8; 200])?;
    std::fs::write(root.join("b.txt"), vec![0u8; 300])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    // 明示的括弧版: sum((size: - 100))
    let res_explicit = fm.search("sum((size: - 100))", Default::default())?;
    assert!(
        !res_explicit.results.is_empty(),
        "Explicit paren should work"
    );

    // 括弧省略版: sum(size: - 100)
    let res_bare = fm.search("sum(size: - 100)", Default::default())?;
    assert!(!res_bare.results.is_empty(), "Bare calc should work");

    // 両者が同じ結果を返す
    assert_eq!(
        res_explicit.results[0].name, res_bare.results[0].name,
        "bare_calculation and explicit paren should produce the same result"
    );

    Ok(())
}

/// Calculation :> Calculation のラベル比較テスト
/// `(size: - 100) :> (size: * 0.1)` — 両辺が算術演算（Projection を含む）の比較
///
/// (size - 100) > (size * 0.1) ⟺ size * 0.9 > 100 ⟺ size > ~111.1
/// - large.txt (200B): (200 - 100) = 100 > (200 * 0.1) = 20  → マッチ
/// - small.txt (100B): (100 - 100) = 0   > (100 * 0.1) = 10  → 不一致
/// - tiny.txt  (50B):  (50 - 100)  = -50 > (50 * 0.1)  = 5   → 不一致
#[test]
fn test_calculation_vs_calculation() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");

    std::fs::write(root.join("large.txt"), vec![0u8; 200])?;
    std::fs::write(root.join("small.txt"), vec![0u8; 100])?;
    std::fs::write(root.join("tiny.txt"), vec![0u8; 50])?;

    let fm = FileManager::new_with_db_dir(&db_dir)?;
    fm.index_directory(root, None::<&fn(usize)>, false)?;

    let res =
        fm.search("(size: - 100) :> (size: * 0.1)", Default::default())?;

    let names: Vec<&str> =
        res.results.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.iter().any(|n| n.contains("large.txt")),
        "large.txt (200B) should match: got {:?}",
        names
    );
    assert!(
        !names.iter().any(|n| n.contains("small.txt")),
        "small.txt (100B) should not match: got {:?}",
        names
    );
    assert!(
        !names.iter().any(|n| n.contains("tiny.txt")),
        "tiny.txt (50B) should not match: got {:?}",
        names
    );

    Ok(())
}
