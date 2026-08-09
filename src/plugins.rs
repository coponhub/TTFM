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

use anyhow::{Context, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use wasmtime::component::*;
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiView};

use crate::db::{BiticalType, TargetTable};
use crate::query::ast::{BasicOp, ComparisonNode, ComparisonOp, Operand, QueryNode};
use crate::tag::{
    Display as TagDisplay, DisplayFormat, DisplayFormats, Index, Query,
    ScanRole, TagFunction,
};
use crate::types::{Bitical, Label, Rank, TagType};
use crate::util::DotOk;

// --- インターフェース用の手動型定義 ---

// indexing::value-type enum
#[allow(dead_code)]
#[derive(ComponentType, Lift, Lower, Debug, Clone, Copy, PartialEq)]
#[component(enum)]
#[repr(u8)]
enum WasmValueType {
    #[component(name = "text")]
    Text,
    #[component(name = "big-int")]
    BigInt,
    #[component(name = "boolean")]
    Boolean,
    #[component(name = "double")]
    Double,
}

// indexing::tag-value variant
#[derive(ComponentType, Lift, Lower, Debug, Clone, PartialEq)]
#[component(variant)]
enum WasmTagValue {
    #[component(name = "text")]
    Text(String),
    #[component(name = "big-int")]
    BigInt(i64),
    #[component(name = "boolean")]
    Boolean(bool),
    #[component(name = "double")]
    Double(f64),
    #[component(name = "empty")]
    Empty,
}

// display::display-format record
#[derive(ComponentType, Lift, Lower, Debug, Clone)]
#[component(record)]
struct WasmDisplayFormat {
    #[component(name = "id")]
    id: String,
    #[component(name = "label")]
    label: String,
}

/// Wasmプラグインを管理するための共有ストア。
struct WasmStore {
    wasi_ctx: WasiCtx,
    resource_table: ResourceTable,
}

impl WasiView for WasmStore {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi_ctx
    }
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.resource_table
    }
}

// スレッドローカルなインスタンスキャッシュ。
// キーは (WasmPlugin ポインタ, プラグイン名) — 同名でも異なるコンポーネントを区別する。
thread_local! {
    static INSTANCE_CACHE: RefCell<HashMap<(*const WasmPlugin, String), (Store<WasmStore>, Instance)>> =
        RefCell::new(HashMap::new());
}

/// Wasmモジュールを保持し、インスタンス化を行う構造体。
pub struct WasmPlugin {
    engine: Engine,
    component: Component,
    linker: Linker<WasmStore>,
}

impl WasmPlugin {
    /// 指定されたWasmファイルからプラグインをロードします。
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let (engine, linker) = Self::create_engine_and_linker()?;
        let component = Component::from_file(&engine, path)?;
        Ok(Self {
            engine,
            component,
            linker,
        })
    }

    /// バイト列からプラグインをロードします（ビルトインプラグイン用）。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let (engine, linker) = Self::create_engine_and_linker()?;
        let component = Component::from_binary(&engine, bytes)?;
        Ok(Self {
            engine,
            component,
            linker,
        })
    }

    fn create_engine_and_linker() -> Result<(Engine, Linker<WasmStore>)> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)?;
        let mut linker: Linker<WasmStore> = Linker::new(&engine);
        wasmtime_wasi::add_to_linker_sync(&mut linker)?;
        Ok((engine, linker))
    }

    /// プラグインを実行可能なアダプターに変換します。
    pub fn into_adapter(self) -> Result<WasmPluginAdapter> {
        // 静的にエクスポートを検出（インスタンス化不要）
        let ct = self.component.component_type();
        let has_indexing = ct
            .get_export(&self.engine, "ttfm:plugin/indexing")
            .is_some();
        let has_display =
            ct.get_export(&self.engine, "ttfm:plugin/display").is_some();

        // core から name / version 取得（インスタンス化1回）
        let mut store = self
            .create_store()
            .context("Failed to create store for introspection")?;
        let instance = self
            .linker
            .instantiate(&mut store, &self.component)
            .context("Failed to instantiate plugin")?;
        let core_export = instance
            .get_export(&mut store, None, "ttfm:plugin/core")
            .context("ttfm:plugin/core not found")?;
        let name_fn_idx = instance
            .get_export(&mut store, Some(&core_export), "name")
            .context("name not found in ttfm:plugin/core")?;
        let name_func = instance
            .get_func(&mut store, name_fn_idx)
            .context("Failed to get name Func")?;
        let (name,) = wasm_call::<(), (String,)>(&name_func, &mut store, ())
            .context("Failed to call name")?;

        // indexing がある場合は get-value-type を呼び出して SQL 型を決定
        let bitical_type = if has_indexing {
            let idx_export = instance
                .get_export(&mut store, None, "ttfm:plugin/indexing")
                .context(
                    "indexing interface not found despite capability flag",
                )?;
            let fn_idx = instance
                .get_export(&mut store, Some(&idx_export), "get-value-type")
                .context("get-value-type function not found")?;
            let func = instance
                .get_func(&mut store, fn_idx)
                .context("Failed to get get-value-type Func")?;
            let (vt,) =
                wasm_call::<(), (WasmValueType,)>(&func, &mut store, ())?;
            match vt {
                WasmValueType::Text => BiticalType::String,
                WasmValueType::BigInt => BiticalType::Integer,
                WasmValueType::Boolean => BiticalType::Boolean,
                WasmValueType::Double => BiticalType::Double,
            }
        } else {
            BiticalType::String
        };

        Ok(WasmPluginAdapter {
            plugin: Arc::new(self),
            name,
            bitical_type,
            has_indexing,
            has_display,
        })
    }

    fn create_store(&self) -> Result<Store<WasmStore>> {
        let wasi_ctx = WasiCtxBuilder::new()
            .inherit_stdout()
            .inherit_stderr()
            .preopened_dir(
                "/",
                "/",
                wasmtime_wasi::DirPerms::READ,
                wasmtime_wasi::FilePerms::READ,
            )?
            .build();
        let resource_table = ResourceTable::new();
        Ok(Store::new(
            &self.engine,
            WasmStore {
                wasi_ctx,
                resource_table,
            },
        ))
    }
}

/// Wasmプラグインを `TagFunction` として扱うためのアダプター。
pub struct WasmPluginAdapter {
    plugin: Arc<WasmPlugin>,
    pub name: String,
    bitical_type: BiticalType,
    has_indexing: bool,
    has_display: bool,
}

impl TagFunction for WasmPluginAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn index(&self) -> Option<&dyn Index> {
        if self.has_indexing {
            Some(self)
        } else {
            None
        }
    }

    fn query(&self) -> &dyn Query {
        self
    }

    fn display(&self) -> Option<&dyn TagDisplay> {
        if self.has_display {
            Some(self)
        } else {
            None
        }
    }

    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::DEFAULT
    }
}

// --- ヘルパー関数 ---

// TypedFunc::call() は必ず post_return() とペアで呼ぶ必要がある。
// この関数がそれを強制する。
fn wasm_call<P, R>(
    func: &Func,
    store: &mut Store<WasmStore>,
    params: P,
) -> Result<R>
where
    P: ComponentNamedList + Lower,
    R: ComponentNamedList + Lift,
{
    let typed: TypedFunc<P, R> = func.typed(&*store)?;
    let result = typed.call(&mut *store, params)?;
    func.post_return(&mut *store)?;
    Ok(result)
}

fn ensure_cached<'a>(
    cache: &'a mut HashMap<
        (*const WasmPlugin, String),
        (Store<WasmStore>, Instance),
    >,
    plugin: &Arc<WasmPlugin>,
    name: &str,
) -> Result<&'a mut (Store<WasmStore>, Instance)> {
    let cache_key = (Arc::as_ptr(plugin), name.to_string());
    if !cache.contains_key(&cache_key) {
        let mut store = plugin.create_store()?;
        let instance =
            plugin.linker.instantiate(&mut store, &plugin.component)?;
        cache.insert(cache_key.clone(), (store, instance));
    }
    Ok(cache.get_mut(&cache_key).unwrap())
}

// --- Index 実装 ---

impl Index for WasmPluginAdapter {
    fn role(&self) -> ScanRole {
        ScanRole::Other
    }

    fn sql_type(&self) -> BiticalType {
        self.bitical_type
    }

    fn target_table(&self) -> TargetTable {
        TargetTable::BaseTags
    }

    fn extract(&self, path: &Path) -> Result<Option<Bitical>> {
        let path_str = std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .to_string();

        INSTANCE_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            let (store, instance) =
                ensure_cached(&mut cache, &self.plugin, &self.name)?;

            let idx_export = instance
                .get_export(&mut *store, None, "ttfm:plugin/indexing")
                .context("indexing interface not found")?;
            let tf_idx = instance
                .get_export(&mut *store, Some(&idx_export), "tag-file")
                .context("tag-file function not found")?;
            let func = instance
                .get_func(&mut *store, tf_idx)
                .context("Failed to get tag-file Func")?;
            let (results,) = wasm_call::<(String,), (Vec<WasmTagValue>,)>(
                &func,
                store,
                (path_str.clone(),),
            )
            .with_context(|| {
                format!("Wasm execution error for file: {path_str}")
            })?;

            Ok(results.into_iter().find_map(Bitical::from_wasm_value))
        })
    }
}

// --- Query 実装 ---

impl Query for WasmPluginAdapter {
    fn logical_type(&self) -> crate::query::logical_schema::LogicalType {
        crate::query::lens_schema::sql_to_logical(self.bitical_type)
    }

    fn interpret(
        &self,
        first: &Operand,
        op: ComparisonOp,
        label: &Label,
    ) -> Result<QueryNode> {
        let label_str = label.as_str().to_string();
        let result = INSTANCE_CACHE.with(|cache| -> Result<Option<String>> {
            let mut cache = cache.borrow_mut();
            let (store, instance) =
                ensure_cached(&mut cache, &self.plugin, &self.name)?;

            let q_export = instance
                .get_export(&mut *store, None, "ttfm:plugin/query")
                .context("query interface not found")?;
            let fn_idx = instance
                .get_export(&mut *store, Some(&q_export), "normalize-label")
                .context("normalize-label not found")?;
            let func = instance
                .get_func(&mut *store, fn_idx)
                .context("Failed to get normalize-label Func")?;
            let (r,) = wasm_call::<(String,), (Option<String>,)>(
                &func,
                store,
                (label_str,),
            )?;
            Ok(r)
        });
        let normalized = match result {
            Ok(Some(s)) => Label::from(s),
            _ => label.clone(),
        };
        QueryNode::Comparison(ComparisonNode {
            first: first.clone(),
            rest: vec![(op, Operand::Literal(normalized))],
        }).to_ok()
    }

    fn expand(
        &self,
        tagtype: &TagType,
        label: &Label,
        tag: &crate::types::TypedTag,
        _schema: &dyn crate::query::logical_schema::LogicalSchema,
    ) -> Result<QueryNode> {
        let tag_type_str = tagtype.as_str().to_string();
        let label_str = label.as_str().to_string();
        let result = INSTANCE_CACHE.with(|cache| -> Result<Option<String>> {
            let mut cache = cache.borrow_mut();
            let (store, instance) =
                ensure_cached(&mut cache, &self.plugin, &self.name)?;

            let q_export = instance
                .get_export(&mut *store, None, "ttfm:plugin/query")
                .context("query interface not found")?;
            let fn_idx = instance
                .get_export(&mut *store, Some(&q_export), "expand")
                .context("expand not found")?;
            let func = instance
                .get_func(&mut *store, fn_idx)
                .context("Failed to get expand Func")?;
            let (r,) = wasm_call::<(String, String), (Option<String>,)>(
                &func,
                store,
                (tag_type_str, label_str),
            )?;
            Ok(r)
        });
        match result {
            Ok(Some(ttql)) => crate::tag::ttql_parse(&ttql),
            _ => {
                let predicate = self.interpret(
                    &Operand::TypeRef(tagtype.clone()),
                    ComparisonOp::Label(BasicOp::Eq),
                    label,
                )?;
                QueryNode::TypedTag(tag.clone().with_node(
                    crate::query::Node::Expanded(Box::new(predicate)),
                ))
            }
        }.to_ok()
    }

    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        let tag_type_str = tagtype.as_str().to_string();
        let result = INSTANCE_CACHE.with(|cache| -> Result<Option<String>> {
            let mut cache = cache.borrow_mut();
            let (store, instance) =
                ensure_cached(&mut cache, &self.plugin, &self.name)?;

            let q_export = instance
                .get_export(&mut *store, None, "ttfm:plugin/query")
                .context("query interface not found")?;
            let fn_idx = instance
                .get_export(&mut *store, Some(&q_export), "expand-projection")
                .context("expand-projection not found")?;
            let func = instance
                .get_func(&mut *store, fn_idx)
                .context("Failed to get expand-projection Func")?;
            let (r,) = wasm_call::<(String,), (Option<String>,)>(
                &func,
                store,
                (tag_type_str,),
            )?;
            Ok(r)
        });
        match result {
            Ok(Some(ttql)) => crate::tag::ttql_parse(&ttql),
            _ => QueryNode::base_nest(Operand::from(tagtype.clone())),
        }
    }
}

// --- Display 実装 ---

impl TagDisplay for WasmPluginAdapter {
    fn formats(&self) -> DisplayFormats {
        let result =
            INSTANCE_CACHE.with(
                |cache| -> Result<(
                    Option<WasmDisplayFormat>,
                    Vec<WasmDisplayFormat>,
                )> {
                    let mut cache = cache.borrow_mut();
                    let (store, instance) =
                        ensure_cached(&mut cache, &self.plugin, &self.name)?;

                    let d_export = instance
                        .get_export(&mut *store, None, "ttfm:plugin/display")
                        .context("display interface not found")?;

                    let def_idx = instance
                        .get_export(
                            &mut *store,
                            Some(&d_export),
                            "default-format",
                        )
                        .context("default-format not found")?;
                    let def_func = instance
                        .get_func(&mut *store, def_idx)
                        .context("Failed to get default-format Func")?;
                    let (default,) = wasm_call::<
                        (),
                        (Option<WasmDisplayFormat>,),
                    >(
                        &def_func, store, ()
                    )?;

                    let fmts_idx = instance
                        .get_export(&mut *store, Some(&d_export), "formats")
                        .context("formats not found")?;
                    let fmts_func = instance
                        .get_func(&mut *store, fmts_idx)
                        .context("Failed to get formats Func")?;
                    let (options,) = wasm_call::<(), (Vec<WasmDisplayFormat>,)>(
                        &fmts_func,
                        store,
                        (),
                    )?;

                    Ok((default, options))
                },
            );
        match result {
            Ok((default, options)) => DisplayFormats {
                default: default
                    .map(|f| DisplayFormat::new(f.id, f.label))
                    .unwrap_or_default(),
                options: options
                    .into_iter()
                    .map(|f| DisplayFormat::new(f.id, f.label))
                    .collect(),
            },
            _ => DisplayFormats::default(),
        }
    }

    fn show(&self, value: &Bitical, format: DisplayFormat) -> String {
        let value_str = value.as_display_name();
        let format_id = format.id.clone();
        let result = INSTANCE_CACHE.with(|cache| -> Result<String> {
            let mut cache = cache.borrow_mut();
            let (store, instance) =
                ensure_cached(&mut cache, &self.plugin, &self.name)?;

            let d_export = instance
                .get_export(&mut *store, None, "ttfm:plugin/display")
                .context("display interface not found")?;
            let fn_idx = instance
                .get_export(&mut *store, Some(&d_export), "show")
                .context("show not found")?;
            let func = instance
                .get_func(&mut *store, fn_idx)
                .context("Failed to get show Func")?;
            let (r,) = wasm_call::<(String, String), (String,)>(
                &func,
                store,
                (value_str.clone(), format_id),
            )?;
            Ok(r)
        });
        result.unwrap_or(value_str)
    }
}

impl Bitical {
    fn from_wasm_value(v: WasmTagValue) -> Option<Bitical> {
        match v {
            WasmTagValue::Text(s) => Some(Bitical::String(s)),
            WasmTagValue::BigInt(i) => Some(Bitical::Integer(i)),
            WasmTagValue::Boolean(b) => Some(Bitical::Boolean(b)),
            WasmTagValue::Double(f) => Some(Bitical::Double(f)),
            WasmTagValue::Empty => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::logical_schema::LogicalType;

    /// mimetype fixture から得た WasmPluginAdapter の bitical_type だけを差し替える。
    /// wasm 呼び出しを伴わない logical_type() の導出ロジックのみを検証するため。
    fn adapter_with_sql_type(bitical_type: BiticalType) -> WasmPluginAdapter {
        let plugin = crate::plugins::WasmPlugin::new(Path::new(
            "plugins/mimetype_plugin.component.wasm",
        ))
        .expect("Failed to load fixture plugin");
        let base = plugin.into_adapter().expect("Failed to create adapter");
        WasmPluginAdapter {
            bitical_type,
            ..base
        }
    }

    #[test]
    fn test_logical_type_derives_integer_from_bigint_sql_type() {
        let adapter = adapter_with_sql_type(BiticalType::Integer);
        assert_eq!(adapter.query().logical_type(), LogicalType::Integer);
    }

    #[test]
    fn test_logical_type_derives_float_from_double_sql_type() {
        let adapter = adapter_with_sql_type(BiticalType::Double);
        assert_eq!(adapter.query().logical_type(), LogicalType::Float);
    }

    #[test]
    fn test_logical_type_derives_boolean_from_boolean_sql_type() {
        let adapter = adapter_with_sql_type(BiticalType::Boolean);
        assert_eq!(adapter.query().logical_type(), LogicalType::Boolean);
    }
}
