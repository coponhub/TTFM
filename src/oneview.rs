use crate::db::{Col, SqlType, TargetTable, Tbl, Val};
use crate::taggers::ColumnDef;
use duckdb::{Connection, Result};
use sea_query::{CaseStatement, Expr, Func, PostgresQueryBuilder, Query};
use std::path::Path;

// ============================================================================
// データ駆動設計のための型定義
// ============================================================================

/// タグテーブルのソース定義
#[derive(Clone, Copy)]
struct TagSource {
    table: Tbl,
    target: TargetTable,
    origin: Val,
}

/// タグテーブルのソース一覧
const TAG_SOURCES: &[TagSource] = &[
    TagSource {
        table: Tbl::BaseTags,
        target: TargetTable::BaseTags,
        origin: Val::System,
    },
    TagSource {
        table: Tbl::SystemTags,
        target: TargetTable::SystemTags,
        origin: Val::System,
    },
    TagSource {
        table: Tbl::UserTags,
        target: TargetTable::UserTags,
        origin: Val::User,
    },
];

/// Physical テーブル（FileReferences, Locations）のソース定義
struct PhysicalSource {
    table: Tbl,
    target: TargetTable,
    /// FileReferences との JOIN が必要か（自テーブルなら不要）
    needs_file_ref_join: bool,
    /// name/filename エイリアスを追加するか
    add_location_aliases: bool,
}

const PHYSICAL_SOURCES: &[PhysicalSource] = &[
    PhysicalSource {
        table: Tbl::FileReferences,
        target: TargetTable::FileReferences,
        needs_file_ref_join: false,
        add_location_aliases: false,
    },
    PhysicalSource {
        table: Tbl::Locations,
        target: TargetTable::Locations,
        needs_file_ref_join: true,
        add_location_aliases: true,
    },
];

// ============================================================================
// ヘルパー関数（ロジック1箇所化）
// ============================================================================

/// label_str の COALESCE 式を生成
fn build_label_str_expr(tbl: Tbl) -> sea_query::SimpleExpr {
    Func::cust(crate::db::DuckDbFunc::Coalesce)
        .args([
            Expr::col((tbl, Col::LabelStr)).into(),
            Expr::col((tbl, Col::LabelInt))
                .cast_as(SqlType::VARCHAR)
                .into(),
            Expr::col((tbl, Col::LabelDouble))
                .cast_as(SqlType::VARCHAR)
                .into(),
            CaseStatement::new()
                .case(Expr::col((tbl, Col::LabelBool)).eq(true), "true")
                .finally("false")
                .into(),
        ])
        .into()
}

/// rank の COALESCE 式を生成
fn build_rank_expr() -> sea_query::SimpleExpr {
    Func::cust(crate::db::DuckDbFunc::Coalesce)
        .args([
            Expr::col((Tbl::FileReferences, Col::Rank)).into(),
            Expr::col((Tbl::ItemReferences, Col::Rank)).into(),
            Expr::val(0).into(),
        ])
        .into()
}

/// item_kind の CASE 式を生成
/// 注意: これは FileReferences と ItemReferences の両方が JOIN されている環境（Tag Source）でのみ使用可能。
fn build_item_kind_expr() -> sea_query::SimpleExpr {
    CaseStatement::new()
        .case(
            Expr::col((Tbl::FileReferences, Col::ItemId)).is_not_null(),
            Expr::val(Into::<&'static str>::into(Val::File)),
        )
        .case(
            Expr::col((Tbl::ItemReferences, Col::ItemId)).is_not_null(),
            Expr::col((Tbl::ItemReferences, Col::ItemKind)),
        )
        .finally(Expr::val(Into::<&'static str>::into(Val::Unknown)))
        .into()
}

/// Oneview のデータソースを識別するための型
enum OneViewSource<'a> {
    /// タグテーブル (Base, System, UserTags)
    Tag(&'a TagSource),
    /// 物理テーブル (FileReferences, Locations) の一般カラム
    Physical { cd: &'a ColumnDef, tbl: Tbl },
    /// Locations テーブルの name/filename エイリアス
    LocationAlias(Val),
    /// ItemReferences (非ファイルアイテム) の unpivot カラム
    ItemRef(Col),
}

// ----------------------------------------------------------------------------
// 各カラムの「仕様（Spec）」を定義する関数群
// ----------------------------------------------------------------------------

fn spec_origin(source: &OneViewSource) -> sea_query::SimpleExpr {
    // UserTags の時だけ User、それ以外は System
    if matches!(source, OneViewSource::Tag(s) if s.table == Tbl::UserTags) {
        Expr::val(Into::<&'static str>::into(Val::User))
            .cast_as(SqlType::VARCHAR)
            .into()
    } else {
        Expr::val(Into::<&'static str>::into(Val::System))
            .cast_as(SqlType::VARCHAR)
            .into()
    }
}

fn spec_rank(source: &OneViewSource) -> sea_query::SimpleExpr {
    match source {
        // Tag系は FileRefs と ItemRefs 両方を JOIN しているため、元々の build_rank_expr が使える
        OneViewSource::Tag(_) => build_rank_expr(),

        // Physical系（FileReferences, Locations）
        OneViewSource::Physical { tbl, .. } => {
            if *tbl == Tbl::FileReferences {
                // FileReferences 自身なら直のカラム
                Expr::col((*tbl, Col::Rank)).into()
            } else {
                // Locations なら FileReferences を JOIN している
                Func::cust(crate::db::DuckDbFunc::Coalesce)
                    .args([
                        Expr::col((Tbl::FileReferences, Col::Rank)).into(),
                        Expr::val(0).into(),
                    ])
                    .into()
            }
        }

        // LocationAlias も Locations 同様
        OneViewSource::LocationAlias(_) => Func::cust(crate::db::DuckDbFunc::Coalesce)
            .args([
                Expr::col((Tbl::FileReferences, Col::Rank)).into(),
                Expr::val(0).into(),
            ])
            .into(),

        // ItemRef は自身のテーブルの Rank カラムを直接使う
        OneViewSource::ItemRef(_) => Expr::col(Col::Rank).into(),
    }
}

fn spec_item_kind(source: &OneViewSource) -> sea_query::SimpleExpr {
    match source {
        // Tag系は両方の JOIN があるため共通ロジックが使える
        OneViewSource::Tag(_) => build_item_kind_expr().cast_as(SqlType::VARCHAR).into(),

        // Physical系およびエイリアスは常に File 確定
        OneViewSource::Physical { .. } | OneViewSource::LocationAlias(_) => {
            Expr::val(Into::<&'static str>::into(Val::File))
                .cast_as(SqlType::VARCHAR)
                .into()
        }

        // ItemRef は自身のカラムを直接使う
        OneViewSource::ItemRef(_) => Expr::col(Col::ItemKind).cast_as(SqlType::VARCHAR).into(),
    }
}

fn spec_type(source: &OneViewSource) -> sea_query::SimpleExpr {
    let expr = match source {
        OneViewSource::Tag(s) => Expr::col((s.table, Col::Type)),
        OneViewSource::Physical { cd, .. } => Expr::val(&cd.name[..]),
        OneViewSource::LocationAlias(v) => Expr::val(Into::<&'static str>::into(*v)),
        OneViewSource::ItemRef(col) => Expr::val(Into::<&'static str>::into(*col)),
    };
    expr.cast_as(SqlType::VARCHAR).into()
}

fn spec_label_str(source: &OneViewSource) -> sea_query::SimpleExpr {
    let expr: sea_query::SimpleExpr = match source {
        OneViewSource::Tag(s) => build_label_str_expr(s.table),
        OneViewSource::Physical { cd, tbl } => {
            Expr::col((*tbl, crate::util::col_to_iden(&cd.name))).into()
        }
        OneViewSource::LocationAlias(_) => Expr::col((Tbl::Locations, Col::Filename)).into(),
        OneViewSource::ItemRef(col) => Expr::col(*col).into(),
    };
    expr.cast_as(SqlType::VARCHAR).into()
}

fn spec_typed_tag(source: &OneViewSource) -> sea_query::SimpleExpr {
    // 素材を集める
    let type_expr = spec_type(source);
    let val_expr: sea_query::SimpleExpr = match source {
        OneViewSource::Tag(s) => build_label_str_expr(s.table),
        OneViewSource::Physical { cd, tbl } => {
            Expr::col((*tbl, crate::util::col_to_iden(&cd.name))).into()
        }
        OneViewSource::LocationAlias(_) => Expr::col((Tbl::Locations, Col::Filename)).into(),
        OneViewSource::ItemRef(col) => Expr::col(*col).into(),
    };

    // "type:value" 形式で結合
    Func::cust(crate::db::DuckDbFunc::Concat)
        .args([
            type_expr.into(),
            Expr::val(":").into(),
            val_expr.cast_as(SqlType::VARCHAR).into(),
        ])
        .into()
}

/// Oneview の共通カラム群を一括設定する「真の集約エンジン」
fn apply_oneview_schema(q: &mut sea_query::SelectStatement, source: OneViewSource) {
    let s = &source;
    add_col(Col::Origin, spec_origin, q, s);
    add_col(Col::Rank, spec_rank, q, s);
    add_col(Col::ItemKind, spec_item_kind, q, s);
    add_col(Col::Type, spec_type, q, s);
    add_col(Col::TypedTag, spec_typed_tag, q, s);
}

fn add_col(
    col: Col,
    f: fn(&OneViewSource) -> sea_query::SimpleExpr,
    q: &mut sea_query::SelectStatement,
    s: &OneViewSource,
) {
    q.expr_as(f(s), col);
}

/// Locations テーブルの name/filename エイリアス用クエリを生成
fn build_location_alias_query(
    type_val: Val,
    parquet_path: &str,
    file_ref_path: &str,
) -> String {
    let mut q = Query::select();
    q.column((Tbl::Locations, Col::ItemId))
        .expr_as(Expr::col((Tbl::Locations, Col::Filename)), Col::LabelStr)
        .expr_as(crate::util::null_as(SqlType::BIGINT), Col::LabelInt)
        .expr_as(crate::util::null_as(SqlType::DOUBLE), Col::LabelDouble)
        .expr_as(crate::util::null_as(SqlType::BOOLEAN), Col::LabelBool);

    // 【仕様の完全集約】全共通カラムをエンジンに委託
    apply_oneview_schema(&mut q, OneViewSource::LocationAlias(type_val));

    q.from_subquery(crate::util::parquet_query(parquet_path), Tbl::Locations)
        .join_subquery(
            sea_query::JoinType::LeftJoin,
            crate::util::parquet_query(file_ref_path),
            Tbl::FileReferences,
            Expr::col((Tbl::Locations, Col::ItemId))
                .eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
        );
    q.to_string(PostgresQueryBuilder)
}

/// ラベルカラム（label_str, label_int, label_double, label_bool）をクエリに追加
///
/// SQLTypeに応じて適切なカラムに値を設定し、他はNULLで埋める。
/// - VARCHAR/UUID → LabelStr にキャスト
/// - BIGINT → LabelInt に直接、LabelStr にもキャスト
/// - DOUBLE → LabelDouble に直接、LabelStr にもキャスト
/// - BOOLEAN → LabelBool に直接、LabelStr にもキャスト
fn apply_label_columns(
    q: &mut sea_query::SelectStatement,
    tbl: Tbl,
    iden: &sea_query::DynIden,
    sql_type: SqlType,
) {
    let label_col = Col::from_sql_type(sql_type);

    // LabelStr は常に設定（値をVARCHARにキャスト）
    q.expr_as(
        Expr::col((tbl, iden.clone())).cast_as(SqlType::VARCHAR),
        Col::LabelStr,
    );

    // 型付きカラムは該当する型のみ値を設定、他はNULL
    let label_columns = [
        (Col::LabelInt, SqlType::BIGINT),
        (Col::LabelDouble, SqlType::DOUBLE),
        (Col::LabelBool, SqlType::BOOLEAN),
    ];

    for (col, null_type) in label_columns {
        if col == label_col {
            q.expr_as(Expr::col((tbl, iden.clone())), col);
        } else {
            q.expr_as(crate::util::null_as(null_type), col);
        }
    }
}

/// Physical Table (FileReferences, Locations) のカラムからクエリを生成
fn build_physical_column_query(
    cd: &crate::taggers::ColumnDef,
    tbl_alias: Tbl,
    parquet_path: &str,
    file_ref_path: Option<&str>,
) -> String {
    let iden = crate::util::col_to_iden(&cd.name);

    let mut q = Query::select();
    q.column((tbl_alias, Col::ItemId));

    // ラベルカラムの設定（型に応じて分岐）
    apply_label_columns(&mut q, tbl_alias, &iden, cd.sql_type);

    // 【仕様の完全集約】全共通カラムをエンジンに委託
    apply_oneview_schema(&mut q, OneViewSource::Physical { cd, tbl: tbl_alias });

    q.from_subquery(crate::util::parquet_query(parquet_path), tbl_alias);

    if let Some(fr_path) = file_ref_path {
        q.join_subquery(
            sea_query::JoinType::LeftJoin,
            crate::util::parquet_query(fr_path),
            Tbl::FileReferences,
            Expr::col((tbl_alias, Col::ItemId))
                .eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
        );
    }

    q.to_string(PostgresQueryBuilder)
}

/// ItemReferences のカラムからクエリを生成
fn build_item_ref_query(col: Col, items_path: &str) -> String {
    let label_col = Col::from_sql_type(SqlType::VARCHAR);
    let mut q = Query::select();
    q.column(Col::ItemId).expr_as(Expr::col(col), label_col);

    // 【仕様の完全集約】全共通カラムをエンジンに委託
    apply_oneview_schema(&mut q, OneViewSource::ItemRef(col));

    q.from_subquery(
        crate::util::parquet_query(items_path),
        Tbl::ItemReferences,
    );
    q.to_string(PostgresQueryBuilder)
}

/// タグソースからSELECT文を生成
fn build_tag_query(
    source: &TagSource,
    path_fn: impl Fn(TargetTable) -> String,
) -> String {
    let tbl = source.table;
    let label_str = build_label_str_expr(tbl);

    let mut q = Query::select();
    q.column((tbl, Col::ItemId))
        .expr_as(label_str, Col::LabelStr)
        .column((tbl, Col::LabelInt))
        .column((tbl, Col::LabelDouble))
        .column((tbl, Col::LabelBool));

    // 【仕様の完全集約】全共通カラムをエンジンに委託
    apply_oneview_schema(&mut q, OneViewSource::Tag(source));

    q.from_subquery(crate::util::parquet_query(&path_fn(source.target)), tbl)
        .join_subquery(
            sea_query::JoinType::LeftJoin,
            crate::util::parquet_query(&path_fn(TargetTable::FileReferences)),
            Tbl::FileReferences,
            Expr::col((tbl, Col::ItemId))
                .eq(Expr::col((Tbl::FileReferences, Col::ItemId))),
        )
        .join_subquery(
            sea_query::JoinType::LeftJoin,
            crate::util::parquet_query(&path_fn(TargetTable::ItemReferences)),
            Tbl::ItemReferences,
            Expr::col((tbl, Col::ItemId))
                .eq(Expr::col((Tbl::ItemReferences, Col::ItemId))),
        );

    q.to_string(PostgresQueryBuilder)
}

pub struct OneView;

impl OneView {
    /// データベース上に oneview ビューを構築（または置換）します。
    pub fn recreate(
        conn: &Connection,
        all_columns: &[ColumnDef],
        db_dir: &Path,
    ) -> anyhow::Result<()> {
        let path = |t| {
            db_dir
                .join(format!("{}.parquet", t))
                .to_string_lossy()
                .into_owned()
        };

        let mut query_parts = Vec::new();

        // Tag系テーブル（BaseTags, SystemTags, UserTags）
        for source in TAG_SOURCES {
            query_parts.push(build_tag_query(source, &path));
        }

        // 4. Physical Tables (FileReferences, Locations)
        let file_ref_path = path(TargetTable::FileReferences);
        for source in PHYSICAL_SOURCES {
            let parquet_path = path(source.target);
            let join_path = source.needs_file_ref_join.then_some(file_ref_path.as_str());

            // カラムごとのクエリを追加
            query_parts.extend(
                all_columns
                    .iter()
                    .filter(|c| c.target_table == source.target)
                    .map(|cd| build_physical_column_query(cd, source.table, &parquet_path, join_path)),
            );

            // Locations用のname/filenameエイリアス
            if source.add_location_aliases {
                query_parts.push(build_location_alias_query(Val::Name, &parquet_path, &file_ref_path));
                query_parts.push(build_location_alias_query(Val::Filename, &parquet_path, &file_ref_path));
            }
        }

        // 5. ItemReferences (非ファイルアイテム) の unpivot
        let items_path = path(TargetTable::ItemReferences);
        for col in Col::item_references_columns() {
            if col == Col::ItemId || col == Col::Rank {
                continue;
            }
            query_parts.push(build_item_ref_query(col, &items_path));
        }

        create_view_union_by_name(conn, "oneview", &query_parts)?;

        Ok(())
    }
}

/// 指定されたSQLパーツを UNION ALL BY NAME で結合し、ビューを作成します。
fn create_view_union_by_name(
    conn: &Connection,
    view_name: &str,
    select_sqls: &[String],
) -> Result<()> {
    // DuckDB 独自の UNION ALL BY NAME を使用するため、ここでは文字列結合を行います。
    // select_sqls の各要素は sea-query で安全に構築されていることが前提です。
    let combined_sql = select_sqls.join("\nUNION ALL BY NAME\n");
    if std::env::var("TTFM_DEBUG").is_ok() {
        println!("DEBUG ONEVIEW SQL:\n{}", combined_sql);
    }
    conn.execute(
        &format!("CREATE OR REPLACE VIEW {} AS {}", view_name, combined_sql),
        [],
    )?;
    Ok(())
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
            WHERE type = 'name' OR type = 'rank'
            GROUP BY item_id 
            HAVING COUNT(DISTINCT label_str) > 1 OR COUNT(DISTINCT label_int) > 1
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
