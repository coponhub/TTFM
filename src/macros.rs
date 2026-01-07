/// ScanEntry構造体とそのスキーマ、およびDuckDBの行からの変換ロジックを定義するマクロ。
#[macro_export]
macro_rules! define_scan_entry {
    ( $( $name:ident : $func:ty ),* $(,)? ) => {
        /// スキャン時に取得されるファイルの基本情報の構造体。
        pub struct ScanEntry {
            $( pub $name: $crate::functions::Field<$func>, )*
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
            /// カラム情報を変換して収集するための内部ヘルパー。
            fn map_schema<T>(
                f: impl FnMut($crate::functions::ScanColumn) -> T
            ) -> Vec<T> {
                Self::schema().into_iter().map(f).collect()
            }

            /// 名称（またはエイリアス）から識別子を生成する内部ヘルパー。
            fn name_to_iden(name: &str) -> sea_query::DynIden {
                use sea_query::IntoIden;
                $crate::db::Col::from_str(name)
                    .map(|c| c.into_iden())
                    .unwrap_or_else(|| $crate::util::alias_from(name))
            }

            /// 全てのカラムの識別子（Iden）を取得します。
            pub fn column_idens() -> Vec<sea_query::DynIden> {
                Self::map_schema(|cd| Self::name_to_iden(&cd.name))
            }

            /// カラム名と型のペアの識別子リストを取得します。
            pub fn columns_with_type() -> Vec<(sea_query::DynIden, sea_query::DynIden)> {
                Self::map_schema(|cd| {
                    (
                        Self::name_to_iden(&cd.name),
                        $crate::util::alias_from(cd.sql_type),
                    )
                })
            }

            /// スキャン時に作成される `temp_scan` テーブルのカラム構成を定義します。
            pub fn schema() -> Vec<$crate::functions::ScanColumn> {
                vec![
                    $(
                        $crate::functions::ScanColumn {
                            name: <$func as $crate::functions::TagDefinition>::NAME,
                            sql_type: <<$func as $crate::functions::TagDefinition>::RustType 
                                as $crate::types::DBType>::db_type(),
                            role: <$func as $crate::functions::TagDefinition>::ROLE,
                        },
                    )*
                ]
            }

            /// パスとメタデータから `ScanEntry` を生成します。
            pub fn from_path_metadata(
                path: &std::path::Path,
                metadata: &std::fs::Metadata,
            ) -> anyhow::Result<Self> {
                Ok(Self {
                    $(
                        $name: $crate::functions::Field {
                            value: <$func as $crate::functions::TagDefinition>::generate(
                                path,
                                Some(metadata),
                            )?
                        },
                    )*
                })
            }

                        /// DuckDBの行(`row`)から `ScanEntry` を生成します。

                        /// 定義されたフィールド順序に従って `row.get(i)` を実行します。

                        pub fn from_row(row: &duckdb::Row) -> duckdb::Result<Self> {

                            let mut _idx = 0;

                            Ok(Self {

                                $(

                                    $name: $crate::functions::Field {

                                        value: row.get({ let i = _idx; _idx += 1; i })?

                                    },

                                )*

                            })

                        }

            

                        /// DuckDBのクエリパラメータとして使用できる形式(`Vec<&dyn ToSql>`)でフィールド値を返します。

                        /// Appender等で使用します。

                        pub fn as_params(&self) -> Vec<&dyn duckdb::ToSql> {

                            vec![

                                $( &self.$name.value, )*

                            ]

                        }

                    }

                };

            }

            