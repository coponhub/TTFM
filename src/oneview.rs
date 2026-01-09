use sea_query::{
    Query, Expr, JoinType, SelectStatement, Alias, Func, IntoIden
};
use std::path::Path;
use crate::db::{Tbl, Col, DuckDbFunc, SqlType};
use crate::taggers::{ColumnDef, TargetTable};
use crate::util;

pub struct OneView;

impl OneView {
    /// データベース上に oneview ビューを構築（または置換）します。
    pub fn recreate(
        conn: &duckdb::Connection,
        all_columns: &[ColumnDef],
        db_dir: &Path,
    ) -> anyhow::Result<()> {
        let ents = db_dir.join("entities.parquet").to_string_lossy().into_owned();
        let base_tags = db_dir.join("base_tags.parquet").to_string_lossy().into_owned();
        let locs = db_dir.join("locations.parquet").to_string_lossy().into_owned();
        let items = db_dir.join("items.parquet").to_string_lossy().into_owned();
        let system_tags = db_dir.join("system_tags.parquet").to_string_lossy().into_owned();
        let user_tags = db_dir.join("user_tags.parquet").to_string_lossy().into_owned();

        let q = Self::construct_query(
            all_columns,
            &ents,
            &base_tags,
            &locs,
            &items,
            &system_tags,
            &user_tags,
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
            .from_subquery(util::parquet_query(ents), Tbl::FileEntities)
            .join_subquery(
                JoinType::InnerJoin,
                util::parquet_query(locs),
                Tbl::Locations,
                Expr::col((Tbl::FileEntities, Col::ItemId)).eq(Expr::col((Tbl::Locations, Col::ItemId)))
            );

        // Base info from other items
        let mut item_master = Query::select();
        item_master
            .column(Col::ItemId)
            .column(Col::Rank)
            .column(Col::ItemKind)
            .expr_as(Expr::col(Col::Content), Col::Name)
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
            .expr_as(Expr::col((Tbl::Master, Col::ItemId)), Col::ItemId)
            .column((Tbl::Master, Col::Rank))
            .column((Tbl::Master, Col::ItemKind))
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
                Expr::col((Tbl::Master, Col::ItemId)).eq(Expr::col((Tbl::UserTags, Col::ItemId)))
            );

        // --- 2. Unified Tag Sources (item_id, origin, type, label) ---
        
        let mut base_q = Query::select();
        
        // A. base_tags
        base_q.column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .column(Col::Type)
            .column(Col::Label)
            .from_subquery(util::parquet_query(base_tags), Tbl::BaseTags);

        // B. locations
        for cd in all_columns
            .iter()
            .filter(|c| c.target_table == TargetTable::Locations)
        {
            let mut sub = Query::select();
            let col_iden = Col::from_str(&cd.name).map(|c| c.into_iden()).unwrap_or_else(|| Alias::new(cd.name.clone()).into_iden());
            sub.column(Col::ItemId)
                .expr_as(Expr::val("system"), Col::Origin)
                .expr_as(Expr::val(cd.name.to_string()), Col::Type)
                .expr_as(
                    Expr::col((Tbl::Locations, col_iden))
                        .cast_as(SqlType::VARCHAR),
                    Col::Label,
                )
                .from_subquery(util::parquet_query(locs), Tbl::Locations);
            base_q.union(sea_query::UnionType::All, sub.to_owned());
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
            .column((Tbl::BaseTags, Col::Origin))
            .column((Tbl::BaseTags, Col::Type))
            .column((Tbl::BaseTags, Col::Label))
            .column((Tbl::Master, Col::Name))
            .from_subquery(base_q, Tbl::BaseTags)
            .join_subquery(
                JoinType::InnerJoin,
                final_master,
                Tbl::Master,
                Expr::col((Tbl::BaseTags, Col::ItemId)).eq(Expr::col((Tbl::Master, Col::ItemId)))
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
