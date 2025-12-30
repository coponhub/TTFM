use ttfm::plugins::WasmPlugin;
use ttfm::{Tagger, TagValue};
use std::path::Path;

#[test]
fn test_wasm_plugin_mimetype() {
    let wasm_path = Path::new("tests/plugins/sample_plugin/sample_plugin.component.wasm");
    
    // プラグインのロード
    let plugin = WasmPlugin::new(wasm_path)
        .expect("Failed to load Wasm plugin");
    
    // アダプターの作成
    let adapter = plugin.into_adapter()
        .expect("Failed to create adapter");
    
    // カラム定義の確認
    let columns = adapter.get_columns();
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].name, "mimetype");
    
    // タグ付けの実行
    let results = adapter.tag_file(Path::new("test.txt"))
        .expect("Failed to execute tag_file");
    
    assert_eq!(results.len(), 1);
    if let TagValue::Text(val) = &results[0] {
        assert_eq!(val, "text/plain");
    } else {
        panic!("Expected Text value, got {:?}", results[0]);
    }
}
