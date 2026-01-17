use crate::db::{Col, DuckDbFunc, TargetTable, Tbl};
use crate::taggers::ColumnDef;
use crate::util;
use sea_query::{Expr, Func, JoinType, Query, SelectStatement};
use std::path::Path;

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
            &path(TargetTable::FileReferences),
            &path(TargetTable::BaseTags),
            &path(TargetTable::Locations),
            &path(TargetTable::ItemReferences),
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
        // --- Unified Tag Sources (item_id, origin, type, label_*) ---

        let mut base_q = Query::select();

        // 1. BaseTags
        base_q
            .column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .column(Col::Type)
            .column(Col::LabelStr)
            .column(Col::LabelInt)
            .column(Col::LabelDouble)
            .column(Col::LabelBool)
            .from_subquery(util::parquet_query(base_tags), Tbl::BaseTags);

        // 2. SystemTags
        let mut stags = Query::select();
        stags
            .column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .column(Col::Type)
            .column(Col::LabelStr)
            .column(Col::LabelInt)
            .column(Col::LabelDouble)
            .column(Col::LabelBool)
            .from_subquery(util::parquet_query(system_tags), Tbl::SystemTags);
        base_q.union(sea_query::UnionType::All, stags.to_owned());

        // 3. UserTags
        let mut utags = Query::select();
        utags
            .column(Col::ItemId)
            .expr_as(Expr::val("user"), Col::Origin)
            .column(Col::Type)
            .column(Col::LabelStr)
            .column(Col::LabelInt)
            .column(Col::LabelDouble)
            .column(Col::LabelBool)
            .from_subquery(util::parquet_query(user_tags), Tbl::UserTags);
        base_q.union(sea_query::UnionType::All, utags.to_owned());

        // 4. Physical Tables Unpivoting (FileReferences, Locations)
        for target in [TargetTable::FileReferences, TargetTable::Locations] {
            let table_iden = match target {
                TargetTable::FileReferences => Tbl::FileReferences,
                TargetTable::Locations => Tbl::Locations,
                _ => unreachable!(),
            };
            let parquet_path = match target {
                TargetTable::FileReferences => ents,
                TargetTable::Locations => locs,
                _ => unreachable!(),
            };

            // 各テーブルの物理カラムをタグとして展開
            for cd in all_columns.iter().filter(|c| c.target_table == target) {
                let col_iden = crate::macros::name_to_iden(&cd.name);
                let col_expr = Expr::col((table_iden, col_iden.clone()));

                // カラムの型に応じて適切な label_x にマッピング
                let (l_str, l_int, l_dbl, l_bool) = match cd.sql_type {
                    crate::db::SqlType::VARCHAR | crate::db::SqlType::UUID => {
                        (col_expr.into(), Expr::val(None::<i64>), Expr::val(None::<f64>), Expr::val(None::<bool>))
                    }
                    crate::db::SqlType::BIGINT => {
                         // BIGINTの場合、col_exprはl_intに使われる
                         // 他はNULL
                         (Expr::val(None::<String>), col_expr.into(), Expr::val(None::<f64>), Expr::val(None::<bool>))
                    }
                    crate::db::SqlType::DOUBLE => {
                         (Expr::val(None::<String>), Expr::val(None::<i64>), col_expr.into(), Expr::val(None::<bool>))
                    }
                    crate::db::SqlType::BOOLEAN => {
                         (Expr::val(None::<String>), Expr::val(None::<i64>), Expr::val(None::<f64>), col_expr.into())
                    }
                };

                // 特殊処理: VARCHARの場合は明示的なCASTが必要な場合がある (特にUUID)
                let l_str_casted = if matches!(cd.sql_type, crate::db::SqlType::UUID) {
                    Expr::cust_with_exprs("CAST($1 AS VARCHAR)", [l_str.into()])
                } else {
                    l_str.into()
                };
                // 既にmatchアームで分岐しているので、l_str_castedを使うのはStringの時だけにする微調整
                // 既にmatchアームで分岐しているので、l_str_castedを使うのはStringの時だけにする微調整
                let final_l_str = l_str_casted;


                let sub = Query::select()
                    .column(Col::ItemId)
                    .expr_as(Expr::val("system"), Col::Origin)
                    .expr_as(Expr::val(cd.name.to_string()), Col::Type)
                    .expr_as(final_l_str, Col::LabelStr)
                    .expr_as(l_int, Col::LabelInt)
                    .expr_as(l_dbl, Col::LabelDouble)
                    .expr_as(l_bool, Col::LabelBool)
                    .from_subquery(util::parquet_query(parquet_path), table_iden)
                    .to_owned();
                base_q.union(sea_query::UnionType::All, sub);
            }

            // 特殊カラム (Rank for FileReferences) - all_columnsに含まれていない場合への対応
            // しかし通常 Rank は functions.rs で定義されているはずなのでループでカバーされるはず。
            // FileReferencesには ItemId, Rank がある。
        }

        // 5. ItemReferences Special Handling
        // ItemReferences has: ItemId, Rank, ItemKind, Content
        // Content -> 'content' tag (String)
        // ItemKind -> 'kind' tag (String)
        // Rank -> 'rank' tag (Int)

        // Content
        let mut items_content = Query::select();
        items_content
            .column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .expr_as(Expr::val("content"), Col::Type)
            .expr_as(Expr::col(Col::Content), Col::LabelStr)
            .expr_as(Expr::val(None::<i64>), Col::LabelInt)
            .expr_as(Expr::val(None::<f64>), Col::LabelDouble)
            .expr_as(Expr::val(None::<bool>), Col::LabelBool)
            .from_subquery(util::parquet_query(items), Tbl::ItemReferences);
        base_q.union(sea_query::UnionType::All, items_content);

        // ItemKind -> "type" tag? or "kind"? 
        // Plan says: item_kind -> type='kind', label_str=...
        let mut items_kind = Query::select();
        items_kind
            .column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .expr_as(Expr::val("kind"), Col::Type)
            .expr_as(Expr::col(Col::ItemKind), Col::LabelStr)
            .expr_as(Expr::val(None::<i64>), Col::LabelInt)
            .expr_as(Expr::val(None::<f64>), Col::LabelDouble)
            .expr_as(Expr::val(None::<bool>), Col::LabelBool)
            .from_subquery(util::parquet_query(items), Tbl::ItemReferences);
        base_q.union(sea_query::UnionType::All, items_kind);

        // Rank -> "rank" tag
        let mut items_rank = Query::select();
        items_rank
            .column(Col::ItemId)
            .expr_as(Expr::val("system"), Col::Origin)
            .expr_as(Expr::val("rank"), Col::Type)
            .expr_as(Expr::val(None::<String>), Col::LabelStr)
            .expr_as(Expr::col(Col::Rank), Col::LabelInt)
            .expr_as(Expr::val(None::<f64>), Col::LabelDouble)
            .expr_as(Expr::val(None::<bool>), Col::LabelBool)
            .from_subquery(util::parquet_query(items), Tbl::ItemReferences);
        base_q.union(sea_query::UnionType::All, items_rank);

        base_q
    }
}

#[cfg(test)]
mod tests {
    use crate::FileManager;
    use tempfile::tempdir;

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
