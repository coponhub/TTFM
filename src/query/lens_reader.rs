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

//! read 方向の値解決（`ReadResolution`）と、それを read 用 SELECT へ組み立てる関数群。
//!
//! `TagFunction::read()` が宣言する type 固有の解決規則（現状 `Prefer`）を消費し、
//! 与えられたソース（`from`）の行を per-item で解決した SELECT を生成する。
//! 解決結果をどこで使うか（どのビュー構築に渡すか）は呼び出し側が決め、ここでは固定しない。

use crate::db::{Col, Pronoun, Tbl};
use crate::query::lens_schema::{Lens, StorageMapping};
use crate::tag::TagRegistry;
use crate::types::{TagType, TypedTag};
use sea_query::{
    Asterisk, Expr, Order, PostgresQueryBuilder, Query, SimpleExpr, UnionType,
};

/// read 解決の規則。コンビネータが生成する単位。
#[derive(Clone, Debug)]
pub enum ReadRule {
    /// この TypedTag に一致する候補を優先する（一致軸の列は lens で解決）。
    Prefer(TypedTag),
    /// 指定 type の行を解決対象 type に relabel した候補として追加する
    /// （real より低優先。name のデフォルト=filename 等のフォールバックを表す）。
    Fallback(TagType),
}

/// read 時の値解決。`ReadRule` の列（合成可能な不定形）。
/// デフォルト（空）＝passthrough（reduce 無し）。`TagFunction::read()` の戻り値。
#[derive(Clone, Debug, Default)]
pub struct ReadResolution(Vec<ReadRule>);

impl ReadResolution {
    /// コンビネータ: 指定 TypedTag に一致する候補を優先する規則を加える。
    pub fn prefer(mut self, tag: TypedTag) -> Self {
        self.0.push(ReadRule::Prefer(tag));
        self
    }

    /// コンビネータ: 指定 type の行を解決対象 type の候補として足す（フォールバック）。
    pub fn fallback(mut self, from_type: TagType) -> Self {
        self.0.push(ReadRule::Fallback(from_type));
        self
    }

    pub(crate) fn rules(&self) -> &[ReadRule] {
        &self.0
    }

    /// `from` の `target` 型の行を per-item 1件へ解決した SELECT 文字列を返す。
    /// `Fallback` は候補 arm（指定 type を `target` に relabel）を足し、`Prefer` は
    /// `DISTINCT ON (item_id)` の順序で優先を決める（real が fallback より先）。
    fn resolution_select(
        &self,
        lens: &Lens,
        from: Tbl,
        target: &str,
    ) -> String {
        // 候補: base（type=target）＋ Fallback で relabel した arm。
        let mut candidates = Query::select();
        candidates
            .column(Asterisk)
            .from(from)
            .and_where(Expr::col(Col::Type).eq(target));
        for rule in self.rules() {
            if let ReadRule::Fallback(ft) = rule {
                // type 列だけ target に relabel（他列は素通り）。
                let mut arm = Query::select();
                arm.expr(crate::db::CustomFunc::star_replace(
                    Col::Type,
                    target,
                ))
                .from(from)
                .and_where(Expr::col(Col::Type).eq(ft.as_str()));
                candidates.union(UnionType::All, arm.take());
            }
        }

        // 候補から DISTINCT ON (item_id) で 1件に解決（Prefer の順序＋非NULL 優先）。
        let mut picked = Query::select();
        picked
            .distinct_on([Col::ItemId])
            .column(Asterisk)
            .from_subquery(candidates.take(), Pronoun::Sub)
            .order_by(Col::ItemId, Order::Asc);
        for rule in self.rules() {
            if let ReadRule::Prefer(tt) = rule {
                if let Some(expr) = prefer_match_expr(lens, tt) {
                    picked.order_by_expr(expr, Order::Desc);
                }
            }
        }
        picked.order_by_expr(
            Expr::col(Col::LabelStr)
                .is_null()
                .and(Expr::col(Col::LabelInt).is_null())
                .and(Expr::col(Col::LabelDouble).is_null())
                .and(Expr::col(Col::LabelBool).is_null()),
            Order::Asc,
        );

        // ORDER BY 式は UNION ALL の arm 直下では使えないため、サブクエリでラップして隔離する。
        let mut wrapped = Query::select();
        wrapped
            .column(Asterisk)
            .from_subquery(picked.take(), Pronoun::Sub);
        wrapped.to_string(PostgresQueryBuilder)
    }
}

/// `Prefer(TypedTag)` の一致式 `col = val` を構築する（列・値は lens で解決）。
fn prefer_match_expr(lens: &Lens, tt: &TypedTag) -> Option<SimpleExpr> {
    let plabel = &tt.label;
    let pcol = match lens.look_up_or_default(&tt.tag_type()).storage {
        StorageMapping::Fixed(c) => c,
        StorageMapping::Basic { column, .. } => column,
        StorageMapping::Composite => return None,
    };
    let (_, pval) = plabel.value().to_col_expr();
    Some(Expr::col(pcol).eq(pval))
}

/// read 解決済みの SELECT 群。`Reader::build` で構築し、ビュー構築（`OneView::recreate` 等）へ渡す。
pub struct Reader(Vec<String>);

impl Reader {
    /// read 解決済みの `Reader` を構築する。`from` の各行に対し、`read()` に規則を持つ
    /// type を per-item で解決し、規則を持たない type は素通りさせる。
    /// 規則を持つ type が無ければ None（解決不要）を返す。
    pub fn build(registry: &TagRegistry, from: Tbl) -> Option<Reader> {
        let lens = Lens::from_registry(registry);

        let overridden: Vec<(TagType, ReadResolution)> = lens
            .descriptors()
            .filter_map(|(t, d)| {
                let res = d.logical_function.as_ref()?.query().read();
                (!res.rules().is_empty()).then(|| (t.clone(), res))
            })
            .collect();
        if overridden.is_empty() {
            return None;
        }
        let overridden_types: Vec<String> = overridden
            .iter()
            .map(|(t, _)| t.as_str().to_string())
            .collect();

        let mut selects = Vec::new();
        // default read = passthrough: 解決対象でない type は全行そのまま。
        let mut pass = Query::select();
        pass.column(Asterisk).from(from).and_where(
            Expr::col(Col::Type).is_not_in(overridden_types.clone()),
        );
        selects.push(pass.to_string(PostgresQueryBuilder));
        // 規則を持つ type ごとに per-item 1件へ解決。
        for (ty, res) in &overridden {
            selects.push(res.resolution_select(&lens, from, ty.as_str()));
        }
        Some(Reader(selects))
    }

    /// 結合対象の SELECT 群。
    pub(crate) fn selects(&self) -> &[String] {
        &self.0
    }
}
