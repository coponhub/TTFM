use std::collections::HashMap;
use std::path::Path;
use tempfile::tempdir;
use ttfm::plugins::WasmPlugin;
use ttfm::tag::{Index, TagFunction};
use ttfm::{FileManager, SearchOptions};

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

/// ユーザープラグインがビルトインと同じパッケージ名を持つ場合、
/// ファイル名に関係なくパッケージ名でオーバーライドが判定されることを検証する。
#[test]
fn test_user_plugin_overrides_builtin_by_package_name() {
    let dir = tempdir().unwrap();
    let status: HashMap<String, bool> = HashMap::new();

    // ユーザープラグインディレクトリにオーバーライドプラグインを配置
    // ファイル名はパッケージ名と無関係（パッケージ名 "mimetype" はWIT get_info()で決まる）
    let user_plugins_dir = dir.path().join("plugins");
    std::fs::create_dir_all(&user_plugins_dir).unwrap();
    std::fs::copy(
        "tests/fixtures/mimetype_override.component.wasm",
        user_plugins_dir.join("my_custom_mimetype.component.wasm"),
    )
    .expect("Failed to copy override plugin");

    let test_file = dir.path().join("test.txt");
    std::fs::write(&test_file, "hello").unwrap();

    // ユーザープラグインあり: オーバーライドプラグインが優先される
    let with_override = {
        let db_dir = dir.path().join("db_override");
        let mut fm = FileManager::new_with_db_dir(&db_dir).unwrap();
        fm.load_plugins(&user_plugins_dir, &status).unwrap();
        fm.load_builtin_plugins(&status).unwrap();
        fm.index_directory(dir.path(), None::<&fn(usize)>, false).unwrap();
        fm.search("mimetype:application/x-test-override", SearchOptions::default())
            .unwrap()
            .results
    };

    // ユーザープラグインなし: ビルトインが使われる
    let without_override = {
        let db_dir = dir.path().join("db_builtin");
        let mut fm = FileManager::new_with_db_dir(&db_dir).unwrap();
        fm.load_builtin_plugins(&status).unwrap();
        fm.index_directory(dir.path(), None::<&fn(usize)>, false).unwrap();
        fm.search("mimetype:application/x-test-override", SearchOptions::default())
            .unwrap()
            .results
    };

    assert!(
        !with_override.is_empty(),
        "オーバーライドプラグインによるmimetypeタグがヒットしていない"
    );
    assert!(
        without_override.is_empty(),
        "ビルトインプラグインがオーバーライドプラグインより優先されている"
    );
}
