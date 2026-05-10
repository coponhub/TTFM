use crate::db::TargetTable;
use crate::indexing::indexer::{
    DynamicRow, ScanHash, TagRow, TaggingResult, TempScanEntry,
};
use crate::taggers::{ColumnDef, TagValue};
use crate::types::ItemId;
use crate::util::DotOk;
use crate::tag::TagRegistry;
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
    registry: &TagRegistry,
    to_process: Vec<(Option<ItemId>, TempScanEntry)>,
    max_id_fn: impl Fn() -> Result<i64>, // 正の最大IDを取得する関数
) -> Result<(Vec<TaggingResult>, Vec<DynamicRow>)> {
    let triager = ItemTriager::new(registry);

    // 1. 各エントリからメタデータを抽出（ハッシュとIDも引き継ぐ）
    let raw_values = triager.extract_all(to_process)?;
    let max_id = max_id_fn()?;

    // 2. ID の割当（既存 ID があれば流用、なければ新規採番）
    let results = triager.assemble_records(raw_values, max_id)?;

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
    ) -> Result<Vec<(Option<ItemId>, Vec<TagValue>, ScanHash)>> {
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
    ) -> Result<Option<(Option<ItemId>, Vec<TagValue>, ScanHash)>> {
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
    ) -> Result<Option<Vec<TagValue>>> {
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
        all_values: Vec<(Option<ItemId>, Vec<TagValue>, ScanHash)>,
        max_id: i64,
    ) -> Result<Vec<TaggingResult>> {
        let columns = self.registry.get_all_columns();
        let mut current_max_id = max_id;

        all_values
            .into_iter()
            .map(|(existing_id, values, hash)| {
                let id = existing_id.unwrap_or_else(|| {
                    current_max_id += 1;
                    ItemId::from(current_max_id)
                });
                self.triage_item(id, values, hash, &columns)
            })
            .collect::<Vec<_>>()
            .to_ok()
    }

    fn triage_item(
        &self,
        id: ItemId,
        values: Vec<TagValue>,
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

    fn classify(&self, id: i64, val: TagValue, col: &ColumnDef) -> TriagePiece {
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
        val: TagValue,
        name: &str,
    ) -> TriagePiece {
        let (l_str, l_int, l_dbl, l_bool) = match val {
            TagValue::Text(s) => (Some(s), None, None, None),
            TagValue::BigInt(i) => (None, Some(i), None, None),
            TagValue::Double(d) => (None, None, Some(d), None),
            TagValue::Boolean(b) => (None, None, None, Some(b)),
            TagValue::Uuid(u) => (Some(u.to_string()), None, None, None),
            TagValue::Null => (None, None, None, None),
            _ => (None, None, None, None),
        };

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
    Entity(TagValue),
    Location(TagValue),
    Tag(TagRow),
    None,
}

pub(crate) struct TriageAccumulator {
    id: i64,
    entities: Vec<TagValue>,
    locations: Vec<TagValue>,
    tags: Vec<TagRow>,
}

impl TriageAccumulator {
    pub(crate) fn new(id: i64) -> Self {
        Self {
            id,
            entities: vec![TagValue::BigInt(0)],
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
    use crate::db::SqlType;
    use crate::indexing::ScanEntry;
    use crate::indexing::indexer::calc_scanhash;
    use crate::util::SafeMetadata;

    #[test]
    fn test_triage_accumulator_logic() {
        let mut acc = TriageAccumulator::new(123);
        acc = acc.collect(TriagePiece::Entity(TagValue::BigInt(100)));
        acc =
            acc.collect(TriagePiece::Location(TagValue::Text("/path".into())));
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
        assert_eq!(res.entity_row.values[1], TagValue::BigInt(100));
        assert_eq!(res.location_row.values[0], TagValue::Text("/path".into()));
        assert_eq!(res.tags[0].tag_type, "ext");
    }

    #[test]
    fn test_triager_classify_logic() {
        let registry = TagRegistry::new();
        let triager = ItemTriager::new(&registry);

        let col_ent = ColumnDef {
            name: "size".to_string(),
            sql_type: SqlType::BIGINT,
            target_table: TargetTable::FileReferences,
        };
        let p_ent = triager.classify(1, TagValue::BigInt(1024), &col_ent);
        assert!(matches!(p_ent, TriagePiece::Entity(TagValue::BigInt(1024))));
    }

    #[test]
    fn test_triager_triage_item_full() {
        let registry = TagRegistry::new();
        let triager = ItemTriager::new(&registry);

        let cols = vec![
            ColumnDef {
                name: "size".into(),
                sql_type: SqlType::BIGINT,
                target_table: TargetTable::FileReferences,
            },
            ColumnDef {
                name: "path".into(),
                sql_type: SqlType::VARCHAR,
                target_table: TargetTable::Locations,
            },
            ColumnDef {
                name: "ext".into(),
                sql_type: SqlType::VARCHAR,
                target_table: TargetTable::BaseTags,
            },
        ];
        let vals = vec![
            TagValue::BigInt(500),
            TagValue::Text("/foo.rs".into()),
            TagValue::Text("rs".into()),
        ];

        let res =
            triager.triage_item(ItemId::from(7), vals, ScanHash(123), &cols);

        assert_eq!(res.entity_row.id, 7);
        assert_eq!(res.scan_hash, ScanHash(123));
        assert_eq!(res.entity_row.values[1], TagValue::BigInt(500));
        assert_eq!(
            res.location_row.values[0],
            TagValue::Text("/foo.rs".into())
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

        let results = triager.assemble_records(input, 500).unwrap();

        assert_eq!(results[0].entity_row.id, 100, "Should reuse existing ID");
        assert_eq!(results[1].entity_row.id, 501, "Should issue next new ID");
    }
}
