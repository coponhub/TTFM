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

use crate::db::Col;
use crate::tag::{Scan, ScanColumn, ScanField};
use crate::types::BiticalAssociate;
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

/// Scan から ScanColumn 情報を取得します。
pub fn get_column_def<F: Scan>() -> ScanColumn {
    ScanColumn {
        name: F::name(),
        bitical_type: <<F as Scan>::Value as BiticalAssociate>::BITICAL,
        role: F::SCAN_ROLE,
    }
}

/// パスとメタデータから ScanField を生成します。
pub fn generate_field<F: Scan>(
    path: &Path,
    metadata: &crate::util::SafeMetadata,
) -> Result<ScanField<F>> {
    Ok(ScanField {
        value: F::scan(path, metadata)?,
    })
}

/// DuckDB の Row からフィールドを順次読み込み、インデックスを更新します。
/// マクロ内での初期化をフラットにするためのヘルパーです。
pub fn read_next_field<F: Scan>(
    row: &duckdb::Row,
    idx: &mut usize,
) -> duckdb::Result<ScanField<F>>
where
    <F as Scan>::Value: FromSql,
{
    let val = row.get(*idx)?;
    *idx += 1;
    Ok(ScanField { value: val })
}

/// ScanEntry構造体とそのスキーマ、およびDuckDBの行からの変換ロジックを定義するマクロ。
#[macro_export]
macro_rules! define_scan_entry {
    ( $( $name:ident : $func:ty ),* $(,)? ) => {
        /// スキャン時に取得されるファイルの基本情報の構造体。
        pub struct ScanEntry {
            $( pub $name: $crate::tag::ScanField<$func>, )*
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
                        cd.bitical_type.into_iden(),
                    )
                }).collect()
            }

            /// スキャン時に作成される `temp_scan` テーブルのカラム構成を定義します。
            pub fn schema() -> Vec<$crate::tag::ScanColumn> {
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

/// `OperandFormat` を宣言順で試す推論レジストリを生成するマクロ。
/// 宣言順は「全形式が譲ったら最後の形式に落ちる」という最下位保証のためだけに使う。
#[macro_export]
macro_rules! define_operand_formats {
    ( $( $name:ident ),+ $(,)? ) => {
        impl $crate::types::Bitical {
            /// 全 `OperandFormat` に宣言順で尋ね、最初に主張した形式が報告する
            /// `LogicalType` を返す。どれも主張しなければ文字列のまま。
            pub fn infer_logical_type_with_range(
                &self,
            ) -> $crate::query::logical_schema::LogicalType {
                use $crate::query::format::OperandFormat;
                let $crate::types::Bitical::String(s) = self else {
                    return self.logical_type();
                };
                $(
                    if let Some(Ok(value)) = <$name as OperandFormat>::parse(s) {
                        return <$name as OperandFormat>::logical_type(&value);
                    }
                )+
                self.logical_type()
            }

            /// 全 `OperandFormat` に宣言順で尋ね、最初に主張した形式のパース結果を
            /// 型ごと保つ。どれも主張しなければ `Formatted::Bitical` のまま。
            pub fn to_formatted(&self) -> $crate::query::format::Formatted {
                use $crate::query::format::OperandFormat;
                let $crate::types::Bitical::String(s) = self else {
                    return $crate::query::format::Formatted::Bitical(self.clone());
                };
                $(
                    if let Some(Ok(value)) = <$name as OperandFormat>::parse(s) {
                        return $crate::query::format::Formatted::$name(value);
                    }
                )+
                $crate::query::format::Formatted::Bitical(self.clone())
            }
        }

        #[derive(Debug, Clone, PartialEq)]
        pub enum Formatted {
            $( $name($name), )+
        }

        impl Formatted {
            pub fn is_point(&self) -> bool {
                use $crate::query::format::OperandFormat;
                match self {
                    $( Formatted::$name(v) => <$name as OperandFormat>::is_point(v), )+
                }
            }

            pub fn as_bitical(&self) -> $crate::types::Bitical {
                use $crate::query::format::OperandFormat;
                match self {
                    $( Formatted::$name(v) => <$name as OperandFormat>::as_bitical(v), )+
                }
            }
        }
    };
}

