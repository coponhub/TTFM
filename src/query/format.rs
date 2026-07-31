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

//! # 入力形式（`OperandFormat`）
//!
//! 値の表記（`1MB` / `2026-02-01` / `File(*)` 等）を読む知識を型ごとに持つ抽象。
//! 実装対象は「文字列から読めて `Operand` になれるもの」で、パース結果の型
//! （`DateTimeRange` / `ByteSizeRange` / `ItemIdRange` / `Bitical`）に実装する。
//!
//! パースは3分岐を返す: `None` = 自分の形式ではない（他形式に譲る）、
//! `Some(Err(_))` = 自分の形式だが値が不正、`Some(Ok(_))` = 成功。

use crate::query::ast::{
    ArithmeticOp, BasicOp, CalculationNode, ComparisonNode, ComparisonOp, Operand,
    QueryNode,
};
use crate::query::logical_schema::LogicalType;
use crate::types::Label;

pub trait OperandFormat: Sized {
    fn parse(s: &str) -> Option<Result<Self, String>>;

    fn to_label(&self, original: &Label) -> Label {
        original.clone()
    }

    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
}

/// バイトサイズ表記（`1MB` / `*MB` / `2.*MB` / `*.5MB`）を解釈した結果。
/// `util::parse_size`（点）と `tag::parse_size_glob`（glob）の2入口を統合する。
/// リテラルは `Range{n,n}`（点）として表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteSizeRange {
    Range { lo: i64, hi: i64 },
    Periodic { lo: i64, hi: i64, multiplier: i64 },
}

impl ByteSizeRange {
    /// バイトサイズ表記を解釈する。`None` = バイトサイズの表記として読めない
    /// （自分の形式ではない）。現状 `Some(Err(_))` を返す経路は無い —
    /// バイトサイズには「表記はしているが値が不正」に相当する状態が無いため
    /// （日付の存在しない日のような概念が無い）。
    pub fn parse(s: &str) -> Option<Result<ByteSizeRange, String>> {
        let upper = s.trim().to_ascii_uppercase();
        if upper.is_empty() {
            return None;
        }
        // 単位の無い `*` は型に依存しない汎用の全一致 glob なのでここでは扱わない。
        if upper == "*" {
            return None;
        }
        let split_at = upper
            .find(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(upper.len());
        let (num_part, unit_part) = upper.split_at(split_at);
        let unit = unit_part.trim();
        if unit.is_empty() {
            return None;
        }
        let multiplier = crate::util::size_unit_multiplier(unit)?;

        if !num_part.contains('*') {
            let val: f64 = num_part.trim().parse().ok()?;
            let bytes = (val * multiplier as f64) as i64;
            return Some(Ok(ByteSizeRange::Range { lo: bytes, hi: bytes }));
        }

        let (int_str, dec_str) = match num_part.split_once('.') {
            Some((i, d)) => (i, Some(d)),
            None => (num_part, None),
        };
        use crate::util::NumericField;
        let int_field = crate::util::parse_numeric_field(int_str)?;
        let dec_field = match dec_str {
            None => None,
            Some(d) => Some(crate::util::parse_numeric_field(d)?),
        };

        match (int_field, dec_field) {
            (NumericField::Free, None) | (NumericField::Free, Some(NumericField::Free)) => {
                Some(Ok(ByteSizeRange::Range { lo: 0, hi: i64::MAX }))
            }
            (NumericField::Free, Some(NumericField::Literal(digits))) => {
                let (lo, hi) = decimal_literal_band(digits, multiplier);
                Some(Ok(ByteSizeRange::Periodic { lo, hi, multiplier }))
            }
            (NumericField::Literal(n_str), Some(NumericField::Free)) => {
                let n: i64 = n_str.parse().ok()?;
                let m = multiplier as i128;
                let lo = (n as i128).checked_mul(m)?;
                let hi = (n as i128 + 1).checked_mul(m)?.checked_sub(1)?;
                Some(Ok(ByteSizeRange::Range {
                    lo: lo.min(i64::MAX as i128) as i64,
                    hi: hi.min(i64::MAX as i128) as i64,
                }))
            }
            _ => None,
        }
    }
}

/// `ByteSizeRange` と比較演算子から条件を組み立てる。周期形は `operand` を剰余
/// （`% multiplier`）へ包んでから、範囲形と同じ区間の境界行列（日付の区間形と同じ形）を
/// 適用する: Eq→区間内（And）/ Ne→区間外（Or）/ Gt・Le→上限側 / Ge・Lt→下限側。
pub fn byte_size_range_condition(
    operand: Operand,
    ctor: fn(BasicOp) -> ComparisonOp,
    op: BasicOp,
    range: ByteSizeRange,
) -> QueryNode {
    let (base, lo, hi) = match range {
        ByteSizeRange::Range { lo, hi } => (operand, lo, hi),
        ByteSizeRange::Periodic { lo, hi, multiplier } => (
            Operand::Calculation(Box::new(CalculationNode {
                left: operand,
                op: ArithmeticOp::Mod,
                right: Operand::Literal(Label::Size(multiplier)),
            })),
            lo,
            hi,
        ),
    };
    let cmp = |bop: BasicOp, val: i64| {
        QueryNode::Comparison(ComparisonNode {
            first: base.clone(),
            rest: vec![(ctor(bop), Operand::Literal(Label::Size(val)))],
        })
    };
    match op {
        BasicOp::Eq => QueryNode::And(vec![cmp(BasicOp::Ge, lo), cmp(BasicOp::Le, hi)]),
        BasicOp::Ne => QueryNode::Or(vec![cmp(BasicOp::Lt, lo), cmp(BasicOp::Gt, hi)]),
        BasicOp::Gt => cmp(BasicOp::Gt, hi),
        BasicOp::Ge => cmp(BasicOp::Ge, lo),
        BasicOp::Lt => cmp(BasicOp::Lt, lo),
        BasicOp::Le => cmp(BasicOp::Le, hi),
    }
}

/// 小数部リテラル `digits`（桁数は不問）1つに対応する値の範囲
/// `[digits/10^k, (digits+1)/10^k) * multiplier` を、下限閉・上限閉の
/// 整数バイト境界 (lo, hi) へ変換する。桁が深く帯がバイト単位で表現できない
/// 場合は lo > hi（空集合）になりうる。
fn decimal_literal_band(digits: &str, multiplier: i64) -> (i64, i64) {
    let k = digits.len() as u32;
    let v: i128 = digits.parse().unwrap_or(0);
    let m = multiplier as i128;
    let Some(base) = 10i128.checked_pow(k) else {
        return (1, 0);
    };
    let lo = ceil_div_i128(v * m, base).min(i64::MAX as i128) as i64;
    let hi = (ceil_div_i128((v + 1) * m, base) - 1).min(i64::MAX as i128) as i64;
    (lo, hi)
}

/// item_id: のローカル形式 glob（例: `File(*)`）を Origin の区画範囲へ解釈した結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemIdRange {
    pub lo: i64,
    pub hi: i64,
}

impl ItemIdRange {
    /// origin ラベルは `Origin::short()` と一致するリテラルのみ対応し、オフセット部は
    /// `*`（区画全域）のみ対応。それ以外は自分の形式ではないとして譲る。
    pub fn parse(s: &str) -> Option<Result<ItemIdRange, String>> {
        use crate::types::Origin;
        use strum::IntoEnumIterator;
        let open = s.find('(')?;
        let label = &s[..open];
        let rest = s.strip_suffix(')')?;
        let offset_pattern = &rest[open + 1..];
        if offset_pattern != "*" {
            return None;
        }
        let origin = Origin::iter().find(|o| o.short() == label)?;
        Some(Ok(ItemIdRange {
            lo: origin.block_lo(),
            hi: origin.block_hi() - 1,
        }))
    }
}

impl OperandFormat for ByteSizeRange {
    fn parse(s: &str) -> Option<Result<Self, String>> {
        ByteSizeRange::parse(s)
    }

    fn to_label(&self, original: &Label) -> Label {
        match self {
            ByteSizeRange::Range { lo, hi } if lo == hi => {
                original.rekey(original.tag_type(), crate::types::Bitical::Integer(*lo))
            }
            _ => original.clone(),
        }
    }

    fn logical_type(&self) -> LogicalType {
        LogicalType::Integer
    }
}

impl OperandFormat for crate::types::DateTimeRange {
    fn parse(s: &str) -> Option<Result<Self, String>> {
        crate::types::DateTimeRange::parse(s)
    }

    fn to_label(&self, original: &Label) -> Label {
        match self {
            crate::types::DateTimeRange::Interval { start, .. } => {
                original.rekey(original.tag_type(), crate::types::Bitical::Integer(*start))
            }
            crate::types::DateTimeRange::Slots(_) => original.clone(),
        }
    }

    fn logical_type(&self) -> LogicalType {
        LogicalType::Integer
    }
}

impl OperandFormat for ItemIdRange {
    fn parse(s: &str) -> Option<Result<Self, String>> {
        ItemIdRange::parse(s)
    }

    fn logical_type(&self) -> LogicalType {
        LogicalType::Integer
    }
}

crate::define_operand_formats! {
    ByteSizeRange,
    crate::types::DateTimeRange,
    ItemIdRange,
    crate::types::Bitical,
}

impl OperandFormat for crate::types::Bitical {
    fn parse(s: &str) -> Option<Result<Self, String>> {
        use crate::types::Bitical;
        if let Ok(i) = s.parse::<i64>() {
            return Some(Ok(Bitical::Integer(i)));
        }
        if let Ok(f) = s.parse::<f64>() {
            return Some(Ok(Bitical::Double(f)));
        }
        match s {
            "true" => Some(Ok(Bitical::Boolean(true))),
            "false" => Some(Ok(Bitical::Boolean(false))),
            _ => Some(Ok(Bitical::String(s.to_string()))),
        }
    }

    fn logical_type(&self) -> LogicalType {
        self.infer_logical_type()
    }
}

fn ceil_div_i128(a: i128, b: i128) -> i128 {
    let d = a.div_euclid(b);
    let r = a.rem_euclid(b);
    if r == 0 {
        d
    } else {
        d + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::logical_schema::LogicalType;

    // --- ByteSizeRange（旧 util::parse_size + tag::parse_size_glob の統合） ---

    #[test]
    fn test_byte_size_range_parses_plain_literal() {
        assert_eq!(
            ByteSizeRange::parse("1MB"),
            Some(Ok(ByteSizeRange::Range { lo: 1048576, hi: 1048576 }))
        );
    }

    #[test]
    fn test_byte_size_range_parses_full_match_glob() {
        assert_eq!(
            ByteSizeRange::parse("*MB"),
            Some(Ok(ByteSizeRange::Range { lo: 0, hi: i64::MAX }))
        );
    }

    #[test]
    fn test_byte_size_range_parses_free_decimal_as_range() {
        // 整数部が2で小数部が自由 → [2.00MB, 3.00MB) の1区間
        assert_eq!(
            ByteSizeRange::parse("2.*MB"),
            Some(Ok(ByteSizeRange::Range { lo: 2_097_152, hi: 3_145_727 }))
        );
    }

    #[test]
    fn test_byte_size_range_parses_free_integer_as_periodic() {
        let result = ByteSizeRange::parse("*.5MB");
        assert!(
            matches!(result, Some(Ok(ByteSizeRange::Periodic { multiplier: 1048576, .. }))),
            "剰余の周期条件になるべき: {result:?}"
        );
    }

    /// フィールド内の部分 glob・未知の単位・非数値はこの形式の対象外。
    #[test]
    fn test_byte_size_range_declines_unrecognized_forms() {
        for pattern in ["1*", "1*2MB", "100XYZ", "abc", "*"] {
            assert_eq!(
                ByteSizeRange::parse(pattern),
                None,
                "自分の形式ではないと譲るべき: {pattern}"
            );
        }
    }

    /// 単位の無い裸の整数はバイトサイズの表記ではない（目印が無い）。
    /// 「単位が無ければバイト」は SizeFn の型文脈の知識であって OperandFormat の規則ではない。
    #[test]
    fn test_byte_size_range_declines_bare_integer() {
        for pattern in ["2026", "0", "123456"] {
            assert_eq!(
                ByteSizeRange::parse(pattern),
                None,
                "should decline a bare integer with no unit: {pattern}"
            );
        }
    }

    // --- ByteSizeRange as OperandFormat ---

    #[test]
    fn test_byte_size_range_operand_format_logical_type() {
        let range = ByteSizeRange::parse("1MB").unwrap().unwrap();
        assert_eq!(OperandFormat::logical_type(&range), LogicalType::Integer);
    }

    #[test]
    fn test_byte_size_range_operand_format_to_label_point() {
        use crate::types::{Bitical, TagType};
        let original = Label::Literal(TagType::Custom("fsize".to_string()), "1MB".to_string());
        let range = ByteSizeRange::parse("1MB").unwrap().unwrap();
        let label = OperandFormat::to_label(&range, &original);
        assert_eq!(
            label,
            original.rekey(original.tag_type(), Bitical::Integer(1_048_576))
        );
    }

    #[test]
    fn test_byte_size_range_operand_format_to_label_pattern_falls_back_to_default() {
        use crate::types::TagType;
        let original = Label::Literal(TagType::Custom("fsize".to_string()), "*MB".to_string());
        let range = ByteSizeRange::parse("*MB").unwrap().unwrap();
        let label = OperandFormat::to_label(&range, &original);
        assert_eq!(label, original);
    }

    // --- DateTimeRange as OperandFormat ---

    #[test]
    fn test_date_time_range_operand_format_logical_type() {
        use crate::types::DateTimeRange;
        let range = DateTimeRange::parse("2026-02-01").unwrap().unwrap();
        assert_eq!(OperandFormat::logical_type(&range), LogicalType::Integer);
    }

    #[test]
    fn test_date_time_range_operand_format_to_label_point() {
        use crate::types::{DateTimeRange, TagType};
        let original =
            Label::Literal(TagType::Custom("captured".to_string()), "2026-02-01".to_string());
        let range = DateTimeRange::parse("2026-02-01").unwrap().unwrap();
        let (start, _) = range.as_interval().unwrap();
        let label = OperandFormat::to_label(&range, &original);
        assert_eq!(
            label,
            original.rekey(original.tag_type(), crate::types::Bitical::Integer(start))
        );
    }

    #[test]
    fn test_date_time_range_operand_format_to_label_pattern_falls_back_to_default() {
        use crate::types::{DateTimeRange, TagType};
        let original =
            Label::Literal(TagType::Custom("captured".to_string()), "*-02-01".to_string());
        let range = DateTimeRange::parse("*-02-01").unwrap().unwrap();
        let label = OperandFormat::to_label(&range, &original);
        assert_eq!(label, original);
    }

    // --- ItemIdRange（旧 tag::translate_item_id_origin_glob の型化） ---

    #[test]
    fn test_item_id_range_parses_origin_block() {
        use crate::types::Origin;
        let range = ItemIdRange::parse("File(*)").unwrap().unwrap();
        assert_eq!(range.lo, Origin::File.block_lo());
        assert_eq!(range.hi, Origin::File.block_hi() - 1);
    }

    #[test]
    fn test_item_id_range_declines_non_wildcard_offset() {
        assert_eq!(ItemIdRange::parse("File(5)"), None);
    }

    #[test]
    fn test_item_id_range_declines_unknown_origin() {
        assert_eq!(ItemIdRange::parse("Bogus(*)"), None);
    }

    #[test]
    fn test_item_id_range_declines_unmarked_forms() {
        for pattern in ["File", "5", "*", "abc"] {
            assert_eq!(
                ItemIdRange::parse(pattern),
                None,
                "should decline a form without the Origin(...) marker: {pattern}"
            );
        }
    }

    // --- ItemIdRange as OperandFormat ---

    #[test]
    fn test_item_id_range_operand_format_logical_type() {
        let range = ItemIdRange::parse("File(*)").unwrap().unwrap();
        assert_eq!(OperandFormat::logical_type(&range), LogicalType::Integer);
    }

    #[test]
    fn test_item_id_range_operand_format_to_label_defaults_to_clone() {
        use crate::types::TagType;
        let original = Label::Literal(TagType::Custom("item_id".to_string()), "File(*)".to_string());
        let range = ItemIdRange::parse("File(*)").unwrap().unwrap();
        let label = OperandFormat::to_label(&range, &original);
        assert_eq!(label, original);
    }

    // --- Bitical as OperandFormat（最下位。全部が譲ったら文字列のまま） ---

    #[test]
    fn test_bitical_operand_format_parse_never_declines() {
        use crate::types::Bitical;
        for s in ["42", "3.14", "true", "false", "hello", "1MB", ""] {
            assert!(
                matches!(Bitical::parse(s), Some(Ok(_))),
                "should always claim as the last resort: {s}"
            );
        }
    }

    #[test]
    fn test_bitical_operand_format_parse_prefers_integer() {
        use crate::types::Bitical;
        assert_eq!(Bitical::parse("42"), Some(Ok(Bitical::Integer(42))));
    }

    #[test]
    fn test_bitical_operand_format_parse_prefers_double_over_string() {
        use crate::types::Bitical;
        assert_eq!(Bitical::parse("3.14"), Some(Ok(Bitical::Double(3.14))));
    }

    #[test]
    fn test_bitical_operand_format_parse_recognizes_boolean() {
        use crate::types::Bitical;
        assert_eq!(Bitical::parse("true"), Some(Ok(Bitical::Boolean(true))));
        assert_eq!(Bitical::parse("false"), Some(Ok(Bitical::Boolean(false))));
    }

    #[test]
    fn test_bitical_operand_format_parse_falls_back_to_string() {
        use crate::types::Bitical;
        assert_eq!(
            Bitical::parse("hello"),
            Some(Ok(Bitical::String("hello".to_string())))
        );
    }

    #[test]
    fn test_bitical_operand_format_logical_type_matches_variant() {
        use crate::types::Bitical;
        assert_eq!(OperandFormat::logical_type(&Bitical::Integer(1)), LogicalType::Integer);
        assert_eq!(OperandFormat::logical_type(&Bitical::Double(1.0)), LogicalType::Float);
        assert_eq!(OperandFormat::logical_type(&Bitical::Boolean(true)), LogicalType::Boolean);
        assert_eq!(
            OperandFormat::logical_type(&Bitical::String("x".to_string())),
            LogicalType::String
        );
    }

    #[test]
    fn test_bitical_operand_format_to_label_defaults_to_clone() {
        use crate::types::{Bitical, TagType};
        let original = Label::Literal(TagType::Custom("custom".to_string()), "42".to_string());
        let range = Bitical::parse("42").unwrap().unwrap();
        let label = OperandFormat::to_label(&range, &original);
        assert_eq!(label, original);
    }
}


