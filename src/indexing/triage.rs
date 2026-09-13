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

use crate::db::{identifier, ColumnDef, Store, TargetTable};
use crate::indexing::indexer::{
    DynamicRow, IndexProgress, ScanHash, TagRow, TaggingResult, TempScanEntry,
};
use crate::tag::TagRegistry;
use crate::types::{Bitical, Biticals, FileRef, ItemId, Origin};
use crate::util::DotOk;
use anyhow::Result;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::path::Path;

/// 指定されたエラーが「ファイルが見つからない」ことに起因するか判定します。
fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .map_or(false, |io_e| io_e.kind() == std::io::ErrorKind::NotFound)
}

// ========================================================
// Triage Phase Orchestrator
// ========================================================

pub(crate) fn run_triage(
    store: &Store,
    registry: &TagRegistry,
    to_process: Vec<(Option<ItemId>, TempScanEntry)>,
    dir_changed: Vec<(Option<ItemId>, TempScanEntry)>,
    on_progress: Option<&(dyn Fn(IndexProgress) + Sync + Send)>,
) -> Result<(Vec<TaggingResult>, Vec<TaggingResult>)> {
    let triager = ItemTriager::new(registry);
    let total = to_process.len() + dir_changed.len();
    let counter = std::sync::atomic::AtomicUsize::new(0);

    if let Some(on_p) = on_progress {
        on_p(IndexProgress::Extracting { current: 0, total });
    }

    // 1. 通常処理エントリからメタデータを抽出
    let raw_values =
        triager.extract_all(to_process, &counter, total, on_progress)?;

    // 2. 新規（既存 ID 無し）の分だけ、file_ref（inode）単位で重複排除してから
    //    db に一括採番を依頼する。同じ file_ref（ハードリンク）には同じ id を渡す。
    let mut new_refs: Vec<FileRef> = raw_values
        .iter()
        .filter(|(id, ..)| id.is_none())
        .map(|(.., file_ref)| *file_ref)
        .collect();
    new_refs.sort();
    new_refs.dedup();
    let new_ids = identifier::next(store, Origin::File, new_refs.len())?;
    let by_file_ref: FxHashMap<FileRef, i64> =
        new_refs.into_iter().zip(new_ids).collect();

    // 3. ID の割当
    let results = triager.assemble_records(raw_values, &by_file_ref)?;

    // 4. 移動のみ (DirChanged) のエントリからパス依存メタデータのみを抽出 (base_tags をスキップ)
    let raw_dir_changed = triager.extract_all_dir_changed(
        dir_changed,
        &counter,
        total,
        on_progress,
    )?;
    let dir_changed_results =
        triager.assemble_records(raw_dir_changed, &FxHashMap::default())?;

    (results, dir_changed_results).to_ok()
}

// ========================================================
// 1. Item Triager
// ========================================================

pub(crate) struct ItemTriager<'a> {
    pub(crate) registry: &'a TagRegistry,
}

impl<'a> ItemTriager<'a> {
    pub(crate) fn new(reg: &'a TagRegistry) -> Self {
        Self { registry: reg }
    }

    fn extract_entries_with_progress<F>(
        &self,
        entries: Vec<(Option<ItemId>, TempScanEntry)>,
        counter: &std::sync::atomic::AtomicUsize,
        total: usize,
        on_progress: Option<&(dyn Fn(IndexProgress) + Sync + Send)>,
        extract_fn: F,
    ) -> Result<Vec<(Option<ItemId>, Biticals, Hashes, FileRef)>>
    where
        F: Fn(
                Option<ItemId>,
                TempScanEntry,
            )
                -> Result<Option<(Option<ItemId>, Biticals, Hashes, FileRef)>>
            + Sync
            + Send,
    {
        entries
            .into_par_iter()
            .map(|(id, e)| {
                let res = extract_fn(id, e);
                if let Some(on_p) = on_progress {
                    let current = counter
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    if current % 50 == 0 || current == total {
                        on_p(IndexProgress::Extracting { current, total });
                    }
                }
                res
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .to_ok()
    }

    pub(crate) fn extract_all(
        &self,
        entries: Vec<(Option<ItemId>, TempScanEntry)>,
        counter: &std::sync::atomic::AtomicUsize,
        total: usize,
        on_progress: Option<&(dyn Fn(IndexProgress) + Sync + Send)>,
    ) -> Result<Vec<(Option<ItemId>, Biticals, Hashes, FileRef)>> {
        self.extract_entries_with_progress(
            entries,
            counter,
            total,
            on_progress,
            |id, e| self.extract_with_hash(id, e),
        )
    }

    /// ファイルからタグを抽出し、元のハッシュ値・inode（file_ref）・ID をセットにして返します。
    fn extract_with_hash(
        &self,
        existing_id: Option<ItemId>,
        entry: TempScanEntry,
    ) -> Result<Option<(Option<ItemId>, Biticals, Hashes, FileRef)>> {
        let path = &entry.entry.path.value;
        let hashes = Hashes(entry.hash, entry.basename_hash);
        let file_ref = entry.entry.inode.value;

        match self.extract_single_file(path)? {
            Some(values) => Ok(Some((existing_id, values, hashes, file_ref))),
            None => Ok(None),
        }
    }

    /// 1つのファイルに対してタグ抽出を試みます。
    fn extract_single_file(&self, path_str: &str) -> Result<Option<Biticals>> {
        let res = self.registry.process_file(Path::new(path_str));

        if let Ok(values) = res {
            return Ok(Some(values));
        }

        let err = res.unwrap_err();
        if is_not_found(&err) {
            Ok(None)
        } else {
            Err(err)
        }
    }

    pub(crate) fn extract_all_dir_changed(
        &self,
        entries: Vec<(Option<ItemId>, TempScanEntry)>,
        counter: &std::sync::atomic::AtomicUsize,
        total: usize,
        on_progress: Option<&(dyn Fn(IndexProgress) + Sync + Send)>,
    ) -> Result<Vec<(Option<ItemId>, Biticals, Hashes, FileRef)>> {
        self.extract_entries_with_progress(
            entries,
            counter,
            total,
            on_progress,
            |id, e| self.extract_with_hash_dir_changed(id, e),
        )
    }

    fn extract_with_hash_dir_changed(
        &self,
        existing_id: Option<ItemId>,
        entry: TempScanEntry,
    ) -> Result<Option<(Option<ItemId>, Biticals, Hashes, FileRef)>> {
        let path = &entry.entry.path.value;
        let hashes = Hashes(entry.hash, entry.basename_hash);
        let file_ref = entry.entry.inode.value;

        match self.extract_single_file_location_only(path)? {
            Some(values) => Ok(Some((existing_id, values, hashes, file_ref))),
            None => Ok(None),
        }
    }

    fn extract_single_file_location_only(
        &self,
        path_str: &str,
    ) -> Result<Option<Biticals>> {
        let res = self
            .registry
            .process_file_location_only(Path::new(path_str));

        if let Ok(values) = res {
            return Ok(Some(values));
        }

        let err = res.unwrap_err();
        if is_not_found(&err) {
            Ok(None)
        } else {
            Err(err)
        }
    }

    pub(crate) fn assemble_records(
        &self,
        all_values: Vec<(Option<ItemId>, Biticals, Hashes, FileRef)>,
        by_file_ref: &FxHashMap<FileRef, i64>,
    ) -> Result<Vec<TaggingResult>> {
        let columns = self.registry.get_all_columns();

        all_values
            .into_iter()
            .map(|(existing_id, values, hashes, file_ref)| {
                let id = match existing_id {
                    Some(id) => id,
                    None => ItemId::from(by_file_ref[&file_ref]),
                };
                self.triage_item(id, values, hashes, &columns)
            })
            .collect::<Vec<_>>()
            .to_ok()
    }

    fn triage_item(
        &self,
        id: ItemId,
        values: Biticals,
        hashes: Hashes,
        cols: &[ColumnDef],
    ) -> TaggingResult {
        let id_i64 = id.as_i64();
        let mut acc = TriageAccumulator::new(id_i64);
        for (v, c) in values.into_iter().zip(cols) {
            acc.feed(v, c);
        }
        let mut res = acc.finish();

        res.scan_hash = hashes.0;
        res.basename_scan_hash = hashes.1;
        res
    }
}

// ========================================================
// 2. Triage Accumulator (Internal helper)
// ========================================================

pub(crate) struct Hashes(pub(crate) ScanHash, pub(crate) ScanHash);

pub(crate) struct TriageAccumulator {
    id: i64,
    entities: Biticals,
    locations: Biticals,
    tags: Vec<TagRow>,
    location_tags: Vec<TagRow>,
}

impl TriageAccumulator {
    pub(crate) fn new(id: i64) -> Self {
        Self {
            id,
            entities: vec![Some(Bitical::Integer(0))],
            locations: Vec::new(),
            tags: Vec::new(),
            location_tags: Vec::new(),
        }
    }

    pub(crate) fn feed(&mut self, val: Option<Bitical>, col: &ColumnDef) {
        match col.target_table {
            TargetTable::FileReferences => {
                self.entities.push(val.clone());
                if let Some(value) = val {
                    self.tags.push(TagRow {
                        item_id: self.id,
                        tag_type: col.name.clone(),
                        value,
                    });
                }
            }
            TargetTable::Locations => {
                self.locations.push(val.clone());
                if let Some(value) = val {
                    self.location_tags.push(TagRow {
                        item_id: self.id,
                        tag_type: col.name.clone(),
                        value,
                    });
                }
            }
            TargetTable::BaseTags => {
                if let Some(value) = val {
                    self.tags.push(TagRow {
                        item_id: self.id,
                        tag_type: col.name.clone(),
                        value,
                    });
                }
            }
            TargetTable::TagsByLocation => {
                if let Some(value) = val {
                    self.location_tags.push(TagRow {
                        item_id: self.id,
                        tag_type: col.name.clone(),
                        value,
                    });
                }
            }
            _ => {}
        }
    }

    pub(crate) fn finish(self) -> TaggingResult {
        TaggingResult {
            entity_row: DynamicRow {
                id: self.id,
                values: self.entities,
            },
            location_row: DynamicRow {
                id: self.id,
                values: self.locations,
            },
            tags: self.tags,
            location_tags: self.location_tags,
            scan_hash: ScanHash(0),
            basename_scan_hash: ScanHash(0),
        }
    }
}

// ========================================================
// Tests
// ========================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::BiticalType;
    use crate::indexing::indexer::{calc_basename_scan_hash, calc_scanhash};
    use crate::indexing::ScanEntry;
    use crate::util::SafeMetadata;

    #[test]
    fn test_triage_accumulator_logic() {
        let mut acc = TriageAccumulator::new(123);
        let col_file_ref = ColumnDef {
            name: "size".into(),
            bitical_type: BiticalType::Integer,
            target_table: TargetTable::FileReferences,
        };
        let col_loc = ColumnDef {
            name: "path".into(),
            bitical_type: BiticalType::String,
            target_table: TargetTable::Locations,
        };
        let col_tag = ColumnDef {
            name: "ext".into(),
            bitical_type: BiticalType::String,
            target_table: TargetTable::BaseTags,
        };
        let col_loc_tag = ColumnDef {
            name: "loc_ext".into(),
            bitical_type: BiticalType::String,
            target_table: TargetTable::TagsByLocation,
        };

        acc.feed(Some(Bitical::Integer(100)), &col_file_ref);
        acc.feed(Some(Bitical::String("/path".into())), &col_loc);
        acc.feed(Some(Bitical::String("rs".into())), &col_tag);
        acc.feed(Some(Bitical::String("rs_loc".into())), &col_loc_tag);

        let res = acc.finish();
        assert_eq!(res.entity_row.id, 123);
        assert_eq!(res.entity_row.values[1], Some(Bitical::Integer(100)));
        assert_eq!(
            res.location_row.values[0],
            Some(Bitical::String("/path".into()))
        );
        assert_eq!(res.tags.len(), 2);
        assert_eq!(res.tags[0].tag_type, "size");
        assert_eq!(res.tags[1].tag_type, "ext");
        assert_eq!(res.location_tags.len(), 2);
        assert_eq!(res.location_tags[0].tag_type, "path");
        assert_eq!(res.location_tags[1].tag_type, "loc_ext");
    }

    #[test]
    fn test_triager_triage_item_full() {
        let registry = TagRegistry::new();
        let triager = ItemTriager::new(&registry);

        let cols = vec![
            ColumnDef {
                name: "size".into(),
                bitical_type: BiticalType::Integer,
                target_table: TargetTable::FileReferences,
            },
            ColumnDef {
                name: "path".into(),
                bitical_type: BiticalType::String,
                target_table: TargetTable::Locations,
            },
            ColumnDef {
                name: "ext".into(),
                bitical_type: BiticalType::String,
                target_table: TargetTable::BaseTags,
            },
        ];
        let vals: Biticals = vec![
            Some(Bitical::Integer(500)),
            Some(Bitical::String("/foo.rs".into())),
            Some(Bitical::String("rs".into())),
        ];

        let res = triager.triage_item(
            ItemId::from(7),
            vals,
            Hashes(ScanHash(123), ScanHash(456)),
            &cols,
        );

        assert_eq!(res.entity_row.id, 7);
        assert_eq!(res.scan_hash, ScanHash(123));
        assert_eq!(res.basename_scan_hash, ScanHash(456));
        assert_eq!(res.entity_row.values[1], Some(Bitical::Integer(500)));
        assert_eq!(
            res.location_row.values[0],
            Some(Bitical::String("/foo.rs".into()))
        );
    }

    #[test]
    fn test_extract_all_with_race_condition() {
        use std::fs::File;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let registry = TagRegistry::with_standard();
        let triager = ItemTriager::new(&registry);

        let paths = vec![
            dir.path().join("file1.txt"),
            dir.path().join("file2.txt"),
            dir.path().join("file3.txt"),
        ];
        for p in &paths {
            File::create(p).unwrap();
        }

        let entries: Vec<(Option<ItemId>, TempScanEntry)> = paths
            .iter()
            .map(|p| {
                let m = std::fs::metadata(p).unwrap();
                let entry =
                    ScanEntry::from_path_metadata(p, &SafeMetadata::new(&m))
                        .unwrap();
                let hash = calc_scanhash(
                    &entry.path.value,
                    entry.mtime.value.0,
                    entry.size.value.0,
                );
                let basename_hash = calc_basename_scan_hash(
                    &entry.path.value,
                    entry.mtime.value.0,
                    entry.size.value.0,
                    entry.inode.value,
                );
                (
                    None,
                    TempScanEntry {
                        entry,
                        hash,
                        basename_hash,
                    },
                )
            })
            .collect();

        std::fs::remove_file(&paths[1]).unwrap();

        let res = triager
            .extract_all(
                entries,
                &std::sync::atomic::AtomicUsize::new(0),
                3,
                None,
            )
            .expect("Should handle missing file");

        assert_eq!(res.len(), 2);
    }

    #[test]
    fn test_assemble_records_id_reuse() {
        let registry = TagRegistry::new();
        let triager = ItemTriager::new(&registry);
        let new_file_ref = FileRef::from_u64_pair(0, 2);
        let input = vec![
            (
                Some(ItemId::from(100)),
                vec![],
                Hashes(ScanHash(1), ScanHash(11)),
                FileRef::from_u64_pair(0, 1),
            ),
            (
                None,
                vec![],
                Hashes(ScanHash(2), ScanHash(22)),
                new_file_ref,
            ),
        ];
        let by_file_ref: FxHashMap<FileRef, i64> =
            [(new_file_ref, 501)].into_iter().collect();

        // 採番済み id（db 役割）を渡すと、新規エントリへ file_ref 経由で配られる。
        let results = triager.assemble_records(input, &by_file_ref).unwrap();

        assert_eq!(results[0].entity_row.id, 100, "Should reuse existing ID");
        assert_eq!(results[1].entity_row.id, 501, "Should use allocated ID");
    }

    #[test]
    fn test_triager_tags_by_location() {
        let registry = TagRegistry::new();
        let triager = ItemTriager::new(&registry);

        let cols = vec![ColumnDef {
            name: "loc_tag".into(),
            bitical_type: BiticalType::String,
            target_table: TargetTable::TagsByLocation,
        }];
        let vals: Biticals = vec![Some(Bitical::String("val".into()))];

        let res = triager.triage_item(
            ItemId::from(9),
            vals,
            Hashes(ScanHash(1), ScanHash(2)),
            &cols,
        );

        assert_eq!(res.location_tags.len(), 1);
        assert_eq!(res.location_tags[0].tag_type, "loc_tag");
    }

    #[test]
    fn test_triage_reports_extracting_progress() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("file.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let registry = TagRegistry::with_standard();
        let triager = ItemTriager::new(&registry);

        let m = std::fs::metadata(&file_path).unwrap();
        let entry =
            ScanEntry::from_path_metadata(&file_path, &SafeMetadata::new(&m))
                .unwrap();
        let hash = calc_scanhash(
            &entry.path.value,
            entry.mtime.value.0,
            entry.size.value.0,
        );
        let basename_hash = calc_basename_scan_hash(
            &entry.path.value,
            entry.mtime.value.0,
            entry.size.value.0,
            entry.inode.value,
        );
        let entries = vec![(
            None,
            TempScanEntry {
                entry,
                hash,
                basename_hash,
            },
        )];

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = std::sync::Arc::clone(&events);
        let cb = move |p| events_clone.lock().unwrap().push(p);

        let counter = std::sync::atomic::AtomicUsize::new(0);
        let res = triager
            .extract_all(entries, &counter, 1, Some(&cb))
            .unwrap();
        assert_eq!(res.len(), 1);

        let captured = events.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        assert_eq!(
            captured[0],
            IndexProgress::Extracting {
                current: 1,
                total: 1
            }
        );
    }

    #[test]
    fn test_triage_pre_flattens_locations_and_file_refs() {
        use std::fs::File;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test_file.rs");
        File::create(&file_path).unwrap();

        let registry = TagRegistry::with_standard();
        let triager = ItemTriager::new(&registry);
        let cols = registry.get_all_columns();

        let values = registry.process_file(&file_path).unwrap();
        let hashes = Hashes(ScanHash(10), ScanHash(20));

        let res = triager.triage_item(ItemId::from(1), values, hashes, &cols);

        assert!(res.location_tags.iter().any(|t| t.tag_type == "extension"));
        assert!(res.location_tags.iter().any(|t| t.tag_type == "parentdir"));
        assert!(res.location_tags.iter().any(|t| t.tag_type == "filename"));
        assert!(res.location_tags.iter().any(|t| t.tag_type == "path"));
        assert!(res.location_tags.iter().any(|t| t.tag_type == "stem"));
        assert!(res.tags.iter().any(|t| t.tag_type == "size"));
        assert!(res.tags.iter().any(|t| t.tag_type == "mtime"));
        assert!(res.tags.iter().any(|t| t.tag_type == "is_dir"));
        assert!(res.tags.iter().any(|t| t.tag_type == "file_id"));
    }
}
