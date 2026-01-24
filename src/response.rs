use crate::types::{
    Intrinsic, ItemId, ItemKind, ItemName, Origin, Rank, SType, TagType, Tags,
};

/// 検索結果を表す構造体。
#[derive(Debug, PartialEq, Clone)]
pub struct SearchResult {
    /// アイテムの一意なID
    pub id: ItemId,
    /// アイテムの種類
    pub item_kind: ItemKind,
    /// 解決済みの名称
    pub name: ItemName,
    /// アイテムの優先度
    pub rank: Rank,
    /// 固定の固有情報
    pub intrinsic: Intrinsic,
    /// アイテムに紐づく動的なタグの集合
    pub tags: Tags,
    /// プロジェクション時に、この結果が代表しているラベル
    pub projected_label: Option<crate::types::Label>,
}

/// データベースから取得した生のタグ情報の断片。
#[derive(Clone)]
pub struct RawTagRow {
    pub id: ItemId,
    pub item_kind: ItemKind,
    pub tag_type: String,
    pub label_str: Option<String>,
    pub label_int: Option<i64>,
    pub label_double: Option<f64>,
    pub label_bool: Option<bool>,
    pub origin: String,
}

impl RawTagRow {
    pub fn from_row(r: &duckdb::Row) -> duckdb::Result<Self> {
        use crate::db::Col;
        use sea_query::Iden;

        let col = |c: Col| {
            let mut s = String::new();
            c.unquoted(&mut s);
            s
        };

        Ok(Self {
            id: r.get(col(Col::ItemId).as_str())?,
            item_kind: r.get(col(Col::ItemKind).as_str())?,
            tag_type: r.get(col(Col::Type).as_str())?,
            label_str: r.get(col(Col::LabelStr).as_str())?,
            label_int: r.get(col(Col::LabelInt).as_str())?,
            label_double: r.get(col(Col::LabelDouble).as_str())?,
            label_bool: r.get(col(Col::LabelBool).as_str())?,
            origin: r.get(col(Col::Origin).as_str())?,
        })
    }
}

/// 検索クエリの結果全体を表す構造体。
#[derive(Debug, PartialEq, Clone, Default)]
pub struct SearchResponse {
    /// ヒットしたアイテムのリスト
    pub results: Vec<SearchResult>,
    /// プロジェクション時の構造化された結果
    pub label_results: Vec<LabelGroup>,
    /// クエリで明示的に投影（Projection）されたタグ型（互換性のため維持）。
    pub type_for_projection: Option<TagType>,
    /// キャッシュ ID（続きがある場合のみ有効）
    pub cid: Option<String>,
    /// 検索結果の総件数（確定している場合）
    pub total_count: Option<usize>,
    /// まだ続き（Next Page）があるかどうか
    pub has_more: bool,
    /// キャッシュ生成等の進捗状況
    pub progress: crate::types::Progress,
}

/// 同一の属性（カラム）構成を持つアイテムのグループ。
#[derive(Debug, Clone)]
pub struct TypeGroup<'a> {
    /// このグループが持つ共通の属性（タグ型）のリスト。
    pub keys: Vec<TagType>,
    /// 所属するアイテムのリスト。
    pub results: Vec<&'a SearchResult>,
}

/// 投影された「一意な値（ラベル）」とそのアイテム集合（所有権あり）。
#[derive(Debug, Clone, PartialEq)]
pub struct LabelGroup {
    /// 投影されたラベルの値
    pub label: crate::types::Label,
    /// このラベルを持つアイテムの集合
    pub results: Vec<SearchResult>,
}

impl SearchResponse {
    /// 通常表示用に、アイテムの Kind とタグ構成（属性構成）が同一なものを集約して返します。
    pub fn iter_type_groups(&self) -> Vec<TypeGroup> {
        use std::collections::{BTreeSet, HashMap};

        let mut groups: HashMap<
            (String, BTreeSet<TagType>),
            Vec<&SearchResult>,
        > = HashMap::new();

        for res in &self.results {
            let mut keys = BTreeSet::new();

            // 固定メタデータ
            if res.intrinsic.size.is_some() {
                keys.insert(TagType::from(SType::Size));
            }
            if res.intrinsic.mtime.is_some() {
                keys.insert(TagType::from(SType::Mtime));
            }
            if res.intrinsic.hash.is_some() {
                keys.insert(TagType::from(SType::Hash));
            }

            // 動的タグ
            for t in &res.tags.types {
                keys.insert(TagType::from(t.as_str()));
            }

            let group_key = (res.item_kind.clone(), keys);
            groups.entry(group_key).or_default().push(res);
        }

        let mut sorted_groups: Vec<TypeGroup> = groups
            .into_iter()
            .map(|((_, keys), results)| TypeGroup {
                keys: keys.into_iter().collect(),
                results,
            })
            .collect();

        // 集約ルール: カラム数の多い順、その次は属性名の昇順でソートして安定表示を確保
        sorted_groups.sort_by(|a, b| {
            b.keys
                .len()
                .cmp(&a.keys.len())
                .then_with(|| a.keys.cmp(&b.keys))
        });

        sorted_groups
    }

    /// クエリで指定された投影項目に基づき、ラベルごとのグループを返します。
    pub fn iter_label_groups(&self) -> Vec<LabelGroup> {
        self.label_results.clone()
    }
}

impl SearchResponse {
    /// 空の検索結果（初期状態）を作成します。
    pub fn new_empty(
        cid: Option<String>,
        has_more: bool,
        type_for_projection: Option<TagType>,
    ) -> Self {
        Self {
            results: Vec::new(),
            label_results: Vec::new(),
            cid,
            has_more,
            total_count: Some(0),
            progress: crate::types::Progress {
                current: 0,
                total: Some(0),
            },
            type_for_projection,
        }
    }

    /// キャッシュ生成が進行中のレスポンスを作成します。
    pub fn new_unfinished(cid: &str, progress: crate::types::Progress) -> Self {
        Self {
            results: Vec::new(),
            label_results: Vec::new(),
            cid: Some(cid.to_string()),
            has_more: true,
            total_count: None,
            progress,
            type_for_projection: None,
        }
    }
}

impl SearchResult {
    /// 指定された ID で空の検索結果を作成します。
    pub fn new_empty(id: ItemId) -> Self {
        Self {
            id,
            item_kind: String::new(),
            name: String::new(),
            rank: 0,
            intrinsic: Intrinsic::default(),
            tags: Tags::new(),
            projected_label: None,
        }
    }

    /// 生のタグ行データをアイテムに適用します。
    pub fn apply_raw_tag(&mut self, row: RawTagRow) {
        use crate::types::{Label, Origin, SType, TagType};

        // 検索結果に必要な基本属性を RawRow から補完
        if self.item_kind.is_empty() {
            self.item_kind = row.item_kind.clone();
        }

        let origin = if row.origin == "system" {
            Origin::System
        } else {
            Origin::User
        };

        let label = if let Some(i) = row.label_int {
            Label::Integer(i)
        } else if let Some(s) = row.label_str {
            Label::String(s)
        } else if let Some(b) = row.label_bool {
            Label::String(b.to_string())
        } else if let Some(d) = row.label_double {
            Label::String(d.to_string())
        } else {
            return;
        };

        let tag_type = TagType::from(row.tag_type.clone());

        // 特殊属性の解決
        match &tag_type {
            TagType::Base(SType::Name) => {
                self.name = label.as_str();
                self.tags.push(tag_type, label, origin);
            }
            TagType::Base(SType::Rank) => {
                self.rank = label.as_i64();
                self.tags.push(tag_type, label, origin);
            }
            TagType::Base(SType::Size) => {
                self.intrinsic.size =
                    Some(crate::types::FileSize(label.as_i64()));
                self.tags.push(tag_type, label, origin);
            }
            TagType::Base(SType::Mtime) => {
                self.intrinsic.mtime =
                    Some(crate::types::FileTimestamp(label.as_i64()));
                self.tags.push(tag_type, label, origin);
            }
            TagType::Base(SType::Hash) => {
                self.intrinsic.hash = Some(label.as_str());
                self.tags.push(tag_type, label, origin);
            }
            TagType::Base(SType::ItemKind) => {
                self.item_kind = label.as_str();
                self.tags.push(tag_type, label, origin);
            }
            _ => {
                self.tags.push(tag_type, label, origin);
            }
        }
    }
    /// 代表的な値（パスやコンテンツ）を取得するヘルパー。
    /// ファイルならパス、Noteならコンテンツなどを返します。
    pub fn primary_value(&self) -> Option<String> {
        // 抽象化された名前があればそれを最優先
        if !self.name.is_empty() {
            return Some(self.name.clone());
        }
        // フォールバックとしてタグの中を探す
        self.get_tag_value("path")
            .or_else(|| self.get_tag_value("content"))
            .or_else(|| self.get_tag_value("value"))
            .or_else(|| self.get_tag_value("filename"))
    }

    /// アイテム全体の集約された由来を取得します。
    /// 一つでもユーザー付与のタグがあれば Origin::User を返します。
    pub fn origin(&self) -> Origin {
        self.tags
            .origins
            .iter()
            .any(|&o| o == Origin::User)
            .then_some(Origin::User)
            .unwrap_or(Origin::System)
    }

    /// 指定されたキーのタグ値を文字列として取得します。
    /// 複数値がある場合は最初の一つを返します。
    pub fn get_tag_value(&self, key: &str) -> Option<String> {
        self.get_all_values(key).into_iter().next()
    }

    /// 指定されたキーの全てのタグ値を文字列として取得します。
    pub fn get_all_values(&self, key: &str) -> Vec<String> {
        self.get_all_labels(&TagType::from(key))
            .into_iter()
            .map(|l| l.as_str())
            .collect()
    }

    /// 指定されたキーの全てのラベルを Label 型として取得します。
    /// 固定メタデータや仮想ラベルも透過的にアクセス可能です。
    pub fn get_all_labels(
        &self,
        tag_type: &TagType,
    ) -> Vec<crate::types::Label> {
        use crate::types::Label;

        // 1. 固定メタデータの解決
        match &tag_type {
            TagType::Base(SType::Size) => {
                return self
                    .intrinsic
                    .size
                    .as_ref()
                    .map(|s| vec![Label::from(s.0)])
                    .unwrap_or_default();
            }
            TagType::Base(SType::Mtime) => {
                return self
                    .intrinsic
                    .mtime
                    .as_ref()
                    .map(|t| vec![Label::from(t.0)])
                    .unwrap_or_default();
            }
            TagType::Base(SType::Hash) => {
                return self
                    .intrinsic
                    .hash
                    .as_ref()
                    .map(|h| vec![Label::from(h.clone())])
                    .unwrap_or_default();
            }
            TagType::Base(SType::Rank) => {
                return vec![Label::from(self.rank)];
            }
            TagType::Base(SType::ItemKind) => {
                return vec![Label::from(self.item_kind.clone())];
            }
            TagType::Base(SType::Name) => {
                return vec![Label::from(self.name.clone())];
            }
            TagType::Base(SType::Origin) => {
                return vec![Label::from(self.origin().to_string())];
            }
            // 仮想ラベル: type: (タグの型一覧)
            TagType::Base(SType::Type) => {
                let mut types: Vec<String> = self.tags.types.clone();
                // 固定属性も型として含める（設計上の整合性）
                if self.intrinsic.size.is_some() {
                    types.push(SType::Size.to_string());
                }
                if self.intrinsic.mtime.is_some() {
                    types.push(SType::Mtime.to_string());
                }
                types.sort();
                types.dedup();
                return types.into_iter().map(Label::from).collect();
            }
            // 仮想ラベル: label: (アイテムが持つ全てのラベル値)
            TagType::Base(SType::Label) => {
                let mut labels: Vec<Label> = self.tags.labels.clone();
                labels.sort();
                labels.dedup();
                return labels;
            }
            // 仮想ラベル: typedtag: (type:label 形式の全タグ)
            TagType::Base(SType::TypedTag) => {
                let mut tts: Vec<String> = self
                    .tags
                    .iter_typed_tags()
                    .map(|tt| tt.to_string())
                    .collect();
                // 固定属性も TypedTag として扱う
                if let Some(s) = &self.intrinsic.size {
                    tts.push(format!("{}:{}", SType::Size, s.0));
                }
                tts.sort();
                return tts.into_iter().map(Label::from).collect();
            }
            _ => {}
        }

        // 2. Tags からのリニアスキャン取得
        self.tags
            .get_values(tag_type)
            .into_iter()
            .map(|v| v.label)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Progress;
    use crate::types::{FileSize, Intrinsic, Label, Origin, TagType};

    fn create_test_result() -> SearchResult {
        let mut tags = Tags::new();
        tags.push(
            TagType::from("extension"),
            Label::String("rs".to_string()),
            Origin::System,
        );
        tags.push(
            TagType::from("project"),
            Label::String("A".to_string()),
            Origin::User,
        );
        tags.push(
            TagType::from("project"),
            Label::String("B".to_string()),
            Origin::User,
        );

        SearchResult {
            id: 1,
            item_kind: "file".to_string(),
            name: "test.rs".to_string(),
            rank: 1,
            intrinsic: Intrinsic {
                size: Some(FileSize(100)),
                mtime: None,
                hash: None,
            },
            tags,
            projected_label: None,
        }
    }

    #[test]
    fn test_get_all_values() {
        let res = create_test_result();

        // 固有属性
        assert_eq!(res.get_all_values("size"), vec!["100"]);
        assert_eq!(res.get_all_values("name"), vec!["test.rs"]);

        // 通常タグ (複数値)
        let mut projects = res.get_all_values("project");
        projects.sort();
        assert_eq!(projects, vec!["A", "B"]);

        // 仮想ラベル type:
        let types = res.get_all_values("type");
        assert!(types.contains(&"extension".to_string()));
        assert!(types.contains(&"project".to_string()));
        assert!(types.contains(&"size".to_string()));

        // 仮想ラベル typedtag:
        let tts = res.get_all_values("typedtag");
        assert!(tts.contains(&"extension:rs".to_string()));
        assert!(tts.contains(&"project:A".to_string()));
        assert!(tts.contains(&"project:B".to_string()));
        assert!(tts.contains(&"size:100".to_string()));
    }

    #[test]
    fn test_iter_type_groups() {
        let res1 = create_test_result();
        let mut res2 = create_test_result();
        res2.name = "other.rs".to_string();

        let response = SearchResponse {
            results: vec![res1, res2],
            label_results: Vec::new(),
            type_for_projection: None,
            cid: None,
            total_count: None,
            has_more: false,
            progress: Progress::default(),
        };

        let groups = response.iter_type_groups();
        // 両方同じ属性構成（size, mtimeなし, projectタグあり 等）なので 1 グループ
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].results.len(), 2);
        // カラムの中に extension (TagType) が含まれるか
        assert!(groups[0].keys.iter().any(|k| k.as_str() == "extension"));
    }

    #[test]
    fn test_iter_label_groups() {
        use crate::types::Label;
        let mut res1 = create_test_result();
        res1.projected_label = Some(Label::from("rs"));
        let mut res2 = create_test_result();
        res2.name = "other.rs".to_string();
        res2.projected_label = Some(Label::from("rs"));

        let label_group = LabelGroup {
            label: Label::from("rs"),
            results: vec![res1.clone(), res2.clone()],
        };

        let response = SearchResponse {
            results: vec![res1, res2],
            label_results: vec![label_group],
            ..Default::default()
        };

        let groups = response.iter_label_groups();
        // extension は両方 "rs" なので 1 グループ
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, Label::from("rs"));
        assert_eq!(groups[0].results.len(), 2);
    }

    #[test]
    fn test_iter_label_groups_numeric_sort() {
        use crate::types::Label;
        let mut res1 = create_test_result();
        res1.projected_label = Some(Label::from(20));

        let mut res2 = create_test_result();
        res2.projected_label = Some(Label::from(100));

        let group1 = LabelGroup {
            label: Label::from(20),
            results: vec![res1.clone()],
        };
        let group2 = LabelGroup {
            label: Label::from(100),
            results: vec![res2.clone()],
        };

        let response = SearchResponse {
            results: vec![res1, res2],
            label_results: vec![group1, group2],
            ..Default::default()
        };

        let groups = response.iter_label_groups();
        assert_eq!(groups.len(), 2);
        // DBから届いた順序（20, 100）が維持される
        assert_eq!(groups[0].label, Label::from(20));
        assert_eq!(groups[1].label, Label::from(100));
    }

    #[test]
    fn test_empty_projection_handling() {
        let response = SearchResponse {
            results: vec![create_test_result()],
            label_results: Vec::new(),
            type_for_projection: None,
            cid: None,
            total_count: None,
            has_more: false,
            progress: Progress::default(),
        };

        let groups = response.iter_label_groups();
        assert!(groups.is_empty());
    }
}
