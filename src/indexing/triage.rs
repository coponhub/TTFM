use crate::taggers::{TagValue, ColumnDef};
use crate::db::{TargetTable};
use crate::{FunctionRegistry, TagFunction};
use crate::functions::{ScanEntry};
use crate::indexing::indexer::{TaggingResult, DynamicRow, TagRow};
use anyhow::Result;
use std::path::{Path};
use rayon::prelude::*;

/// 指定されたエラーが「ファイルが見つからない」ことに起因するか判定します。
fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .map_or(false, |io_e| io_e.kind() == std::io::ErrorKind::NotFound)
}

// ========================================================
// Triage Phase Orchestrator
// ========================================================

pub(crate) fn run_triage(
    registry: &FunctionRegistry,
    to_tag: Vec<ScanEntry>,
    moved: Vec<(i64, String)>,
    max_id_fn: impl Fn() -> Result<i64>, // 正の最大IDを取得する関数
) -> Result<(Vec<TaggingResult>, Vec<DynamicRow>)> {
    let triager = ItemTriager::new(registry);

    let raw_values = triager.extract_all(to_tag)?;
    let max_id = max_id_fn()?;
    let results = triager.assemble_records(raw_values, max_id)?;
    let moved_rows = triager.rebuild_moved_locations(moved)?;

    Ok((results, moved_rows))
}

// ========================================================
// 1. Item Triager
// ========================================================

pub(crate) struct ItemTriager<'a> {
    pub(crate) registry: &'a FunctionRegistry,
}

impl<'a> ItemTriager<'a> {
    pub(crate) fn new(
        reg: &'a FunctionRegistry,
    ) -> Self {
        Self {
            registry: reg,
        }
    }

    pub(crate) fn extract_all(&self, entries: Vec<ScanEntry>) -> Result<Vec<Vec<TagValue>>> {
        let results: Result<Vec<Option<Vec<TagValue>>>> = entries
            .into_par_iter()
            .map(|e| self.extract_single_file(&e.path.value))
            .collect();

        Ok(results?.into_iter().flatten().collect())
    }

    /// 1つのファイルに対してタグ抽出を試みます。
    /// ファイル消失 (NotFound) の場合は Ok(None) を返し、スキップを通知します。
    fn extract_single_file(&self, path_str: &str) -> Result<Option<Vec<TagValue>>> {
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
        all_values: Vec<Vec<TagValue>>,
        max_id: i64,
    ) -> Result<Vec<TaggingResult>> {
        let columns = self.registry.get_all_columns();

        all_values
            .into_iter()
            .enumerate()
            .map(|(i, values)| {
                // 正の整数空間での採番: max_id + index + 1
                let id = max_id + (i as i64) + 1;
                Ok(self.triage_item(id, values, &columns))
            })
            .collect()
    }

    fn triage_item(
        &self,
        id: i64,
        values: Vec<TagValue>,
        cols: &[ColumnDef],
    ) -> TaggingResult {
        values
            .into_iter()
            .zip(cols)
            .map(|(v, c)| self.classify(id, v, c))
            .fold(TriageAccumulator::new(id), |acc, p| acc.collect(p))
            .finish()
    }

    fn classify(&self, id: i64, val: TagValue, col: &ColumnDef) -> TriagePiece {
        match col.target_table {
            TargetTable::FileEntities => TriagePiece::Entity(val),
            TargetTable::Locations => TriagePiece::Location(val),
            TargetTable::BaseTags => self.triage_base_tag(id, val, &col.name),
            _ => TriagePiece::None,
        }
    }

    fn triage_base_tag(&self, id: i64, val: TagValue, name: &str) -> TriagePiece {
        val.into_string()
            .filter(|s| !s.is_empty())
            .map(|label| {
                TriagePiece::Tag(TagRow {
                    item_id: id,
                    tag_type: name.to_string(),
                    label,
                })
            })
            .unwrap_or(TriagePiece::None)
    }

    pub(crate) fn rebuild_moved_locations(
        &self,
        moved: Vec<(i64, String)>,
    ) -> Result<Vec<DynamicRow>> {
        let functions = self.registry.all_functions();
        moved
            .into_iter()
            .map(|(id, path)| {
                let values =
                    self.rebuild_values_from_path(Path::new(&path), functions);
                Ok(DynamicRow { id, values })
            })
            .collect()
    }

    fn rebuild_values_from_path(
        &self,
        path: &Path,
        functions: &[Box<dyn TagFunction>],
    ) -> Vec<TagValue> {
        functions
            .iter()
            .flat_map(|f| self.rebuild_values_for_function(path, f.as_ref()))
            .collect()
    }

    fn rebuild_values_for_function(
        &self,
        path: &Path,
        func: &dyn TagFunction,
    ) -> Vec<TagValue> {
        func.tagger()
            .into_iter()
            .flat_map(|t| t.get_columns())
            .filter_map(|col| {
                (col.target_table == TargetTable::Locations)
                    .then(|| func.generate_from_path(path).unwrap_or(TagValue::Null))
            })
            .collect()
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
        }
    }
}

// ========================================================
// Tests
// ========================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::SafeMetadata;

    #[test]
    fn test_triage_accumulator_logic() {
        let mut acc = TriageAccumulator::new(123);
        acc = acc.collect(TriagePiece::Entity(TagValue::BigInt(100)));
        acc = acc.collect(TriagePiece::Location(TagValue::Text("/path".into())));
        acc = acc.collect(TriagePiece::Tag(TagRow {
            item_id: 123,
            tag_type: "ext".into(),
            label: "rs".into(),
        }));
        let res = acc.finish();
        assert_eq!(res.entity_row.id, 123);
        assert_eq!(res.entity_row.values[1], TagValue::BigInt(100));
        assert_eq!(res.location_row.values[0], TagValue::Text("/path".into()));
        assert_eq!(res.tags[0].tag_type, "ext");
    }

    #[test]
    fn test_triager_base_tag_logic() {
        let registry = FunctionRegistry::new();
        let triager = ItemTriager::new(&registry);

        let val = TagValue::Text("rs".to_string());
        let piece = triager.triage_base_tag(100, val, "extension");
        if let TriagePiece::Tag(tag) = piece {
            assert_eq!(tag.item_id, 100);
            assert_eq!(tag.tag_type, "extension");
            assert_eq!(tag.label, "rs");
        } else {
            panic!("Should be TriagePiece::Tag");
        }
    }

    #[test]
    fn test_triager_classify_logic() {
        let registry = FunctionRegistry::new();
        let triager = ItemTriager::new(&registry);

        let col_ent = ColumnDef {
            name: "size".to_string(),
            sql_type: "BIGINT",
            target_table: TargetTable::FileEntities,
        };
        let p_ent = triager.classify(1, TagValue::BigInt(1024), &col_ent);
        assert!(matches!(p_ent, TriagePiece::Entity(TagValue::BigInt(1024))));
    }

    #[test]
    fn test_triager_triage_item_full() {
        let registry = FunctionRegistry::new();
        let triager = ItemTriager::new(&registry);

        let cols = vec![
            ColumnDef { name: "size".into(), sql_type: "BIGINT", target_table: TargetTable::FileEntities },
            ColumnDef { name: "path".into(), sql_type: "TEXT", target_table: TargetTable::Locations },
            ColumnDef { name: "ext".into(), sql_type: "TEXT", target_table: TargetTable::BaseTags },
        ];
        let vals = vec![
            TagValue::BigInt(500),
            TagValue::Text("/foo.rs".into()),
            TagValue::Text("rs".into()),
        ];

        let res = triager.triage_item(7, vals, &cols);

        assert_eq!(res.entity_row.id, 7);
        // Entity: [Rank(0), Size(500)]
        assert_eq!(res.entity_row.values.len(), 2);
        assert_eq!(res.entity_row.values[1], TagValue::BigInt(500));

        assert_eq!(res.location_row.id, 7);
        assert_eq!(res.location_row.values[0], TagValue::Text("/foo.rs".into()));

        assert_eq!(res.tags.len(), 1);
        assert_eq!(res.tags[0].tag_type, "ext");
        assert_eq!(res.tags[0].label, "rs");
    }

    #[test]
    fn test_triager_rebuild_from_path_strict() {
        let registry = FunctionRegistry::with_standard();
        let triager = ItemTriager::new(&registry);

        let path = Path::new("/test/dir/file.txt");
        let functions = registry.all_functions();
        let values = triager.rebuild_values_from_path(path, functions);
        assert_eq!(values.len(), 4);
    }

    #[test]
    fn test_is_not_found_helper() {
        use std::io::{Error, ErrorKind};
        let io_err = Error::new(ErrorKind::NotFound, "missing");
        let anyhow_err = anyhow::Error::from(io_err);
        assert!(is_not_found(&anyhow_err));

        let io_err_other = Error::new(ErrorKind::PermissionDenied, "denied");
        let anyhow_err_other = anyhow::Error::from(io_err_other);
        assert!(!is_not_found(&anyhow_err_other));
    }

    #[test]
    fn test_triager_extract_skip_missing() {
        let registry = FunctionRegistry::with_standard();
        let triager = ItemTriager::new(&registry);

        // 存在しないパス
        let res = triager.extract_single_file("non_existent_file_12345");
        assert!(res.is_ok());
        assert!(res.unwrap().is_none(), "Should return None (skip) for missing file");
    }

    #[test]
    fn test_extract_all_with_race_condition() {
        use tempfile::tempdir;
        use std::fs::File;

        let dir = tempdir().unwrap();
        let registry = FunctionRegistry::with_standard();
        let triager = ItemTriager::new(&registry);

        // 1. ファイルをいくつか作成
        let paths = vec![
            dir.path().join("file1.txt"),
            dir.path().join("file2.txt"),
            dir.path().join("file3.txt"),
        ];
        for p in &paths {
            File::create(p).unwrap();
        }

        // 2. ScanEntry を作成 (この時点では全ファイル存在)
        let entries: Vec<ScanEntry> = paths
            .iter()
            .map(|p| {
                let metadata = std::fs::metadata(p).unwrap();
                ScanEntry::from_path_metadata(p, &SafeMetadata::new(&metadata))
                    .unwrap()
            })
            .collect();

        // 3. 1つだけファイルを削除して、今回の不具合状況（競合）を再現
        std::fs::remove_file(&paths[1]).unwrap();

        // 4. 実行: 修正前なら Err になっていたはず
        let res = triager
            .extract_all(entries)
            .expect("Should not fail even if file is missing during extraction");

        // 5. 検証: 2つだけ成功し、1つは安全にスキップされているはず
        assert_eq!(res.len(), 2, "Expected 2 files to be processed, 1 skipped");
    }
}
