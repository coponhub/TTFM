// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

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
fn test_repro_top_level_arithmetic_error_msg() {
    let input = "parentdir: &: count(extension:rs) / parentdir: &: count()";
    let err = parse(input).unwrap_err();
    let err_msg = format!("{}", err);
    println!("Actual Error Output:\n{}", err_msg);

    // 改善された具体的案が含まれていることを確認
    assert!(err_msg.contains("Arithmetic operations require parentheses when mixed with other operations at the same level"));
    assert!(err_msg.contains("Did you mean: '((parentdir: &: count(extension:rs)) / (parentdir: &: count()))'"));
}

#[test]
fn test_repro_nested_arithmetic_error_msg() {
    let input =
        "(parentdir: &: count(extension:rs) / parentdir: &: count()) :> 1";
    let err = parse(input).unwrap_err();
    let err_msg = format!("{}", err);
    println!("Actual Error Output:\n{}", err_msg);

    // 現在の不適切な（余計な括弧を含む）提案が改善されることを期待するテスト
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
