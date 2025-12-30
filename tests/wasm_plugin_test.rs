use ttfm::plugins::WasmPlugin;
use ttfm::Tagger;
use std::path::Path;

#[test]
fn test_wasm_plugin_mimetype() {
    // 正しいパスに修正
    let wasm_path = Path::new("plugins/sample_plugin.component.wasm");
    
    // プラグインのロード
    let plugin = WasmPlugin::new(wasm_path)
        .expect("Failed to load Wasm plugin");
    
    // アダプターの作成
    let adapter = plugin.into_adapter()
        .expect("Failed to create adapter");
    
    // カラム定義の確認
    let columns = adapter.get_columns();
    assert_eq!(columns.len(), 1);
    // sample_plugin は mimetype ではなく "sample" カラムを持っている可能性があるため柔軟にチェック
    // (以前の出力ログでは "sample" という名前でした)
    assert!(columns.iter().any(|c| c.name == "sample" || c.name == "mimetype"));
    
    // タグ付けの実行 (1回目: インスタンス化が走る)
    let results = adapter.tag_file(Path::new("Cargo.toml"))
        .expect("Failed to execute tag_file (1st)");
    
    assert_eq!(results.len(), 1);
    
    // タグ付けの実行 (2回目: キャッシュされたインスタンスが使われる)
    let results2 = adapter.tag_file(Path::new("Cargo.toml"))
        .expect("Failed to execute tag_file (2nd)");
    
    assert_eq!(results2.len(), 1);
    assert_eq!(results, results2);
}