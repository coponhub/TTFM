// Copyright (C) 2026 Kensuke Aoyagi
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
    DynamicRow, ScanHash, TagRow, TaggingResult, TempScanEntry,
};
use crate::tag::TagRegistry;
use crate::types::{Bitical, Biticals, ItemId, Origin};
use crate::util::DotOk;
use anyhow::Result;
use rayon::prelude::*;
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
) -> Result<(Vec<TaggingResult>, Vec<DynamicRow>)> {
    let triager = ItemTriager::new(registry);

    // 1. 各エントリからメタデータを抽出（ハッシュとIDも引き継ぐ）
    let raw_values = triager.extract_all(to_process)?;

    // 2. 新規（既存 ID 無し）の分だけ db に一括採番を依頼する。
    //    採番（連番生成）は db（identifier::next）の責務。
    let new_count = raw_values.iter().filter(|(id, _, _)| id.is_none()).count();
    let new_ids = identifier::next(store, Origin::File, new_count)?;

    // 3. ID の割当（既存 ID があれば流用、なければ採番済み id を配る）
    let results = triager.assemble_records(raw_values, new_ids)?;

    // 移動用の再構築ロジックは merge.rs で吸収されるため、ここでは常に空
    (results, Vec::new()).to_ok()
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

    pub(crate) fn extract_all(
        &self,
        entries: Vec<(Option<ItemId>, TempScanEntry)>,
    ) -> Result<Vec<(Option<ItemId>, Biticals, ScanHash)>> {
        entries
            .into_par_iter()
            .map(|(id, e)| self.extract_with_hash(id, e))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .to_ok()
    }

    /// ファイルからタグを抽出し、元のハッシュ値と ID をセットにして返します。
    fn extract_with_hash(
        &self,
        existing_id: Option<ItemId>,
        entry: TempScanEntry,
    ) -> Result<Option<(Option<ItemId>, Biticals, ScanHash)>> {
        let path = &entry.entry.path.value;
        let hash = entry.hash;

        match self.extract_single_file(path)? {
            Some(values) => Ok(Some((existing_id, values, hash))),
            None => Ok(None),
        }
    }

    /// 1つのファイルに対してタグ抽出を試みます。
    fn extract_single_file(
        &self,
        path_str: &str,
    ) -> Result<Option<Biticals>> {
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

    pub(crate) fn assemble_records(
        &self,
        all_values: Vec<(Option<ItemId>, Biticals, ScanHash)>,
        new_ids: Vec<i64>,
    ) -> Result<Vec<TaggingResult>> {
        let columns = self.registry.get_all_columns();
        let mut new_ids = new_ids.into_iter();

        all_values
            .into_iter()
            .map(|(existing_id, values, hash)| {
                let id = match existing_id {
                    Some(id) => id,
                    None => ItemId::from(
                        new_ids
                            .next()
                            .expect("attach must supply one id per new entry"),
                    ),
                };
                self.triage_item(id, values, hash, &columns)
            })
            .collect::<Vec<_>>()
            .to_ok()
    }

    fn triage_item(
        &self,
        id: ItemId,
        values: Biticals,
        hash: ScanHash,
        cols: &[ColumnDef],
    ) -> TaggingResult {
        let id_i64 = id.as_i64();
        let mut res = values
            .into_iter()
            .zip(cols)
            .map(|(v, c)| self.classify(id_i64, v, c))
            .fold(TriageAccumulator::new(id_i64), |acc, p| acc.collect(p))
            .finish();

        res.scan_hash = hash;
        res
    }

    fn classify(
        &self,
        id: i64,
        val: Option<Bitical>,
        col: &ColumnDef,
    ) -> TriagePiece {
        match col.target_table {
            TargetTable::FileReferences => TriagePiece::Entity(val),
            TargetTable::Locations => TriagePiece::Location(val),
            TargetTable::BaseTags => self.triage_base_tag(id, val, &col.name),
            _ => TriagePiece::None,
        }
    }

    fn triage_base_tag(
        &self,
        id: i64,
        val: Option<Bitical>,
        name: &str,
    ) -> TriagePiece {
        let (l_str, l_int, l_dbl, l_bool) = Bitical::to_eav_columns(val);

        // 何らかの値があれば TagPiece を生成 (Null以外なら何かあるはず)
        if l_str.is_none()
            && l_int.is_none()
            && l_dbl.is_none()
            && l_bool.is_none()
        {
            return TriagePiece::None;
        }

        TriagePiece::Tag(TagRow {
            item_id: id,
            tag_type: name.to_string(),
            label_str: l_str,
            label_int: l_int,
            label_double: l_dbl,
            label_bool: l_bool,
        })
    }
}

// ========================================================
// 2. Triage Accumulator (Internal helper)
// ========================================================

pub(crate) enum TriagePiece {
    Entity(Option<Bitical>),
    Location(Option<Bitical>),
    Tag(TagRow),
    None,
}

pub(crate) struct TriageAccumulator {
    id: i64,
    entities: Biticals,
    locations: Biticals,
    tags: Vec<TagRow>,
}

impl TriageAccumulator {
    pub(crate) fn new(id: i64) -> Self {
        Self {
            id,
            entities: vec![Some(Bitical::Integer(0))],
            locations: Vec::new(),
            tags: Vec::new(),
        }
    }

    pub(crate) fn collect(mut self, piece: TriagePiece) -> Self {
        match piece {
            TriagePiece::Entity(v) => self.entities.push(v),
            TriagePiece::Location(v) => self.locations.push(v),
            TriagePiece::Tag(t) => self.tags.push(t),
            TriagePiece::None => {}
        }
        self
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
            scan_hash: ScanHash(0),
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
    use crate::indexing::indexer::calc_scanhash;
    use crate::indexing::ScanEntry;
    use crate::util::SafeMetadata;

    #[test]
    fn test_triage_accumulator_logic() {
        let mut acc = TriageAccumulator::new(123);
        acc = acc.collect(TriagePiece::Entity(Some(Bitical::Integer(100))));
        acc = acc.collect(TriagePiece::Location(Some(Bitical::String(
            "/path".into(),
        ))));
        acc = acc.collect(TriagePiece::Tag(TagRow {
            item_id: 123,
            tag_type: "ext".into(),
            label_str: Some("rs".into()),
            label_int: None,
            label_double: None,
            label_bool: None,
        }));
        let res = acc.finish();
        assert_eq!(res.entity_row.id, 123);
        assert_eq!(res.entity_row.values[1], Some(Bitical::Integer(100)));
        assert_eq!(
            res.location_row.values[0],
            Some(Bitical::String("/path".into()))
        );
        assert_eq!(res.tags[0].tag_type, "ext");
    }

    #[test]
    fn test_triager_classify_logic() {
        let registry = TagRegistry::new();
        let triager = ItemTriager::new(&registry);

        let col_ent = ColumnDef {
            name: "size".to_string(),
            bitical_type: BiticalType::Integer,
            target_table: TargetTable::FileReferences,
        };
        let p_ent =
            triager.classify(1, Some(Bitical::Integer(1024)), &col_ent);
        assert!(matches!(
            p_ent,
            TriagePiece::Entity(Some(Bitical::Integer(1024)))
        ));
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

        let res =
            triager.triage_item(ItemId::from(7), vals, ScanHash(123), &cols);

        assert_eq!(res.entity_row.id, 7);
        assert_eq!(res.scan_hash, ScanHash(123));
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
                (None, TempScanEntry { entry, hash })
            })
            .collect();

        std::fs::remove_file(&paths[1]).unwrap();

        let res = triager
            .extract_all(entries)
            .expect("Should handle missing file");

        assert_eq!(res.len(), 2);
    }

    #[test]
    fn test_assemble_records_id_reuse() {
        let registry = TagRegistry::new();
        let triager = ItemTriager::new(&registry);
        let input = vec![
            (Some(ItemId::from(100)), vec![], ScanHash(1)),
            (None, vec![], ScanHash(2)),
        ];

        // 採番済み id（db 役割）を渡すと、新規エントリへ順に配られる。
        let results = triager.assemble_records(input, vec![501]).unwrap();

        assert_eq!(results[0].entity_row.id, 100, "Should reuse existing ID");
        assert_eq!(results[1].entity_row.id, 501, "Should use allocated ID");
    }
}
