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

use tempfile::TempDir;
use ttfm::{
    db::Store,
    edit::{
        edit,
        write::{write, TagOp, WriteAction},
        QueryType, WriteOptions,
    },
    indexing::Indexer,
    tag::{TagFunction, TagRegistry},
    types::{ItemId, ItemKind, SType, TagType, TypedTag},
    SearchOptions,
};

fn setup() -> (Store, TagRegistry, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("files");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("foo.txt"), "content").unwrap();

    let db_dir = base.join("db");
    let registry = TagRegistry::with_standard();
    let store = Store::open(&db_dir).unwrap();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    Indexer::new(&store, &registry)
        .run_single(&root, None::<&fn(usize)>, false)
        .unwrap();
    (store, registry, dir)
}

// `type:*` は型定義アイテムをフラットリストで返すべき: 組み込み登録型は固定
// Sys id の Stored、データ中で使用中のユーザー定義タグの型は Volatile として
// 両方を含む。
#[test]
fn type_includes_builtin_and_user_defined_types() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    // (b) の準備: project:x を付与。edit 経由なので type 定義行は作られない。
    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:x"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;

    // (a) 組み込み登録型（hash）が固定 Sys id の Stored・registry の default_rank で載る
    let hash_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "hash")
        .expect("type: query should include built-in type 'hash'");
    assert!(
        hash_item.id.is_stored(),
        "hash should be Stored with a fixed Sys id (built-in type definition)"
    );
    assert_eq!(
        ttfm::types::Origin::within(hash_item.id.as_i64()),
        ttfm::types::Origin::Builtin,
    );
    assert_eq!(
        hash_item.rank,
        ttfm::rank::get_rank_by_name(&registry, "hash"),
        "hash rank should fall back to the registry's default_rank"
    );

    // (b) ユーザー定義タグの型（project）が Volatile で載る
    let project_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "project")
        .expect("type: query should include user-defined tag type 'project'");
    assert!(
        !project_item.id.is_stored(),
        "project type should not be Stored (no definition row was ever created)"
    );

    Ok(())
}

// `tag:*` は全 tag 定義アイテムを返すべき: ユーザーが実際に付与したタグ
// （実体化されていない Volatile 合成）を含む。
#[test]
fn tag_includes_used_tag_definitions() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:x"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "tag:*",
        SearchOptions::default(),
    )?;

    let tag_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "project:x")
        .expect("tag: query should include used tag 'project:x'");
    assert!(
        !tag_item.id.is_stored(),
        "project:x should not be Stored (no definition row was ever created)"
    );

    Ok(())
}

// 定義アイテムが Stored（write で実体化済み）の場合、type: query はその
// Stored 行だけを返すべきで、同名の Volatile 合成と重複してはいけない。
#[test]
fn type_prefers_stored_definition_over_volatile_duplicate() -> anyhow::Result<()>
{
    let (store, registry, _dir) = setup();

    let resp = write(
        &store,
        &registry,
        vec![WriteAction::Add {
            item: ItemId::Volatile(0),
            tags: vec![
                TagOp::Append(TypedTag::new(SType::ItemKind, "type")),
                TagOp::Append(TypedTag::new(SType::Content, "hash")),
                TagOp::Append(TypedTag::new(SType::Rank, 999)),
            ],
        }],
    )?;
    let stored_id = resp.new_item_ids[0];

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;

    let hash_items: Vec<_> = results
        .results
        .iter()
        .filter(|r| r.raw_repr() == "hash")
        .collect();
    assert_eq!(
        hash_items.len(),
        1,
        "hash should appear exactly once (Stored, not duplicated by Volatile), got {:?}",
        hash_items
    );
    assert!(
        !hash_items[0].id.is_volatile(),
        "hash should be Stored after materialization"
    );
    assert_eq!(hash_items[0].id, ItemId::from(stored_id));
    assert_eq!(
        hash_items[0].rank, 999,
        "hash rank should be the Stored (user-edited) rank, not the registry default"
    );

    Ok(())
}

// Step 9.1: Stored 定義行にユーザーが付けたタグ（実 EAV タグ）は、
// type:* の表示に実タグとして出るべき。
#[test]
fn type_stored_row_shows_user_attached_tags() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    let resp = write(
        &store,
        &registry,
        vec![WriteAction::Add {
            item: ItemId::Volatile(0),
            tags: vec![
                TagOp::Append(TypedTag::new(SType::ItemKind, "type")),
                TagOp::Append(TypedTag::new(SType::Content, "hash")),
                TagOp::Append(TypedTag::new(SType::Rank, 999)),
            ],
        }],
    )?;
    let stored_id = resp.new_item_ids[0];

    edit(
        &store,
        &registry,
        &format!("item_id:{stored_id}"),
        Some("project:x"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;

    let hash_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "hash")
        .expect("hash should still be in the type: result");
    assert!(
        hash_item
            .get_all_values("project")
            .contains(&"x".to_string()),
        "Stored definition item's user-attached tag should be visible \
         in type:* display, got tags: {:?}",
        hash_item.tags
    );

    Ok(())
}

// type:* の Volatile 行には type:"type" タグが合成されるべき。
// (組み込み型は Step 4 以降 Sys id の Stored になるため、Volatile のままの
// データ中で使用中の型（project）を例に使う。)
#[test]
fn type_volatile_items_get_type_instance_tag() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:x"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;

    let project_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "project")
        .expect("project should be in the type: result as Volatile");
    assert!(!project_item.id.is_stored());
    assert!(
        project_item.get_all_values("type").contains(&"type".to_string()),
        "Volatile type-definition item should carry type:\"type\", got tags: {:?}",
        project_item.tags
    );

    Ok(())
}

// tag:* の Volatile 行には type:"tag" タグが合成されるべき。
#[test]
fn tag_volatile_items_get_type_instance_tag() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:x"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "tag:*",
        SearchOptions::default(),
    )?;

    let tag_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "project:x")
        .expect("project:x should be in the tag: result as Volatile");
    assert!(!tag_item.id.is_stored());
    assert!(
        tag_item.get_all_values("type").contains(&"tag".to_string()),
        "Volatile tag-definition item should carry type:\"tag\", got tags: {:?}",
        tag_item.tags
    );

    Ok(())
}

// type:* の Volatile 行には name タグ（型名）が tags.entries に
// 実体として乗るべき（representative 経由の get_all_values フォールバックに
// 頼らず、iter_type_groups のカラム検出が使う経路そのものを検証する）。
// (組み込み型は Step 4 以降 Sys id の Stored になるため、Volatile のままの
// データ中で使用中の型（project）を例に使う。)
#[test]
fn type_volatile_items_get_name_tag() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:x"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;

    let project_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "project")
        .expect("project should be in the type: result as Volatile");
    assert!(
        project_item.tags.entries.iter().any(|e| {
            e.typed_tag.tag_type() == TagType::from("name") && e.typed_tag.as_str() == "project"
        }),
        "Volatile type-definition item should have a real name tag entry, got tags: {:?}",
        project_item.tags
    );

    Ok(())
}

// tag:* の Volatile 行にも同様に name タグが tags.entries に乗るべき。
#[test]
fn tag_volatile_items_get_name_tag() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:x"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "tag:*",
        SearchOptions::default(),
    )?;

    let tag_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "project:x")
        .expect("project:x should be in the tag: result as Volatile");
    assert!(
        tag_item.tags.entries.iter().any(|e| {
            e.typed_tag.tag_type() == TagType::from("name")
                && e.typed_tag.as_str() == "project:x"
        }),
        "Volatile tag-definition item should have a real name tag entry, got tags: {:?}",
        tag_item.tags
    );

    Ok(())
}

// バグA: Or の一方の枝が type: プロジェクション、もう一方が定義参照の完全一致検索
// （未登録の値）の場合、完全一致検索側が合成する Volatile 行が fetch の src 再結合で
// 脱落してはいけない。
#[test]
fn definition_ref_exact_match_survives_or_with_projection() -> anyhow::Result<()>
{
    let (store, registry, _dir) = setup();

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type: | type:\"nosuchtype\"",
        SearchOptions::default(),
    )?;

    let nosuchtype_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "nosuchtype")
        .expect(
            "Or should include the Volatile row synthesized by the exact-match branch",
        );
    assert!(
        !nosuchtype_item.id.is_stored(),
        "nosuchtype should not be Stored (never registered as a definition row)"
    );

    Ok(())
}

// バグA (glob検索側): Or の一方の枝が type:* の glob検索、もう一方が未登録の
// 完全一致検索の場合も同様に脱落してはいけない。
#[test]
fn definition_ref_exact_match_survives_or_with_glob_search(
) -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:* | type:\"nosuchtype\"",
        SearchOptions::default(),
    )?;

    let nosuchtype_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "nosuchtype")
        .expect(
            "Or should include the Volatile row synthesized by the exact-match branch",
        );
    assert!(!nosuchtype_item.id.is_stored());

    Ok(())
}

// Step 7: count(type:*) は type: の列挙アイテム数（type:* の fetch 結果件数）と
// 一致すべき。
#[test]
fn count_type_matches_enumeration_row_count() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    let enum_results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;
    let expected = enum_results.results.len();

    let count_results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "count(type:*)",
        SearchOptions::default(),
    )?;
    let val: f64 = count_results.results[0].raw_repr().parse()?;

    assert_eq!(val as usize, expected);

    Ok(())
}

// Step 7: count(type:"nosuchtype") は未登録値の完全一致検索が合成する Volatile
// 行1件を数えるべき。
#[test]
fn count_type_exact_match_unregistered_value_is_one() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "count(type:\"nosuchtype\")",
        SearchOptions::default(),
    )?;
    let val: f64 = results.results[0].raw_repr().parse()?;

    assert_eq!(val as usize, 1);

    Ok(())
}

// Step 7: count(type:* | extension:rs) は type: の distinct 型数と
// extension:rs にマッチする実アイテム数の和（両者は素な集合）になるべき。
#[test]
fn count_definition_ref_mixed_or_with_projection() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path().canonicalize()?;
    let root = base.join("files");
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("foo.txt"), "content")?;
    std::fs::write(root.join("a.rs"), "fn main() {}")?;
    std::fs::write(root.join("b.rs"), "fn main() {}")?;

    let db_dir = base.join("db");
    let registry = TagRegistry::with_standard();
    let store = Store::open(&db_dir)?;
    Indexer::new(&store, &registry).initialize_tables()?;
    Indexer::new(&store, &registry).run_single(&root, None::<&fn(usize)>, false)?;

    let type_count = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?
    .results
    .len();

    let rs_count = ttfm::search::search_nowarn(
        &store,
        &registry,
        "extension:rs",
        SearchOptions::default(),
    )?
    .results
    .len();

    let count_results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "count(type:* | extension:rs)",
        SearchOptions::default(),
    )?;
    let val: f64 = count_results.results[0].raw_repr().parse()?;

    assert_eq!(val as usize, type_count + rs_count);

    Ok(())
}

// Plan: 定義アイテムの id 区画 — Step 1(a)
// 組み込み型（hash）は type:* に Sys(x) の Stored id で載り、
// 同一クエリを2回実行しても id が変わらない（列挙順固定・安定性）。
#[test]
fn builtin_type_appears_as_stable_sys_stored_id() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    let first = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;
    let hash_first = first
        .results
        .iter()
        .find(|r| r.raw_repr() == "hash")
        .expect("type: query should include built-in type 'hash'");
    assert!(
        hash_first.id.is_stored(),
        "built-in type 'hash' should be Stored with a fixed Sys id, got {:?}",
        hash_first.id
    );
    assert_eq!(
        ttfm::types::Origin::within(hash_first.id.as_i64()),
        ttfm::types::Origin::Builtin,
        "built-in type 'hash' id should live in the Builtin block"
    );

    let second = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;
    let hash_second = second
        .results
        .iter()
        .find(|r| r.raw_repr() == "hash")
        .expect("type: query should include built-in type 'hash'");
    assert_eq!(
        hash_first.id, hash_second.id,
        "built-in type 'hash' id should be stable across repeated queries"
    );

    Ok(())
}

// Plan: 定義アイテムの id 区画 — Step 1(b)
// データ中で使用中の型（例: project）は従来通り Volatile のまま。
#[test]
fn used_type_remains_volatile() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:x"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;

    let project_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "project")
        .expect("type: query should include user-defined tag type 'project'");
    assert!(
        !project_item.id.is_stored(),
        "source-3 type 'project' should remain not-Stored"
    );

    Ok(())
}

// Plan: 定義アイテムの id 区画 — Step 4
// 組み込み型（hash）は固定 Sys id を持ち Stored になるが、まだ実体化（write）
// されていない限りは実 EAV 行を持たない。tags は type:/name: の合成のままで
// あるべきで、実 EAV への LEFT JOIN が空になって tags が空リストに落ちては
// いけない（Stored/Volatile の判別に item_id の非NULL性ではなく
// Volatile CTE 由来フラグを使うことの直接的な検証）。
#[test]
fn builtin_type_stored_via_sys_id_still_gets_synthetic_tags(
) -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;

    let hash_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "hash")
        .expect("type: query should include built-in type 'hash'");
    assert!(hash_item.id.is_stored());
    assert!(
        hash_item
            .get_all_values("type")
            .contains(&"type".to_string()),
        "built-in Stored (Sys id) type-definition item should still carry \
         synthetic type:\"type\", got tags: {:?}",
        hash_item.tags
    );
    assert!(
        hash_item.tags.entries.iter().any(|e| {
            e.typed_tag.tag_type() == TagType::from("name")
                && e.typed_tag.as_str() == "hash"
        }),
        "built-in Stored (Sys id) type-definition item should still carry \
         a synthetic name tag entry, got tags: {:?}",
        hash_item.tags
    );

    Ok(())
}

// Step 5/6: 定義アイテムの合成行は fetch 時に区画（Origin）を判定する。
// 組み込み（Sys id）は最初から Stored なので id 自体から Builtin と分かる。
// データ中で使用中の型・プラグイン型は Settling(Origin, _) になり、
// 内部信号である origin: タグ自体は res.tags には残らない（Rust 側では
// ItemId::Stored/Settling が区画を代用するため、タグとして露出する必要がない）。
#[test]
fn builtin_type_origin_is_derivable_from_stored_id() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;

    let hash_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "hash")
        .expect("type: query should include built-in type 'hash'");
    assert!(hash_item.id.is_stored());
    assert_eq!(
        ttfm::types::Origin::within(hash_item.id.as_i64()),
        ttfm::types::Origin::Builtin
    );
    assert!(
        !hash_item
            .tags
            .entries
            .iter()
            .any(|e| e.typed_tag.tag_type() == TagType::from("origin")),
        "origin should not be exposed as a tag, got tags: {:?}",
        hash_item.tags
    );

    Ok(())
}

#[test]
fn used_type_settles_into_user_block() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:x"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;

    let project_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "project")
        .expect("type: query should include user-defined tag type 'project'");
    assert!(matches!(
        project_item.id,
        ttfm::types::ItemId::Settling(ttfm::types::Origin::User, _)
    ));
    assert!(
        !project_item
            .tags
            .entries
            .iter()
            .any(|e| e.typed_tag.tag_type() == TagType::from("origin")),
        "origin should not be exposed as a tag, got tags: {:?}",
        project_item.tags
    );

    Ok(())
}

// プラグイン登録型（register_plugin、固定 Sys id を持たない）は Plugin 区画へ settle される。
#[test]
fn plugin_type_settles_into_plugin_block() -> anyhow::Result<()> {
    let (store, mut registry, _dir) = setup();

    struct MockPluginType;
    impl TagFunction for MockPluginType {
        fn name(&self) -> &str {
            "qtest"
        }
    }
    registry.register_plugin(MockPluginType);

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;

    let qtest_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "qtest")
        .expect("type: query should include plugin-registered type 'qtest'");
    assert!(matches!(
        qtest_item.id,
        ttfm::types::ItemId::Settling(ttfm::types::Origin::Plugin, _)
    ));
    assert!(
        !qtest_item
            .tags
            .entries
            .iter()
            .any(|e| e.typed_tag.tag_type() == TagType::from("origin")),
        "origin should not be exposed as a tag, got tags: {:?}",
        qtest_item.tags
    );

    Ok(())
}

// Step 6: 組み込み型を編集すると、同じ固定 Sys id のまま Stored 化される
// （id は不変。§4 の指定 id での insert）。
#[test]
fn builtin_type_edit_materializes_with_stable_sys_id() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    let before = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;
    let hash_before = before
        .results
        .iter()
        .find(|r| r.raw_repr() == "hash")
        .expect("type: should include built-in 'hash'");
    assert_eq!(
        hash_before.item_kind,
        ItemKind::Type,
        "built-in 'hash' has a fixed Sys id and is Stored by spec even before materialization"
    );
    let sys_id = hash_before.id;

    edit(
        &store,
        &registry,
        "type:\"hash\"",
        Some("rank:5"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let after = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;
    let hash_after = after
        .results
        .iter()
        .find(|r| r.raw_repr() == "hash")
        .expect(
            "type: should still include built-in 'hash' after materialization",
        );

    assert_eq!(
        hash_after.id, sys_id,
        "materialization must keep the same fixed Sys id"
    );
    assert_eq!(
        hash_after.item_kind,
        ItemKind::Type,
        "should now be a real Stored 'type' item, not Volatile"
    );
    assert_eq!(hash_after.rank, 5);

    Ok(())
}

// DB 未登録の組み込み型（固定 Sys id）は仕様上 Stored であり、item_kind は
// 'type' であるべき（'volatile' ではない）。TypedTag から抽出された使用中の型
// （Settling）は Volatile のまま。
#[test]
fn builtin_type_item_kind_is_type_without_db_row() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    // 使用中の型（Settling）の対照例として project:x を付与
    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:x"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;

    let hash_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "hash")
        .expect("type: query should include built-in type 'hash'");
    assert_eq!(
        hash_item.item_kind,
        ItemKind::Type,
        "built-in type with a fixed Sys id is Stored by spec; item_kind must be 'type'"
    );

    let project_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "project")
        .expect("type: query should include used type 'project'");
    assert_eq!(
        project_item.item_kind,
        ItemKind::Volatile,
        "used type extracted from TypedTags is Volatile (Settling)"
    );

    Ok(())
}

// `type:*` の検索結果は TypeFn の優先 Order（item_id 降順、区画をまたぐ生 id 順）
// で並ぶべき。id 未確定（Settling）の行は item_id を持たないため末尾に載る。
#[test]
fn type_results_are_ordered_by_item_id_desc() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    // id 未確定（Settling）の対照例として project:x を付与
    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:x"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;

    // Stored（固定 Sys id の組み込み）は生 id の降順で並ぶ
    let stored_ids: Vec<i64> = results
        .results
        .iter()
        .filter(|r| r.id.is_stored())
        .map(|r| r.id.as_i64())
        .collect();
    assert!(!stored_ids.is_empty());
    assert!(
        stored_ids.windows(2).all(|w| w[0] > w[1]),
        "stored ids must be in descending raw-id order, got: {:?}",
        stored_ids
    );

    // id 未確定（Settling）の行は Stored の後に載る
    let last_stored = results
        .results
        .iter()
        .rposition(|r| r.id.is_stored())
        .unwrap();
    let first_non_stored = results
        .results
        .iter()
        .position(|r| !r.id.is_stored())
        .expect("used type 'project' should be included as Settling");
    assert!(
        last_stored < first_non_stored,
        "Settling rows (no item_id yet) must come after all Stored rows"
    );

    Ok(())
}

// データ中で使用中の型を編集すると、従来通り User 区画に採番される（回帰確認）。
#[test]
fn used_type_edit_materializes_in_user_block() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:x"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;
    edit(
        &store,
        &registry,
        "type:\"project\"",
        Some("rank:5"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;
    let project_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "project")
        .expect("type: should still include 'project' after materialization");

    assert!(project_item.id.is_stored());
    assert_eq!(
        ttfm::types::Origin::within(project_item.id.as_i64()),
        ttfm::types::Origin::User
    );
    assert_eq!(project_item.rank, 5);

    Ok(())
}

// プラグイン登録型を編集すると、Plugin 区画から動的採番される。
#[test]
fn plugin_type_edit_materializes_in_plugin_block() -> anyhow::Result<()> {
    let (store, mut registry, _dir) = setup();

    struct MockPluginType;
    impl TagFunction for MockPluginType {
        fn name(&self) -> &str {
            "qtest"
        }
    }
    registry.register_plugin(MockPluginType);

    edit(
        &store,
        &registry,
        "type:\"qtest\"",
        Some("rank:5"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "type:*",
        SearchOptions::default(),
    )?;
    let qtest_item = results
        .results
        .iter()
        .find(|r| r.raw_repr() == "qtest")
        .expect("type: should still include 'qtest' after materialization");

    assert!(qtest_item.id.is_stored());
    assert_eq!(
        ttfm::types::Origin::within(qtest_item.id.as_i64()),
        ttfm::types::Origin::Plugin
    );
    assert_eq!(qtest_item.rank, 5);

    Ok(())
}

// Step 7: Nest 文脈内の count(type:*) はグループに依存しない定数（type: は
// group ごとに変わらない）になり、意味論としては正しいが count(type:) との
// 混同が起きやすいため誘導 warning を出すべき。
#[test]
fn count_type_in_nest_context_emits_guidance_warning() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    let mut warnings: Vec<ttfm::query::error::Warning> = Vec::new();
    ttfm::search::search(
        &store,
        &registry,
        "parentdir: &: count(type:*)",
        SearchOptions::default(),
        &mut warnings,
    )?;

    assert!(
        warnings.iter().any(|w| w.0.contains("count(type:)")),
        "expected a guidance warning mentioning count(type:), got {:?}",
        warnings
    );

    Ok(())
}

#[test]
fn test_origin_builtin_and_system_search_does_not_contain_user_or_plugin_types(
) -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    // ユーザー定義タグ (project:x) を付与することで、DB 上に user origin のタグ行を存在させる
    edit(
        &store,
        &registry,
        "filename:foo.txt",
        Some("project:x"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    // 1. origin:builtin の検証
    let results_builtin = ttfm::search::search_nowarn(
        &store,
        &registry,
        "origin:builtin",
        SearchOptions::default(),
    )?;
    for item in &results_builtin.results {
        let labels = item.get_all_labels(&ttfm::types::TagType::Base(
            ttfm::types::SType::Origin,
        ));
        assert!(!labels.is_empty(), "each item must have an origin label");
        let origin_str = labels[0].as_str();
        assert_eq!(
            origin_str, "builtin",
            "expected origin: builtin, but got item with origin: {} (name: {:?})",
            origin_str, item.representative
        );
    }

    // 2. origin:system の検証 (system 由来のみ: builtin, file, plugin)
    let results_system = ttfm::search::search_nowarn(
        &store,
        &registry,
        "origin:system",
        SearchOptions::default(),
    )?;
    for item in &results_system.results {
        let labels = item.get_all_labels(&ttfm::types::TagType::Base(
            ttfm::types::SType::Origin,
        ));
        assert!(!labels.is_empty(), "each item must have an origin label");
        let origin_str = labels[0].as_str();
        assert!(
            origin_str == "builtin" || origin_str == "file" || origin_str == "plugin",
            "expected origin: builtin/file/plugin, but got item with origin: {} (name: {:?})",
            origin_str, item.representative
        );
    }

    Ok(())
}

// 定義アイテム二重表示バグ:
// - 問題1: プラグイン型を実体化(materialize)すると、oneview が
//   item_references の item_kind/content 列を unpivot した合成行が
//   Plugin origin 判定に紛れ込み、builtin の Stored 定義（content/item_kind 等）
//   と重複するスプリアスな Volatile 行が生まれうる。レジストリ登録名は同名の
//   二重登録ができないはずなので、search "origin:*" の結果にレジストリ登録名が
//   複数回出現してはいけない（特定の2名に限らず、登録名全体で確認する）。
// - 問題2: OriginFn::expand は Or([ColumnMatch(origin), DefinitionRef]) を
//   生成するため、定義が materialize されると同じ item が物理行
//   （ColumnMatch枝）と定義行（DefinitionRef枝）の両方にマッチし、
//   UNION で重複排除されず同一 item_id が2行表示されうる。
#[test]
fn origin_search_has_no_duplicate_registered_definition_names(
) -> anyhow::Result<()> {
    let (store, mut registry, _dir) = setup();

    struct MockPluginType;
    impl TagFunction for MockPluginType {
        fn name(&self) -> &str {
            "qtest"
        }
    }
    registry.register_plugin(MockPluginType);

    // プラグイン型 qtest を実体化。
    edit(
        &store,
        &registry,
        "type:\"qtest\"",
        Some("rank:5"),
        QueryType::Tag,
        None,
        WriteOptions::default(),
        &mut Vec::new(),
    )?;

    let results = ttfm::search::search_nowarn(
        &store,
        &registry,
        "origin:*",
        SearchOptions::default(),
    )?;

    for (name, _rank) in registry.iter_all_for_rank() {
        let matches: Vec<_> = results
            .results
            .iter()
            .filter(|r| r.raw_repr() == name)
            .collect();
        assert!(
            matches.len() <= 1,
            "registered name {:?} should not appear more than once \
             (registry names cannot be double-registered), got {:?}",
            name,
            matches
        );
    }

    let mut seen: std::collections::HashSet<ItemId> =
        std::collections::HashSet::new();
    for r in &results.results {
        assert!(
            seen.insert(r.id),
            "item {:?} should appear at most once in origin:* results",
            r.id
        );
    }

    Ok(())
}

// `type:` の glob検索結果に、実在しない型定義アイテムが混入してはいけない。
// （TypeFn::expand が origin 推測用に押し込んでいたダミー candidate が
// 定数行として UNION され、検索結果に漏れていたバグの回帰テスト）
#[test]
fn type_glob_does_not_include_nonexistent_types() -> anyhow::Result<()> {
    let (store, registry, _dir) = setup();

    for query in ["type:*", "type:__*"] {
        let results = ttfm::search::search_nowarn(
            &store,
            &registry,
            query,
            SearchOptions::default(),
        )?;
        assert!(
            results
                .results
                .iter()
                .all(|r| !r.raw_repr().starts_with("__dummy")),
            "{} must not include a dummy type item, got: {:?}",
            query,
            results
                .results
                .iter()
                .map(|r| r.raw_repr())
                .collect::<Vec<_>>()
        );
    }

    Ok(())
}
