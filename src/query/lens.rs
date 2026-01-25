use crate::db::Col;
use crate::query::QueryFunction;
use crate::query_functions::*;
use crate::types::{SType, TagType};
use std::collections::HashMap;

/// タグの物理的な格納場所
#[derive(Debug, PartialEq, Clone)]
pub enum StorageMapping {
    /// oneview の直接のカラム
    Column(Col),
    /// oneview の行ベースのタグ (特定のラベルカラム + タグ名)
    RowTag { column: Col, tag_key: String },
    /// 他のタグに展開される論理タグ
    Virtual,
}

/// タグのメタデータ記述
pub struct TagDescriptor {
    pub tag_type: TagType,
    pub storage: StorageMapping,
    pub logical_function: Option<Box<dyn QueryFunction>>,
}

/// タグ知識の統合レジストリ
pub struct Lens {
    registry: HashMap<TagType, TagDescriptor>,
}

impl Lens {
    pub fn new() -> Self {
        Self {
            registry: HashMap::new(),
        }
    }

    /// 標準的なタグ定義を登録済みの Lens を返します。
    pub fn with_standard() -> Self {
        let mut lens = Self::new();
        for desc in base_column_descriptors() {
            lens.register(desc);
        }
        for desc in row_tag_descriptors() {
            lens.register(desc);
        }
        for desc in virtual_tag_descriptors() {
            lens.register(desc);
        }
        lens
    }

    /// タグ定義を登録します。既存の定義がある場合はマージします。
    pub fn register(&mut self, descriptor: TagDescriptor) {
        if let Some(existing) = self.registry.get_mut(&descriptor.tag_type) {
            // 物理ストレージ定義があれば上書き（Virtual は物理を上書きしない）
            if descriptor.storage != StorageMapping::Virtual {
                existing.storage = descriptor.storage;
            }
            // 論理関数が提供されていれば上書き
            if descriptor.logical_function.is_some() {
                existing.logical_function = descriptor.logical_function;
            }
        } else {
            self.registry.insert(descriptor.tag_type.clone(), descriptor);
        }
    }

    /// 指定されたタグの定義を検索します。
    pub fn look_up(&self, tag: &TagType) -> Option<&TagDescriptor> {
        self.registry.get(tag)
    }
}

// --- 純粋関数 (初期化用データ定義) ---

fn base_column_descriptors() -> Vec<TagDescriptor> {
    let cols = vec![
        (SType::ItemId, Col::ItemId),
        (SType::FileId, Col::FileId),
        (SType::Rank, Col::Rank),
        (SType::Origin, Col::Origin),
        (SType::ItemKind, Col::ItemKind),
        (SType::Type, Col::Type),
        (SType::TypedTag, Col::TypedTag),
        (SType::Label, Col::LabelStr),
        (SType::ScanHash, Col::ScanHash),
    ];

    cols.into_iter()
        .map(|(stype, col)| TagDescriptor {
            tag_type: TagType::Base(stype),
            storage: StorageMapping::Column(col),
            logical_function: None,
        })
        .collect()
}

fn row_tag_descriptors() -> Vec<TagDescriptor> {
    let tags = vec![
        (SType::Path, Col::LabelStr),
        (SType::Parentdir, Col::LabelStr),
        (SType::Filename, Col::LabelStr),
        (SType::Stem, Col::LabelStr),
        (SType::Extension, Col::LabelStr),
        (SType::IsDir, Col::LabelBool),
        (SType::Size, Col::LabelInt),
        (SType::Mtime, Col::LabelInt),
        (SType::Hash, Col::LabelStr),
        (SType::Content, Col::LabelStr),
        (SType::Name, Col::LabelStr),
        (SType::TypeFromExt, Col::LabelStr),
        (SType::SizeStr, Col::LabelStr),
        (SType::ModifiedStr, Col::LabelStr),
    ];

    tags.into_iter()
        .map(|(stype, col)| {
            let key: &'static str = stype.into();
            TagDescriptor {
                tag_type: TagType::Base(stype),
                storage: StorageMapping::RowTag {
                    column: col,
                    tag_key: key.to_string(),
                },
                logical_function: None,
            }
        })
        .collect()
}

fn virtual_tag_descriptors() -> Vec<TagDescriptor> {
    let v_tags: Vec<(SType, Box<dyn QueryFunction>)> = vec![
        (SType::Directory, Box::new(DirectoryQuery)),
        (SType::Filename, Box::new(FilenameQuery)),
        (SType::Extension, Box::new(ExtensionQuery)),
        (SType::Path, Box::new(PathQuery)),
        (SType::Parentdir, Box::new(ParentDirQuery)),
        (SType::Name, Box::new(NameQuery)),
        (SType::Size, Box::new(SizeQuery)),
        (SType::Mtime, Box::new(MtimeQuery)),
        (SType::ItemKind, Box::new(ItemKindQuery)),
        (SType::Rank, Box::new(RankQuery)),
        (SType::Origin, Box::new(OriginQuery)),
        (SType::Type, Box::new(TypeQuery)),
        (SType::Label, Box::new(LabelQuery)),
        (SType::TypedTag, Box::new(TypedTagQuery)),
    ];

    v_tags
        .into_iter()
        .map(|(stype, func)| TagDescriptor {
            tag_type: TagType::Base(stype),
            storage: StorageMapping::Virtual,
            logical_function: Some(func),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SType;

    #[test]
    fn test_lens_with_standard_includes_rank() {
        let lens = Lens::with_standard();
        let found = lens.look_up(&TagType::Base(SType::Rank)).unwrap();
        // マージ論理により、Column 定義が Virtual を上書きしているはず
        assert_eq!(found.storage, StorageMapping::Column(Col::Rank));
        assert!(found.logical_function.is_some());
    }

    #[test]
    fn test_lens_with_standard_includes_origin() {
        let lens = Lens::with_standard();
        let found = lens.look_up(&TagType::Base(SType::Origin)).unwrap();
        assert_eq!(found.storage, StorageMapping::Column(Col::Origin));
        assert!(found.logical_function.is_some());
    }

    #[test]
    fn test_lens_with_standard_includes_size() {
        let lens = Lens::with_standard();
        let found = lens.look_up(&TagType::Base(SType::Size)).unwrap();
        if let StorageMapping::RowTag { column, tag_key } = &found.storage {
            assert_eq!(*column, Col::LabelInt);
            assert_eq!(tag_key, "size");
        } else {
            panic!("Expected RowTag mapping for size, got {:?}", found.storage);
        }
        assert!(found.logical_function.is_some());
    }

    #[test]
    fn test_lens_with_standard_includes_directory_as_virtual() {
        let lens = Lens::with_standard();
        let found = lens.look_up(&TagType::Base(SType::Directory)).unwrap();
        assert_eq!(found.storage, StorageMapping::Virtual);
        assert!(found.logical_function.is_some());
    }

    #[test]
    fn test_lens_look_up_unknown_tag_returns_none() {
        let lens = Lens::with_standard();
        let unknown = TagType::from("magic_tag_that_does_not_exist");
        assert!(lens.look_up(&unknown).is_none());
    }

    #[test]
    fn test_lens_filename_is_virtual() {
        let lens = Lens::with_standard();
        let found = lens.look_up(&TagType::Base(SType::Filename)).unwrap();
        // Virtual が最後に登録され、かつマージにより以前の RowTag を上書きしない（関数だけ上書き）
        // ...はずだが、今回の実装では descriptor.storage != Virtual の時だけ物理をを優先。
        // Filename は RowTag -> Virtual の順に登録される。
        // RowTag 登録時: storage=RowTag
        // Virtual 登録時: storage=Virtual なので existing.storage は更新されない。
        // 結果、物理情報（RowTag）を保持しつつ論理関数を持つ。
        if let StorageMapping::RowTag { .. } = &found.storage {
            // OK
        } else {
            panic!("Expected RowTag for filename, got {:?}", found.storage);
        }
        assert!(found.logical_function.is_some());
    }

    #[test]
    fn test_lens_all_standard_tags_are_resolvable() {
        let lens = Lens::with_standard();
        let standard_types = vec![
            SType::ItemId,
            SType::FileId,
            SType::Rank,
            SType::Origin,
            SType::ItemKind,
            SType::Type,
            SType::TypedTag,
            SType::Label,
            SType::Size,
            SType::Extension,
            SType::Mtime,
            SType::Path,
            SType::Filename,
            SType::Parentdir,
            SType::Stem,
            SType::IsDir,
            SType::Hash,
            SType::Content,
            SType::TypeFromExt,
            SType::SizeStr,
            SType::ModifiedStr,
            SType::Directory,
            SType::Name,
            SType::ScanHash,
        ];

        for stype in standard_types {
            let tag_type = TagType::Base(stype);
            let found = lens.look_up(&tag_type);
            assert!(
                found.is_some(),
                "Standard tag {:?} should be resolvable",
                stype
            );
        }
    }
}
