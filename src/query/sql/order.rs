// Copyright (C) 2026 Kensuke Aoyagi
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

//! 解決済みの並び順（ResolvedOrder）を SQL の ORDER BY に適用する。
//! 直接カラムはそのまま、EAV タグ値のキーは src への相関サブクエリで引く。
//! キーのタグを持たない行は NULLS LAST で末尾に置く。

use super::subquery;
use crate::db::{Col, Pronoun::OrdSrc, Src, Tbl};
use crate::query::lens_resolver::{ResolvedOrder, ResolvedOrderKey};
use sea_query::{
    Alias, Expr, NullOrdering, Order as SqlOrder, Query, SelectStatement,
    SimpleExpr,
};

/// `src` の item_id を FROM 句の外（相関サブクエリ内）から一意に参照する式。
/// OneView はテーブル名、Parquet は IntoTableRef が付ける別名 "src" で修飾する。
pub(super) fn src_item_id(src: &Src) -> SimpleExpr {
    match src {
        Src::OneView => Expr::col((Tbl::OneView, Col::ItemId)).into(),
        Src::Parquet(_) => Expr::col((Alias::new("src"), Col::ItemId)).into(),
    }
}

/// 解決済みの並び順を `q` の ORDER BY に適用する。
/// `outer_item_id` は EAV タグ相関に使う「この行の item_id」を指す式
/// （呼び出し側の FROM/GROUP BY スコープで一意に参照できるもの）。
pub(super) fn apply_resolved_order(
    q: &mut SelectStatement,
    resolved: &[ResolvedOrder],
    src: &Src,
    outer_item_id: SimpleExpr,
) {
    for ro in resolved {
        let order = if ro.desc {
            SqlOrder::Desc
        } else {
            SqlOrder::Asc
        };
        match &ro.key {
            ResolvedOrderKey::Column(col) => {
                q.order_by_with_nulls(*col, order, NullOrdering::Last);
            }
            ResolvedOrderKey::Tag { tag_type, col } => {
                let mut sub = Query::select();
                sub.column((OrdSrc, *col))
                    .from_as(src, OrdSrc)
                    .and_where(
                        Expr::col((OrdSrc, Col::ItemId))
                            .eq(outer_item_id.clone()),
                    )
                    .and_where(
                        Expr::col((OrdSrc, Col::Type)).eq(tag_type.as_str()),
                    )
                    .limit(1);
                q.order_by_expr_with_nulls(
                    subquery(sub),
                    order,
                    NullOrdering::Last,
                );
            }
        }
    }
}
