use sea_query::{
    Query, Expr, JoinType, SelectStatement, Func
};
use std::path::Path;
use crate::db::{Tbl, Col, DuckDbFunc, TargetTable};
use crate::taggers::{ColumnDef};
use crate::util;

pub struct OneView;

impl OneView {
    /// データベース上に oneview ビューを構築（または置換）します。
    pub fn recreate(
        conn: &duckdb::Connection,
        all_columns: &[ColumnDef],
        db_dir: &Path,
    ) -> anyhow::Result<()> {
        let path = |t| {
            db_dir
                .join(format!("{}.parquet", t))
                .to_string_lossy()
                .into_owned()
        };

        let q = Self::construct_query(
            all_columns,
            &path(TargetTable::FileEntities),
            &path(TargetTable::BaseTags),
            &path(TargetTable::Locations),
            &path(TargetTable::ItemEntities),
            &path(TargetTable::SystemTags),
            &path(TargetTable::UserTags),
        );

        util::create_or_replace_view(conn, Tbl::OneView, q)?;
        Ok(())
    }

    /// データベース全体の統合ビュー（oneview）を構築する SQL クエリを生成します。
    pub fn construct_query(
        all_columns: &[ColumnDef],
        ents: &str,
        base_tags: &str,
        locs: &str,
        items: &str,
        system_tags: &str,
        user_tags: &str,
    ) -> SelectStatement {
        // --- 1. Unified Master Info (ID, Rank, Name, ItemKind) ---
        
        // Base info from files
        let mut file_master = Query::select();
        file_master
            .column((Tbl::FileEntities, Col::ItemId))
            .column((Tbl::FileEntities, Col::Rank))
            .expr_as(Expr::val("file"), Col::ItemKind)
            .expr_as(Expr::col((Tbl::Locations, Col::Filename)), Col::Name)
            .column((Tbl::Locations, Col::Size))
            .column((Tbl::Locations, Col::Mtime))
            .from_subquery(util::parquet_query(ents), Tbl::FileEntities)
            .join_subquery(
                JoinType::InnerJoin,
                util::parquet_query(locs),
                Tbl::Locations,
                Expr::col((Tbl::FileEntities, Col::ItemId))
                    .eq(Expr::col((Tbl::Locations, Col::ItemId)))
            );

        // Base info from other items
        let mut item_master = Query::select();
        item_master
            .expr_as(Expr::col(Col::ItemId), Col::ItemId)
            .expr_as(Expr::col(Col::Rank), Col::Rank)
            .expr_as(Expr::col(Col::ItemKind), Col::ItemKind)
            .expr_as(Expr::col(Col::Content), Col::Name)
            .expr_as(Expr::val(0), Col::Size)
            .expr_as(Expr::val(0), Col::Mtime)
            .from_subquery(util::parquet_query(items), Tbl::Item);

        let mut all_master_base = file_master;
        all_master_base.union(sea_query::UnionType::All, item_master.to_owned());

        // Name override from user tags
        let mut user_names = Query::select();
        user_names
            .column(Col::ItemId)
            .expr_as(Expr::col(Col::Label), Col::Name)
            .from_subquery(util::parquet_query(user_tags), Tbl::BaseTags)
            .and_where(Expr::col(Col::Type).eq("name"));

        let mut final_master = Query::select();
        final_master
            .column((Tbl::Master, Col::ItemId))
            .column((Tbl::Master, Col::Rank))
            .column((Tbl::Master, Col::ItemKind))
            .column((Tbl::Master, Col::Size))
            .column((Tbl::Master, Col::Mtime))
            .expr_as(
                Func::cust(DuckDbFunc::Coalesce).args([
                    Expr::col((Tbl::UserTags, Col::Name)).into(),
                    Expr::col((Tbl::Master, Col::Name)).into(),
                ]),
                Col::Name
            )
            .from_subquery(all_master_base, Tbl::Master)
            .join_subquery(
                JoinType::LeftJoin,
                user_names,
                Tbl::UserTags,
                Expr::col((Tbl::Master, Col::ItemId))
                    .eq(Expr::col((Tbl::UserTags, Col::ItemId)))
            );

        // --- 2. Unified Tag Sources (item_id, origin, type, label) ---
        
        let mut base_q = Query::select();
        
        // A. base_tags
        base_q.column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .column(Col::Type)
            .column(Col::Label)
            .from_subquery(util::parquet_query(base_tags), Tbl::BaseTags);

        // B. Column-based system tags (file_entities & locations)
        for target in [TargetTable::FileEntities, TargetTable::Locations] {
            let table_iden = match target {
                TargetTable::FileEntities => Tbl::FileEntities,
                TargetTable::Locations => Tbl::Locations,
                _ => unreachable!(),
            };
            let parquet_path = match target {
                TargetTable::FileEntities => ents,
                TargetTable::Locations => locs,
                _ => unreachable!(),
            };

            for cd in all_columns.iter().filter(|c| {
                c.target_table == target 
                && c.name != "size" && c.name != "mtime" && c.name != "rank"
            }) {
                let col_iden = crate::macros::name_to_iden(&cd.name);
                let sub = Query::select()
                    .column(Col::ItemId)
                    .expr_as(Expr::val("system"), Col::Origin)
                    .expr_as(Expr::val(cd.name.to_string()), Col::Type)
                    .expr_as(
                        Expr::cust_with_exprs(
                            "CAST($1 AS VARCHAR)",
                            [Expr::col((table_iden, col_iden)).into()],
                        ),
                        Col::Label,
                    )
                    .from_subquery(util::parquet_query(parquet_path), table_iden)
                    .to_owned();
                base_q.union(sea_query::UnionType::All, sub);
            }
        }

        // C. item_entities (content)
        let mut items_content = Query::select();
        items_content
            .column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .expr_as(Expr::val("content"), Col::Type)
            .expr_as(Expr::col(Col::Content), Col::Label)
            .from_subquery(util::parquet_query(items), Tbl::Item);
        base_q.union(sea_query::UnionType::All, items_content.to_owned());

        // D. system_tags
        let mut stags = Query::select();
        stags
            .column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .column(Col::Type)
            .column(Col::Label)
            .from_subquery(util::parquet_query(system_tags), Tbl::BaseTags);
        base_q.union(sea_query::UnionType::All, stags.to_owned());

        // E. user_tags
        let mut utags = Query::select();
        utags
            .column(Col::ItemId)
            .expr_as(Expr::val("user"), Col::Origin)
            .column(Col::Type)
            .column(Col::Label)
            .from_subquery(util::parquet_query(user_tags), Tbl::BaseTags);
        base_q.union(sea_query::UnionType::All, utags.to_owned());

        // --- 3. Final Assembly (Assemble Tags with Master Info) ---

        Query::select()
            .column((Tbl::BaseTags, Col::ItemId))
            .column((Tbl::Master, Col::ItemKind))
            .column((Tbl::Master, Col::Rank))
            .column((Tbl::Master, Col::Size))
            .column((Tbl::Master, Col::Mtime))
            .column((Tbl::BaseTags, Col::Origin))
            .column((Tbl::BaseTags, Col::Type))
            .column((Tbl::BaseTags, Col::Label))
            .column((Tbl::Master, Col::Name))
            .from_subquery(base_q, Tbl::BaseTags)
            .join_subquery(
                JoinType::InnerJoin,
                final_master,
                Tbl::Master,
                Expr::col((Tbl::BaseTags, Col::ItemId))
                    .eq(Expr::col((Tbl::Master, Col::ItemId)))
            )
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use crate::FileManager;

    #[test]
    fn test_oneview_consistency() {
        let dir = tempdir().unwrap();
        let db_dir = dir.path().join(".ttfm/db");
        let fm = FileManager::new_with_db_dir(&db_dir).unwrap();

        // Noteを作成してタグを付ける
        let note_id = fm.add_item("note", "Consistency Test Memo").unwrap();
        fm.tag_item(&note_id.to_string(), "testtag:true").unwrap();

        // oneview ビューを直接クエリして不整合をチェック
        // 同じIDなのに異なるNameまたは異なるRankを持つグループがあるか探す
        let sql = "
            SELECT item_id 
            FROM oneview 
            GROUP BY item_id 
            HAVING COUNT(DISTINCT name) > 1 OR COUNT(DISTINCT rank) > 1
        ";
        
        let mut stmt = fm.conn.prepare(sql).unwrap();
        let inconsistent_ids: Vec<i64> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert!(
            inconsistent_ids.is_empty(), 
            "Inconsistency found in oneview for IDs: {:?}. \
             Each item must have exactly one unique Name and Rank \
             across all its tag rows.", 
            inconsistent_ids
        );
    }
}

        