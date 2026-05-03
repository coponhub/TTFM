use crate::db::Col;
use crate::indexing::functions::{Field, ScanColumn, TagDefinition};
use crate::types::DBType;
use crate::util::alias_from;
use anyhow::Result;
use duckdb::types::FromSql;
use sea_query::IntoIden;
use std::path::Path;

/// 名称（またはエイリアス）から sea-query の識別子（Iden）を生成します。
pub fn name_to_iden(name: &str) -> sea_query::DynIden {
    Col::from_str(name)
        .map(|c| c.into_iden())
        .unwrap_or_else(|| alias_from(name))
}

/// TagDefinition から ScanColumn 情報を取得します。
pub fn get_column_def<F: TagDefinition>() -> ScanColumn {
    ScanColumn {
        name: F::name(),
        sql_type: <<F as TagDefinition>::RustType as DBType>::db_type(),
        role: F::ROLE,
    }
}

/// パスとメタデータから Field を生成します。
pub fn generate_field<F: TagDefinition>(
    path: &Path,
    metadata: &crate::util::SafeMetadata,
) -> Result<Field<F>> {
    Ok(Field {
        value: F::generate(path, metadata)?,
    })
}

/// DuckDB の Row からフィールドを順次読み込み、インデックスを更新します。
/// マクロ内での初期化をフラットにするためのヘルパーです。
pub fn read_next_field<F: TagDefinition>(
    row: &duckdb::Row,
    idx: &mut usize,
) -> duckdb::Result<Field<F>>
where
    <F as TagDefinition>::RustType: FromSql,
{
    let val = row.get(*idx)?;
    *idx += 1;
    Ok(Field { value: val })
}

/// ScanEntry構造体とそのスキーマ、およびDuckDBの行からの変換ロジックを定義するマクロ。
#[macro_export]
macro_rules! define_scan_entry {
    ( $( $name:ident : $func:ty ),* $(,)? ) => {
        /// スキャン時に取得されるファイルの基本情報の構造体。
        pub struct ScanEntry {
            $( pub $name: $crate::indexing::functions::Field<$func>, )*
        }

        impl std::fmt::Debug for ScanEntry {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("ScanEntry")
                    $( .field(stringify!($name), &self.$name) )*
                    .finish()
            }
        }

        impl PartialEq for ScanEntry {
            fn eq(&self, other: &Self) -> bool {
                $( self.$name == other.$name )&&*
            }
        }

        impl Clone for ScanEntry {
            fn clone(&self) -> Self {
                Self {
                    $( $name: self.$name.clone(), )*
                }
            }
        }

        impl ScanEntry {
            /// 名称（またはエイリアス）から識別子を生成する内部ヘルパー。
            fn name_to_iden(name: &str) -> sea_query::DynIden {
                $crate::macros::name_to_iden(name)
            }

            /// 全てのカラムの識別子（Iden）を取得します。
            pub fn column_idens() -> Vec<sea_query::DynIden> {
                Self::schema().into_iter().map(|cd| Self::name_to_iden(&cd.name)).collect()
            }

            /// カラム名と型のペアの識別子リストを取得します。
            pub fn columns_with_type() -> Vec<(sea_query::DynIden, sea_query::DynIden)> {
                use sea_query::IntoIden;
                Self::schema().into_iter().map(|cd| {
                    (
                        Self::name_to_iden(&cd.name),
                        cd.sql_type.into_iden(),
                    )
                }).collect()
            }

            /// スキャン時に作成される `temp_scan` テーブルのカラム構成を定義します。
            pub fn schema() -> Vec<$crate::indexing::functions::ScanColumn> {
                vec![ $( $crate::macros::get_column_def::<$func>() ),* ]
            }

            /// パスとメタデータから `ScanEntry` を生成します。
            pub fn from_path_metadata(
                path: &std::path::Path,
                metadata: &$crate::util::SafeMetadata,
            ) -> anyhow::Result<Self> {
                #[allow(unused_imports)]
                use $crate::util::DotOk;

                $( let $name = $crate::macros::generate_field::<$func>(path, metadata)?; )*

                Self { $( $name ),* }.to_ok()
            }

            /// DuckDBの行(`row`)から `ScanEntry` を生成します。
            pub fn from_row(row: &duckdb::Row) -> duckdb::Result<Self> {
                Self::from_row_with_offset(row, 0)
            }

            /// 指定されたオフセットから DuckDBの行(`row`)を読み込み `ScanEntry` を生成します。
            pub fn from_row_with_offset(row: &duckdb::Row, offset: usize) -> duckdb::Result<Self> {
                #[allow(unused_imports)]
                use $crate::util::DotOk;
                let mut _idx = offset;

                $( let $name = $crate::macros::read_next_field::<$func>(row, &mut _idx)?; )*

                Self { $( $name ),* }.to_ok()
            }

            /// DuckDBのクエリパラメータとして使用できる形式(`Vec<&dyn ToSql>`)でフィールド値を返します。
            /// Appender等で使用します。
            pub fn as_params(&self) -> Vec<&dyn duckdb::ToSql> {
                vec![ $( &self.$name.value ),* ]
            }
        }
    };
}

/// Item関連のカラム順序を一元管理するマクロ。
/// 構造体定義、SELECTプロジェクション、カラムリスト生成を一度の定義で行います。
#[macro_export]
macro_rules! define_item_schema {
    ($($field:ident => $col:ident),* $(,)?) => {
        pub(crate) struct ItemRow {
            $(pub $field: ::sea_query::SimpleExpr,)*
        }

        impl ItemRow {
            pub fn select(self) -> ::sea_query::SelectStatement {
                let mut q = ::sea_query::Query::select();
                $(q.expr_as(self.$field, $crate::db::Col::$col);)*
                q
            }

            pub fn all_columns() -> [$crate::db::Col; 6] {
                [$($crate::db::Col::$col),*]
            }
        }
    };
}
