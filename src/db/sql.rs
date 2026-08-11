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

//! DuckDB 固有の SQL 式を型安全に構築するヘルパー群（`CustomFunc`）。
//!
//! 検索（query/sql）・採番（indexing）の双方から使われる SQL パーツの住所。
//! db 土台層に置くことで indexing → query の逆流依存を避ける。

use super::{BiticalType, Col, DuckDbFunc};

/// DuckDB 固有の複雑な構文を型安全に構築するためのヘルパー。
pub struct CustomFunc;

impl CustomFunc {
    /// TRY_CAST(expr AS BIGINT) を生成します。
    pub fn try_cast_bigint<E: Into<sea_query::SimpleExpr>>(
        expr: E,
    ) -> sea_query::SimpleExpr {
        sea_query::Expr::cust_with_exprs(
            "TRY_CAST($1 AS BIGINT)",
            [expr.into()],
        )
    }

    /// list(expr) を生成します。
    pub fn list<E: Into<sea_query::SimpleExpr>>(
        expr: E,
    ) -> sea_query::SimpleExpr {
        sea_query::Func::cust(DuckDbFunc::List)
            .arg(expr.into())
            .into()
    }

    /// list(expr ORDER BY ...) を生成します。
    pub fn list_with_order<E, O>(
        expr: E,
        order_bys: Vec<(O, sea_query::Order)>,
    ) -> sea_query::SimpleExpr
    where
        E: Into<sea_query::SimpleExpr>,
        O: sea_query::IntoIden,
    {
        let mut sql = "list($1".to_string();
        if !order_bys.is_empty() {
            sql.push_str(" ORDER BY ");
            let orders = order_bys
                .into_iter()
                .map(|(col, ord)| {
                    let mut s = String::new();
                    col.into_iden().unquoted(&mut s);
                    format!(
                        "\"{}\" {}",
                        s,
                        if matches!(ord, sea_query::Order::Asc) {
                            "ASC"
                        } else {
                            "DESC"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&orders);
        }
        sql.push(')');
        sea_query::Expr::cust_with_exprs(sql, [expr.into()])
    }

    /// struct_pack(col1 := col1, ...) を生成します。
    pub fn struct_pack<I>(columns: &[I]) -> sea_query::SimpleExpr
    where
        I: sea_query::IntoIden + Clone,
    {
        let fields = columns
            .iter()
            .map(|c| {
                let mut s = String::new();
                c.clone().into_iden().unquoted(&mut s);
                format!("\"{}\" := \"{}\"", s, s)
            })
            .collect::<Vec<_>>()
            .join(", ");
        sea_query::Expr::cust(format!("struct_pack({})", fields))
    }

    /// list_slice(list, start, end) を生成します。
    pub fn list_slice<E: Into<sea_query::SimpleExpr>>(
        expr: E,
        start: usize,
        end: usize,
    ) -> sea_query::SimpleExpr {
        sea_query::Func::cust(DuckDbFunc::ListSlice)
            .args([
                expr.into(),
                sea_query::Expr::val(start as i64).into(),
                sea_query::Expr::val(end as i64).into(),
            ])
            .into()
    }

    /// MAX(expr) FILTER (WHERE cond) を生成します。
    pub fn max_filter<E, F>(expr: E, filter_expr: F) -> sea_query::SimpleExpr
    where
        E: Into<sea_query::SimpleExpr>,
        F: Into<sea_query::SimpleExpr>,
    {
        sea_query::Expr::cust_with_exprs(
            "MAX($1) FILTER (WHERE $2)",
            [expr.into(), filter_expr.into()],
        )
    }

    /// ANY_VALUE(expr) FILTER (WHERE cond) を生成します。
    pub fn any_value_filter<E, F>(
        expr: E,
        filter_expr: F,
    ) -> sea_query::SimpleExpr
    where
        E: Into<sea_query::SimpleExpr>,
        F: Into<sea_query::SimpleExpr>,
    {
        sea_query::Expr::cust_with_exprs(
            "ANY_VALUE($1) FILTER (WHERE $2)",
            [expr.into(), filter_expr.into()],
        )
    }

    /// any_value(expr) を生成します。
    pub fn any_value<E: Into<sea_query::SimpleExpr>>(
        expr: E,
    ) -> sea_query::SimpleExpr {
        sea_query::Func::cust(DuckDbFunc::AnyValue)
            .arg(expr.into())
            .into()
    }

    /// ID割り当て用のウィンドウ関数式を生成します。
    /// start_id を先頭（最初の行）として昇順に採番します
    /// （Builtin 区画内で start_id, start_id+1, ... と積み上げる）。
    pub fn assign_id_window(start_id: i64) -> sea_query::SimpleExpr {
        sea_query::Expr::cust_with_exprs(
            "$1 + (row_number() OVER (ORDER BY rank DESC, content ASC) - 1)",
            [sea_query::Expr::val(start_id).into()],
        )
    }

    /// item_id をローカル形式 `"Sys(N)"` / `"User(N)"` に変換する SQL CASE 式を返す。
    /// `item_id_expr` は SQL 中で item_id を参照する式文字列（テーブル修飾可）。
    /// `within` と同じく Origin を lo 降順でスキャンし、
    /// 各区画に対して `value_for(origin)` が返す SQL 式を CASE ブランチに並べる。
    /// 最低 lo の区画だけ ELSE になる（`within` の `unwrap_or` 相当）。
    fn item_id_case_expr<F>(item_id_expr: &str, value_for: F) -> String
    where
        F: Fn(crate::types::Origin) -> String,
    {
        use crate::types::Origin;
        use strum::IntoEnumIterator;
        let mut origins: Vec<(i64, Origin)> =
            Origin::iter().map(|o| (o.block_lo(), o)).collect();
        origins.sort_by(|a, b| b.0.cmp(&a.0));

        let mut sql = format!("CASE WHEN {item_id_expr} IS NULL THEN NULL");
        for (i, &(lo, origin)) in origins.iter().enumerate() {
            let val = value_for(origin);
            if i < origins.len() - 1 {
                sql.push_str(&format!(
                    " WHEN {item_id_expr} >= {lo} THEN {val}"
                ));
            } else {
                sql.push_str(&format!(" ELSE {val} END"));
            }
        }
        sql
    }

    pub fn item_id_display(item_id_expr: &str) -> String {
        Self::item_id_case_expr(item_id_expr, |o| {
            let lo = o.block_lo();
            let label = o.short();
            format!("CONCAT('{label}(', CAST({item_id_expr} - ({lo}) AS VARCHAR), ')')")
        })
    }

    /// item_id から Origin 名（"builtin" / "user" / "file" / "plugin"）を返す SQL 式。
    pub fn item_id_origin(item_id_expr: &str) -> String {
        Self::item_id_case_expr(item_id_expr, |o| format!("'{o}'"))
    }

    pub fn item_id_origin_qualified(tbl: super::Tbl, col: Col) -> String {
        let qualified =
            format!("{}.{}", sea_query::Iden::to_string(&tbl), col);
        Self::item_id_origin(&qualified)
    }

    /// row_number() OVER (PARTITION BY ... ORDER BY ...) を生成します。
    pub fn row_number_over<P, O>(
        partition_by: P,
        order_bys: Vec<(O, sea_query::Order)>,
    ) -> sea_query::SimpleExpr
    where
        P: sea_query::IntoIden,
        O: sea_query::IntoIden,
    {
        let mut sql = "row_number() OVER (PARTITION BY ".to_string();
        let mut p_name = String::new();
        partition_by.into_iden().unquoted(&mut p_name);
        sql.push_str(&format!("\"{}\"", p_name));

        if !order_bys.is_empty() {
            sql.push_str(" ORDER BY ");
            let orders = order_bys
                .into_iter()
                .map(|(col, ord)| {
                    let mut s = String::new();
                    col.into_iden().unquoted(&mut s);
                    format!(
                        "\"{}\" {}",
                        s,
                        if matches!(ord, sea_query::Order::Asc) {
                            "ASC"
                        } else {
                            "DESC"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&orders);
        }
        sql.push(')');
        sea_query::Expr::cust(sql)
    }

    /// タグ値の UNION 型。アーム名は `BiticalType::union_arm`（収束先）の Display、
    /// 型名は Iden 綴り、順序は BiticalType の宣言順から導出します。
    /// 同一アームに収束する型は最初の1つだけ列挙します。
    pub fn union_type() -> String {
        use strum::IntoEnumIterator;
        let mut seen = Vec::new();
        let arms: Vec<String> = BiticalType::iter()
            .filter_map(|t| {
                let arm = t.union_arm();
                if seen.contains(&arm) {
                    return None;
                }
                seen.push(arm);
                Some(format!(
                    "\"{}\" {}",
                    arm,
                    sea_query::Iden::to_string(&arm)
                ))
            })
            .collect();
        format!("UNION({})", arms.join(", "))
    }

    /// union_value(arm := expr)::UNION(...) — BiticalType に対応する UNION アームを生成し、
    /// 完全な UNION 型にキャストします。CASE 式の型統一に必要です。
    pub fn union_value<E: Into<sea_query::SimpleExpr>>(
        bitical_type: BiticalType,
        expr: E,
    ) -> sea_query::SimpleExpr {
        sea_query::Expr::cust_with_exprs(
            &format!(
                "union_value(\"{}\" := $1)::{}",
                bitical_type.union_arm(),
                Self::union_type()
            ),
            [expr.into()],
        )
    }

    /// struct_pack("tag_type" := t, "value" := v, "origin" := o) を生成します。
    pub fn struct_pack_tag(
        tag_type: sea_query::SimpleExpr,
        value: sea_query::SimpleExpr,
        origin: sea_query::SimpleExpr,
    ) -> sea_query::SimpleExpr {
        sea_query::Expr::cust_with_exprs(
            "struct_pack(\"tag_type\" := $1, \"value\" := $2, \"origin\" := $3)",
            [tag_type, value, origin],
        )
    }

    /// list_value(v1, v2, ...) を生成します。
    pub fn list_value(
        exprs: impl IntoIterator<Item = sea_query::SimpleExpr>,
    ) -> sea_query::SimpleExpr {
        sea_query::Func::cust(DuckDbFunc::ListValue)
            .args(exprs)
            .into()
    }

    /// 代表値リスト要素の UNION 型。全 BiticalType が独立アームを持ち、
    /// アーム名は Display、型名は Iden 綴り、順序は宣言順から導出します。
    pub fn representative_union_type() -> String {
        use strum::IntoEnumIterator;
        let arms: Vec<String> = BiticalType::iter()
            .map(|t| format!("\"{}\" {}", t, sea_query::Iden::to_string(&t)))
            .collect();
        format!("UNION({})", arms.join(", "))
    }

    /// スカラー値を代表値リスト型 LIST(UNION(...)) に変換します。
    pub fn as_representative<E: Into<sea_query::SimpleExpr>>(
        expr: E,
    ) -> sea_query::SimpleExpr {
        sea_query::Expr::cust_with_exprs(
            &format!(
                "list_value(CAST($1 AS {}))",
                Self::representative_union_type()
            ),
            [expr.into()],
        )
    }

    /// DuckDB `* REPLACE(value AS col)` を生成する。`*`（全列）を出しつつ
    /// 指定列 `col` だけ `value` に差し替える select 項。
    pub fn star_replace<E: Into<sea_query::SimpleExpr>>(
        col: Col,
        value: E,
    ) -> sea_query::SimpleExpr {
        sea_query::Expr::cust_with_exprs(
            &format!(
                "* REPLACE($1 AS \"{}\")",
                sea_query::Iden::to_string(&col)
            ),
            [value.into()],
        )
    }

    /// typeof(expr) を生成します。
    pub fn type_of<E: Into<sea_query::SimpleExpr>>(
        expr: E,
    ) -> sea_query::SimpleExpr {
        sea_query::Func::cust(DuckDbFunc::TypeOf)
            .arg(expr.into())
            .into()
    }

    /// EAV の型付きラベルカラムを走査し、非 NULL のカラムを対応する UNION アーム
    /// に変換する CASE 式を生成します。カラムと型の対応は
    /// `BiticalType::from_col`、走査順は `BiticalType::to_columns_scan_order`
    /// から導出します。
    pub fn eav_union_value() -> sea_query::SimpleExpr {
        let arms = BiticalType::to_columns_scan_order()
            .map(|c| (c, BiticalType::from_col(c)));
        let ((first_col, first_type), rest) =
            arms.split_first().expect("EAV columns are non-empty");
        let init = sea_query::Expr::case(
            sea_query::Expr::col(*first_col).is_not_null(),
            Self::union_value(*first_type, sea_query::Expr::col(*first_col)),
        );
        rest.iter()
            .fold(init, |cs, (col, bitical_type)| {
                cs.case(
                    sea_query::Expr::col(*col).is_not_null(),
                    Self::union_value(
                        *bitical_type,
                        sea_query::Expr::col(*col),
                    ),
                )
            })
            .finally(sea_query::Expr::val(Option::<String>::None))
            .into()
    }

    /// TRY_CAST(expr AS DOUBLE) を生成します。
    pub fn try_cast_double<E: Into<sea_query::SimpleExpr>>(
        expr: E,
    ) -> sea_query::SimpleExpr {
        sea_query::Expr::cust_with_exprs(
            "TRY_CAST($1 AS DOUBLE)",
            [expr.into()],
        )
    }

    /// COUNT(*) を生成します。
    pub fn count_star() -> sea_query::SimpleExpr {
        sea_query::Func::cust(DuckDbFunc::Count)
            .arg(sea_query::Expr::cust("*"))
            .into()
    }

    /// string_agg(expr, separator) を生成します。
    pub fn string_agg<E, S>(expr: E, separator: S) -> sea_query::SimpleExpr
    where
        E: Into<sea_query::SimpleExpr>,
        S: Into<sea_query::SimpleExpr>,
    {
        sea_query::Func::cust(DuckDbFunc::StringAgg)
            .args([expr.into(), separator.into()])
            .into()
    }

    /// count(*) OVER (PARTITION BY col) を生成します。
    pub fn count_over<P>(partition_by: P) -> sea_query::SimpleExpr
    where
        P: sea_query::IntoIden,
    {
        Self::count_over_multi(&[partition_by.into_iden()])
    }

    /// count(*) OVER (PARTITION BY col1, col2, ...) を生成します。
    pub fn count_over_multi(
        partition_cols: &[sea_query::DynIden],
    ) -> sea_query::SimpleExpr {
        let cols = partition_cols
            .iter()
            .map(|c| {
                let mut s = String::new();
                c.unquoted(&mut s);
                format!("\"{}\"", s)
            })
            .collect::<Vec<_>>()
            .join(", ");
        sea_query::Expr::cust(format!("count(*) OVER (PARTITION BY {})", cols))
    }

    /// row_number() OVER (PARTITION BY col1, col2, ... ORDER BY ...) を生成します。
    pub fn row_number_over_multi<O>(
        partition_cols: &[sea_query::DynIden],
        order_bys: Vec<(O, sea_query::Order)>,
    ) -> sea_query::SimpleExpr
    where
        O: sea_query::IntoIden,
    {
        let cols = partition_cols
            .iter()
            .map(|c| {
                let mut s = String::new();
                c.unquoted(&mut s);
                format!("\"{}\"", s)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql = format!("row_number() OVER (PARTITION BY {}", cols);
        if !order_bys.is_empty() {
            sql.push_str(" ORDER BY ");
            let orders = order_bys
                .into_iter()
                .map(|(col, ord)| {
                    let mut s = String::new();
                    col.into_iden().unquoted(&mut s);
                    format!(
                        "\"{}\" {}",
                        s,
                        if matches!(ord, sea_query::Order::Asc) {
                            "ASC"
                        } else {
                            "DESC"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&orders);
        }
        sql.push(')');
        sea_query::Expr::cust(sql)
    }
}
