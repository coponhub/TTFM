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

use crate::types::{
    Bitical, Intrinsic, ItemId, ItemKind, Label, Origin, Rank, SType, TagType,
    Tags,
};

/// 検索・編集操作の共通アイテム表現。
#[derive(Debug, PartialEq, Clone)]
pub struct Item {
    /// アイテムの一意なID
    pub id: ItemId,
    /// アイテムの種類
    pub item_kind: ItemKind,
    /// この結果を代表するラベル（Projectionではラベル値、通常はpath/nameなど）
    pub representative: Vec<Label>,
    /// アイテムの優先度
    pub rank: Rank,
    /// 固定の固有情報
    pub intrinsic: Intrinsic,
    /// アイテムに紐づく動的なタグの集合
    pub tags: Tags,
    /// プロジェクション時に、この結果が代表しているラベル
    pub item_count: Option<crate::types::Label>,
}

/// データベースから取得した生のタグ情報の断片。
#[derive(Clone)]
pub struct RawTagRow {
    pub id: ItemId,
    pub item_kind: ItemKind,
    pub tag_type: String,
    pub value: Option<Bitical>,
    pub origin: String,
}

impl RawTagRow {
    pub fn from_row(r: &duckdb::Row) -> duckdb::Result<Self> {
        use crate::db::{BiticalType, Col};
        use duckdb::types::Value;
        use sea_query::Iden;

        let col = |c: Col| {
            let mut s = String::new();
            c.unquoted(&mut s);
            s
        };

        // label_str は全型の VARCHAR フォールバックを兼ねるため、
        // 型付きカラムが先に評価される走査順で最初の非 NULL を採用する。
        let mut value = None;
        for c in BiticalType::to_columns_scan_order() {
            let v: Value = r.get(col(c).as_str())?;
            if let Some(b) = Bitical::from_scalar_db_value(&v) {
                value = Some(b);
                break;
            }
        }

        Ok(Self {
            id: r.get(col(Col::ItemId).as_str())?,
            item_kind: r.get(col(Col::ItemKind).as_str())?,
            tag_type: r.get(col(Col::Type).as_str())?,
            value,
            origin: r.get(col(Col::Origin).as_str())?,
        })
    }

    pub fn from_map(
        map: &duckdb::types::OrderedMap<String, duckdb::types::Value>,
    ) -> Option<Self> {
        use duckdb::types::Value;

        let get = |key: &str| map.get(&key.to_string());

        let tag_type = match get("tag_type")? {
            Value::Text(s) => s.clone(),
            _ => return None,
        };
        let value = get("value").cloned().and_then(Bitical::from_db_value);
        let origin = match get("origin") {
            Some(Value::Text(s)) => s.clone(),
            _ => "system".to_string(),
        };

        Some(Self {
            id: ItemId::Volatile(0),
            item_kind: ItemKind::Volatile,
            tag_type,
            value,
            origin,
        })
    }
}

/// 検索クエリの結果全体を表す構造体。
#[derive(Debug, PartialEq, Clone, Default)]
pub struct SearchResponse {
    /// ヒットしたアイテムのリスト
    pub results: Vec<Item>,
    /// キャッシュ ID（続きがある場合のみ有効）
    pub cid: Option<String>,
    /// 検索結果の総件数（確定している場合）
    pub total_count: Option<usize>,
    /// まだ続き（Next Page）があるかどうか
    pub has_more: bool,
    /// キャッシュ生成等の進捗状況
    pub progress: crate::types::Progress,
    /// この SearchResponse を生成した SearchQuery 文字列
    pub query: String,
}

/// 同一の属性（カラム）構成を持つアイテムのグループ。
#[derive(Debug, Clone)]
pub struct TypeGroup<'a> {
    /// このグループが持つ共通の属性（タグ型）のリスト。
    pub keys: Vec<TagType>,
    /// 所属するアイテムのリスト。
    pub results: Vec<&'a Item>,
}

/// ページングされた結果を保持する構造体。
#[derive(Debug, Clone)]
pub struct PagedResult<T> {
    pub items: Vec<T>,
    pub has_more: bool,
    pub current_page: usize,
}

impl<T> PagedResult<T> {
    pub fn new(mut items: Vec<T>, limit: usize, offset: usize) -> Self {
        let has_more = limit > 0 && items.len() > limit;
        if has_more {
            items.truncate(limit);
        }
        let current_page = if limit > 0 { (offset / limit) + 1 } else { 1 };
        Self {
            items,
            has_more,
            current_page,
        }
    }
}

impl SearchResponse {
    /// 通常表示用に、アイテムの Kind とタグ構成（属性構成）が同一なものを集約して返します。
    pub fn iter_type_groups(&self) -> Vec<TypeGroup<'_>> {
        use std::collections::{BTreeSet, HashMap};

        let mut groups: HashMap<(ItemKind, BTreeSet<TagType>), Vec<&Item>> =
            HashMap::new();

        for res in &self.results {
            let mut keys = BTreeSet::new();

            // 固定メタデータ
            keys.insert(TagType::from(SType::Origin));
            if res.intrinsic.size.is_some() {
                keys.insert(TagType::from(SType::Size));
            }
            if res.intrinsic.mtime.is_some() {
                keys.insert(TagType::from(SType::Mtime));
            }
            if res.intrinsic.hash.is_some() {
                keys.insert(TagType::from(SType::Hash));
            }
            keys.insert(TagType::from(SType::ItemKind));
            for label in &res.representative {
                keys.insert(label.tag_type());
            }

            // 動的タグ
            for entry in &res.tags.entries {
                keys.insert(entry.label.tag_type());
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
}

impl SearchResponse {
    /// 空の検索結果（初期状態）を作成します。
    pub fn new_empty(
        cid: Option<String>,
        has_more: bool,
        query: impl Into<String>,
    ) -> Self {
        Self {
            results: Vec::new(),
            cid,
            has_more,
            total_count: Some(0),
            progress: crate::types::Progress {
                current: 0,
                total: Some(0),
                is_done: true,
            },
            query: query.into(),
        }
    }

    /// キャッシュ生成が進行中のレスポンスを作成します。
    pub fn new_unfinished(
        cid: &str,
        progress: crate::types::Progress,
        query: impl Into<String>,
    ) -> Self {
        Self {
            results: Vec::new(),
            cid: Some(cid.to_string()),
            has_more: true,
            total_count: None,
            progress,
            query: query.into(),
        }
    }

    /// 検索結果から通常のレスポンスを組み立てます。
    /// `has_more` 時は総数不明、それ以外は `offset + 件数` を総数とします。
    pub fn from_results(
        results: Vec<Item>,
        cid: Option<String>,
        has_more: bool,
        n: usize,
        offset: usize,
        query: impl Into<String>,
    ) -> Self {
        let (total_count, progress_total) = if has_more {
            (None, None)
        } else {
            let total = offset + results.len();
            (Some(total), Some(total))
        };
        Self {
            results,
            cid,
            has_more,
            total_count,
            progress: crate::types::Progress {
                current: total_count.unwrap_or(n),
                total: progress_total,
                is_done: !has_more,
            },
            query: query.into(),
        }
    }

    /// `value` タグを持つ Volatile item に `query:` ラベルを注入する。
    /// 計算値の由来保持（EDIT.md §5.7(B)）。
    pub fn query_into_tags(&mut self) {
        use crate::types::{Bitical, Label, Origin, SType, TagType};
        let query_str = self.query.clone();
        let query_tag_type = TagType::Base(SType::Query);
        let value_tag_type = TagType::Base(SType::Value);
        for item in &mut self.results {
            if !item.id.is_volatile() {
                continue;
            }
            let has_value = item
                .tags
                .entries
                .iter()
                .any(|e| e.label.tag_type() == value_tag_type);
            if has_value {
                item.tags.push(
                    Label::Other(
                        query_tag_type.clone(),
                        Bitical::String(query_str.clone()),
                    ),
                    Origin::Builtin,
                );
            }
        }
    }

    /// 結果が Projection (ラベルグループ) 形式かどうかを判定します。
    /// projection の `item:` タグ（メンバー一覧）は検索 SQL が Builtin origin で生成するため、
    /// Builtin origin の `item:` のみを判定対象とする。保存 note 由来やユーザー命名の `item:`
    /// タグ（User origin）では誤発火しない。
    pub fn has_projection_results(&self) -> bool {
        use crate::types::Origin;
        self.results
            .first()
            .map(|r| {
                r.tags.entries.iter().any(|e| {
                    e.label.tag_type().as_str() == "item"
                        && matches!(e.origin, Origin::Builtin)
                })
            })
            .unwrap_or(false)
    }
}

impl Item {
    /// 指定された ID で空の検索結果を作成します。
    pub fn new_empty(id: ItemId, kind: ItemKind) -> Self {
        Self {
            id,
            item_kind: kind,
            representative: vec![],
            rank: 0,
            intrinsic: Intrinsic::default(),
            tags: Tags::new(),
            item_count: None,
        }
    }

    /// representative の全ラベルを " &: " で結合した文字列を返します。
    pub fn raw_repr(&self) -> String {
        self.representative
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(" &: ")
    }

    /// 解決済みのタグ情報をアイテムに適用します。
    pub fn apply_tag(&mut self, label: crate::types::Label, origin: Origin) {
        use crate::types::Label;

        // 検索結果に必要な基本属性を補完 (パターンマッチングによる直感的な代入)
        match &label {
            Label::Rank(i) => self.rank = *i,
            Label::Size(i) => {
                self.intrinsic.size = Some(crate::types::FileSize(*i))
            }
            Label::Mtime(i) => {
                self.intrinsic.mtime = Some(crate::types::FileTimestamp(*i))
            }
            Label::Hash(s) => self.intrinsic.hash = Some(s.clone()),
            Label::ItemKind(s) => {
                if let Ok(k) = s.as_str().parse::<ItemKind>() {
                    self.item_kind = k;
                }
            }
            _ => {} // 通常タグは特化フィールド更新なし
        }

        // 全てのタグ（特殊属性含む）を Tags にプッシュ
        self.tags.push(label, origin);
    }

    /// 生のタグ行データをアイテムに適用します (DEPRECATED: 互換性のために維持)。
    #[deprecated(note = "Use apply_tag instead with resolved Label and Origin")]
    pub fn apply_raw_tag(&mut self, row: RawTagRow) {
        use crate::types::{Bitical, Label, Origin, TagType};

        // oneview の origin 列は常に小分類 (builtin/user/file/plugin) の妥当な文字列。
        // 想定外の値は安全側 (非User) の Builtin にフォールバックする。
        let origin = row.origin.parse::<Origin>().unwrap_or(Origin::Builtin);
        // DB の実 NULL でも「値のない値タグ」としてそのまま記録する
        // （集約結果が空の場合の value: NULL 等）。表示は "NULL" に揃える。
        let value = row
            .value
            .unwrap_or_else(|| Bitical::String("NULL".to_string()));
        let label = Label::resolve(TagType::from(row.tag_type), value);
        self.apply_tag(label, origin);
    }
    /// 代表的な値（パスやコンテンツ）を取得するヘルパー。
    /// ファイルならパス、Noteならコンテンツなどを返します。
    pub fn primary_value(&self) -> Option<String> {
        if !self.representative.is_empty() {
            return Some(self.raw_repr());
        }
        use crate::types::SType;
        self.get_all_labels(&SType::Path.into())
            .first()
            .map(|l| l.as_str().to_string())
            .or_else(|| self.get_tag_value("content"))
            .or_else(|| self.get_tag_value("value"))
            .or_else(|| self.get_tag_value("filename"))
    }

    /// アイテム自身の由来 (Origin, 小分類) を解決します。
    /// 1. tags 内に origin タグ（oneview 属性）があればそれを使用
    /// 2. なければ ItemID から判定（Stored は区画、Settling は保持値）
    /// 3. なければ、簡易判定（代表名がビルトイン型名かどうか）
    pub fn origin(&self) -> Origin {
        if let Some(e) = self
            .tags
            .entries
            .iter()
            .find(|e| e.label.tag_type() == TagType::Base(SType::Origin))
        {
            if let Ok(o) = e.label.as_str().parse::<Origin>() {
                return o;
            }
        }

        match self.id {
            ItemId::Stored(id) => return Origin::within(id),
            ItemId::Settling(origin, _) => return origin,
            ItemId::Volatile(_) => {}
        }

        let name_repr = self.primary_value().unwrap_or_default();
        if <SType as std::str::FromStr>::from_str(&name_repr).is_ok() {
            Origin::Builtin
        } else {
            Origin::User
        }
    }

    /// アイテム全体の大分類 (LargeOrigin) を取得します。`origin()` の収束値です。
    pub fn large_origin(&self) -> crate::types::LargeOrigin {
        self.origin().large()
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
                return self.representative.clone();
            }
            TagType::Base(SType::Origin) => {
                return vec![Label::resolve(
                    TagType::Base(SType::Origin),
                    Bitical::String(self.origin().to_string()),
                )];
            }
            // 仮想ラベル: type: (タグの型一覧)
            TagType::Base(SType::Type) => {
                // 1. Tags に明示的な type タグがあればそれを優先 (volatile用)
                let type_values: Vec<Label> = self
                    .tags
                    .entries
                    .iter()
                    .filter(|e| {
                        e.label.tag_type() == TagType::Base(SType::Type)
                    })
                    .map(|e| e.label.clone())
                    .collect();
                if !type_values.is_empty() {
                    return type_values;
                }

                // 2. なければ従来の挙動（存在する全てのタグ種を返す）
                let mut lab_vals = Vec::new();
                for entry in &self.tags.entries {
                    lab_vals.push(Label::resolve(
                        TagType::Base(SType::Type),
                        Bitical::String(entry.label.tag_type().to_string()),
                    ));
                }
                if self.intrinsic.size.is_some() {
                    lab_vals.push(Label::resolve(
                        TagType::Base(SType::Type),
                        Bitical::String("size".to_string()),
                    ));
                }
                if self.intrinsic.mtime.is_some() {
                    lab_vals.push(Label::resolve(
                        TagType::Base(SType::Type),
                        Bitical::String("mtime".to_string()),
                    ));
                }
                // ItemKind 自体も型として含める
                lab_vals.push(Label::resolve(
                    TagType::Base(SType::Type),
                    Bitical::String(self.item_kind.as_str().to_string()),
                ));

                lab_vals.sort_by_cached_key(|l| l.to_string());
                lab_vals.dedup();
                return lab_vals;
            }

            // 仮想ラベル: label: (アイテムが持つ全てのラベル値)
            TagType::Base(SType::Label) => {
                let mut labels: Vec<Label> =
                    self.tags.entries.iter().map(|e| e.label.clone()).collect();
                labels.sort();
                labels.dedup();
                return labels;
            }
            // 仮想ラベル: tag: (type:label 形式の全タグ)
            TagType::Base(SType::TypedTag) => {
                let mut labels = Vec::new();
                // 明示的なタグ
                labels
                    .extend(self.tags.entries.iter().map(|e| e.label.clone()));
                // 固定属性
                labels.push(Label::from(self.item_kind));
                if let Some(s) = &self.intrinsic.size {
                    labels.push(Label::resolve(
                        TagType::Base(SType::Size),
                        Bitical::Integer(s.0),
                    ));
                }
                if let Some(t) = &self.intrinsic.mtime {
                    labels.push(Label::resolve(
                        TagType::Base(SType::Mtime),
                        Bitical::Integer(t.0),
                    ));
                }

                let mut tts: Vec<String> = labels
                    .into_iter()
                    .map(|l| format!("{}:{}", l.tag_type(), l.as_str()))
                    .collect();
                tts.sort();
                tts.dedup();
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

    fn create_test_result() -> Item {
        let mut tags = Tags::new();
        tags.push(
            Label::resolve(TagType::from("extension"), "rs".into()),
            Origin::Builtin,
        );
        tags.push(
            Label::resolve(TagType::from("project"), "A".into()),
            Origin::User,
        );
        tags.push(
            Label::resolve(TagType::from("project"), "B".into()),
            Origin::User,
        );

        Item {
            id: 1.into(),
            item_kind: ItemKind::File,
            representative: vec![Label::Name("test.rs".to_string())],
            rank: 1,
            intrinsic: Intrinsic {
                size: Some(FileSize(100)),
                mtime: None,
                hash: None,
            },
            tags,
            item_count: None,
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

        // 仮想ラベル tag:
        let tts = res.get_all_values("tag");
        assert!(tts.contains(&"extension:rs".to_string()));
        assert!(tts.contains(&"project:A".to_string()));
        assert!(tts.contains(&"project:B".to_string()));
        assert!(tts.contains(&"size:100".to_string()));
    }

    #[test]
    fn test_iter_type_groups() {
        let res1 = create_test_result();
        let mut res2 = create_test_result();
        res2.representative = vec![Label::Name("other.rs".to_string())];

        let response = SearchResponse {
            results: vec![res1, res2],
            cid: None,
            total_count: None,
            has_more: false,
            progress: Progress::default(),
            query: String::new(),
        };

        let groups = response.iter_type_groups();
        // 両方同じ属性構成（size, mtimeなし, projectタグあり 等）なので 1 グループ
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].results.len(), 2);
        // カラムの中に extension (TagType) が含まれるか
        assert!(response
            .results
            .iter()
            .all(|r| r.item_kind == ItemKind::File));
    }

    #[test]
    fn test_search_result_new_empty_scalar() {
        let id = ItemId::new_volatile();
        let res = Item::new_empty(id, ItemKind::Volatile);

        assert!(res.representative.is_empty());
        assert_eq!(res.item_kind, ItemKind::Volatile);
    }

    #[test]
    fn test_search_result_new_empty_label() {
        let label_id = ItemId::new_volatile();
        let result = Item::new_empty(label_id, ItemKind::Volatile);

        assert!(result.representative.is_empty());
        assert_eq!(result.item_kind, ItemKind::Volatile);
        assert!(result.tags.is_empty());
    }

    // value タグを持つ Volatile item にのみ query: が注入される。
    #[test]
    fn query_into_tags_adds_query_only_to_value_items() {
        use crate::types::{Bitical, ItemKind, Origin, SType, TagType};

        let query_str = "count(extension:rs)".to_string();

        let mut item_with_value =
            Item::new_empty(ItemId::Volatile(0), ItemKind::Volatile);
        item_with_value.tags.push(
            Label::Other(TagType::Base(SType::Value), Bitical::Integer(42)),
            Origin::Builtin,
        );

        let item_without_value =
            Item::new_empty(ItemId::Volatile(1), ItemKind::Volatile);

        let mut resp = SearchResponse {
            results: vec![item_with_value, item_without_value],
            query: query_str.clone(),
            ..Default::default()
        };
        resp.query_into_tags();

        let has_query = |item: &Item| {
            item.tags
                .entries
                .iter()
                .any(|e| e.label.tag_type() == TagType::Base(SType::Query))
        };
        assert!(has_query(&resp.results[0]), "value item should get query:");
        assert!(
            !has_query(&resp.results[1]),
            "non-value item must not get query:"
        );
    }

    // 本物の Projection 結果かどうかは、item タグが Builtin origin かどうかで見分ける
    // （検索 SQL が合成したものだけが Builtin origin になるため）。保存 note 由来や
    // ユーザー命名の item: タグ（User origin）を誤って本物と判定しないことを確認する。
    #[test]
    fn has_projection_results_detects_genuine_projection_via_builtin_origin() {
        use crate::types::{Bitical, ItemKind, Origin, TagType};

        let item_tag = |origin| {
            let mut it = Item::new_empty(ItemId::Stored(1), ItemKind::Note);
            it.tags.push(
                Label::Other(
                    TagType::from("item"),
                    Bitical::String("foo.txt#User(1)".into()),
                ),
                origin,
            );
            it
        };

        let genuine = SearchResponse {
            results: vec![item_tag(Origin::Builtin)],
            ..Default::default()
        };
        assert!(
            genuine.has_projection_results(),
            "Builtin origin の item タグは本物の Projection 結果と判定されるべき"
        );

        let not_genuine = SearchResponse {
            results: vec![item_tag(Origin::User)],
            ..Default::default()
        };
        assert!(
            !not_genuine.has_projection_results(),
            "User origin の item タグは本物の Projection 結果ではないと判定されるべき"
        );
    }

    #[test]
    fn test_item_get_all_labels_origin() {
        use crate::types::{Bitical, Origin, SType, TagType};

        // 1. tags の中に origin タグがある場合
        let mut item = Item::new_empty(ItemId::Volatile(0), ItemKind::File);
        item.tags.push(
            Label::Other(
                TagType::Base(SType::Origin),
                Bitical::String("plugin".to_string()),
            ),
            Origin::Plugin,
        );
        let labels = item.get_all_labels(&TagType::Base(SType::Origin));
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].as_str(), "plugin");

        // 2. tags になく、id が Stored の場合（区画から解決）
        let item_stored = Item::new_empty(ItemId::Stored(-10), ItemKind::File); // Builtin 区画
        let labels = item_stored.get_all_labels(&TagType::Base(SType::Origin));
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].as_str(), "builtin");

        // 3. tags になく、id が Settling の場合（Settlingの保持値を使用）
        let item_settling =
            Item::new_empty(ItemId::Settling(Origin::File, 0), ItemKind::File);
        let labels =
            item_settling.get_all_labels(&TagType::Base(SType::Origin));
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].as_str(), "file");

        // 4. なければ、簡易判定（代表名がビルトイン型名かどうか）
        let mut item_fallback =
            Item::new_empty(ItemId::Volatile(0), ItemKind::Volatile);
        item_fallback.representative = vec![Label::Name("size".to_string())];
        let labels =
            item_fallback.get_all_labels(&TagType::Base(SType::Origin));
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].as_str(), "builtin");
    }

    #[test]
    fn test_item_origin() {
        use crate::types::{Bitical, Origin, SType, TagType};

        // 1. tags の中に origin タグがある場合
        let mut item = Item::new_empty(ItemId::Volatile(0), ItemKind::File);
        item.tags.push(
            Label::Other(
                TagType::Base(SType::Origin),
                Bitical::String("plugin".to_string()),
            ),
            Origin::Plugin,
        );
        assert_eq!(item.origin(), Origin::Plugin);

        // 2. tags になく、id が Stored の場合（区画から解決）
        let item_stored = Item::new_empty(ItemId::Stored(-10), ItemKind::File); // Builtin 区画
        assert_eq!(item_stored.origin(), Origin::Builtin);

        // 3. tags になく、id が Settling の場合（Settlingの保持値を使用）
        let item_settling =
            Item::new_empty(ItemId::Settling(Origin::File, 0), ItemKind::File);
        assert_eq!(item_settling.origin(), Origin::File);

        // 4. なければ、簡易判定（代表名がビルトイン型名かどうか）
        let mut item_fallback =
            Item::new_empty(ItemId::Volatile(0), ItemKind::Volatile);
        item_fallback.representative = vec![Label::Name("size".to_string())];
        assert_eq!(item_fallback.origin(), Origin::Builtin);

        let mut item_fallback_user =
            Item::new_empty(ItemId::Volatile(0), ItemKind::Volatile);
        item_fallback_user.representative =
            vec![Label::Name("my_custom_type".to_string())];
        assert_eq!(item_fallback_user.origin(), Origin::User);
    }

    #[test]
    fn test_item_large_origin() {
        use crate::types::{LargeOrigin, Origin};

        let item_user = Item::new_empty(ItemId::Stored(1), ItemKind::File); // User 区画
        assert_eq!(item_user.large_origin(), LargeOrigin::User);

        let item_plugin = Item::new_empty(
            ItemId::Settling(Origin::Plugin, 0),
            ItemKind::File,
        );
        assert_eq!(item_plugin.large_origin(), LargeOrigin::System);
    }
}
