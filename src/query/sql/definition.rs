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

//! 定義アイテム（`type:*` / `tag:"X"` 等の完全一致検索・glob検索）の SQL 構築。
//! エントリポイントは lens_builder の `filter_definitions` / `add_definitions` で、
//! pick / fetch パスからは lens 経由でのみ使用する。

use crate::db::{
    BiticalType, Col, CustomFunc,
    Pronoun::{Agg, Representative, Scalar, Stored, Sub, Volatile},
    QueryResultCol, Src, Val,
};
use crate::query::ast::Candidate;
use crate::types::{ItemId, ItemKind, Origin};
use sea_query::{
    BinOper, CaseStatement, CommonTableExpression, Expr, Func, JoinType, Query,
    SelectStatement, UnionType, WithClause,
};

/// 定義アイテムの name 絞り込みまで解決済みの DefinitionRef。
/// lens_builder の resolve_definition_name_filter が組み立て、
/// 本ファイルの SQL 構築関数群がそのまま消費する。
pub(crate) struct ResolvedDefinition {
    pub kind: ItemKind,
    pub pattern: String,
    pub exact: bool,
    pub candidates: Vec<Candidate>,
    pub origins: Vec<Origin>,
    pub reserved: Vec<String>,
    pub recorded: bool,
}

/// 定義アイテムの行本体（item_id, name, rank, item_kind）。
///
/// - Stored 定義行（ユーザー編集済み。同名の Volatile 合成に常に優先）
/// - 展開段階で candidates にされた登録型名＋default_rank
/// - データ中で使用中の型（type 定義）／タグ（tag 定義、user 由来）。
///   rank は既定値。candidates に同名があればそちらの default_rank を優先
///
/// 合成 Volatile 行の item_id は、candidates の3要素目（出所を表す ItemId）が
/// `Stored`（組み込み、with_standard 由来）ならその固定 Sys id を
/// そのままクエリに埋め込み、`Settling`（プラグイン登録、またはデータ中で使用中の未登録型/タグ）は
/// NULL。NULL の行は fetch 後、fetcher 側で `ItemId::new_volatile()` により
/// 一意な揮発 id を採番し、Settling の場合は続けて origin タグから
/// `settle()` される。
fn build_definition_rows(
    src: &Src,
    resolved: &ResolvedDefinition,
) -> SelectStatement {
    let ResolvedDefinition {
        kind,
        candidates,
        origins,
        reserved,
        recorded,
        ..
    } = resolved;
    let kind = *kind;
    let recorded = *recorded;
    let mut with_clause = WithClause::new();

    // Stored 定義行。
    let stored_q = Query::select()
        .column(Col::ItemId)
        .expr_as(Expr::col(Col::LabelStr), Col::Name)
        .column(Col::Rank)
        .from(src)
        .and_where(Expr::col(Col::Type).eq("content"))
        .and_where(Expr::col(Col::ItemKind).eq(kind.as_str()))
        .to_owned();
    with_clause.cte(
        CommonTableExpression::new()
            .query(stored_q)
            .table_name(Stored)
            .to_owned(),
    );

    // データ中で使用中の型/タグ。
    let used_col = match kind {
        ItemKind::Tag => Col::TypedTag,
        _ => Col::Type,
    };
    let mut used_q = Query::select();
    used_q
        .distinct()
        .expr_as(Expr::col(used_col), Col::Name)
        .expr_as(Expr::val(crate::rank::SystemRank::DEFAULT), Col::Rank)
        .expr_as(Expr::val(None::<i64>), Col::ItemId)
        .from(src);
    if matches!(kind, ItemKind::Tag) {
        // タグ定義はユーザーデータ。system 由来の (type, label) ペアは含めない。
        used_q.and_where(Expr::col(Col::Origin).eq("user"));
    }
    let excluded_names: Vec<&str> = candidates
        .iter()
        .map(|c| c.name.as_str())
        .chain(reserved.iter().map(|s| s.as_str()))
        .collect();
    if !excluded_names.is_empty() {
        used_q.and_where(Expr::col(used_col).is_not_in(excluded_names));
    }
    if !origins.is_empty() {
        used_q.and_where(
            Expr::col(Col::Origin)
                .is_in(origins.iter().map(|o| o.as_str().to_string())),
        );
    }

    // candidates（定数リスト）∪ データ中で使用中の型/タグ。組み込み（Stored）は
    // candidates の 3要素目に固定 Sys id を持つ（プラグイン登録・データ中で
    // 使用中の Settling は NULL）。
    let mut raw_q: Option<SelectStatement> = None;
    for c in candidates {
        let sys_id: Option<i64> = c.id.is_stored().then(|| c.id.as_i64());
        let row = Query::select()
            .expr_as(Expr::val(c.name.as_str()), Col::Name)
            .expr_as(Expr::val(c.rank), Col::Rank)
            .expr_as(Expr::val(sys_id), Col::ItemId)
            .to_owned();
        match &mut raw_q {
            Some(q) => {
                q.union(UnionType::All, row);
            }
            None => raw_q = Some(row),
        }
    }
    let raw_q = match raw_q {
        Some(mut q) => {
            q.union(UnionType::All, used_q);
            q
        }
        None => used_q,
    };

    // Stored に同名がある候補を除外（Stored 優先の anti-join）。item_id は
    // 組み込みなら固定 Sys id、それ以外（プラグイン登録・データ中で使用中）は NULL
    // （raw_q からそのまま引き継ぐ）。
    let volatile_q = Query::select()
        .column(Col::ItemId)
        .column(Col::Name)
        .column(Col::Rank)
        .from_subquery(raw_q, Sub)
        .and_where(Expr::col(Col::Name).not_in_subquery(
            Query::select().column(Col::Name).from(Stored).to_owned(),
        ))
        .to_owned();
    with_clause.cte(
        CommonTableExpression::new()
            .query(volatile_q)
            .table_name(Volatile)
            .to_owned(),
    );

    // 固定 Sys id を持つ組み込み（item_id 非 NULL）は仕様上 Stored なので
    // item_kind は kind そのもの。id 未確定（Settling/未登録）のみ volatile。
    let volatile_sel = Query::select()
        .column(Col::ItemId)
        .column(Col::Name)
        .column(Col::Rank)
        .expr_as(
            Expr::case(
                Expr::col(Col::ItemId).is_not_null(),
                Expr::val(kind.as_str()),
            )
            .finally(Expr::val(ItemKind::Volatile.as_str())),
            Col::ItemKind,
        )
        .from(Volatile)
        .to_owned();

    // recorded=false（OriginFn 経由）は Stored 行を出力しない。ColumnMatch 枝が
    // 物理化済み item を既に拾うため、DefinitionRef 枝は未物理化の Volatile 合成
    // のみ担えばよい（Stored CTE 自体は上の anti-join に必要なので残す）。
    let mut q = if recorded {
        let mut stored_sel = Query::select();
        stored_sel
            .column(Col::ItemId)
            .column(Col::Name)
            .column(Col::Rank)
            .expr_as(Expr::val(kind.as_str()), Col::ItemKind)
            .from(Stored);
        stored_sel.union(UnionType::All, volatile_sel);
        stored_sel
    } else {
        volatile_sel
    };
    q.with_cte(with_clause);
    q
}

/// `^prefix` 形式を前方一致 glob に変換する（build_column_match_sql と同じ規則）。
fn glob_pattern(s: &str) -> String {
    match s.strip_prefix('^') {
        Some(rest) => format!("{}*", rest),
        None => s.to_string(),
    }
}

/// 定義アイテムの name 列に対する絞り込み条件を返す。
/// glob検索（unquoted）は glob パターンマッチ、完全一致検索（quoted literal）は
/// クオート意味論を維持するため等値比較。
fn name_condition(pattern: &str, exact: bool) -> sea_query::SimpleExpr {
    if exact {
        Expr::col(Col::Name).eq(pattern)
    } else {
        Expr::col(Col::Name)
            .binary(BinOper::Custom("GLOB"), Expr::val(glob_pattern(pattern)))
    }
}

/// 定義アイテムを name で絞り込んだ pick SQL（item_id, rank, item_kind）。
/// `exact` が true なら等値比較（完全一致検索）、false なら glob パターンマッチ（glob検索）。
pub(crate) fn build_definition_pick_sql(
    src: &Src,
    resolved: &ResolvedDefinition,
) -> SelectStatement {
    Query::select()
        .column(Col::ItemId)
        .column(Col::Rank)
        .column(Col::ItemKind)
        .from_subquery(build_definition_rows(src, resolved), Sub)
        .and_where(name_condition(&resolved.pattern, resolved.exact))
        .to_owned()
}

/// 定義アイテムを name で絞り込んだ fetch SQL（tags・representative・name 付き）。
/// 列順は src 由来の items fetch（definition union 用の列追加後）と位置を
/// 揃える: item_id, rank, item_kind, tags, representative, name。
/// name 列は UNION 後の並び替え専用（デコードには使わない）。
/// tags は DB にタグ行があるか（実タグの LEFT JOIN 結果が NULL か）で
/// 判別する。DBから取得した定義行は必ずタグ行（content 行）を持つため
/// 実 EAV タグを載せ、タグ行を持たない行（組み込みの固定 Sys id・
/// 使用中の型/タグ等）は type/name/origin タグをクエリ時に組み立てる。
/// limit/offset は適用しない（Or で他枝と UNION した後にまとめて適用するため、
/// 呼び出し側が責任を持つ）。
pub(crate) fn build_definition_fetch_sql(
    src: &Src,
    resolved: &ResolvedDefinition,
) -> SelectStatement {
    let kind = resolved.kind;
    let candidates = &resolved.candidates;
    let union_val = CustomFunc::eav_union_value();
    let struct_expr = CustomFunc::struct_pack_tag(
        Expr::col(Col::Type).into(),
        union_val,
        Expr::col(Col::Origin).into(),
    );
    let tags_agg_sql = Query::select()
        .column(Col::ItemId)
        .expr_as(CustomFunc::list(struct_expr), QueryResultCol::Tags)
        .from(src)
        .group_by_col(Col::ItemId)
        .to_owned();

    // origin タグの値: candidates の3要素目（ItemId）が Settling(Plugin, _)
    // の名前は Val::Plugin、item_id が非 NULL（Stored = 固定 Sys id を持つ
    // 組み込み）は Val::Builtin、どちらでもなければデータ中で使用中の
    // 未登録型/タグの Val::User。「レジストリに実在する候補か」は ItemId の
    // variant 自体が表すため、sys_id の有無からの推測は行わない
    // （完全一致検索が合成するプレースホルダは Settling(User, _) になり、
    // プラグインと混同しない）。
    let builtin: &'static str = Val::Builtin.into();
    let user: &'static str = Val::User.into();
    let plugin_names: Vec<&str> = candidates
        .iter()
        .filter(|c| {
            matches!(c.id, ItemId::Settling(crate::types::Origin::Plugin, _))
        })
        .map(|c| c.name.as_str())
        .collect();
    let mut origin_value = CaseStatement::new();
    if !plugin_names.is_empty() {
        let plugin: &'static str = Val::Plugin.into();
        origin_value = origin_value
            .case(Expr::col((Sub, Col::Name)).is_in(plugin_names), plugin);
    }
    let origin_value = origin_value
        .case(Expr::col((Sub, Col::ItemId)).is_not_null(), builtin)
        .finally(user);

    // type:/name:/origin: 自体は常にクエリ時にエンジンが合成する信号タグであり、
    // Builtin (TTFM エンジン自身) が origin となる。

    let volatile_type_tag = CustomFunc::list_value([
        CustomFunc::struct_pack_tag(
            Expr::val(Col::Type.as_str()).into(),
            CustomFunc::union_value(
                BiticalType::String,
                Expr::val(kind.as_str()),
            ),
            Expr::val(builtin).into(),
        ),
        CustomFunc::struct_pack_tag(
            Expr::val(Col::Name.as_str()).into(),
            CustomFunc::union_value(
                BiticalType::String,
                Expr::col((Sub, Col::Name)),
            ),
            Expr::val(builtin).into(),
        ),
        CustomFunc::struct_pack_tag(
            Expr::val(Col::Origin.as_str()).into(),
            CustomFunc::union_value(BiticalType::String, origin_value),
            Expr::val(builtin).into(),
        ),
    ]);
    let stored_tags = Func::coalesce([
        Expr::col((Agg, QueryResultCol::Tags)).into(),
        CustomFunc::list_value([]),
    ]);

    let mut q = Query::select();
    q.column((Sub, Col::ItemId))
        .column((Sub, Col::Rank))
        .column((Sub, Col::ItemKind))
        .expr_as(
            Expr::case(
                Expr::col((Agg, QueryResultCol::Tags)).is_null(),
                volatile_type_tag,
            )
            .finally(stored_tags),
            QueryResultCol::Tags,
        )
        .expr_as(
            CustomFunc::as_representative(Expr::col((Sub, Col::Name))),
            Representative,
        )
        .column((Sub, Col::Name))
        .from_subquery(build_definition_rows(src, resolved), Sub)
        .join_subquery(
            JoinType::LeftJoin,
            tags_agg_sql,
            Agg,
            Expr::col((Sub, Col::ItemId)).equals((Agg, Col::ItemId)),
        )
        .and_where(name_condition(&resolved.pattern, resolved.exact));
    q
}

/// 定義アイテムを name で絞り込んだ、name 1列のみの SQL（カウント用）。
/// 複数枝を UNION (Distinct) で束ねて枝間の name 重複を排除するために使う。
pub(crate) fn build_definition_name_sql(
    src: &Src,
    resolved: &ResolvedDefinition,
) -> SelectStatement {
    Query::select()
        .column(Col::Name)
        .from_subquery(build_definition_rows(src, resolved), Sub)
        .and_where(name_condition(&resolved.pattern, resolved.exact))
        .to_owned()
}

/// 定義枝（複数可）が指す定義アイテムの distinct 件数を返すスカラー SQL。
/// 各枝は build_definition_name_sql と同一の列（name 1列）であること。
/// UNION (Distinct) で枝間の name 重複を排除済みのため COUNT(*) でよい。
pub(crate) fn build_definitions_count_sql(
    branches: Vec<SelectStatement>,
) -> SelectStatement {
    let unioned = branches
        .into_iter()
        .reduce(|mut acc, next| {
            acc.union(UnionType::Distinct, next);
            acc
        })
        .expect("branches must not be empty");
    Query::select()
        .expr_as(CustomFunc::count_star(), Scalar)
        .from_subquery(unioned, Sub)
        .to_owned()
}

/// 定義枝（と src 由来枝）の fetch SQL 群を UNION で束ね、並び替えと
/// limit/offset を UNION 後に適用する。各枝は build_definition_fetch_sql と
/// 同一の列順であること。
///
/// 並び替えは `orders`（明示的な並び順）を先頭に適用し、既定の並び
/// （rank DESC, name ASC NULLS LAST, item_id DESC NULLS LAST）を
/// タイブレーカーとして残す。name は定義行では EAV タグではなく列として
/// 持つ（id 未確定の行はタグ行を持たない）ため、name キーは列に読み替える。
pub(crate) fn build_add_definitions_sql(
    src: &Src,
    branches: Vec<SelectStatement>,
    orders: &[crate::query::lens_resolver::ResolvedOrder],
    n: usize,
    offset: usize,
) -> SelectStatement {
    use super::util::wrap_in_subquery_star;
    use crate::query::lens_resolver::{ResolvedOrder, ResolvedOrderKey};
    use sea_query::{NullOrdering, Order};

    let unioned = branches
        .into_iter()
        .map(wrap_in_subquery_star)
        .reduce(|mut acc, next| {
            acc.union(UnionType::Distinct, next);
            acc
        })
        .expect("branches must not be empty");

    let mut q = if orders.is_empty() {
        unioned
    } else {
        // EAV タグ値の相関サブクエリから UNION 行の item_id を参照するため
        // サブクエリで包んで別名を付ける。
        use crate::db::Pronoun::View;
        let mut outer = Query::select()
            .column(sea_query::Asterisk)
            .from_subquery(unioned, View)
            .to_owned();
        let mapped: Vec<ResolvedOrder> = orders
            .iter()
            .map(|o| match &o.key {
                ResolvedOrderKey::Tag { tag_type, .. }
                    if tag_type == Col::Name.as_str() =>
                {
                    ResolvedOrder {
                        key: ResolvedOrderKey::Column(Col::Name),
                        desc: o.desc,
                    }
                }
                _ => o.clone(),
            })
            .collect();
        super::order::apply_resolved_order(
            &mut outer,
            &mapped,
            src,
            Expr::col((View, Col::ItemId)).into(),
        );
        outer
    };
    q.order_by(Col::Rank, Order::Desc)
        .order_by_with_nulls(Col::Name, Order::Asc, NullOrdering::Last)
        .order_by_with_nulls(Col::ItemId, Order::Desc, NullOrdering::Last);
    if n > 0 {
        q.limit((n + 1) as u64);
    }
    if offset > 0 {
        q.offset(offset as u64);
    }
    q
}
