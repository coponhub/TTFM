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

use anyhow::Result;
use std::fs::{self, File};
use tempfile::tempdir;
use ttfm::db::Store;
use ttfm::indexing::Indexer;
use ttfm::query::error::Warning;
use ttfm::search::{search_short, SearchOptions};
use ttfm::tag::TagRegistry;

fn setup_test_index() -> Result<(Store, TagRegistry, tempfile::TempDir)> {
    let dir = tempdir()?;
    let root = dir.path();
    let db_dir = root.join(".ttfm/db");
    fs::create_dir_all(root.join("docs"))?;
    File::create(root.join("docs/readme.rs"))?;
    File::create(root.join("main.rs"))?;

    let registry = TagRegistry::with_standard();
    let store = Store::open(&db_dir)?;
    Indexer::new(&store, &registry).initialize_tables()?;
    Indexer::new(&store, &registry).run_single(root, None, false)?;

    Ok((store, registry, dir))
}

#[test]
fn test_search_short_projection_fast_path() -> Result<()> {
    let (store, registry, _dir) = setup_test_index()?;
    let mut sink: Vec<Warning> = Vec::new();
    let options = SearchOptions {
        short: true,
        n: Some(10),
        ..Default::default()
    };
    let resp =
        search_short(&store, &registry, "extension:", options, &mut sink)?;
    assert!(!resp.results.is_empty());
    for item in &resp.results {
        assert!(!item.representative.is_empty());
        assert!(item.tags.entries.is_empty());
    }
    Ok(())
}

#[test]
fn test_search_short_with_nvalue_retains_raw_nvalue() -> Result<()> {
    let (store, registry, _dir) = setup_test_index()?;
    let mut sink: Vec<Warning> = Vec::new();
    let options = SearchOptions {
        short: true,
        n: Some(10),
        ..Default::default()
    };
    let resp = search_short(
        &store,
        &registry,
        "parentdir: &: count(extension:rs)",
        options,
        &mut sink,
    )?;
    assert!(!resp.results.is_empty());
    let docs_dir = _dir.path().join("docs");
    let expected = format!("{}\t1", docs_dir.display());
    assert!(resp
        .results
        .iter()
        .any(|r| r.representative.display_short(&registry) == expected));
    Ok(())
}

#[test]
fn test_search_short_scalar_aggregation_retains_raw_value() -> Result<()> {
    let (store, registry, _dir) = setup_test_index()?;
    let mut sink: Vec<Warning> = Vec::new();
    let options = SearchOptions {
        short: true,
        ..Default::default()
    };
    let resp = search_short(
        &store,
        &registry,
        "count(extension:rs)",
        options,
        &mut sink,
    )?;
    assert_eq!(resp.results.len(), 1);
    assert_eq!(resp.results[0].representative.display_short(&registry), "2");
    Ok(())
}

#[test]
fn test_search_short_name_with_fallback() -> Result<()> {
    let (store, registry, _dir) = setup_test_index()?;
    let mut sink: Vec<Warning> = Vec::new();
    let options = SearchOptions {
        short: true,
        n: Some(10),
        ..Default::default()
    };
    let resp =
        search_short(&store, &registry, "name:", options.clone(), &mut sink)?;
    assert!(!resp.results.is_empty());
    let names: Vec<String> = resp
        .results
        .iter()
        .map(|r| r.representative.display_short(&registry))
        .collect();
    // Initially without user tag, fallback to filenames: "readme.rs" and "main.rs"
    assert!(names.contains(&"readme.rs".to_string()));
    assert!(names.contains(&"main.rs".to_string()));

    // Verify tags partition for name does not exist without user tags (read-side fallback)
    assert!(!store.tags_dir().join("type=name").exists());

    // Add user tag name:custom_doc
    let item_id = resp.results[0].id;
    ttfm::edit::write::write_and_refresh(
        &store,
        &registry,
        vec![ttfm::edit::write::WriteAction::Add {
            item: item_id,
            tags: vec![ttfm::edit::write::TagOp::Append(
                ttfm::types::TypedTag::new("name", "custom_doc"),
            )],
        }],
        None,
    )?;

    // Now type=name partition exists
    assert!(store.tags_dir().join("type=name").exists());

    // Re-query short search with name:
    let resp2 = search_short(&store, &registry, "name:", options, &mut sink)?;
    let names2: Vec<String> = resp2
        .results
        .iter()
        .map(|r| r.representative.display_short(&registry))
        .collect();
    assert!(names2.contains(&"custom_doc".to_string()));
    Ok(())
}

#[test]
fn test_search_short_rank_discrete() -> Result<()> {
    let (store, registry, _dir) = setup_test_index()?;
    let mut sink: Vec<Warning> = Vec::new();
    let options = SearchOptions {
        short: true,
        n: Some(10),
        ..Default::default()
    };
    let resp = search_short(&store, &registry, "rank:", options, &mut sink)?;
    assert!(!resp.results.is_empty());
    let ranks: Vec<String> = resp
        .results
        .iter()
        .map(|r| r.representative.display_short(&registry))
        .collect();
    assert!(ranks.contains(&"0".to_string()));

    // Verify tags partition for rank exists
    assert!(store.tags_dir().join("type=rank").exists());
    Ok(())
}
