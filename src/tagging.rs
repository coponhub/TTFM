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

use crate::db::{identifier, Col, Store, TargetTable, Tbl};
use crate::tag::TagRegistry;
use crate::types::Origin;
use crate::util::{self, ExecuteSql, IdenExt, ParquetExt, SelectExt};
use anyhow::Result;
use sea_query::{Expr, PostgresQueryBuilder, Query};

/// 新しいアイテム（Type, Label, Note等）をデータベースに追加します。
pub fn add_item(
    store: &Store,
    registry: &TagRegistry,
    kind: &str,
    content: &str,
) -> Result<i64> {
    let path = store.path_for_target(TargetTable::ItemReferences);
    if !path.exists() {
        return Err(anyhow::anyhow!(
            "Item entities table not found. Please run index first."
        ));
    }

    let path_str = path.to_string_lossy();
    let new_id = identifier::next(store, Origin::User, 1)?[0];

    let temp_table = Tbl::Item;
    util::parquet_query(&path_str).create_table_as(&store.conn, temp_table)?;

    Query::insert()
        .into_table(temp_table)
        .columns([Col::ItemId, Col::ItemKind, Col::Content])
        .values_panic([new_id.into(), kind.into(), content.into()])
        .execute(&store.conn)?;

    Query::select()
        .column(sea_query::Asterisk)
        .from(temp_table)
        .order_by(Col::ItemId, sea_query::Order::Asc)
        .to_owned()
        .save_parquet(&store.conn, &path)?;
    temp_table.drop_table(&store.conn)?;

    refresh_view(store, registry)?;
    Ok(new_id)
}

pub fn get_or_create_item(
    store: &Store,
    registry: &TagRegistry,
    kind: &str,
    content: &str,
) -> Result<i64> {
    let path = store.path_for_target(TargetTable::ItemReferences);
    let query = Query::select()
        .column(Col::ItemId)
        .from_subquery(
            util::parquet_query(&path.to_string_lossy()),
            Tbl::ItemReferences,
        )
        .and_where(Expr::col(Col::ItemKind).eq(kind))
        .and_where(Expr::col(Col::Content).eq(content))
        .to_string(PostgresQueryBuilder);

    if let Ok(id) = store.conn.query_row(&query, [], |r| r.get(0)) {
        Ok(id)
    } else {
        add_item(store, registry, kind, content)
    }
}

fn refresh_view(store: &Store, registry: &TagRegistry) -> Result<()> {
    let all_columns = registry.get_all_columns();
    let reader = crate::query::lens_reader::Reader::build(
        registry,
        crate::db::Tbl::_OneView,
    );
    crate::oneview::OneView::recreate(
        &store.conn,
        &all_columns,
        reader,
        &store.db_dir,
    )
}
