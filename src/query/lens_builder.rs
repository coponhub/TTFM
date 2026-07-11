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

//! lens が発行するクエリビルダー（複数行の SQL を返すエントリポイント群）。
//! スキーマの定義・引き当て（`Lens`／`StorageMapping`／条件構築）は
//! `lens_schema` に置き、こちらは pick/fetch パスから lens 経由でのみ
//! 使用される SQL 生成に専念する。実体が長い構築処理は `sql/` 側
//! （`schema_pieces`／`definition`）に委譲する。

use crate::db::Col;
use crate::query::lens_schema::fixed_attributes;
use crate::types::LabelValue;

/// Fixed 属性名の定数リストに、元の物理行を表す NULL マーカー行を先頭に加えた
/// 1列（name）のサブクエリを返す。base との CROSS JOIN で「元の行＋属性ごとの
/// 合成行」を追加スキャンなしの1パスで展開するために使う。
fn attribute_names() -> sea_query::SelectStatement {
    use sea_query::{Expr, Query, UnionType};

    let mut q = Query::select();
    q.expr_as(Expr::val(None::<String>), Col::Name);
    for (stype, _) in fixed_attributes() {
        let mut row = Query::select();
        row.expr_as(Expr::val(stype.as_str()), Col::Name);
        q.union(UnionType::All, row);
    }
    q
}

/// item_kind/rank/origin 等の Fixed 属性は、アイテムが実際に持つ値だが oneview
/// 上に行として現れないため、`type:` プロジェクションから抜け落ちる。`col` が
/// type 列の場合、`base`（`to_label_select` の出力＝Representative/item_id の
/// 2列）の各 item に属性名を型名として補った SELECT を返す（それ以外の列では
/// base をそのまま返す）。src への追加スキャンは行わず、base と属性名定数リスト
/// の CROSS JOIN で展開する（対象属性が oneview の全行で非 NULL であることが
/// 前提。将来 NULL になりうる属性を足す場合は、その属性のみ保有テーブルへの
/// EXISTS で存在確認を付けること）。
pub(crate) fn complement_type(
    base: sea_query::SelectStatement,
    col: Col,
) -> sea_query::SelectStatement {
    use crate::db::{
        CustomFunc,
        Pronoun::{Representative, L, R},
    };
    use sea_query::{Expr, Query};

    if col != Col::Type {
        return base;
    }
    let mut stmt = Query::select();
    stmt.distinct()
        .expr_as(
            Expr::case(
                Expr::col((R, Col::Name)).is_null(),
                Expr::col((L, Representative)),
            )
            .finally(CustomFunc::as_representative(Expr::col((R, Col::Name)))),
            Representative,
        )
        .column((L, Col::ItemId))
        .from_subquery(base, L)
        .from_subquery(attribute_names(), R);
    stmt
}

/// `complement_type` と同じ理由で Fixed 属性を型名として補うが、`nest()` の
/// Deduped CTE は `(item_id, type, rank)` の3列を必要とする点が異なる
/// （type 値ごとに item をグルーピングし rank で代表を選ぶ処理が後続にあるため）。
/// `base` は Deduped の元スキャン（picked_ids 制限済み）。展開方式は
/// `complement_type` と同じ（追加スキャンなしの CROSS JOIN）。
pub(crate) fn complement_type_groups(
    base: sea_query::SelectStatement,
    col: Col,
) -> sea_query::SelectStatement {
    use crate::db::Pronoun::{L, R};
    use sea_query::{Expr, Func, Query};

    if col != Col::Type {
        return base;
    }
    let mut stmt = Query::select();
    stmt.distinct()
        .column((L, Col::ItemId))
        .expr_as(
            Func::coalesce([
                Expr::col((R, Col::Name)).into(),
                Expr::col((L, Col::Type)).into(),
            ]),
            Col::Type,
        )
        .column((L, Col::Rank))
        .from_subquery(base, L)
        .from_subquery(attribute_names(), R);
    stmt
}

/// `col` が type 列の場合、Fixed 属性（item_kind/rank/origin 等）も含めた
/// DISTINCT な型の個数を数える SELECT を返す（`ids_sql` に含まれる item のみ
/// 対象）。`col` が type 列でなければ None（呼び出し側は通常の COUNT DISTINCT
/// にフォールバックする）。`count(type:)` 用。
pub(crate) fn count_types(
    src: &crate::db::Src,
    col: Col,
    ids_sql: &sea_query::SelectStatement,
) -> Option<sea_query::SelectStatement> {
    use crate::db::Pronoun::{Representative, Scalar, Sub};
    use crate::query::sql::schema_pieces::build_lens_select_column;
    use sea_query::{Expr, Query};

    if col != Col::Type {
        return None;
    }
    let base = build_lens_select_column(src, Col::Type, ids_sql.clone());
    let types = complement_type(base, Col::Type);

    let mut stmt = Query::select();
    stmt.from_subquery(types, Sub)
        .expr_as(Expr::col(Representative).count_distinct(), Scalar);
    Some(stmt)
}

/// 定義アイテムの name 絞り込み条件を DefinitionRef から解決し、
/// ResolvedDefinition にまとめる。unquoted String は glob検索でそのまま
/// candidates を使う。それ以外（quoted Literal 等）はクオート意味論を
/// 維持するため等値の完全一致検索とし、candidates の中に同名の登録済み
/// 候補（Stored/Settling(Plugin, _)）があればそれをそのまま使う
/// （Stored/Volatile の区別を保つため）。無ければ、未登録・未使用の名前でも
/// Volatile 1 行が合成されるよう、User 区画のプレースホルダを合成する。
fn resolve_definition_name_filter(
    def: &crate::query::ast::DefinitionRef,
    default_rank: crate::types::Rank,
) -> crate::query::sql::definition::ResolvedDefinition {
    use crate::query::ast::Candidate;
    use crate::query::sql::definition::ResolvedDefinition;
    use crate::types::{ItemId, Origin};

    let (pattern, exact, candidates) =
        if let LabelValue::String(pattern) = def.value.value() {
            (pattern, false, def.candidates.clone())
        } else {
            let v = def.value.value().as_display_name();
            let entry = def
                .candidates
                .iter()
                .find(|c| c.name == v)
                .cloned()
                .unwrap_or_else(|| Candidate {
                    name: v.clone(),
                    rank: default_rank,
                    id: ItemId::Settling(Origin::User, 0),
                });
            (v, true, vec![entry])
        };
    ResolvedDefinition {
        kind: def.kind,
        pattern,
        exact,
        candidates,
        origins: def.origins.clone(),
        reserved: def.reserved.clone(),
        recorded: def.recorded,
    }
}

/// 定義アイテムの参照（`tag:"X"` の完全一致検索 / `type:*` の glob検索）の pick SQL。
/// 定義アイテム（Stored 定義行 ∪ registry 由来の candidates ∪ 使用中の型/タグの
/// Volatile 合成）を name で絞り込む。
pub(crate) fn filter_definitions(
    src: &crate::db::Src,
    def: &crate::query::ast::DefinitionRef,
    default_rank: crate::types::Rank,
) -> sea_query::SelectStatement {
    let resolved = resolve_definition_name_filter(def, default_rank);
    crate::query::sql::definition::build_definition_pick_sql(src, &resolved)
}

/// 定義参照1枝ぶんの SQL（fetch 行スキーマ:
/// 空 tags・representative・並び替え専用の name 付き）。
/// 絞り込みロジックは filter_definitions と同一で、スキーマだけが異なる。
/// limit/offset は適用しない（呼び出し側が Or の他枝との UNION 後にまとめて適用する）。
fn definition_branch_sql(
    src: &crate::db::Src,
    def: &crate::query::ast::DefinitionRef,
    default_rank: crate::types::Rank,
) -> sea_query::SelectStatement {
    let resolved = resolve_definition_name_filter(def, default_rank);
    crate::query::sql::definition::build_definition_fetch_sql(src, &resolved)
}

/// 定義枝（複数可、Or 経由で到達可能なもの）が指す定義アイテムの distinct 件数の
/// スカラー SQL。`defs` は DefinitionRef 以外を含んではならない
/// （split_definition_branches の構築上、常にこの前提が成り立つ）。
pub(crate) fn count_definitions(
    src: &crate::db::Src,
    defs: &[crate::query::lens_resolver::ResolvedNode],
) -> sea_query::SelectStatement {
    use crate::query::lens_resolver::ResolvedNode;

    let branches = defs
        .iter()
        .map(|node| {
            let ResolvedNode::DefinitionRef { def, default_rank, .. } = node
            else {
                unreachable!(
                    "count_definitions: defs must only contain DefinitionRef"
                );
            };
            let resolved =
                resolve_definition_name_filter(def, *default_rank);
            crate::query::sql::definition::build_definition_name_sql(
                src, &resolved,
            )
        })
        .collect();
    crate::query::sql::definition::build_definitions_count_sql(branches)
}

/// 定義枝（単体または Or 経由で到達可能なもの）を fetch 結果に加える統合 SQL。
/// 各定義枝の SQL と src 由来枝の fetch（`rest_fetch`、列順は
/// 定義枝の SQL に揃えたもの）を束ね、並び替え（`orders` が空なら既定）と
/// limit/offset を結合後に適用する。`defs` は DefinitionRef 以外を含んではならない。
pub(crate) fn add_definitions(
    src: &crate::db::Src,
    defs: &[&crate::query::lens_resolver::ResolvedNode],
    rest_fetch: Option<sea_query::SelectStatement>,
    n: usize,
    offset: usize,
    orders: &[crate::query::lens_resolver::ResolvedOrder],
) -> anyhow::Result<sea_query::SelectStatement> {
    use crate::query::lens_resolver::ResolvedNode;

    let mut branches = Vec::with_capacity(defs.len() + 1);
    for node in defs {
        let ResolvedNode::DefinitionRef { def, default_rank, .. } = node
        else {
            anyhow::bail!("add_definitions: DefinitionRef 以外の枝: {:?}", node);
        };
        branches.push(definition_branch_sql(src, def, *default_rank));
    }
    branches.extend(rest_fetch);
    Ok(crate::query::sql::definition::build_add_definitions_sql(
        src, branches, orders, n, offset,
    ))
}
