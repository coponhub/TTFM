use wasmtime::component::*;
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView, ResourceTable};
use anyhow::{Result, Context};
use crate::util::DotOk;
use std::path::Path;
use std::sync::Arc;
use std::cell::RefCell;
use std::collections::HashMap;

use crate::functions::{TagFunction, exists_in_tags};
use crate::taggers::{Tagger, ColumnDef, TagValue};
use crate::types::TypedTag;

// WIT定義から自動生成
bindgen!({
    path: "wit/plugin.wit",
    world: "plugin",
});

// 生成された型のパスを短縮
use exports::ttfm::plugin::core::PluginKind;
use exports::ttfm::plugin::tag_function::TagValue as WasmVal;

/// Wasmプラグインを管理するための共有ストア。
/// WASIの実行コンテキストとリソーステーブルを保持します。
struct WasmStore {
    wasi_ctx: WasiCtx,
    resource_table: ResourceTable,
}

impl WasiView for WasmStore {
    fn ctx(&mut self) -> &mut WasiCtx { &mut self.wasi_ctx }
    fn table(&mut self) -> &mut ResourceTable { &mut self.resource_table }
}

// スレッドローカルなインスタンスキャッシュ。
// (Plugin名, (Store, Pluginインスタンス)) の形式で保持します。
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
        // WASI (Preview 2) の機能をリンカーに追加
        wasmtime_wasi::add_to_linker_sync(&mut linker)?;

        Ok(Self {
            engine,
            component,
            linker,
        })
    }

    /// プラグインを実行可能なアダプターに変換します。
    pub fn into_adapter(self) -> Result<WasmPluginAdapter> {
        // カラム情報を取得するために一度だけインスタンス化
        let mut store = self
            .create_store()
            .context("Failed to create store for introspection")?;
        let plugin = Plugin::instantiate(&mut store, &self.component, &self.linker)
            .context("Failed to instantiate plugin for introspection")?;
        let info = plugin
            .ttfm_plugin_core()
            .call_get_info(&mut store)
            .context("Failed to call get_info")?;

        let interface = plugin.ttfm_plugin_tag_function();
        let wasm_cols = interface
            .call_get_columns(&mut store)
            .context("Failed to call get_columns")?;
        let columns = wasm_cols
            .into_iter()
            .map(|c| ColumnDef {
                name: c.name,
                sql_type: Box::leak(c.sql_type.into_boxed_str()),
                target_table: crate::db::TargetTable::BaseTags,
            })
            .collect();

        Ok(WasmPluginAdapter {
            plugin: Arc::new(self),
            name: info.name,
            kind: info.kind,
            columns,
        })
    }

    /// WASIコンテキストを含む新しいStoreを作成します。
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
    /// プラグインの名前（タグ名）
    pub name: String,
    #[allow(dead_code)]
    kind: PluginKind,
    /// キャッシュされたカラム定義
    columns: Vec<ColumnDef>,
}

impl Tagger for WasmPluginAdapter {
    fn get_columns(&self) -> Vec<ColumnDef> {
        self.columns.clone()
    }

    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let abs_path =
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let path_str = abs_path.to_string_lossy().to_string();

        // スレッドローカルキャッシュからインスタンスを取得、なければ作成
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

            let (store, plugin) = cache.get_mut(&self.name).ok_or_else(|| {
                anyhow::anyhow!("Plugin instance missing from cache")
            })?;
            let interface = plugin.ttfm_plugin_tag_function();

            interface.call_tag_file(store, &path_str).with_context(|| {
                format!("Wasm execution error for file: {}", path_str)
            })
        })?;

        results
            .into_iter()
            .map(convert_tag_value)
            .collect::<Vec<_>>()
            .to_ok()
    }
}

impl TagFunction for WasmPluginAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn tagger(&self) -> Option<&dyn Tagger> {
        Some(self)
    }

    fn to_expr(&self, tag: &TypedTag) -> Option<sea_query::SimpleExpr> {
        if tag.tagtype.0 == self.name {
            return Some(exists_in_tags(&self.name, &tag.label.0, false));
        }
        None
    }
}

/// Wasm側の `TagValue` をホスト側の `TagValue` に変換します。
fn convert_tag_value(v: WasmVal) -> TagValue {
    match v {
        WasmVal::Text(s) => TagValue::Text(s),
        WasmVal::BigInt(i) => TagValue::BigInt(i),
        WasmVal::Boolean(b) => TagValue::Boolean(b),
        WasmVal::Empty => TagValue::Null,
    }
}
