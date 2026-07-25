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

use std::collections::HashMap;
use std::path::Path;
use tempfile::tempdir;
use ttfm::plugins::WasmPlugin;
use ttfm::query::ast::{Operand, QueryNode};
use ttfm::query::logical_schema::LogicalSchema;
use ttfm::search;
use ttfm::tag::{Index, TagFunction};
use ttfm::types::{Bitical, Label, TagType};
use ttfm::SearchOptions;

/// プラグインが display を実装している場合、
/// registry.format_display() で show() が呼ばれることを検証する。
#[test]
fn test_plugin_display_show_applied_in_format_display() {
    use ttfm::tag::{Display, DisplayFormat, DisplayFormats};

    struct IconTag;
    impl TagFunction for IconTag {
        fn name(&self) -> &str {
            "icon_tag"
        }
        fn index(&self) -> Option<&dyn ttfm::tag::Index> {
            None
        }
        fn display(&self) -> Option<&dyn ttfm::tag::Display> {
            Some(self)
        }
    }
    impl Display for IconTag {
        fn formats(&self) -> DisplayFormats {
            DisplayFormats {
                default: DisplayFormat {
                    id: "icon".to_string(),
                    label: "Icon".to_string(),
                },
                options: vec![],
            }
        }
        fn show(&self, value: &Bitical, _format: DisplayFormat) -> String {
            let s = match value {
                Bitical::String(s) => s.clone(),
                Bitical::Integer(n) => n.to_string(),
                _ => String::new(),
            };
            format!("● {s}")
        }
    }

    let mut registry = ttfm::tag::TagRegistry::with_standard();
    registry.register(IconTag);

    // format_display が show() を呼んで "● modified" を返すことを確認
    let result = registry.format_display("icon_tag", "modified");
    assert_eq!(
        result, "● modified",
        "format_display が display::show() を呼んでいない"
    );
}

/// プラグインが normalize_label を実装している場合、
/// search 時にラベルの短縮形が正規化されて検索できることを検証する。
///
/// 現状バグ: expand_comparison_with_recursion が TagRegistry::with_standard() を
/// ハードコードしているため、動的登録プラグインの normalize_label が呼ばれない。
#[test]
fn test_plugin_normalize_label_applied_in_search() {
    use std::path::Path as StdPath;

    struct ShortLabelTag;
    impl TagFunction for ShortLabelTag {
        fn name(&self) -> &str {
            "my_status"
        }
        fn index(&self) -> Option<&dyn ttfm::tag::Index> {
            Some(self)
        }
        fn query(&self) -> &dyn ttfm::tag::Query {
            self
        }
    }
    impl ttfm::tag::Index for ShortLabelTag {
        fn extract(&self, _path: &StdPath) -> anyhow::Result<Option<Bitical>> {
            // 全ファイルに "modified" タグを付与
            Ok(Some(Bitical::String("modified".to_string())))
        }
    }
    impl ttfm::tag::Query for ShortLabelTag {
        fn normalize_label(&self, label: &Label) -> Label {
            match label.as_str().as_str() {
                "m" => Label::from("modified"),
                "c" => Label::from("clean"),
                _ => label.clone(),
            }
        }
    }

    let dir = tempdir().unwrap();
    let db_dir = dir.path().join("db");
    let store = ttfm::db::Store::open(&db_dir).unwrap();

    let mut registry = ttfm::tag::TagRegistry::with_standard();
    registry.register(ShortLabelTag);

    let test_file = dir.path().join("test.txt");
    std::fs::write(&test_file, "hello").unwrap();

    ttfm::indexing::Indexer::new(&store, &registry)
        .initialize_tables()
        .unwrap();
    ttfm::indexing::Indexer::new(&store, &registry)
        .run(dir.path(), None::<&fn(usize)>, false)
        .unwrap();

    // "my_status:m" で検索 → normalize_label("m") == "modified" なのでヒットするはず
    let results = search::search_nowarn(
        &store,
        &registry,
        "my_status:m",
        SearchOptions::default(),
    )
    .unwrap()
    .results;

    assert!(
        !results.is_empty(),
        "normalize_label が search に適用されていない: my_status:m → my_status:modified のヒットがゼロ"
    );
}

#[test]
fn test_wasm_plugin_mimetype() {
    let wasm_path = Path::new("plugins/sample_plugin.component.wasm");

    let plugin =
        WasmPlugin::new(wasm_path).expect("Failed to load Wasm plugin");
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
        ttfm::indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();
        registry.load_from_dir(&user_plugins_dir, &status).unwrap();
        registry.load_builtins(&status).unwrap();
        ttfm::indexing::Indexer::new(&store, &registry)
            .run(dir.path(), None::<&fn(usize)>, false)
            .unwrap();
        search::search_nowarn(
            &store,
            &registry,
            "mimetype:application/x-test-override",
            SearchOptions::default(),
        )
        .unwrap()
        .results
    };

    // ユーザープラグインなし: ビルトインが使われる
    let without_override = {
        let db_dir = dir.path().join("db_builtin");
        let mut registry = ttfm::tag::TagRegistry::with_standard();
        let store = ttfm::db::Store::open(&db_dir).unwrap();
        ttfm::indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();
        registry.load_builtins(&status).unwrap();
        ttfm::indexing::Indexer::new(&store, &registry)
            .run(dir.path(), None::<&fn(usize)>, false)
            .unwrap();
        search::search_nowarn(
            &store,
            &registry,
            "mimetype:application/x-test-override",
            SearchOptions::default(),
        )
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
    let plugin =
        WasmPlugin::new(Path::new("plugins/sample_plugin.component.wasm"))
            .expect("Failed to load sample plugin");
    plugin.into_adapter().expect("Failed to create adapter")
}

/// プラグインが display インターフェースを実装していれば adapter.display() は Some を返す
#[test]
fn test_wasm_adapter_display_is_some() {
    let adapter = load_sample_adapter();
    assert!(
        adapter.display().is_some(),
        "adapter.display() should return Some"
    );
}

/// プラグインが normalize-label で None を返す場合、ラベルは変更されない
#[test]
fn test_wasm_adapter_normalize_label_default() {
    let adapter = load_sample_adapter();
    let query = adapter.query();
    let label = Label::from("hello");
    assert_eq!(query.normalize_label(&label).as_str(), "hello");
}

/// プラグインが expand で None を返す場合、TypedTag のデフォルト動作を使う
#[test]
fn test_wasm_adapter_expand_default_returns_typed_tag() {
    let adapter = load_sample_adapter();
    let query = adapter.query();
    let tag_type = TagType::from("sample");
    let label = Label::from("foo");
    let typed_tag = ttfm::types::TypedTag::new(tag_type.clone(), label.clone());
    let node = query.expand(
        &tag_type,
        &label,
        &typed_tag,
        &ttfm::query::lens_schema::Lens::base_standard(),
    );
    assert_eq!(node, QueryNode::TypedTag(typed_tag));
}

/// プラグインが expand-projection で None を返す場合、Projection のデフォルト動作を使う
#[test]
fn test_wasm_adapter_expand_projection_default() {
    let adapter = load_sample_adapter();
    let query = adapter.query();
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

/// indexing のみ実装（query 未 export）のプラグインでも adapter.query() は
/// 常に使え（Option ではない）、デフォルト展開が使われる（Step 4.1）
#[test]
fn test_wasm_adapter_query_available_without_query_export() {
    let plugin =
        WasmPlugin::new(Path::new("plugins/mimetype_plugin.component.wasm"))
            .expect("Failed to load mimetype plugin");
    let adapter = plugin.into_adapter().expect("Failed to create adapter");

    // query 未 export でも adapter.query() は &dyn Query を返す
    let query = adapter.query();

    let tag_type = TagType::from("mimetype");
    let label = Label::from("text/plain");
    let typed_tag = ttfm::types::TypedTag::new(tag_type.clone(), label.clone());
    let node = query.expand(
        &tag_type,
        &label,
        &typed_tag,
        &ttfm::query::lens_schema::Lens::base_standard(),
    );
    assert_eq!(node, QueryNode::TypedTag(typed_tag));
}

/// indexing のみのプラグイン型も Lens::iter_all_for_rank に載る（定義一覧の完全化）
#[test]
fn test_lens_iter_all_for_rank_includes_indexing_only_plugin_type() {
    let mut registry = ttfm::tag::TagRegistry::with_standard();
    let plugin =
        WasmPlugin::new(Path::new("plugins/mimetype_plugin.component.wasm"))
            .expect("Failed to load mimetype plugin");
    let adapter = plugin.into_adapter().expect("Failed to create adapter");
    registry.register_plugin(adapter);

    let lens = ttfm::query::lens_schema::Lens::from_registry(&registry);
    let all = lens.iter_all_for_rank();
    let mimetype = all.iter().find(|(t, _, _)| t.as_str() == "mimetype");
    assert!(
        mimetype.is_some(),
        "indexing のみのプラグイン型 (mimetype) が Lens 列挙に含まれていない"
    );
    assert!(
        matches!(
            mimetype.unwrap().2,
            ttfm::types::ItemId::Settling(ttfm::types::Origin::Plugin, _)
        ),
        "プラグイン登録型は固定 Sys id を持たない"
    );
}

/// 実 WASM プラグイン（native mock ではなく実コンポーネント）を register_plugin
/// した場合も、その型の定義アイテムを編集すると Plugin 区画へ実体化されることを
/// 確認する smoke テスト（Step 7）。
#[test]
fn test_wasm_plugin_type_edit_materializes_in_plugin_block() {
    use ttfm::db::Store;
    use ttfm::edit::{edit, QueryType, WriteOptions};
    use ttfm::indexing::Indexer;

    let dir = tempdir().unwrap();
    let root = dir.path().join("files");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("foo.txt"), "content").unwrap();

    let db_dir = dir.path().join("db");
    let mut registry = ttfm::tag::TagRegistry::with_standard();
    let adapter = load_sample_adapter();
    let name = adapter.name().to_string();
    registry.register_plugin(adapter);

    let store = Store::open(&db_dir).unwrap();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    Indexer::new(&store, &registry)
        .run(&root, None::<&fn(usize)>, false)
        .unwrap();

    edit(
        &store,
        &registry,
        &format!("type:\"{name}\""),
        Some("rank:5"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )
    .expect("edit should materialize the plugin type definition");

    let results =
        search::search_nowarn(&store, &registry, "type:*", SearchOptions::default())
            .unwrap();
    let plugin_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == name)
        .expect("type: should include the real WASM plugin's type after materialization");

    assert!(plugin_item.id.is_stored());
    assert_eq!(
        ttfm::types::Origin::within(plugin_item.id.as_i64()),
        ttfm::types::Origin::Plugin
    );
    assert_eq!(plugin_item.rank, 5);
}
