use std::collections::HashMap;
use std::path::Path;
use tempfile::tempdir;
use ttfm::search;
use ttfm::plugins::WasmPlugin;
use ttfm::tag::{Index, Query, TagFunction};
use ttfm::types::{Label, LabelValue, TagType};
use ttfm::query::ast::{ComparisonNode, ComparisonOp, Operand, QueryNode};
use ttfm::SearchOptions;

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
        "tests/fixtures/mimetype_override_test_plugin.component.wasm",
        user_plugins_dir.join("my_custom_mimetype.component.wasm"),
    )
    .expect("Failed to copy override plugin");

    let test_file = dir.path().join("test.txt");
    std::fs::write(&test_file, "hello").unwrap();

    // ユーザープラグインあり: オーバーライドプラグインが優先される
    let with_override = {
        let db_dir = dir.path().join("db_override");
        let mut registry = ttfm::tag::TagRegistry::with_standard();
        let store = ttfm::db::Store::open(&db_dir).unwrap();
        ttfm::indexing::Indexer::new(&store, &registry).initialize_tables().unwrap();
        let cache = ttfm::CacheManager::new(store.db_dir.join("cache"), 0);
        registry.load_from_dir(&user_plugins_dir, &status).unwrap();
        registry.load_builtins(&status).unwrap();
        ttfm::indexing::Indexer::new(&store, &registry).run(dir.path(), None::<&fn(usize)>, false).unwrap();
        search::search(&store, &registry, &cache, "mimetype:application/x-test-override", SearchOptions::default())
            .unwrap()
            .results
    };

    // ユーザープラグインなし: ビルトインが使われる
    let without_override = {
        let db_dir = dir.path().join("db_builtin");
        let mut registry = ttfm::tag::TagRegistry::with_standard();
        let store = ttfm::db::Store::open(&db_dir).unwrap();
        ttfm::indexing::Indexer::new(&store, &registry).initialize_tables().unwrap();
        let cache = ttfm::CacheManager::new(store.db_dir.join("cache"), 0);
        registry.load_builtins(&status).unwrap();
        ttfm::indexing::Indexer::new(&store, &registry).run(dir.path(), None::<&fn(usize)>, false).unwrap();
        search::search(&store, &registry, &cache, "mimetype:application/x-test-override", SearchOptions::default())
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

fn load_sample_adapter() -> ttfm::plugins::WasmPluginAdapter {
    let plugin = WasmPlugin::new(Path::new("plugins/sample_plugin.component.wasm"))
        .expect("Failed to load sample plugin");
    plugin.into_adapter().expect("Failed to create adapter")
}

/// プラグインが query インターフェースを実装していれば adapter.query() は Some を返す
#[test]
fn test_wasm_adapter_query_is_some() {
    let adapter = load_sample_adapter();
    assert!(adapter.query().is_some(), "adapter.query() should return Some");
}

/// プラグインが display インターフェースを実装していれば adapter.display() は Some を返す
#[test]
fn test_wasm_adapter_display_is_some() {
    let adapter = load_sample_adapter();
    assert!(adapter.display().is_some(), "adapter.display() should return Some");
}

/// プラグインが normalize-label で None を返す場合、ラベルは変更されない
#[test]
fn test_wasm_adapter_normalize_label_default() {
    let adapter = load_sample_adapter();
    let query = adapter.query().expect("adapter.query() should be Some");
    let label = Label::from("hello");
    assert_eq!(query.normalize_label(&label).as_str(), "hello");
}

/// プラグインが expand で None を返す場合、TypedTag のデフォルト動作を使う
#[test]
fn test_wasm_adapter_expand_default_returns_typed_tag() {
    let adapter = load_sample_adapter();
    let query = adapter.query().expect("adapter.query() should be Some");
    let tag_type = TagType::from("sample");
    let label = Label::from("foo");
    let typed_tag = ttfm::types::TypedTag::new(tag_type.clone(), label.clone());
    let node = query.expand(&tag_type, &label, &typed_tag);
    assert_eq!(node, QueryNode::TypedTag(typed_tag));
}

/// プラグインが expand-projection で None を返す場合、Projection のデフォルト動作を使う
#[test]
fn test_wasm_adapter_expand_projection_default() {
    let adapter = load_sample_adapter();
    let query = adapter.query().expect("adapter.query() should be Some");
    let tag_type = TagType::from("sample");
    let node = query.expand_projection(&tag_type);
    let expected = QueryNode::Projection(Operand::from(tag_type));
    assert_eq!(node, expected);
}

/// プラグインが display::default-format で None を返す場合、デフォルトフォーマット id は "raw"
#[test]
fn test_wasm_adapter_display_formats_default() {
    let adapter = load_sample_adapter();
    let display = adapter.display().expect("adapter.display() should be Some");
    let formats = display.formats();
    assert_eq!(formats.default.id, "raw");
    assert!(formats.options.is_empty());
}
