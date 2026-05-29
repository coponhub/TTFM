use crate::db::{Col, Store, TargetTable, Tbl};
use crate::tag::TagRegistry;
use crate::util::{self, ExecuteSql, IdenExt, SelectExt};
use anyhow::{Context, Result};
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
    let query_min = Query::select()
        .expr(Expr::col(Col::ItemId).min())
        .from_subquery(util::parquet_query(&path_str), Tbl::ItemReferences)
        .to_string(PostgresQueryBuilder);

    let min_id: i64 = store
        .conn
        .query_row(&query_min, [], |r| r.get(0))
        .unwrap_or(0);
    let new_id = if min_id > -1 { -1 } else { min_id - 1 };

    let temp_table = Tbl::Item;
    util::parquet_query(&path_str).create_table_as(&store.conn, temp_table)?;

    Query::insert()
        .into_table(temp_table)
        .columns([Col::ItemId, Col::ItemKind, Col::Content])
        .values_panic([new_id.into(), kind.into(), content.into()])
        .execute(&store.conn)?;

    temp_table.write_parquet(&store.conn, &path)?;
    temp_table.drop_table(&store.conn)?;

    refresh_view(store, registry)?;
    Ok(new_id)
}

/// アイテム（ファイルまたは Item Entity）にタグを付与します。
pub fn tag_item(
    store: &Store,
    registry: &TagRegistry,
    item: &str,
    tag_str: &str,
) -> Result<()> {
    let (key, value) = tag_str
        .split_once(':')
        .context("Tag must be in 'key:value' format")?;

    get_or_create_item(store, registry, "type", key)?;
    get_or_create_item(store, registry, "tag", tag_str)?;

    let item_id = if let Ok(id) = item.parse::<i64>() {
        id
    } else {
        let query_path = Query::select()
            .column(Col::ItemId)
            .from_subquery(
                util::parquet_query(
                    &store.path_for_target(TargetTable::Locations).to_string_lossy(),
                ),
                Tbl::Locations,
            )
            .and_where(Expr::col(Col::Path).eq(item))
            .to_string(PostgresQueryBuilder);

        if let Ok(id) = store.conn.query_row(&query_path, [], |r| r.get(0)) {
            id
        } else {
            let query_name = Query::select()
                .column(Col::ItemId)
                .from(Tbl::OneView)
                .and_where(Expr::col(Col::Type).eq("name"))
                .and_where(Expr::col(Col::LabelStr).eq(item))
                .to_string(PostgresQueryBuilder);

            store.conn.query_row(&query_name, [], |r| r.get(0)).context(
                format!("Item not found by path or name: {}", item),
            )?
        }
    };

    append_tag_to_parquet(
        store,
        store.path_for_target(TargetTable::UserTags),
        Tbl::UserTagsDiff,
        Col::ItemId,
        item_id,
        key,
        value,
    )?;

    refresh_view(store, registry)?;
    Ok(())
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

pub(crate) fn append_tag_to_parquet(
    store: &Store,
    path: std::path::PathBuf,
    temp_table: Tbl,
    id_col: Col,
    id: i64,
    key: &str,
    value: &str,
) -> Result<()> {
    let path_str = path.to_string_lossy();

    util::parquet_query(&path_str).create_table_as(&store.conn, temp_table)?;

    let val_i64 = value.parse::<i64>().ok();
    let val_f64 = value.parse::<f64>().ok();
    let val_bool = value.parse::<bool>().ok();

    Query::insert()
        .into_table(temp_table)
        .columns([
            id_col,
            Col::Type,
            Col::LabelStr,
            Col::LabelInt,
            Col::LabelDouble,
            Col::LabelBool,
        ])
        .values_panic([
            id.into(),
            key.into(),
            Some(value).into(),
            val_i64.into(),
            val_f64.into(),
            val_bool.into(),
        ])
        .execute(&store.conn)?;

    let query = Query::select()
        .column(sea_query::Asterisk)
        .from(temp_table.clone())
        .order_by(Col::Type, sea_query::Order::Asc)
        .order_by(Col::LabelInt, sea_query::Order::Asc)
        .order_by(Col::LabelStr, sea_query::Order::Asc)
        .order_by(Col::ItemId, sea_query::Order::Asc)
        .to_owned();

    util::save_parquet(&store.conn, &query, &path, None)?;
    temp_table.drop_table(&store.conn)?;

    Ok(())
}

fn refresh_view(store: &Store, registry: &TagRegistry) -> Result<()> {
    let all_columns = registry.get_all_columns();
    crate::oneview::OneView::recreate(&store.conn, &all_columns, &store.db_dir)
}
