use std::path::Path;
use ttfm::plugins::WasmPlugin;
use ttfm::tag::{Index, TagFunction};

#[test]
fn test_wasm_plugin_mimetype() {
    let wasm_path = Path::new("plugins/sample_plugin.component.wasm");

    let plugin = WasmPlugin::new(wasm_path).expect("Failed to load Wasm plugin");
    let adapter = plugin.into_adapter().expect("Failed to create adapter");

    let name = adapter.name();
    assert!(name == "sample" || name == "mimetype");

    // 1回目: インスタンス化が走る
    let result1 = adapter
        .extract(Path::new("Cargo.toml"))
        .expect("Failed to execute extract (1st)");

    // 2回目: キャッシュされたインスタンスが使われる
    let result2 = adapter
        .extract(Path::new("Cargo.toml"))
        .expect("Failed to execute extract (2nd)");

    assert_eq!(result1, result2);
}
