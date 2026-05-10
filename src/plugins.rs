use anyhow::{Context, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use wasmtime::component::*;
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiView};

use crate::db::{SqlType, TargetTable};
use crate::tag::{Index, ScanRole, TagFunction};
use crate::types::{LabelValue, Rank};

// WIT定義から自動生成
bindgen!({
    path: "wit/plugin.wit",
    world: "plugin",
});

// 生成された型のパスを短縮
use exports::ttfm::plugin::core::{PluginKind, ValueType};
use exports::ttfm::plugin::indexing_function::TagValue as WasmVal;

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
thread_local! {
    static INSTANCE_CACHE: RefCell<HashMap<String, (Store<WasmStore>, Plugin)>> =
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
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)?;
        let component = Component::from_file(&engine, path)?;

        let mut linker: Linker<WasmStore> = Linker::new(&engine);
        wasmtime_wasi::add_to_linker_sync(&mut linker)?;

        Ok(Self {
            engine,
            component,
            linker,
        })
    }

    /// プラグインを実行可能なアダプターに変換します。
    pub fn into_adapter(self) -> Result<WasmPluginAdapter> {
        let mut store = self
            .create_store()
            .context("Failed to create store for introspection")?;
        let plugin =
            Plugin::instantiate(&mut store, &self.component, &self.linker)
                .context("Failed to instantiate plugin for introspection")?;
        let info = plugin
            .ttfm_plugin_core()
            .call_get_info(&mut store)
            .context("Failed to call get_info")?;

        let sql_type = match info.value_type {
            ValueType::Text => SqlType::VARCHAR,
            ValueType::BigInt => SqlType::BIGINT,
            ValueType::Boolean => SqlType::BOOLEAN,
            ValueType::Double => SqlType::DOUBLE,
        };

        Ok(WasmPluginAdapter {
            plugin: Arc::new(self),
            name: info.name,
            #[allow(dead_code)]
            kind: info.kind,
            sql_type,
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
    kind: PluginKind,
    sql_type: SqlType,
}

impl TagFunction for WasmPluginAdapter {
    fn name(&self) -> &str {
        &self.name
    }
    fn index(&self) -> Option<&dyn Index> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::DEFAULT
    }
}

impl Index for WasmPluginAdapter {
    fn role(&self) -> ScanRole {
        ScanRole::Other
    }
    fn sql_type(&self) -> SqlType {
        self.sql_type
    }
    fn target_table(&self) -> TargetTable {
        TargetTable::BaseTags
    }
    fn extract(&self, path: &Path) -> Result<Option<LabelValue>> {
        let abs_path =
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let path_str = abs_path.to_string_lossy().to_string();

        let results = INSTANCE_CACHE.with(|cache| -> Result<Vec<WasmVal>> {
            let mut cache = cache.borrow_mut();

            if !cache.contains_key(&self.name) {
                let mut store = self.plugin.create_store()?;
                let plugin = Plugin::instantiate(
                    &mut store,
                    &self.plugin.component,
                    &self.plugin.linker,
                )?;
                cache.insert(self.name.clone(), (store, plugin));
            }

            let (store, plugin) =
                cache.get_mut(&self.name).ok_or_else(|| {
                    anyhow::anyhow!("Plugin instance missing from cache")
                })?;
            let interface = plugin.ttfm_plugin_indexing_function();

            interface.call_tag_file(store, &path_str).with_context(|| {
                format!("Wasm execution error for file: {}", path_str)
            })
        })?;

        Ok(results.into_iter().find_map(convert_wasm_val))
    }
}

/// Wasm側の `TagValue` をホスト側の `LabelValue` に変換します。
fn convert_wasm_val(v: WasmVal) -> Option<LabelValue> {
    match v {
        WasmVal::Text(s) => Some(LabelValue::String(s)),
        WasmVal::BigInt(i) => Some(LabelValue::Integer(i)),
        WasmVal::Boolean(b) => Some(LabelValue::Boolean(b)),
        WasmVal::Double(f) => Some(LabelValue::Double(f.to_bits())),
        WasmVal::Empty => None,
    }
}
