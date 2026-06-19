// Copyright (C) 2026 coponhub
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

use crate::db::{SqlType, TargetTable};
use crate::query::ast::{
    BasicOp, ComparisonNode, ComparisonOp, Operand, QueryNode,
};
use crate::taggers::TagValue;
use crate::types::{DBType, Label, LabelValue, Rank, SType, TagType, TypedTag};
use crate::util::{parse_datetime, DatetimeRange, SafeMetadata};
use anyhow::Result;
use chrono::TimeZone as _;
use path_slash::PathExt as _;
use indexmap::IndexMap;
use std::fmt::Debug;
use std::path::Path;
use std::sync::Arc;

// ============================================================
// ScanRole
// ============================================================

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ScanRole {
    Location,
    ScanId,
    Integrity,
    Other,
    /// インデックス作成時の抽出対象外（定義とランクのみ提供）
    DefinitionOnly,
}

// ============================================================
// Scan trait — スキャン時の型付きデータ取得 (Phase 4)
// ============================================================

pub struct ScanColumn {
    pub name: &'static str,
    pub sql_type: SqlType,
    pub role: ScanRole,
}

/// スキャン時の型安全なフィールドコンテナ。
pub struct ScanField<F: Scan> {
    pub value: F::Value,
}

impl<F: Scan> std::fmt::Debug for ScanField<F>
where
    F::Value: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}

impl<F: Scan> PartialEq for ScanField<F>
where
    F::Value: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<F: Scan> Clone for ScanField<F>
where
    F::Value: Clone,
{
    fn clone(&self) -> Self {
        Self { value: self.value.clone() }
    }
}

/// スキャン時の型付きデータ取得トレイト。
/// associated type を持つため object-safe ではなく、`define_scan_entry!` マクロ経由でのみ使用する。
pub trait Scan {
    fn name() -> &'static str;
    type Value: DBType + Debug + PartialEq + Clone;
    const SCAN_ROLE: ScanRole;
    fn scan(path: &Path, metadata: &SafeMetadata) -> Result<Self::Value>;
}

// ============================================================
// DisplayFormat / DisplayFormats
// ============================================================

/// タグ値の表示フォーマット定義。
pub struct DisplayFormat {
    pub id: String,
    pub label: String,
}

impl DisplayFormat {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into() }
    }
}

impl Default for DisplayFormat {
    fn default() -> Self {
        DisplayFormat {
            id: "raw".into(),
            label: "Raw".into(),
        }
    }
}

/// 利用可能な表示フォーマット一覧とデフォルト。
pub struct DisplayFormats {
    pub default: DisplayFormat,
    pub options: Vec<DisplayFormat>,
}

impl Default for DisplayFormats {
    fn default() -> Self {
        Self {
            default: DisplayFormat::default(),
            options: vec![],
        }
    }
}

// ============================================================
// Index trait
// ============================================================

/// インデックス時のファイルからタグ値を抽出するロジック。
pub trait Index: Send + Sync {
    fn role(&self) -> ScanRole {
        ScanRole::Other
    }

    fn extract(&self, path: &Path) -> Result<Option<LabelValue>>;

    /// パスのみから値を生成できる場合に返す（移動検知など）。
    fn extract_from_path(&self, _path: &Path) -> Option<LabelValue> {
        None
    }

    /// DB カラムの SQL 型。
    fn sql_type(&self) -> SqlType {
        SqlType::VARCHAR
    }

    /// 書き込み先テーブル。
    fn target_table(&self) -> TargetTable {
        TargetTable::Locations
    }

    /// インデックス保存用の TagValue を返す。UUID 等の特殊型はここをオーバーライド。
    fn extract_tag_value(&self, path: &Path) -> Result<TagValue> {
        match self.extract(path)? {
            None => Ok(TagValue::Null),
            Some(LabelValue::String(s)) | Some(LabelValue::Literal(s)) => {
                Ok(TagValue::Text(s))
            }
            Some(LabelValue::Integer(i)) => Ok(TagValue::BigInt(i)),
            Some(LabelValue::Boolean(b)) => Ok(TagValue::Boolean(b)),
            Some(LabelValue::Double(bits)) => {
                Ok(TagValue::Double(f64::from_bits(bits)))
            }
            Some(LabelValue::Null) => Ok(TagValue::Null),
            Some(LabelValue::Date(_)) => Ok(TagValue::Null),
        }
    }
}

// ============================================================
// LogicalType / LogicalSchema
// ============================================================

pub use crate::query::logical_schema::{LogicalSchema, LogicalType};

// ============================================================
// LogicalRole
// ============================================================

/// タグの論理的な格納役割。Lens が TagDescriptor を自動生成するために使用。
pub enum LogicalRole {
    /// 他タグへ展開する論理タグ。物理ストレージなし（例: directory:）。
    Composite,
    /// 汎用行テーブルに格納。デフォルト。プラグイン定義タグも含む。
    Basic,
    /// 専用 DB カラム（rank, origin 等）。宣言のみ。カラム定義は base_column_descriptors が管理。
    Fixed,
}

// ============================================================
// Query trait
// ============================================================

/// クエリ展開と正規化のロジック。
pub trait Query: Send + Sync {
    /// タグを QueryNode へ展開する。デフォルトはそのまま TypedTag。
    fn expand(
        &self,
        _tagtype: &TagType,
        _label: &Label,
        tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::TypedTag(tag.clone())
    }

    /// Projection（type:形式）を QueryNode へ展開する。
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }

    /// ラベル値を正規化する（例: "1MB" → 1048576）。
    fn normalize_label(&self, label: &Label) -> Label {
        label.clone()
    }

    /// タグのストレージ上の役割。
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Basic
    }

    /// Label の論理型。
    fn logical_type(&self) -> LogicalType {
        LogicalType::Any
    }

    /// DB の `type` カラムに格納されるキー。None = タグ名をそのまま使用。
    /// FilenameFn のみ Some("name") を返す（旧来の DB キー互換）。
    fn storage_key(&self) -> Option<&'static str> {
        None
    }

    /// 比較ノードを展開する。デフォルトはリテラルを normalize_label して ComparisonNode を返す。
    fn expand_comparison(&self, node: ComparisonNode) -> QueryNode {
        let mut node = node;
        if let Operand::Literal(label) = &mut node.first {
            *label = self.normalize_label(label);
        }
        for (_, rhs) in &mut node.rest {
            if let Operand::Literal(label) = rhs {
                *label = self.normalize_label(label);
            }
        }
        QueryNode::Comparison(node)
    }
}

// ============================================================
// Display trait
// ============================================================

/// タグ値の表示フォーマット変換ロジック。
/// `show` メソッド名は std::fmt::Display::fmt との混同を避けるため採用。
pub trait Display: Send + Sync {
    fn formats(&self) -> DisplayFormats {
        DisplayFormats::default()
    }

    fn show(&self, value: &LabelValue, format: DisplayFormat) -> String;
}

// ============================================================
// TagFunction trait
// ============================================================

/// タグの統合定義単位。Index・Query・Display の3コンポーネントを束ねる。
pub trait TagFunction: Send + Sync {
    fn name(&self) -> &str;

    fn index(&self) -> Option<&dyn Index> {
        None
    }

    fn query(&self) -> Option<&dyn Query> {
        None
    }

    fn display(&self) -> Option<&dyn Display> {
        None
    }

    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::DEFAULT
    }
}

// ============================================================
// TagRegistry
// ============================================================

/// TagFunction を一括管理するレジストリ。
pub struct TagRegistry {
    /// 登録順序を保持しつつ名前でのルックアップも O(1) で行える IndexMap
    functions: IndexMap<String, Arc<dyn TagFunction>>,
}

impl Default for TagRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TagRegistry {
    pub fn new() -> Self {
        Self {
            functions: IndexMap::new(),
        }
    }

    pub fn register(&mut self, func: impl TagFunction + 'static) {
        let arc = Arc::new(func);
        self.functions.insert(arc.name().to_string(), arc);
    }

    pub fn get(&self, name: &str) -> Option<&dyn TagFunction> {
        self.functions.get(name).map(|f| f.as_ref())
    }

    pub fn get_arc(&self, name: &str) -> Option<Arc<dyn TagFunction>> {
        self.functions.get(name).cloned()
    }

    pub fn iter_arcs(
        &self,
    ) -> impl Iterator<Item = Arc<dyn TagFunction>> + '_ {
        self.functions.values().cloned()
    }

    pub fn iter_all_for_rank(
        &self,
    ) -> impl Iterator<Item = (&str, Rank)> + '_ {
        self.functions.values().map(|f| (f.name(), f.default_rank()))
    }

    /// 標準タグを全登録したレジストリを返す。
    /// 最初の 9 つは Index 実装あり（カラム順序 = 旧 FunctionRegistry と同一）。
    pub fn with_standard() -> Self {
        let mut reg = Self::new();
        // --- インデックス関数（カラム順序を維持） ---
        reg.register(FileIdFn);
        reg.register(PathFn);
        reg.register(ParentDirFn);
        reg.register(FilenameFn);
        reg.register(StemFn);
        reg.register(ExtensionFn);
        reg.register(IsDirFn);
        reg.register(SizeFn);
        reg.register(MtimeFn);
        // --- クエリ専用関数 ---
        reg.register(DirectoryFn);
        reg.register(HashFn);
        reg.register(ContentFn);
        reg.register(NameFn);
        reg.register(ItemIdFn);
        reg.register(ItemKindFn);
        reg.register(RankFn);
        reg.register(OriginFn);
        reg.register(TypeFn);
        reg.register(LabelFn);
        reg.register(TypedTagFn);
        reg
    }

    /// 指定タグ名の `TagFunction::normalize_label` を呼び出す。
    /// `LabelValue::Literal` はどの関数にも委譲せずそのまま返す。
    pub fn normalize_label(&self, tag_name: &str, label: &Label) -> Label {
        if matches!(label.value(), LabelValue::Literal(_)) {
            return label.clone();
        }
        if let Some(f) = self.get(tag_name) {
            if let Some(q) = f.query() {
                return q.normalize_label(label);
            }
        }
        label.clone()
    }

    /// 全 TagFunction の `normalize_label` を登録の降順に試し、最初に変換された結果を返す。
    /// タグ文脈によらず全リテラルに適用される変換（サイズ単位・日付文字列等）に使用。
    /// `LabelValue::Literal` はどの関数にも委譲せずそのまま返す。
    pub fn normalize_label_any(&self, label: &Label) -> Label {
        if matches!(label.value(), LabelValue::Literal(_)) {
            return label.clone();
        }
        for f in self.functions.values().rev() {
            if let Some(q) = f.query() {
                let normalized = q.normalize_label(label);
                if normalized != *label {
                    return normalized;
                }
            }
        }
        label.clone()
    }

    pub fn get_all_columns(&self) -> Vec<crate::taggers::ColumnDef> {
        self.functions
            .values()
            .filter_map(|f| {
                f.index().map(|idx| crate::taggers::ColumnDef {
                    name: f.name().to_string(),
                    sql_type: idx.sql_type(),
                    target_table: idx.target_table(),
                })
            })
            .collect()
    }

    pub fn process_file(
        &self,
        path: &Path,
    ) -> Result<Vec<crate::taggers::TagValue>> {
        self.functions
            .values()
            .filter_map(|f| f.index())
            .map(|idx| idx.extract_tag_value(path))
            .collect()
    }

    pub fn expand_comparison(&self, node: ComparisonNode) -> QueryNode {
        let name = find_tagtype_name_in_comparison(&node);
        let Some(name) = name else {
            return QueryNode::Comparison(node);
        };
        let Some(func) = self.get(&name) else {
            return QueryNode::Comparison(node);
        };
        let Some(q) = func.query() else {
            return QueryNode::Comparison(node);
        };
        q.expand_comparison(node)
    }

    pub fn register_plugin(&mut self, func: impl TagFunction + 'static) {
        self.register(func);
    }

    /// タグ名とDB生値から、デフォルトDisplayフォーマットで表示用文字列を返す。
    /// Display impl がなければ生値をそのまま返す。
    pub fn format_display(&self, tag_name: &str, raw: &str) -> String {
        let Some(func) = self.get(tag_name) else {
            return raw.to_string();
        };
        let Some(disp) = func.display() else {
            return raw.to_string();
        };
        let lv = raw.parse::<i64>()
            .map(LabelValue::Integer)
            .unwrap_or_else(|_| LabelValue::String(raw.to_string()));
        disp.show(&lv, disp.formats().default)
    }

    /// ディレクトリから `.wasm` プラグインをロードし登録する。
    pub fn load_from_dir(
        &mut self,
        dir: impl AsRef<std::path::Path>,
        status: &std::collections::HashMap<String, bool>,
    ) -> Result<()> {
        let dir = dir.as_ref();
        if !dir.exists() || !dir.is_dir() {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("wasm") {
                match crate::plugins::WasmPlugin::new(&path) {
                    Ok(plugin) => {
                        let adapter = plugin.into_adapter()?;
                        if !*status.get(&adapter.name).unwrap_or(&true) {
                            continue;
                        }
                        if self.get(&adapter.name).is_some() {
                            eprintln!(
                                "Warning: Plugin with name '{}' already registered. Skipping {:?}.",
                                adapter.name, path
                            );
                            continue;
                        }
                        self.register_plugin(adapter);
                    }
                    Err(e) => {
                        eprintln!("Warning: Failed to load plugin {:?}: {}", path, e);
                    }
                }
            }
        }
        Ok(())
    }

    /// ビルトインWasmプラグインをロードし登録する。
    pub fn load_builtins(
        &mut self,
        status: &std::collections::HashMap<String, bool>,
    ) -> Result<()> {
        let builtins: &[&[u8]] = &[
            include_bytes!("../plugins/mimetype_plugin.component.wasm"),
        ];
        for bytes in builtins {
            match crate::plugins::WasmPlugin::from_bytes(bytes) {
                Ok(plugin) => {
                    let adapter = plugin.into_adapter()?;
                    if !*status.get(&adapter.name).unwrap_or(&true) {
                        continue;
                    }
                    if self.get(&adapter.name).is_some() {
                        continue;
                    }
                    self.register_plugin(adapter);
                }
                Err(e) => {
                    eprintln!("Warning: Failed to load built-in plugin: {}", e);
                }
            }
        }
        Ok(())
    }
}

fn find_tagtype_name_in_comparison(node: &ComparisonNode) -> Option<String> {
    fn tagtype_name(tt: &TagType) -> Option<String> {
        if let TagType::Base(stype) = tt {
            let s: &'static str = (*stype).into();
            Some(s.to_string())
        } else {
            None
        }
    }

    fn find_in_querynode(qnode: &QueryNode) -> Option<&TagType> {
        match qnode {
            QueryNode::Projection(Operand::TypeRef(tt)) => Some(tt),
            QueryNode::And(nodes) | QueryNode::Or(nodes) => {
                nodes.iter().find_map(find_in_querynode)
            }
            QueryNode::Difference(l, _) => find_in_querynode(l),
            _ => None,
        }
    }

    fn find_in_operand(op: &Operand) -> Option<String> {
        use crate::query::ast::AggregationNode;
        match op {
            Operand::TypeRef(tt) => tagtype_name(tt),
            Operand::Aggregation(agg) => {
                let inner = match agg.as_ref() {
                    AggregationNode::Count(q) => q,
                    AggregationNode::Arithmetic { inner, .. } => inner,
                };
                find_in_querynode(inner.as_ref()).and_then(tagtype_name)
            }
            _ => None,
        }
    }

    find_in_operand(&node.first)
        .or_else(|| node.rest.iter().find_map(|(_, op)| find_in_operand(op)))
}

// ============================================================
// ttql_parse / ttql!
// ============================================================

/// TTQL クエリ文字列を QueryNode に変換する。
/// パースエラー時はパニックする（expand() 内での使用を想定）。
pub fn ttql_parse(query: &str) -> QueryNode {
    crate::parse(query).unwrap_or_else(|e| panic!("{e}"))
}

/// format!() を内包する ttql_parse 呼び出しマクロ。
/// 例: ttql!("is_dir:false & {}", tag_name)
#[macro_export]
macro_rules! ttql {
    ($($arg:tt)*) => {
        $crate::tag::ttql_parse(&::std::format!($($arg)*))
    };
}

// ============================================================
// Standard tag implementations
// ============================================================

fn normalize_path_str(s: &str) -> String {
    Path::new(s).to_slash_lossy().to_string()
}

fn get_safe_meta(path: &Path) -> Result<SafeMetadata> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(SafeMetadata::new(&m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(e.into()),
        Err(_) => Ok(SafeMetadata::recovered()),
    }
}

// --- DirectoryFn ---
pub(crate) struct DirectoryFn;
impl TagFunction for DirectoryFn {
    fn name(&self) -> &str {
        SType::Directory.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for DirectoryFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Composite
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::Filename, label.clone())),
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, true)),
        ])
    }
    fn expand_projection(&self, _tt: &TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, true)),
            QueryNode::Projection(Operand::from(TagType::Base(SType::Filename))),
        ])
    }
}

// --- FilenameFn ---
pub(crate) struct FilenameFn;
impl TagFunction for FilenameFn {
    fn name(&self) -> &str {
        SType::Filename.into()
    }
    fn index(&self) -> Option<&dyn Index> {
        Some(self)
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::FILENAME
    }
}
impl Index for FilenameFn {
    fn role(&self) -> ScanRole { ScanRole::Location }
    fn extract(&self, path: &Path) -> Result<Option<LabelValue>> {
        get_safe_meta(path)?;
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        Ok(Some(LabelValue::String(name)))
    }
    fn extract_from_path(&self, path: &Path) -> Option<LabelValue> {
        Some(LabelValue::String(
            path.file_name().unwrap_or_default().to_string_lossy().to_string(),
        ))
    }
}
impl Query for FilenameFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn storage_key(&self) -> Option<&'static str> {
        Some("name")
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::Filename, label.clone())),
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
        ])
    }
    fn expand_projection(&self, _tt: &TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
            QueryNode::Projection(Operand::from(TagType::Base(SType::Filename))),
        ])
    }
}

// --- ExtensionFn ---
pub(crate) struct ExtensionFn;
impl TagFunction for ExtensionFn {
    fn name(&self) -> &str {
        SType::Extension.into()
    }
    fn index(&self) -> Option<&dyn Index> {
        Some(self)
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Index for ExtensionFn {
    fn role(&self) -> ScanRole { ScanRole::Location }
    fn extract(&self, path: &Path) -> Result<Option<LabelValue>> {
        Ok(path.extension().map(|e| LabelValue::String(e.to_string_lossy().to_lowercase())))
    }
    fn extract_from_path(&self, path: &Path) -> Option<LabelValue> {
        path.extension().map(|e| LabelValue::String(e.to_string_lossy().to_lowercase()))
    }
}
impl Query for ExtensionFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn normalize_label(&self, label: &Label) -> Label {
        let s = label.as_str().to_lowercase();
        let s = s.trim_start_matches('.');
        Label::Extension(s.to_string())
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        let label = self.normalize_label(label);
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::Extension, label)),
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
        ])
    }
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
            QueryNode::Projection(Operand::from(tagtype.clone())),
        ])
    }
}

// --- PathFn ---
pub struct PathFn;
impl TagFunction for PathFn {
    fn name(&self) -> &str {
        SType::Path.into()
    }
    fn index(&self) -> Option<&dyn Index> {
        Some(self)
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::PATH
    }
}
impl Scan for PathFn {
    fn name() -> &'static str { "path" }
    type Value = String;
    const SCAN_ROLE: ScanRole = ScanRole::Location;
    fn scan(path: &Path, _: &SafeMetadata) -> Result<String> {
        Ok(path.to_slash_lossy().to_string())
    }
}
impl Index for PathFn {
    fn role(&self) -> ScanRole { ScanRole::Location }
    fn extract(&self, path: &Path) -> Result<Option<LabelValue>> {
        Ok(Some(LabelValue::String(path.to_slash_lossy().to_string())))
    }
    fn extract_from_path(&self, path: &Path) -> Option<LabelValue> {
        Some(LabelValue::String(path.to_slash_lossy().to_string()))
    }
}
impl Query for PathFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        let normalized = normalize_path_str(&label.as_str());
        let lv = match label.value() {
            LabelValue::Literal(_) => LabelValue::Literal(normalized),
            _ => LabelValue::String(normalized),
        };
        QueryNode::TypedTag(TypedTag::new(SType::Path, lv))
    }
}

// --- ParentDirFn ---
pub(crate) struct ParentDirFn;
impl TagFunction for ParentDirFn {
    fn name(&self) -> &str {
        SType::Parentdir.into()
    }
    fn index(&self) -> Option<&dyn Index> {
        Some(self)
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::PARENT_DIR
    }
}
impl Index for ParentDirFn {
    fn role(&self) -> ScanRole { ScanRole::Location }
    fn extract(&self, path: &Path) -> Result<Option<LabelValue>> {
        let parent = path.parent()
            .map(|p| p.to_slash_lossy().to_string())
            .unwrap_or_default();
        Ok(Some(LabelValue::String(parent)))
    }
    fn extract_from_path(&self, path: &Path) -> Option<LabelValue> {
        path.parent().map(|p| LabelValue::String(p.to_slash_lossy().to_string()))
    }
}
impl Query for ParentDirFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        let normalized = normalize_path_str(&label.as_str());
        let lv = match label.value() {
            LabelValue::Literal(_) => LabelValue::Literal(normalized),
            _ => LabelValue::String(normalized),
        };
        QueryNode::TypedTag(TypedTag::new(SType::Parentdir, lv))
    }
}

// --- StemFn ---
pub(crate) struct StemFn;
impl TagFunction for StemFn {
    fn name(&self) -> &str {
        SType::Stem.into()
    }
    fn index(&self) -> Option<&dyn Index> {
        Some(self)
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Index for StemFn {
    fn extract(&self, path: &Path) -> Result<Option<LabelValue>> {
        let stem = path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        Ok(Some(LabelValue::String(stem)))
    }
    fn target_table(&self) -> TargetTable { TargetTable::BaseTags }
}
impl Query for StemFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
}

// --- IsDirFn ---
pub(crate) struct IsDirFn;
impl TagFunction for IsDirFn {
    fn name(&self) -> &str {
        SType::IsDir.into()
    }
    fn index(&self) -> Option<&dyn Index> {
        Some(self)
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Index for IsDirFn {
    fn extract(&self, path: &Path) -> Result<Option<LabelValue>> {
        let m = get_safe_meta(path)?;
        Ok(Some(LabelValue::Boolean(m.is_dir())))
    }
    fn sql_type(&self) -> SqlType { SqlType::BOOLEAN }
    fn target_table(&self) -> TargetTable { TargetTable::BaseTags }
}
impl Query for IsDirFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::Boolean
    }
}

// --- HashFn ---
pub(crate) struct HashFn;
impl TagFunction for HashFn {
    fn name(&self) -> &str {
        SType::Hash.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for HashFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
}

// --- ContentFn ---
pub(crate) struct ContentFn;
impl TagFunction for ContentFn {
    fn name(&self) -> &str {
        SType::Content.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::CONTENT
    }
}
impl Query for ContentFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
}

// --- FileIdFn ---
pub struct FileIdFn;
impl TagFunction for FileIdFn {
    fn name(&self) -> &str {
        SType::FileId.into()
    }
    fn index(&self) -> Option<&dyn Index> {
        Some(self)
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Scan for FileIdFn {
    fn name() -> &'static str { "file_id" }
    type Value = crate::types::FileRef;
    const SCAN_ROLE: ScanRole = ScanRole::ScanId;
    fn scan(path: &Path, _: &SafeMetadata) -> Result<crate::types::FileRef> {
        crate::get_file_ref(path)
    }
}
impl Index for FileIdFn {
    fn role(&self) -> ScanRole { ScanRole::ScanId }
    fn extract(&self, path: &Path) -> Result<Option<LabelValue>> {
        let r = crate::get_file_ref(path)?;
        Ok(Some(LabelValue::String(r.to_string())))
    }
    fn extract_from_path(&self, path: &Path) -> Option<LabelValue> {
        crate::get_file_ref(path).ok().map(|r| LabelValue::String(r.to_string()))
    }
    fn sql_type(&self) -> SqlType { SqlType::UUID }
    fn target_table(&self) -> TargetTable { TargetTable::FileReferences }
    fn extract_tag_value(&self, path: &Path) -> Result<TagValue> {
        Ok(TagValue::Uuid(crate::get_file_ref(path)?))
    }
}
impl Query for FileIdFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
}

// --- NameFn ---
pub(crate) struct NameFn;
impl TagFunction for NameFn {
    fn name(&self) -> &str {
        SType::Name.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::NAME
    }
}
impl Query for NameFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::TypedTag(TypedTag::new(SType::Name, label.clone()))
    }
}

// --- SizeFn ---
pub struct SizeFn;
impl TagFunction for SizeFn {
    fn name(&self) -> &str {
        SType::Size.into()
    }
    fn index(&self) -> Option<&dyn Index> {
        Some(self)
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
    fn display(&self) -> Option<&dyn Display> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::SIZE
    }
}
impl Scan for SizeFn {
    fn name() -> &'static str { "size" }
    type Value = crate::types::FileSize;
    const SCAN_ROLE: ScanRole = ScanRole::Integrity;
    fn scan(_: &Path, meta: &SafeMetadata) -> Result<crate::types::FileSize> {
        Ok(crate::types::FileSize(meta.len()))
    }
}
impl Index for SizeFn {
    fn role(&self) -> ScanRole { ScanRole::Integrity }
    fn extract(&self, path: &Path) -> Result<Option<LabelValue>> {
        let m = get_safe_meta(path)?;
        Ok(Some(LabelValue::Integer(m.len())))
    }
    fn sql_type(&self) -> SqlType { SqlType::BIGINT }
}
impl Query for SizeFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::Integer
    }
    fn normalize_label(&self, label: &Label) -> Label {
        if let Some(bytes) = crate::util::parse_size(&label.as_str()) {
            Label::Size(bytes)
        } else {
            label.clone()
        }
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        let label = self.normalize_label(label);
        QueryNode::TypedTag(TypedTag::new(TagType::from(SType::Size), label))
    }
}
impl Display for SizeFn {
    fn formats(&self) -> DisplayFormats {
        DisplayFormats {
            default: DisplayFormat::new("si", "KB / MB"),
            options: vec![
                DisplayFormat::new("si",     "KB / MB"),
                DisplayFormat::new("binary", "KiB / MiB"),
            ],
        }
    }
    fn show(&self, value: &LabelValue, format: DisplayFormat) -> String {
        let bytes = match value {
            LabelValue::Integer(i) => *i,
            _ => return value.as_display_name(),
        };
        match format.id.as_str() {
            "binary" => format_size_binary(bytes),
            _ => format_size_si(bytes),
        }
    }
}

fn format_size_si(bytes: i64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{}B", bytes);
    }
    let mut val = bytes as f64;
    let mut idx = 0;
    while val >= 1024.0 && idx < UNITS.len() - 1 {
        val /= 1024.0;
        idx += 1;
    }
    format!("{:.1}{}", val, UNITS[idx])
}

fn format_size_binary(bytes: i64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{}B", bytes);
    }
    let mut val = bytes as f64;
    let mut idx = 0;
    while val >= 1024.0 && idx < UNITS.len() - 1 {
        val /= 1024.0;
        idx += 1;
    }
    format!("{:.1}{}", val, UNITS[idx])
}

// ---------------------------------------------------------------------------
// DateTime::to_range — tag.rs で impl（BasicOp との循環依存を避けるため）
// ---------------------------------------------------------------------------
impl crate::types::DateTime {
    /// オペレータに応じた (BasicOp, timestamp) 条件リストを返す。
    /// Eq  → [(Ge, floor_ts), (Le, ceil_ts)]
    /// Gt  → [(Gt, ceil_ts)]
    /// Ge  → [(Ge, floor_ts)]
    /// Lt  → [(Lt, floor_ts)]
    /// Le  → [(Le, ceil_ts)]
    /// Ne  → [(Lt, floor_ts), (Gt, ceil_ts)]
    pub fn to_range(
        &self,
        op: crate::query::ast::BasicOp,
    ) -> Vec<(crate::query::ast::BasicOp, i64)> {
        use crate::query::ast::BasicOp;
        use crate::types::DateTime;
        use chrono::{Local, TimeZone};

        let to_ts = |ndt: chrono::NaiveDateTime| -> i64 {
            Local
                .from_local_datetime(&ndt)
                .earliest()
                .map(|t| t.timestamp())
                .unwrap_or(0)
        };

        if let DateTime::Instant(instant) = self {
            return vec![(op, instant.timestamp())];
        }

        let floor_ts = self.floor().map(to_ts).unwrap_or(0);
        let ceil_ts = self.ceiling().map(to_ts).unwrap_or(0);

        match op {
            BasicOp::Eq => vec![(BasicOp::Ge, floor_ts), (BasicOp::Le, ceil_ts)],
            BasicOp::Gt => vec![(BasicOp::Gt, ceil_ts)],
            BasicOp::Ge => vec![(BasicOp::Ge, floor_ts)],
            BasicOp::Lt => vec![(BasicOp::Lt, floor_ts)],
            BasicOp::Le => vec![(BasicOp::Le, ceil_ts)],
            BasicOp::Ne => vec![(BasicOp::Lt, floor_ts), (BasicOp::Gt, ceil_ts)],
        }
    }
}

// --- MtimeFn ---
pub struct MtimeFn;
impl TagFunction for MtimeFn {
    fn name(&self) -> &str {
        SType::Mtime.into()
    }
    fn index(&self) -> Option<&dyn Index> {
        Some(self)
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
    fn display(&self) -> Option<&dyn Display> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::MTIME
    }
}
impl Scan for MtimeFn {
    fn name() -> &'static str { "mtime" }
    type Value = crate::types::FileTimestamp;
    const SCAN_ROLE: ScanRole = ScanRole::Integrity;
    fn scan(_: &Path, meta: &SafeMetadata) -> Result<crate::types::FileTimestamp> {
        Ok(crate::types::FileTimestamp(meta.modified()))
    }
}
impl Index for MtimeFn {
    fn role(&self) -> ScanRole { ScanRole::Integrity }
    fn extract(&self, path: &Path) -> Result<Option<LabelValue>> {
        let m = get_safe_meta(path)?;
        Ok(Some(LabelValue::Integer(m.modified())))
    }
    fn sql_type(&self) -> SqlType { SqlType::BIGINT }
}
fn mtime_range_op(first: &Operand, op: ComparisonOp, range: DatetimeRange) -> Vec<QueryNode> {
    let basic_op = match op {
        ComparisonOp::Label(b) | ComparisonOp::Scalar(b) => b,
    };
    let make_op = |b: BasicOp| match op {
        ComparisonOp::Label(_) => ComparisonOp::Label(b),
        ComparisonOp::Scalar(_) => ComparisonOp::Scalar(b),
    };
    match basic_op {
        BasicOp::Eq => vec![
            QueryNode::Comparison(ComparisonNode {
                first: first.clone(),
                rest: vec![(make_op(BasicOp::Ge), Operand::Literal(Label::Mtime(range.start)))],
            }),
            QueryNode::Comparison(ComparisonNode {
                first: first.clone(),
                rest: vec![(make_op(BasicOp::Le), Operand::Literal(Label::Mtime(range.end)))],
            }),
        ],
        BasicOp::Ne => vec![QueryNode::Or(vec![
            QueryNode::Comparison(ComparisonNode {
                first: first.clone(),
                rest: vec![(make_op(BasicOp::Lt), Operand::Literal(Label::Mtime(range.start)))],
            }),
            QueryNode::Comparison(ComparisonNode {
                first: first.clone(),
                rest: vec![(make_op(BasicOp::Gt), Operand::Literal(Label::Mtime(range.end)))],
            }),
        ])],
        BasicOp::Gt => vec![QueryNode::Comparison(ComparisonNode {
            first: first.clone(),
            rest: vec![(make_op(BasicOp::Gt), Operand::Literal(Label::Mtime(range.end)))],
        })],
        BasicOp::Ge => vec![QueryNode::Comparison(ComparisonNode {
            first: first.clone(),
            rest: vec![(make_op(BasicOp::Ge), Operand::Literal(Label::Mtime(range.start)))],
        })],
        BasicOp::Lt => vec![QueryNode::Comparison(ComparisonNode {
            first: first.clone(),
            rest: vec![(make_op(BasicOp::Lt), Operand::Literal(Label::Mtime(range.start)))],
        })],
        BasicOp::Le => vec![QueryNode::Comparison(ComparisonNode {
            first: first.clone(),
            rest: vec![(make_op(BasicOp::Le), Operand::Literal(Label::Mtime(range.end)))],
        })],
    }
}
impl Display for MtimeFn {
    fn formats(&self) -> DisplayFormats {
        DisplayFormats {
            default: DisplayFormat::new("human", "Human Readable"),
            options: vec![
                DisplayFormat::new("human",    "Human Readable"),
                DisplayFormat::new("relative", "Relative"),
                DisplayFormat::new("iso",      "ISO 8601"),
                DisplayFormat::new("raw",      "Raw"),
            ],
        }
    }
    fn show(&self, value: &LabelValue, format: DisplayFormat) -> String {
        let secs = match value {
            LabelValue::Integer(i) => *i,
            _ => return value.as_display_name(),
        };
        match format.id.as_str() {
            "raw" => secs.to_string(),
            "iso" => chrono::DateTime::from_timestamp(secs, 0)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_else(|| secs.to_string()),
            "relative" => format_mtime_relative(secs),
            _ => chrono::Local
                .timestamp_opt(secs, 0)
                .single()
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| secs.to_string()),
        }
    }
}

fn format_mtime_relative(secs: i64) -> String {
    let diff = chrono::Local::now().timestamp() - secs;
    let abs = diff.unsigned_abs();
    let (val, unit) = if abs < 60 {
        (abs, "second")
    } else if abs < 3600 {
        (abs / 60, "minute")
    } else if abs < 86400 {
        (abs / 3600, "hour")
    } else if abs < 86400 * 30 {
        (abs / 86400, "day")
    } else if abs < 86400 * 365 {
        (abs / (86400 * 30), "month")
    } else {
        (abs / (86400 * 365), "year")
    };
    let s = if val == 1 { "" } else { "s" };
    if diff >= 0 {
        format!("{} {}{} ago", val, unit, s)
    } else {
        format!("in {} {}{}", val, unit, s)
    }
}

/// 構造化日付文字列（YYYY-MM-DD / YYYY-MM）のみを DateTime に変換する。
/// 4桁年単体・自然言語は対象外（normalize_label 用）。
fn parse_date_literal(s: &str) -> Option<crate::types::DateTime> {
    use chrono::NaiveDate;
    let s = s.trim();
    let parts: Vec<&str> = s
        .split(|c| c == '/' || c == '-')
        .filter(|p| !p.is_empty())
        .collect();
    // YYYY-MM-DD / YYYY/MM/DD
    if parts.len() == 3
        && parts[0].len() == 4
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
    {
        let y: i32 = parts[0].parse().ok()?;
        let m: u32 = parts[1].parse().ok()?;
        let d: u32 = parts[2].parse().ok()?;
        return NaiveDate::from_ymd_opt(y, m, d).map(crate::types::DateTime::Date);
    }
    // YYYY-MM / YYYY/MM
    if parts.len() == 2
        && parts[0].len() == 4
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
    {
        let y: i32 = parts[0].parse().ok()?;
        let m: u32 = parts[1].parse().ok()?;
        NaiveDate::from_ymd_opt(y, m, 1)?;
        return Some(crate::types::DateTime::YearMonth { year: y, month: m });
    }
    None
}

impl Query for MtimeFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::Integer
    }
    fn normalize_label(&self, label: &Label) -> Label {
        if let LabelValue::String(s) = label.value() {
            if let Some(dt) = parse_date_literal(&s) {
                return Label::Date(dt);
            }
        }
        label.clone()
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        if let LabelValue::Date(dt) = label.value() {
            let first = Operand::TypeRef(SType::Mtime.into());
            let nodes: Vec<_> = dt.to_range(BasicOp::Eq).into_iter().map(|(b, ts)| {
                QueryNode::Comparison(ComparisonNode {
                    first: first.clone(),
                    rest: vec![(ComparisonOp::Label(b), Operand::Literal(Label::Mtime(ts)))],
                })
            }).collect();
            return match nodes.len() {
                1 => nodes.into_iter().next().unwrap(),
                _ => QueryNode::And(nodes),
            };
        }
        if let Some(range) = crate::util::parse_datetime(&label.as_str()) {
            if range.start == range.end {
                QueryNode::TypedTag(TypedTag::new(SType::Mtime, range.start))
            } else {
                QueryNode::And(vec![
                    QueryNode::Comparison(ComparisonNode {
                        first: Operand::TypeRef(SType::Mtime.into()),
                        rest: vec![(
                            ComparisonOp::Label(BasicOp::Ge),
                            Operand::Literal(Label::Mtime(range.start)),
                        )],
                    }),
                    QueryNode::Comparison(ComparisonNode {
                        first: Operand::TypeRef(SType::Mtime.into()),
                        rest: vec![(
                            ComparisonOp::Label(BasicOp::Le),
                            Operand::Literal(Label::Mtime(range.end)),
                        )],
                    }),
                ])
            }
        } else {
            QueryNode::TypedTag(TypedTag::new(SType::Mtime, label.clone()))
        }
    }
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }
    fn expand_comparison(&self, node: ComparisonNode) -> QueryNode {
        let mut first = node.first.clone();
        let rest = node.rest;

        if let Operand::Literal(label) = &mut first {
            if let Some(range) = parse_datetime(&label.as_str()) {
                *label = Label::Mtime(range.start);
            }
        }

        let mut conditions = Vec::new();
        for (op, rhs) in rest {
            if let Operand::Literal(label) = &rhs {
                let basic_op = match op {
                    ComparisonOp::Label(b) | ComparisonOp::Scalar(b) => b,
                };
                let make_op = |b: BasicOp| match op {
                    ComparisonOp::Label(_) => ComparisonOp::Label(b),
                    ComparisonOp::Scalar(_) => ComparisonOp::Scalar(b),
                };
                if let LabelValue::Date(dt) = label.value() {
                    let nodes = dt.to_range(basic_op).into_iter().map(|(b, ts)| {
                        QueryNode::Comparison(ComparisonNode {
                            first: first.clone(),
                            rest: vec![(make_op(b), Operand::Literal(Label::Mtime(ts)))],
                        })
                    });
                    conditions.extend(nodes);
                    continue;
                }
                if let Some(range) = parse_datetime(&label.as_str()) {
                    conditions.extend(mtime_range_op(&first, op, range));
                    continue;
                }
            }
            conditions.push(QueryNode::Comparison(ComparisonNode {
                first: first.clone(),
                rest: vec![(op, rhs)],
            }));
        }

        match conditions.len() {
            1 => conditions.remove(0),
            _ => QueryNode::And(conditions),
        }
    }
}

// --- ItemIdFn ---
pub(crate) struct ItemIdFn;
impl TagFunction for ItemIdFn {
    fn name(&self) -> &str {
        SType::ItemId.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for ItemIdFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::Integer
    }
    /// ローカル形式（"User(0)"/"Sys(10)"）を raw i64 へ正規化する。
    /// 生整数文字列や Literal 以外はそのまま返す。
    fn normalize_label(&self, label: &Label) -> Label {
        let s = match label.value() {
            LabelValue::String(s) | LabelValue::Literal(s) => s,
            _ => return label.clone(),
        };
        match crate::db::identifier::parse(&s) {
            Ok(id) => Label::ItemId(id),
            Err(_) => label.clone(),
        }
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::ItemId,
            label: label.clone(),
        }
    }
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }
}

// --- ItemKindFn ---
pub(crate) struct ItemKindFn;
impl TagFunction for ItemKindFn {
    fn name(&self) -> &str {
        SType::ItemKind.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::ITEM_KIND
    }
}
impl Query for ItemKindFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::ItemKind,
            label: label.clone(),
        }
    }
}

// --- RankFn ---
pub(crate) struct RankFn;
impl TagFunction for RankFn {
    fn name(&self) -> &str {
        SType::Rank.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for RankFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::Integer
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::TypedTag(TypedTag::new(SType::Rank, label.clone()))
    }
}

// --- OriginFn ---
pub(crate) struct OriginFn;
impl TagFunction for OriginFn {
    fn name(&self) -> &str {
        SType::Origin.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for OriginFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::Origin,
            label: label.clone(),
        }
    }
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }
}

// --- TypeFn ---
pub(crate) struct TypeFn;
impl TagFunction for TypeFn {
    fn name(&self) -> &str {
        SType::Type.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for TypeFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::Type,
            label: label.clone(),
        }
    }
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }
}

// --- LabelFn ---
pub(crate) struct LabelFn;
impl TagFunction for LabelFn {
    fn name(&self) -> &str {
        SType::Label.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for LabelFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::Label,
            label: label.clone(),
        }
    }
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }
}

// --- TypedTagFn ---
pub(crate) struct TypedTagFn;
impl TagFunction for TypedTagFn {
    fn name(&self) -> &str {
        SType::TypedTag.into()
    }
    fn query(&self) -> Option<&dyn Query> {
        Some(self)
    }
}
impl Query for TypedTagFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
    ) -> QueryNode {
        QueryNode::ColumnMatch {
            tag: SType::TypedTag,
            label: label.clone(),
        }
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    struct SimpleTag;
    impl TagFunction for SimpleTag {
        fn name(&self) -> &str {
            "simple"
        }
    }

    struct IndexedTag;
    impl TagFunction for IndexedTag {
        fn name(&self) -> &str {
            "indexed"
        }
        fn index(&self) -> Option<&dyn Index> {
            Some(self)
        }
        fn default_rank(&self) -> Rank {
            42
        }
    }
    impl Index for IndexedTag {
        fn extract(&self, _path: &Path) -> Result<Option<LabelValue>> {
            Ok(Some(LabelValue::String("value".to_string())))
        }
        fn role(&self) -> ScanRole {
            ScanRole::Location
        }
    }

    struct QueryTag;
    impl TagFunction for QueryTag {
        fn name(&self) -> &str {
            "qtest"
        }
        fn query(&self) -> Option<&dyn Query> {
            Some(self)
        }
    }
    impl Query for QueryTag {
        fn normalize_label(&self, label: &Label) -> Label {
            Label::Other(
                TagType::from("qtest"),
                LabelValue::String(label.as_str().to_uppercase()),
            )
        }
    }

    // --- TagRegistry ---

    #[test]
    fn test_registry_register_and_get() {
        let mut reg = TagRegistry::new();
        reg.register(SimpleTag);
        assert!(reg.get("simple").is_some());
        assert!(reg.get("unknown").is_none());
    }

    #[test]
    fn test_registry_overwrite() {
        let mut reg = TagRegistry::new();
        reg.register(SimpleTag);
        reg.register(SimpleTag);
        assert!(reg.get("simple").is_some());
    }

    #[test]
    fn test_registry_get_arc() {
        let mut reg = TagRegistry::new();
        reg.register(SimpleTag);
        let arc = reg.get_arc("simple");
        assert!(arc.is_some());
    }

    #[test]
    fn test_registry_iter_arcs() {
        let mut reg = TagRegistry::new();
        reg.register(SimpleTag);
        let count = reg.iter_arcs().count();
        assert_eq!(count, 1);
    }

    // --- TagFunction defaults ---

    #[test]
    fn test_tag_function_defaults() {
        assert!(SimpleTag.index().is_none());
        assert!(SimpleTag.query().is_none());
        assert!(SimpleTag.display().is_none());
    }

    // --- Index ---

    #[test]
    fn test_index_role_and_rank() {
        let f = IndexedTag;
        assert_eq!(f.index().unwrap().role(), ScanRole::Location);
        assert_eq!(f.default_rank(), 42);
    }

    #[test]
    fn test_index_extract() {
        let result = IndexedTag.extract(Path::new("/dummy")).unwrap();
        assert_eq!(result, Some(LabelValue::String("value".to_string())));
    }

    #[test]
    fn test_index_default_extract_from_path() {
        let result = IndexedTag.extract_from_path(Path::new("/dummy"));
        assert!(result.is_none());
    }

    // --- Query ---

    #[test]
    fn test_query_normalize_label() {
        let q = QueryTag;
        let qry = q.query().unwrap();
        let label = Label::Other(
            TagType::from("qtest"),
            LabelValue::String("hello".to_string()),
        );
        let normalized = qry.normalize_label(&label);
        assert_eq!(normalized.as_str(), "HELLO");
    }

    // --- Phase 3: normalize_label ---

    #[test]
    fn test_size_normalize_label_string_converts() {
        let q = SizeFn;
        let label = Label::Other(
            TagType::from("size"),
            LabelValue::String("1MB".to_string()),
        );
        let normalized = q.query().unwrap().normalize_label(&label);
        assert_eq!(normalized, Label::Size(1_048_576));
    }

    #[test]
    fn test_registry_normalize_label_literal_skipped() {
        // Literal ("quoted") must bypass normalize_label entirely at registry level
        let reg = TagRegistry::with_standard();
        let label = Label::Other(
            TagType::from("size"),
            LabelValue::Literal("1MB".to_string()),
        );
        let normalized = reg.normalize_label("size", &label);
        assert_eq!(normalized, label);
    }

    #[test]
    fn test_mtime_normalize_label_date_string() {
        let q = MtimeFn;
        let label = Label::Other(
            TagType::from("mtime"),
            LabelValue::String("2026-02-01".to_string()),
        );
        let normalized = q.query().unwrap().normalize_label(&label);
        match normalized {
            Label::Date(crate::types::DateTime::Date(d)) => {
                assert_eq!(d, chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap());
            }
            other => panic!("expected Label::Date(Date(_)), got {:?}", other),
        }
    }

    #[test]
    fn test_mtime_normalize_label_year_string() {
        // 4桁年単体は normalize_label の対象外（mtime expand_comparison で処理）
        let q = MtimeFn;
        let label = Label::Other(
            TagType::from("mtime"),
            LabelValue::String("2026".to_string()),
        );
        let normalized = q.query().unwrap().normalize_label(&label);
        assert_eq!(normalized, label, "bare YYYY should be unchanged by normalize_label");
    }

    #[test]
    fn test_mtime_normalize_label_non_date_unchanged() {
        let q = MtimeFn;
        let label = Label::Other(
            TagType::from("mtime"),
            LabelValue::String("not-a-date".to_string()),
        );
        let normalized = q.query().unwrap().normalize_label(&label);
        assert_eq!(normalized, label);
    }

    #[test]
    fn test_registry_normalize_label_size() {
        let reg = TagRegistry::with_standard();
        let label = Label::Other(
            TagType::from("size"),
            LabelValue::String("2MB".to_string()),
        );
        let normalized = reg.normalize_label("size", &label);
        assert_eq!(normalized, Label::Size(2_097_152));
    }

    #[test]
    fn test_registry_normalize_label_mtime() {
        let reg = TagRegistry::with_standard();
        let label = Label::Other(
            TagType::from("mtime"),
            LabelValue::String("2026-01-01".to_string()),
        );
        let normalized = reg.normalize_label("mtime", &label);
        match normalized {
            Label::Date(crate::types::DateTime::Date(d)) => {
                assert_eq!(d, chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
            }
            other => panic!("expected Label::Date(Date(_)), got {:?}", other),
        }
    }

    #[test]
    fn test_registry_normalize_label_unknown_unchanged() {
        let reg = TagRegistry::with_standard();
        let label = Label::Other(
            TagType::from("custom"),
            LabelValue::String("hello".to_string()),
        );
        let normalized = reg.normalize_label("custom", &label);
        assert_eq!(normalized, label);
    }

    // --- DisplayFormat / DisplayFormats ---

    #[test]
    fn test_display_format_default() {
        let fmt = DisplayFormat::default();
        assert_eq!(fmt.id, "raw");
        assert_eq!(fmt.label, "Raw");
    }

    #[test]
    fn test_display_formats_default() {
        let fmts = DisplayFormats::default();
        assert_eq!(fmts.default.id, "raw");
        assert!(fmts.options.is_empty());
    }

    // --- ttql_parse / ttql! ---

    #[test]
    fn test_ttql_parse_basic() {
        let node = ttql_parse("extension:rs");
        match node {
            QueryNode::TypedTag(_) | QueryNode::And(_) => {}
            other => panic!("Unexpected QueryNode: {:?}", other),
        }
    }

    #[test]
    fn test_ttql_macro() {
        let tag = "extension";
        let node = crate::ttql!("{tag}:rs");
        match node {
            QueryNode::TypedTag(_) | QueryNode::And(_) => {}
            other => panic!("Unexpected QueryNode: {:?}", other),
        }
    }

    // --- Phase 2: DateTime::to_range ---

    #[test]
    fn test_datetime_to_range_date_eq() {
        use chrono::NaiveDate;
        use crate::query::ast::BasicOp;
        let d = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let dt = crate::types::DateTime::Date(d);
        let ranges = dt.to_range(BasicOp::Eq);
        assert_eq!(ranges.len(), 2);
        let ops: Vec<BasicOp> = ranges.iter().map(|(op, _)| *op).collect();
        assert!(ops.contains(&BasicOp::Ge));
        assert!(ops.contains(&BasicOp::Le));
        let ge_ts = ranges.iter().find(|(op, _)| *op == BasicOp::Ge).unwrap().1;
        let le_ts = ranges.iter().find(|(op, _)| *op == BasicOp::Le).unwrap().1;
        assert!(ge_ts < le_ts);
    }

    #[test]
    fn test_datetime_to_range_date_gt() {
        use chrono::NaiveDate;
        use crate::query::ast::BasicOp;
        let d = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let dt = crate::types::DateTime::Date(d);
        let ranges = dt.to_range(BasicOp::Gt);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].0, BasicOp::Gt);
    }

    #[test]
    fn test_datetime_to_range_year_eq() {
        use crate::query::ast::BasicOp;
        let dt = crate::types::DateTime::Year(2026);
        let ranges = dt.to_range(BasicOp::Eq);
        assert_eq!(ranges.len(), 2);
        let ge_ts = ranges.iter().find(|(op, _)| *op == BasicOp::Ge).unwrap().1;
        let le_ts = ranges.iter().find(|(op, _)| *op == BasicOp::Le).unwrap().1;
        assert!(ge_ts < le_ts);
    }

    #[test]
    fn test_datetime_to_range_instant_any_op() {
        use chrono::{Local, TimeZone};
        use crate::query::ast::BasicOp;
        let local_dt = Local.with_ymd_and_hms(2026, 2, 1, 12, 0, 0).unwrap();
        let ts = local_dt.timestamp();
        let dt = crate::types::DateTime::Instant(local_dt);
        let ranges = dt.to_range(BasicOp::Gt);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].0, BasicOp::Gt);
        assert_eq!(ranges[0].1, ts);
    }

    // --- Phase 6: MtimeFn::expand ---

    #[test]
    fn test_mtime_expand_date_label_becomes_range() {
        use chrono::NaiveDate;
        use crate::query::ast::{BasicOp, ComparisonOp};
        use crate::types::{DateTime, Label, SType, TagType, TypedTag};
        let date = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let label = Label::Date(DateTime::Date(date));
        let tag_type = TagType::from(SType::Mtime);
        let typed_tag = TypedTag::new(SType::Mtime, label.clone());
        let result = MtimeFn.query().unwrap().expand(&tag_type, &label, &typed_tag);
        // Date Eq → And([Ge(floor), Le(ceil)])
        let QueryNode::And(nodes) = result else { panic!("expected And, got {:?}", result) };
        assert_eq!(nodes.len(), 2);
        let ops: Vec<_> = nodes.iter().map(|n| {
            if let QueryNode::Comparison(c) = n { c.rest[0].0.clone() } else { panic!() }
        }).collect();
        assert!(ops.contains(&ComparisonOp::Label(BasicOp::Ge)));
        assert!(ops.contains(&ComparisonOp::Label(BasicOp::Le)));
    }

    // --- Phase 5: MtimeFn::expand_comparison ---

    #[test]
    fn test_mtime_expand_comparison_date_label_gt() {
        use chrono::NaiveDate;
        use crate::query::ast::{BasicOp, ComparisonNode, ComparisonOp, Operand};
        use crate::types::{DateTime, Label, LabelValue, SType, TagType};
        let date = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let dt = DateTime::Date(date);
        let node = ComparisonNode {
            first: Operand::TypeRef(TagType::from(SType::Mtime)),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Gt),
                Operand::Literal(Label::Date(dt.clone())),
            )],
        };
        let result = MtimeFn.query().unwrap().expand_comparison(node);
        // Gt → ceil_ts で単一条件
        let QueryNode::Comparison(c) = result else { panic!("expected Comparison, got {:?}", result) };
        let (op, rhs) = &c.rest[0];
        assert_eq!(*op, ComparisonOp::Label(BasicOp::Gt));
        let Operand::Literal(label) = rhs else { panic!() };
        assert!(matches!(label.value(), LabelValue::Integer(_)), "should be Mtime(i64)");
    }

    #[test]
    fn test_mtime_expand_comparison_date_label_eq_expands_to_and() {
        use chrono::NaiveDate;
        use crate::query::ast::{BasicOp, ComparisonNode, ComparisonOp, Operand};
        use crate::types::{DateTime, Label, SType, TagType};
        let date = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let dt = DateTime::Date(date);
        let node = ComparisonNode {
            first: Operand::TypeRef(TagType::from(SType::Mtime)),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Eq),
                Operand::Literal(Label::Date(dt)),
            )],
        };
        let result = MtimeFn.query().unwrap().expand_comparison(node);
        // Eq → And([Ge, Le])
        assert!(matches!(result, QueryNode::And(_)), "expected And for Eq, got {:?}", result);
    }
}
