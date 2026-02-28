use ttfm::query::parser::parse;

#[test]
fn test_repro_arithmetic_error_msg() {
    let input =
        "(parentdir: &: count(extension:rs)) / (parentdir: &: count()) :> 1";
    let err = parse(input).unwrap_err();
    let err_msg = format!("{}", err);
    println!("Actual Error Output:\n{}", err_msg);

    // 改善された具体的案が含まれていることを確認
    assert!(err_msg.contains("Did you mean: '((parentdir: &: count(extension:rs)) / (parentdir: &: count())) :> 1'"));
}

#[test]
fn test_repro_simple_error_msg() {
    let input = "count() :> 1";
    let err = parse(input).unwrap_err();
    let err_msg = format!("{}", err);

    // 単純な誤用ケース（リグレッション防止用）
    assert!(err_msg.contains("Label Comparison cannot be applied to Aggregation/Calculation ('count()')"));
}
