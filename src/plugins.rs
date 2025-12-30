use wasmtime::component::*;
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView, ResourceTable};
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use crate::functions::{TagFunction, escape};
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
        
        Ok(Self { engine, component, linker })
    }

    /// プラグインを実行可能なアダプターに変換します。
    pub fn into_adapter(self) -> Result<WasmPluginAdapter> {
        // WASIコンテキストを作成し、ルートディレクトリへの読み取り権限を付与
        let wasi_ctx = WasiCtxBuilder::new()
            .inherit_stdout()
            .inherit_stderr()
            .preopened_dir("/", "/", wasmtime_wasi::DirPerms::READ, wasmtime_wasi::FilePerms::READ)?
            .build();
            
        let resource_table = ResourceTable::new();
            
        let mut store = Store::new(&self.engine, WasmStore { wasi_ctx, resource_table });
        let plugin = Plugin::instantiate(&mut store, &self.component, &self.linker)?;
        
        // プラグイン情報を取得
        let info = plugin.ttfm_plugin_core().call_get_info(&mut store)?;
        
        Ok(WasmPluginAdapter {
            instance: Arc::new(Mutex::new(WasmInstance { store, plugin })),
            name: info.name,
            kind: info.kind,
        })
    }
}

/// インスタンス化されたWasmプラグインの実体。
struct WasmInstance {
    store: Store<WasmStore>,
    plugin: Plugin,
}

/// Wasmプラグインを `TagFunction` として扱うためのアダプター。
pub struct WasmPluginAdapter {
    instance: Arc<Mutex<WasmInstance>>,
    /// プラグインの名前（タグ名）
    pub name: String,
    #[allow(dead_code)]
    kind: PluginKind,
}

impl Tagger for WasmPluginAdapter {
    fn get_columns(&self) -> Vec<ColumnDef> {
        let mut inst = self.instance.lock().unwrap();
        let WasmInstance { store, plugin } = &mut *inst;
        let interface = plugin.ttfm_plugin_tag_function();
        let cols = interface.call_get_columns(store).unwrap_or_default();
        
        cols.into_iter().map(|c| ColumnDef {
            name: c.name,
            sql_type: Box::leak(c.sql_type.into_boxed_str()), 
        }).collect()
    }

    fn tag_file(&self, path: &Path) -> Result<Vec<TagValue>> {
        let mut inst = self.instance.lock().unwrap();
        
        // WASIのルートマッピングに合わせて絶対パスに変換
        let abs_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let path_str = abs_path.to_string_lossy().to_string();
        
        let WasmInstance { store, plugin } = &mut *inst;
        let interface = plugin.ttfm_plugin_tag_function();
        let results = interface.call_tag_file(store, &path_str)
            .map_err(|e| anyhow::anyhow!("Wasm execution error: {}", e))?;
            
        let converted: Vec<TagValue> = results.into_iter().map(convert_tag_value).collect();
        if !converted.is_empty() {
            println!("Plugin {} tagged {:?} with {:?}", self.name, path, converted);
        }
        Ok(converted)
    }
}

impl TagFunction for WasmPluginAdapter {
    fn tagger(&self) -> &dyn Tagger {
        self
    }

    fn to_sql(&self, tag: &TypedTag) -> Option<String> {
        if tag.tagtype.0 == self.name {
            let val = escape(&tag.tag.0);
            return Some(format!("{} ILIKE '%{}%'", self.name, val));
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