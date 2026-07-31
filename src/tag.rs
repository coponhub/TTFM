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

use crate::db::{BiticalType, TargetTable};
use crate::query::ast::{
    BasicOp, Candidate, ComparisonNode, ComparisonOp, DefinitionRef, Operand,
    QueryNode,
};
use crate::response::Item;
use crate::types::{
    Bitical, BiticalAssociate, Biticals, DateTime, ItemId, ItemKind, Label,
    LargeOrigin, Origin, Rank, SType, TagType, TypedTag,
};
use crate::util::{DotOk, SafeMetadata};
use anyhow::Result;
use chrono::TimeZone as _;
use indexmap::IndexMap;
use path_slash::PathExt as _;
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
    pub bitical_type: BiticalType,
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
        Self {
            value: self.value.clone(),
        }
    }
}

/// スキャン時の型付きデータ取得トレイト。
/// associated type を持つため object-safe ではなく、`define_scan_entry!` マクロ経由でのみ使用する。
pub trait Scan {
    fn name() -> &'static str;
    type Value: BiticalAssociate + Debug + PartialEq + Clone;
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
        Self {
            id: id.into(),
            label: label.into(),
        }
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

    fn extract(&self, path: &Path) -> Result<Option<Bitical>>;

    /// パスのみから値を生成できる場合に返す（移動検知など）。
    fn extract_from_path(&self, _path: &Path) -> Option<Bitical> {
        None
    }

    /// DB カラムの SQL 型。
    fn sql_type(&self) -> BiticalType {
        BiticalType::String
    }

    /// 書き込み先テーブル。
    fn target_table(&self) -> TargetTable {
        TargetTable::Locations
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
    /// read 時の値解決。デフォルトは passthrough（空＝reduce 無し）。
    /// type 固有の選別（name の user 由来優先など）を宣言する type のみ上書きする。
    fn read(&self) -> crate::query::lens_reader::ReadResolution {
        crate::query::lens_reader::ReadResolution::default()
    }

    /// タグを QueryNode へ展開する。デフォルトはそのまま TypedTag。
    /// 値がその型として解釈できない場合はエラーを返す。
    fn expand(
        &self,
        tagtype: &TagType,
        label: &Label,
        tag: &TypedTag,
        schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        if let Some(kind) = self.item_kind() {
            // 定義アイテム参照: tag:/type: などは item_references の定義行を参照する。
            // value は自身の型を付与した representative（未登録時の Volatile 用）。
            QueryNode::DefinitionRef(DefinitionRef {
                kind,
                value: label.rekey(tagtype.clone(), label.value()),
                candidates: Vec::new(),
                origins: Vec::new(),
                reserved: schema
                    .iter_all_for_rank()
                    .into_iter()
                    .map(|(t, _, _)| t.as_str().to_string())
                    .collect(),
                recorded: true,
            })
            .to_ok()
        } else {
            QueryNode::TypedTag(tag.clone()).to_ok()
        }
    }

    /// この TagFunction が「定義アイテム」を表すなら、その ItemKind を返す。
    /// `tag:`→Tag, `type:`→Type のように、item_references の定義行を参照する種別を宣言する。
    /// None（既定）の場合は通常のタグとして扱う。
    fn item_kind(&self) -> Option<ItemKind> {
        None
    }

    /// Projection（type:形式）を QueryNode へ展開する。
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }

    /// ラベル値を正規化する（例: "1MB" → 1048576）。
    /// 値がその型として解釈できない場合はエラーを返す。
    fn normalize_label(&self, label: &Label) -> Result<Label> {
        label.clone().to_ok()
    }

    /// タグのストレージ上の役割。
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Basic
    }

    /// Label の論理型。
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }

    /// DB の `type` カラムに格納されるキー。None = タグ名をそのまま使用。
    fn storage_key(&self) -> Option<&'static str> {
        None
    }

    /// 比較ノードを展開する。デフォルトはリテラルを normalize_label して ComparisonNode を返す。
    /// 値がその型として解釈できない場合はエラーを返す。
    fn expand_comparison(&self, node: ComparisonNode) -> Result<QueryNode> {
        let mut node = node;
        if let Operand::Literal(label) = &mut node.first {
            *label = self.normalize_label(label)?;
        }
        for (_, rhs) in &mut node.rest {
            if let Operand::Literal(label) = rhs {
                *label = self.normalize_label(label)?;
            }
        }
        QueryNode::Comparison(node).to_ok()
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

    /// この型のアイテムを表示するときの優先 Order（複数キー可）。
    /// 既定は空 = 優先なし（検索側の既定の並びに従う）。
    fn preferred_order(&self) -> Vec<crate::types::Order> {
        Vec::new()
    }

    fn show(&self, value: &Bitical, format: DisplayFormat) -> String;
}

// ============================================================
// Edit trait
// ============================================================

pub enum EditStrategy {
    Append,
    Replace,
    RemoveOnly,
    ModifyInjection,
    Relocate,
    SetFileAttr,
}

pub trait Edit: Send + Sync {
    fn strategy(&self) -> EditStrategy;
    fn validate(&self, new: &Label) -> Result<Label> {
        Ok(new.clone())
    }
    fn inject(&self, _item: &Item) -> Option<Label> {
        None
    }
}

// ============================================================
// TagFunction trait
// ============================================================

/// タグの統合定義単位。Index・Query・Display・Edit の4コンポーネントを束ねる。
pub trait TagFunction: Send + Sync {
    fn name(&self) -> &str;

    fn index(&self) -> Option<&dyn Index> {
        None
    }

    /// クエリ展開・正規化のロジック。宣言しない場合は Query の全デフォルト実装が使われる。
    fn query(&self) -> &dyn Query {
        struct DefaultQuery;
        impl Query for DefaultQuery {}
        static DEFAULT: DefaultQuery = DefaultQuery;
        &DEFAULT
    }

    fn display(&self) -> Option<&dyn Display> {
        None
    }

    fn edit(&self) -> Option<&dyn Edit> {
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
    /// `register()` 呼び出し順に採番される固定オフセット（Sys id の基）。
    /// `register_plugin()` では採番しない（プラグインは Sys 区画対象外）。
    builtin_offsets: std::collections::HashMap<String, u32>,
    next_builtin_offset: u32,
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
            builtin_offsets: std::collections::HashMap::new(),
            next_builtin_offset: 0,
        }
    }

    pub fn register(&mut self, func: impl TagFunction + 'static) {
        let arc = Arc::new(func);
        let name = arc.name().to_string();
        if !self.functions.contains_key(&name) {
            self.builtin_offsets
                .insert(name.clone(), self.next_builtin_offset);
            self.next_builtin_offset += 1;
        }
        self.functions.insert(name, arc);
    }

    /// `register()` で登録された組み込み関数の列挙順オフセット。
    /// `register_plugin()` 経由の登録には存在しない（Sys 区画対象外）。
    pub fn builtin_offset(&self, name: &str) -> Option<u32> {
        self.builtin_offsets.get(name).copied()
    }

    /// 固定 Sys id のオフセットから組み込み型名を逆引きする（builtin_offset の逆写像）。
    pub fn builtin_name_for_offset(&self, offset: u32) -> Option<&str> {
        self.builtin_offsets
            .iter()
            .find(|(_, &v)| v == offset)
            .map(|(k, _)| k.as_str())
    }

    pub fn get(&self, name: &str) -> Option<&dyn TagFunction> {
        self.functions.get(name).map(|f| f.as_ref())
    }

    pub fn get_arc(&self, name: &str) -> Option<Arc<dyn TagFunction>> {
        self.functions.get(name).cloned()
    }

    pub fn iter_arcs(&self) -> impl Iterator<Item = Arc<dyn TagFunction>> + '_ {
        self.functions.values().cloned()
    }

    pub fn iter_all_for_rank(&self) -> impl Iterator<Item = (&str, Rank)> + '_ {
        self.functions
            .values()
            .map(|f| (f.name(), f.default_rank()))
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

    pub fn get_all_columns(&self) -> Vec<crate::db::ColumnDef> {
        self.functions
            .values()
            .filter_map(|f| {
                f.index().map(|idx| crate::db::ColumnDef {
                    name: f.name().to_string(),
                    bitical_type: idx.sql_type(),
                    target_table: idx.target_table(),
                })
            })
            .collect()
    }

    pub fn process_file(&self, path: &Path) -> Result<Biticals> {
        self.functions
            .values()
            .filter_map(|f| f.index())
            .map(|idx| idx.extract(path))
            .collect()
    }

    pub fn register_plugin(&mut self, func: impl TagFunction + 'static) {
        let arc = Arc::new(func);
        self.functions.insert(arc.name().to_string(), arc);
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
        let lv = raw
            .parse::<i64>()
            .map(Bitical::Integer)
            .unwrap_or_else(|_| Bitical::String(raw.to_string()));
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
                        eprintln!(
                            "Warning: Failed to load plugin {:?}: {}",
                            path, e
                        );
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
        let builtins: &[&[u8]] =
            &[include_bytes!("../plugins/mimetype_plugin.component.wasm")];
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

// ============================================================
// ttql_parse / ttql!
// ============================================================

/// TTQL クエリ文字列を QueryNode に変換する。
/// パースエラー時はパニックする（expand() 内での使用を想定）。
pub fn ttql_parse(query: &str) -> QueryNode {
    crate::query::parser::parse_nowarn(query).unwrap_or_else(|e| panic!("{e}"))
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
    fn query(&self) -> &dyn Query {
        self
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
        _schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::retag(SType::Filename, label)),
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, true)),
        ]).to_ok()
    }
    fn expand_projection(&self, _tt: &TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, true)),
            QueryNode::Projection(Operand::from(TagType::Base(
                SType::Filename,
            ))),
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
    fn query(&self) -> &dyn Query {
        self
    }
    fn edit(&self) -> Option<&dyn Edit> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::FILENAME
    }
}
impl Edit for FilenameFn {
    fn strategy(&self) -> EditStrategy {
        EditStrategy::Relocate
    }
}
impl Index for FilenameFn {
    fn role(&self) -> ScanRole {
        ScanRole::Location
    }
    fn extract(&self, path: &Path) -> Result<Option<Bitical>> {
        get_safe_meta(path)?;
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        Ok(Some(Bitical::String(name)))
    }
    fn extract_from_path(&self, path: &Path) -> Option<Bitical> {
        Some(Bitical::String(
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
        ))
    }
}
impl Query for FilenameFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
        _schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::retag(SType::Filename, label)),
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
        ]).to_ok()
    }
    fn expand_projection(&self, _tt: &TagType) -> QueryNode {
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
            QueryNode::Projection(Operand::from(TagType::Base(
                SType::Filename,
            ))),
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
    fn query(&self) -> &dyn Query {
        self
    }
    fn edit(&self) -> Option<&dyn Edit> {
        Some(self)
    }
}
impl Edit for ExtensionFn {
    fn strategy(&self) -> EditStrategy {
        EditStrategy::Relocate
    }
}
impl Index for ExtensionFn {
    fn role(&self) -> ScanRole {
        ScanRole::Location
    }
    fn extract(&self, path: &Path) -> Result<Option<Bitical>> {
        Ok(path
            .extension()
            .map(|e| Bitical::String(e.to_string_lossy().to_lowercase())))
    }
    fn extract_from_path(&self, path: &Path) -> Option<Bitical> {
        path.extension()
            .map(|e| Bitical::String(e.to_string_lossy().to_lowercase()))
    }
}
impl Query for ExtensionFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn normalize_label(&self, label: &Label) -> Result<Label> {
        let s = label.as_str().to_lowercase();
        let s = s.trim_start_matches('.');
        let has_alnum = s.chars().any(|c| c.is_ascii_alphanumeric());
        let has_disallowed_symbol =
            s.chars().any(|c| !matches!(c, '#' | '$' | '~' | '_'));
        if !has_alnum && has_disallowed_symbol {
            return Ok(label.clone());
        }
        Label::Extension(s.to_string()).to_ok()
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
        _schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        let label = self.normalize_label(label)?;
        QueryNode::And(vec![
            QueryNode::TypedTag(TypedTag::retag(SType::Extension, &label)),
            QueryNode::TypedTag(TypedTag::new(SType::IsDir, false)),
        ]).to_ok()
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
    fn query(&self) -> &dyn Query {
        self
    }
    fn edit(&self) -> Option<&dyn Edit> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::PATH
    }
}
impl Edit for PathFn {
    fn strategy(&self) -> EditStrategy {
        EditStrategy::Relocate
    }
}
impl Scan for PathFn {
    fn name() -> &'static str {
        "path"
    }
    type Value = String;
    const SCAN_ROLE: ScanRole = ScanRole::Location;
    fn scan(path: &Path, _: &SafeMetadata) -> Result<String> {
        Ok(path.to_slash_lossy().to_string())
    }
}
impl Index for PathFn {
    fn role(&self) -> ScanRole {
        ScanRole::Location
    }
    fn extract(&self, path: &Path) -> Result<Option<Bitical>> {
        Ok(Some(Bitical::String(path.to_slash_lossy().to_string())))
    }
    fn extract_from_path(&self, path: &Path) -> Option<Bitical> {
        Some(Bitical::String(path.to_slash_lossy().to_string()))
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
        _schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        let normalized = normalize_path_str(&label.as_str());
        let new_label = label
            .rekey(TagType::Base(SType::Path), Bitical::String(normalized));
        QueryNode::TypedTag(TypedTag { label: new_label }).to_ok()
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
    fn query(&self) -> &dyn Query {
        self
    }
    fn edit(&self) -> Option<&dyn Edit> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::PARENT_DIR
    }
}
impl Edit for ParentDirFn {
    fn strategy(&self) -> EditStrategy {
        EditStrategy::Relocate
    }
}
impl Index for ParentDirFn {
    fn role(&self) -> ScanRole {
        ScanRole::Location
    }
    fn extract(&self, path: &Path) -> Result<Option<Bitical>> {
        let parent = path
            .parent()
            .map(|p| p.to_slash_lossy().to_string())
            .unwrap_or_default();
        Ok(Some(Bitical::String(parent)))
    }
    fn extract_from_path(&self, path: &Path) -> Option<Bitical> {
        path.parent()
            .map(|p| Bitical::String(p.to_slash_lossy().to_string()))
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
        _schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        let normalized = normalize_path_str(&label.as_str());
        let new_label = label.rekey(
            TagType::Base(SType::Parentdir),
            Bitical::String(normalized),
        );
        QueryNode::TypedTag(TypedTag { label: new_label }).to_ok()
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
    fn query(&self) -> &dyn Query {
        self
    }
    fn edit(&self) -> Option<&dyn Edit> {
        Some(self)
    }
}
impl Edit for StemFn {
    fn strategy(&self) -> EditStrategy {
        EditStrategy::Relocate
    }
}
impl Index for StemFn {
    fn extract(&self, path: &Path) -> Result<Option<Bitical>> {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        Ok(Some(Bitical::String(stem)))
    }
    fn target_table(&self) -> TargetTable {
        TargetTable::BaseTags
    }
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
    fn query(&self) -> &dyn Query {
        self
    }
}
impl Index for IsDirFn {
    fn extract(&self, path: &Path) -> Result<Option<Bitical>> {
        let m = get_safe_meta(path)?;
        Ok(Some(Bitical::Boolean(m.is_dir())))
    }
    fn sql_type(&self) -> BiticalType {
        BiticalType::Boolean
    }
    fn target_table(&self) -> TargetTable {
        TargetTable::BaseTags
    }
}
impl Query for IsDirFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::Boolean
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        tag: &TypedTag,
        _schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        let s = label.as_str();
        if !crate::util::is_glob_pattern(&s) {
            return Ok(QueryNode::TypedTag(tag.clone()));
        }
        let first = Operand::TypeRef(SType::IsDir.into());
        let nodes: Vec<QueryNode> = [true, false]
            .into_iter()
            .filter(|b| crate::util::glob_match(&s, &b.to_string()))
            .map(|b| {
                QueryNode::Comparison(ComparisonNode {
                    first: first.clone(),
                    rest: vec![(
                        ComparisonOp::Label(BasicOp::Eq),
                        Operand::Literal(Label::IsDir(b)),
                    )],
                })
            })
            .collect();
        match nodes.len() {
            1 => nodes.into_iter().next().unwrap(),
            _ => QueryNode::Or(nodes),
        }.to_ok()
    }
    fn expand_comparison(&self, node: ComparisonNode) -> Result<QueryNode> {
        let mut first = node.first;
        if let Operand::Literal(label) = &mut first {
            *label = self.normalize_label(label)?;
        }

        let mut conditions = Vec::new();
        for (op, rhs) in node.rest {
            if let Operand::Literal(label) = &rhs {
                let s = label.as_str();
                if crate::util::is_glob_pattern(&s) {
                    let basic_op = match op {
                        ComparisonOp::Label(b) | ComparisonOp::Scalar(b) => b,
                    };
                    let ctor: fn(BasicOp) -> ComparisonOp = match op {
                        ComparisonOp::Label(_) => ComparisonOp::Label,
                        ComparisonOp::Scalar(_) => ComparisonOp::Scalar,
                    };
                    match basic_op {
                        BasicOp::Eq | BasicOp::Ne => {
                            let want_match = basic_op == BasicOp::Eq;
                            let nodes: Vec<QueryNode> = [true, false]
                                .into_iter()
                                .filter(|b| {
                                    crate::util::glob_match(&s, &b.to_string())
                                        == want_match
                                })
                                .map(|b| {
                                    QueryNode::Comparison(ComparisonNode {
                                        first: first.clone(),
                                        rest: vec![(
                                            ctor(BasicOp::Eq),
                                            Operand::Literal(Label::IsDir(b)),
                                        )],
                                    })
                                })
                                .collect();
                            conditions.push(match nodes.len() {
                                1 => nodes.into_iter().next().unwrap(),
                                _ => QueryNode::Or(nodes),
                            });
                            continue;
                        }
                        BasicOp::Gt
                        | BasicOp::Ge
                        | BasicOp::Lt
                        | BasicOp::Le
                            if crate::util::is_full_match_glob(&s) =>
                        {
                            // 全一致 glob × 順序演算子は全型共通の全域規則（Gt/Lt→FALSE、Ge/Le→TRUE）に揃える
                            let always_true =
                                matches!(basic_op, BasicOp::Ge | BasicOp::Le);
                            conditions.push(if always_true {
                                QueryNode::And(vec![])
                            } else {
                                QueryNode::Or(vec![])
                            });
                            continue;
                        }
                        _ => {}
                    }
                }
            }
            let mut rhs = rhs;
            if let Operand::Literal(label) = &mut rhs {
                *label = self.normalize_label(label)?;
            }
            conditions.push(QueryNode::Comparison(ComparisonNode {
                first: first.clone(),
                rest: vec![(op, rhs)],
            }));
        }

        match conditions.len() {
            1 => conditions.remove(0),
            _ => QueryNode::And(conditions),
        }.to_ok()
    }
}

// --- HashFn ---
pub(crate) struct HashFn;
impl TagFunction for HashFn {
    fn name(&self) -> &str {
        SType::Hash.into()
    }
    fn query(&self) -> &dyn Query {
        self
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
    fn query(&self) -> &dyn Query {
        self
    }
    fn edit(&self) -> Option<&dyn Edit> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::CONTENT
    }
}
impl Edit for ContentFn {
    fn strategy(&self) -> EditStrategy {
        EditStrategy::ModifyInjection
    }
    fn inject(&self, item: &Item) -> Option<Label> {
        Some(Label::Content(item.raw_repr()))
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
    fn query(&self) -> &dyn Query {
        self
    }
}
impl Scan for FileIdFn {
    fn name() -> &'static str {
        "file_id"
    }
    type Value = crate::types::FileRef;
    const SCAN_ROLE: ScanRole = ScanRole::ScanId;
    fn scan(path: &Path, _: &SafeMetadata) -> Result<crate::types::FileRef> {
        crate::get_file_ref(path)
    }
}
impl Index for FileIdFn {
    fn role(&self) -> ScanRole {
        ScanRole::ScanId
    }
    fn extract(&self, path: &Path) -> Result<Option<Bitical>> {
        Ok(Some(Bitical::Uuid(crate::get_file_ref(path)?)))
    }
    fn extract_from_path(&self, path: &Path) -> Option<Bitical> {
        crate::get_file_ref(path)
            .ok()
            .map(|r| Bitical::String(r.to_string()))
    }
    fn sql_type(&self) -> BiticalType {
        BiticalType::Uuid
    }
    fn target_table(&self) -> TargetTable {
        TargetTable::FileReferences
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
    fn query(&self) -> &dyn Query {
        self
    }
    fn edit(&self) -> Option<&dyn Edit> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::NAME
    }
}
impl Edit for NameFn {
    fn strategy(&self) -> EditStrategy {
        EditStrategy::Replace
    }
}
impl Query for NameFn {
    fn read(&self) -> crate::query::lens_reader::ReadResolution {
        // §4.1: name は user 由来を優先し、無ければ filename にフォールバック。
        crate::query::lens_reader::ReadResolution::default()
            .prefer(TypedTag::new(SType::Origin, Origin::User.to_string()))
            .fallback(TagType::Base(SType::Filename))
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
        _schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        QueryNode::TypedTag(TypedTag::retag(SType::Name, label)).to_ok()
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
    fn query(&self) -> &dyn Query {
        self
    }
    fn display(&self) -> Option<&dyn Display> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::SIZE
    }
}
impl Scan for SizeFn {
    fn name() -> &'static str {
        "size"
    }
    type Value = crate::types::FileSize;
    const SCAN_ROLE: ScanRole = ScanRole::Integrity;
    fn scan(_: &Path, meta: &SafeMetadata) -> Result<crate::types::FileSize> {
        Ok(crate::types::FileSize(meta.len()))
    }
}
impl Index for SizeFn {
    fn role(&self) -> ScanRole {
        ScanRole::Integrity
    }
    fn extract(&self, path: &Path) -> Result<Option<Bitical>> {
        let m = get_safe_meta(path)?;
        Ok(Some(Bitical::Integer(m.len())))
    }
    fn sql_type(&self) -> BiticalType {
        BiticalType::Integer
    }
}
impl Query for SizeFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::Integer
    }
    fn normalize_label(&self, label: &Label) -> Result<Label> {
        use crate::query::format::ByteSizeRange;
        let s = label.as_str();
        match ByteSizeRange::parse(&s) {
            Some(Ok(ByteSizeRange::Range { lo, hi })) if lo == hi => {
                return Label::Size(lo).to_ok();
            }
            Some(Ok(_)) => {
                return Label::Other(
                    TagType::from(SType::Size),
                    Bitical::String(s.to_string()),
                ).to_ok();
            }
            _ => {}
        }
        // 単位の無い数値は ByteSizeRange が主張しないので、SizeFn 自身がバイトと読む
        // （MtimeFn が裸の整数を年と読むのと同じ構造）。
        if let Some(bytes) = crate::util::parse_size(&s) {
            return Label::Size(bytes).to_ok();
        }
        label.clone().to_ok()
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
        _schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        use crate::query::format::ByteSizeRange;
        let normalized = self.normalize_label(label)?;
        if matches!(normalized, Label::Size(_)) {
            return QueryNode::TypedTag(TypedTag::retag(
                TagType::from(SType::Size),
                &normalized,
            )).to_ok();
        }
        let s = label.as_str();
        if let Some(Ok(range)) = ByteSizeRange::parse(&s) {
            return crate::query::format::byte_size_range_condition(
                Operand::TypeRef(SType::Size.into()),
                ComparisonOp::Label,
                BasicOp::Eq,
                range,
            ).to_ok();
        }
        if crate::util::is_full_match_glob(&s) {
            return QueryNode::TypedTag(TypedTag::retag(
                TagType::from(SType::Size),
                &normalized,
            )).to_ok();
        }
        Err(crate::query::error::tag_value_not_interpretable("size", &s))
    }
    fn expand_comparison(&self, node: ComparisonNode) -> Result<QueryNode> {
        use crate::query::format::ByteSizeRange;
        let mut first = node.first;
        if let Operand::Literal(label) = &mut first {
            *label = self.normalize_label(label)?;
        }

        let mut conditions = Vec::new();
        for (op, rhs) in node.rest {
            if let Operand::Literal(label) = &rhs {
                let normalized_rhs = self.normalize_label(label)?;
                if !matches!(normalized_rhs, Label::Size(_)) {
                    let s = label.as_str();
                    if let Some(Ok(range)) = ByteSizeRange::parse(&s) {
                        let (basic_op, ctor): (
                            BasicOp,
                            fn(BasicOp) -> ComparisonOp,
                        ) = match op {
                            ComparisonOp::Label(b) => (b, ComparisonOp::Label),
                            ComparisonOp::Scalar(b) => {
                                (b, ComparisonOp::Scalar)
                            }
                        };
                        conditions.push(crate::query::format::byte_size_range_condition(
                            first.clone(),
                            ctor,
                            basic_op,
                            range,
                        ));
                        continue;
                    }
                    if !crate::util::is_full_match_glob(&s) {
                        return Err(crate::query::error::tag_value_not_interpretable(
                            "size", &s,
                        ));
                    }
                }
            }
            let mut rhs = rhs;
            if let Operand::Literal(label) = &mut rhs {
                *label = self.normalize_label(label)?;
            }
            conditions.push(QueryNode::Comparison(ComparisonNode {
                first: first.clone(),
                rest: vec![(op, rhs)],
            }));
        }

        match conditions.len() {
            1 => conditions.remove(0),
            _ => QueryNode::And(conditions),
        }.to_ok()
    }
}
impl Display for SizeFn {
    fn formats(&self) -> DisplayFormats {
        DisplayFormats {
            default: DisplayFormat::new("si", "KB / MB"),
            options: vec![
                DisplayFormat::new("si", "KB / MB"),
                DisplayFormat::new("binary", "KiB / MiB"),
            ],
        }
    }
    fn show(&self, value: &Bitical, format: DisplayFormat) -> String {
        let bytes = match value {
            Bitical::Integer(i) => *i,
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
    format!("{:.2}{}", val, UNITS[idx])
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
    format!("{:.2}{}", val, UNITS[idx])
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
    fn query(&self) -> &dyn Query {
        self
    }
    fn display(&self) -> Option<&dyn Display> {
        Some(self)
    }
    fn edit(&self) -> Option<&dyn Edit> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::MTIME
    }
}
impl Edit for MtimeFn {
    fn strategy(&self) -> EditStrategy {
        EditStrategy::SetFileAttr
    }
}
impl Scan for MtimeFn {
    fn name() -> &'static str {
        "mtime"
    }
    type Value = crate::types::FileTimestamp;
    const SCAN_ROLE: ScanRole = ScanRole::Integrity;
    fn scan(
        _: &Path,
        meta: &SafeMetadata,
    ) -> Result<crate::types::FileTimestamp> {
        Ok(crate::types::FileTimestamp(meta.modified()))
    }
}
impl Index for MtimeFn {
    fn role(&self) -> ScanRole {
        ScanRole::Integrity
    }
    fn extract(&self, path: &Path) -> Result<Option<Bitical>> {
        let m = get_safe_meta(path)?;
        Ok(Some(Bitical::Integer(m.modified())))
    }
    fn sql_type(&self) -> BiticalType {
        BiticalType::Integer
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

fn parse_mtime_bare_year(s: &str) -> Option<crate::types::DateTimeRange> {
    if s.len() != 4 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i32 = s.parse().ok()?;
    crate::types::DateTime::Year(y).to_interval()
}

impl Query for MtimeFn {
    fn logical_type(&self) -> LogicalType {
        LogicalType::Integer
    }
    fn normalize_label(&self, label: &Label) -> Result<Label> {
        if let Bitical::String(s) = label.value() {
            // 構造化日付（YYYY-MM-DD / YYYY-MM）のみ対象。4桁年単体・自然言語は
            // ここでは扱わない（expand/expand_comparison の parse_datetime 経由）。
            // DateTime::parse_structured は自然言語を試さないため、"today" 等が
            // 同じ Date 型に収束して誤って対象化される心配がない。
            if let Some(Ok(
                dt @ (DateTime::Date(_) | DateTime::YearMonth { .. }),
            )) = DateTime::parse_structured(&s)
            {
                return Label::Date(dt).to_ok();
            }
        }
        label.clone().to_ok()
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
        _schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        if let Label::Date(dt) = label {
            return QueryNode::DateTimeRange {
                first: Operand::TypeRef(SType::Mtime.into()),
                op: BasicOp::Eq,
                range: dt.to_interval().expect(
                    "Date-precision DateTime must always succeed floor/ceiling",
                ),
            }.to_ok();
        }
        let s = label.as_str();
        if let Some(Ok(range)) = crate::types::DateTimeRange::parse(&s) {
            return QueryNode::DateTimeRange {
                first: Operand::TypeRef(SType::Mtime.into()),
                op: BasicOp::Eq,
                range,
            }.to_ok();
        }
        if let Some(range) = parse_mtime_bare_year(&s) {
            return QueryNode::DateTimeRange {
                first: Operand::TypeRef(SType::Mtime.into()),
                op: BasicOp::Eq,
                range,
            }.to_ok();
        }
        if crate::util::is_full_match_glob(&s) {
            return QueryNode::TypedTag(TypedTag::retag(SType::Mtime, label))
                .to_ok();
        }
        Err(crate::query::error::tag_value_not_interpretable("mtime", &s))
    }
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }
    fn expand_comparison(&self, node: ComparisonNode) -> Result<QueryNode> {
        let mut first = node.first.clone();
        let rest = node.rest;

        if let Operand::Literal(label) = &mut first {
            let range = crate::types::DateTimeRange::parse(&label.as_str())
                .and_then(|r| r.ok())
                .or_else(|| parse_mtime_bare_year(&label.as_str()));
            if let Some(range) = range {
                let (start, _) = range
                    .as_interval()
                    .expect("parse_datetime only ever returns interval form");
                *label = Label::Mtime(start);
            }
        }

        let mut conditions = Vec::new();
        for (op, rhs) in rest {
            if let Operand::Literal(label) = &rhs {
                let basic_op = match op {
                    ComparisonOp::Label(b) | ComparisonOp::Scalar(b) => b,
                };
                let s = label.as_str();
                let range = if let Label::Date(dt) = label {
                    Some(dt.to_interval().expect(
                        "Date-precision DateTime must always succeed floor/ceiling",
                    ))
                } else {
                    crate::types::DateTimeRange::parse(&s)
                        .and_then(|r| r.ok())
                        .or_else(|| parse_mtime_bare_year(&s))
                };
                if let Some(range) = range {
                    conditions.push(QueryNode::DateTimeRange {
                        first: first.clone(),
                        op: basic_op,
                        range,
                    });
                    continue;
                }
                if !matches!(label, Label::Date(_))
                    && !crate::util::is_full_match_glob(&s)
                {
                    return Err(crate::query::error::tag_value_not_interpretable(
                        "mtime", &s,
                    ));
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
        }.to_ok()
    }
}
impl Display for MtimeFn {
    fn formats(&self) -> DisplayFormats {
        DisplayFormats {
            default: DisplayFormat::new("human", "Human Readable"),
            options: vec![
                DisplayFormat::new("human", "Human Readable"),
                DisplayFormat::new("relative", "Relative"),
                DisplayFormat::new("iso", "ISO 8601"),
                DisplayFormat::new("raw", "Raw"),
            ],
        }
    }
    fn show(&self, value: &Bitical, format: DisplayFormat) -> String {
        let secs = match value {
            Bitical::Integer(i) => *i,
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

// --- ItemIdFn ---
pub(crate) struct ItemIdFn;
impl TagFunction for ItemIdFn {
    fn name(&self) -> &str {
        SType::ItemId.into()
    }
    fn query(&self) -> &dyn Query {
        self
    }
    fn edit(&self) -> Option<&dyn Edit> {
        Some(self)
    }
}
impl Edit for ItemIdFn {
    fn strategy(&self) -> EditStrategy {
        EditStrategy::RemoveOnly
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
    fn normalize_label(&self, label: &Label) -> Result<Label> {
        let s = match label.value() {
            Bitical::String(s) => s,
            _ => return label.clone().to_ok(),
        };
        match crate::db::identifier::parse(&s) {
            Ok(id) => Label::ItemId(id),
            Err(_) => label.clone(),
        }.to_ok()
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
        _schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        let s = label.as_str();
        if crate::util::is_glob_pattern(&s) {
            if let Some(Ok(crate::query::format::ItemIdRange { lo, hi })) =
                crate::query::format::ItemIdRange::parse(&s)
            {
                let first = Operand::TypeRef(SType::ItemId.into());
                return QueryNode::And(vec![
                    QueryNode::Comparison(ComparisonNode {
                        first: first.clone(),
                        rest: vec![(
                            ComparisonOp::Label(BasicOp::Ge),
                            Operand::Literal(Label::ItemId(lo)),
                        )],
                    }),
                    QueryNode::Comparison(ComparisonNode {
                        first,
                        rest: vec![(
                            ComparisonOp::Label(BasicOp::Le),
                            Operand::Literal(Label::ItemId(hi)),
                        )],
                    }),
                ]).to_ok();
            }
            if !crate::util::is_full_match_glob(&s) {
                return Err(crate::query::error::tag_value_not_interpretable(
                    "item_id", &s,
                ));
            }
        }
        QueryNode::ColumnMatch {
            tag: SType::ItemId,
            label: label.clone(),
        }.to_ok()
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
    fn query(&self) -> &dyn Query {
        self
    }
    fn edit(&self) -> Option<&dyn Edit> {
        Some(self)
    }
    fn default_rank(&self) -> Rank {
        crate::rank::SystemRank::ITEM_KIND
    }
}
impl Edit for ItemKindFn {
    fn strategy(&self) -> EditStrategy {
        EditStrategy::ModifyInjection
    }
    fn inject(&self, item: &Item) -> Option<Label> {
        let kind = match item.representative.as_slice() {
            [l] if l.tag_type() == TagType::Base(SType::TypedTag) => {
                ItemKind::Tag
            }
            [l] if l.tag_type() == TagType::Base(SType::Type) => ItemKind::Type,
            _ => ItemKind::Note,
        };
        Some(Label::from(kind))
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
        _schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        QueryNode::ColumnMatch {
            tag: SType::ItemKind,
            label: label.clone(),
        }.to_ok()
    }
}

// --- RankFn ---
pub(crate) struct RankFn;
impl TagFunction for RankFn {
    fn name(&self) -> &str {
        SType::Rank.into()
    }
    fn query(&self) -> &dyn Query {
        self
    }
    fn edit(&self) -> Option<&dyn Edit> {
        Some(self)
    }
}
impl Edit for RankFn {
    fn strategy(&self) -> EditStrategy {
        EditStrategy::Replace
    }
}
impl Query for RankFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::Integer
    }
    fn normalize_label(&self, label: &Label) -> Result<Label> {
        match label.value() {
            Bitical::String(s) => match s.parse::<i64>() {
                Ok(n) => Label::Rank(n.into()).to_ok(),
                Err(_) => label.clone().to_ok(),
            },
            _ => label.clone().to_ok(),
        }
    }
    fn expand(
        &self,
        _tt: &TagType,
        label: &Label,
        _tag: &TypedTag,
        _schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        let normalized = self.normalize_label(label)?;
        let s = label.as_str();
        if matches!(normalized.value(), Bitical::String(_))
            && !crate::util::is_full_match_glob(&s)
        {
            return Err(crate::query::error::tag_value_not_interpretable(
                "rank", &s,
            ));
        }
        QueryNode::TypedTag(TypedTag::retag(SType::Rank, &normalized)).to_ok()
    }
}

// --- OriginFn ---
pub(crate) struct OriginFn;
impl TagFunction for OriginFn {
    fn name(&self) -> &str {
        SType::Origin.into()
    }
    fn query(&self) -> &dyn Query {
        self
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
        schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        // "system" は大分類のエイリアス。実値である小分類 (Builtin/File/Plugin)
        // への Or として展開する (ITEM.md の Origin 大分類)。
        if label.as_str() == LargeOrigin::System.as_str() {
            use strum::IntoEnumIterator;
            return QueryNode::Or(
                Origin::iter()
                    .filter(|o| o.is_system())
                    .map(|o| {
                        let sub_label = Label::resolve(
                            TagType::Base(SType::Origin),
                            Bitical::String(o.as_str().to_string()),
                        );
                        self.expand(_tt, &sub_label, _tag, schema)
                    })
                    .collect::<Result<Vec<_>>>()?,
            ).to_ok();
        }

        let origin_str = label.as_str();
        let mut sub_queries = vec![QueryNode::ColumnMatch {
            tag: SType::Origin,
            label: label.clone(),
        }];

        use strum::IntoEnumIterator;
        let matched_origins: Vec<Origin> = Origin::iter()
            .filter(|o| crate::util::glob_match(&origin_str, o.as_str()))
            .collect();

        let all_entries = schema.iter_all_for_rank();
        let reserved: Vec<String> = all_entries
            .iter()
            .map(|(t, _, _)| t.as_str().to_string())
            .collect();

        for target_org in matched_origins {
            let candidates: Vec<Candidate> = all_entries
                .iter()
                .filter(|(_, _, id)| match id {
                    ItemId::Stored(val) => Origin::within(*val) == target_org,
                    ItemId::Settling(org, _) => *org == target_org,
                    ItemId::Volatile(_) => target_org == Origin::User,
                })
                .map(|(t, r, id)| Candidate {
                    name: t.as_str().to_string(),
                    rank: *r,
                    id: *id,
                })
                .collect();

            if !candidates.is_empty() {
                let val = Label::resolve(
                    TagType::Base(SType::Type),
                    Bitical::String("*".to_string()),
                );
                sub_queries.push(QueryNode::DefinitionRef(DefinitionRef {
                    kind: ItemKind::Type,
                    value: val,
                    candidates,
                    origins: vec![target_org],
                    reserved: reserved.clone(),
                    recorded: false,
                }));
            }
        }

        if sub_queries.len() == 1 {
            sub_queries.remove(0)
        } else {
            QueryNode::Or(sub_queries)
        }.to_ok()
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
    fn query(&self) -> &dyn Query {
        self
    }
    fn display(&self) -> Option<&dyn Display> {
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
    fn item_kind(&self) -> Option<ItemKind> {
        Some(ItemKind::Type)
    }
    fn expand(
        &self,
        tagtype: &TagType,
        label: &Label,
        _tag: &TypedTag,
        schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        // registry の登録型名＋default_rank＋出所（ItemId）を
        // candidates にする。完全一致検索でも
        // Stored（組み込み）/Settling（プラグイン）の区別を保つため、
        // 検索経路によらず常に同じ candidates を渡す。GLOB によるパターン
        // 絞り込み自体は SQL 側の定義アイテム列挙サブクエリに委ねる。
        let candidates: Vec<Candidate> = schema
            .iter_all_for_rank()
            .into_iter()
            .map(|(t, r, id)| Candidate {
                name: t.as_str().to_string(),
                rank: r,
                id,
            })
            .collect();
        let reserved = candidates.iter().map(|c| c.name.clone()).collect();
        QueryNode::DefinitionRef(DefinitionRef {
            kind: ItemKind::Type,
            value: label.rekey(tagtype.clone(), label.value()),
            candidates,
            origins: Vec::new(),
            reserved,
            recorded: true,
        }).to_ok()
    }
    fn expand_projection(&self, tagtype: &TagType) -> QueryNode {
        QueryNode::Projection(Operand::from(tagtype.clone()))
    }
}
impl Display for TypeFn {
    // 型定義アイテムは item_id 降順（区画をまたぐ生 id 順）で表示する
    fn preferred_order(&self) -> Vec<crate::types::Order> {
        vec![crate::types::Order::desc(SType::ItemId)]
    }
    fn show(&self, value: &Bitical, _format: DisplayFormat) -> String {
        value.as_display_name()
    }
}

// --- LabelFn ---
pub(crate) struct LabelFn;
impl TagFunction for LabelFn {
    fn name(&self) -> &str {
        SType::Label.into()
    }
    fn query(&self) -> &dyn Query {
        self
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
        _schema: &dyn LogicalSchema,
    ) -> Result<QueryNode> {
        QueryNode::ColumnMatch {
            tag: SType::Label,
            label: label.clone(),
        }.to_ok()
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
    fn query(&self) -> &dyn Query {
        self
    }
}
impl Query for TypedTagFn {
    fn logical_role(&self) -> LogicalRole {
        LogicalRole::Fixed
    }
    fn logical_type(&self) -> LogicalType {
        LogicalType::String
    }
    fn item_kind(&self) -> Option<ItemKind> {
        Some(ItemKind::Tag)
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
        fn extract(&self, _path: &Path) -> Result<Option<Bitical>> {
            Ok(Some(Bitical::String("value".to_string())))
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
        fn query(&self) -> &dyn Query {
            self
        }
    }
    impl Query for QueryTag {
        fn normalize_label(&self, label: &Label) -> Result<Label> {
            Label::Other(
                TagType::from("qtest"),
                Bitical::String(label.as_str().to_uppercase()),
            ).to_ok()
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

    // Plan: 定義アイテムの id 区画 — Step 3
    // register() は呼び出し順に builtin_offset を採番する（宣言不要・列挙順固定）。
    #[test]
    fn test_register_assigns_sequential_builtin_offset() {
        let mut reg = TagRegistry::new();
        reg.register(SimpleTag);
        reg.register(IndexedTag);
        assert_eq!(reg.builtin_offset("simple"), Some(0));
        assert_eq!(reg.builtin_offset("indexed"), Some(1));
    }

    // register_plugin() は builtin_offset を採番しない（Sys 区画対象外）。
    #[test]
    fn test_register_plugin_has_no_builtin_offset() {
        let mut reg = TagRegistry::new();
        reg.register_plugin(SimpleTag);
        assert!(reg.get("simple").is_some());
        assert_eq!(reg.builtin_offset("simple"), None);
    }

    // Plan: 定義アイテムの id 区画 — Step 6
    // builtin_name_for_offset は builtin_offset の逆写像（Sys id 実体化時の名前解決に使う）。
    #[test]
    fn test_builtin_name_for_offset_reverses_builtin_offset() {
        let mut reg = TagRegistry::new();
        reg.register(SimpleTag);
        reg.register(IndexedTag);
        assert_eq!(reg.builtin_name_for_offset(0), Some("simple"));
        assert_eq!(reg.builtin_name_for_offset(1), Some("indexed"));
        assert_eq!(reg.builtin_name_for_offset(99), None);
    }

    // with_standard() の登録順がそのまま builtin_offset になる（先頭は file_id）。
    #[test]
    fn test_with_standard_builtin_offsets_follow_declaration_order() {
        let reg = TagRegistry::with_standard();
        assert_eq!(reg.builtin_offset("file_id"), Some(0));
        assert_eq!(reg.builtin_offset("path"), Some(1));
        assert_eq!(reg.builtin_offset("hash"), Some(10));
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
        assert!(SimpleTag.display().is_none());
        // query は常に存在し、宣言しなければ Query の全デフォルト実装が使われる
        let label = Label::from("x");
        assert_eq!(SimpleTag.query().normalize_label(&label).unwrap(), label);
        assert_eq!(SimpleTag.query().logical_type(), LogicalType::String);
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
        assert_eq!(result, Some(Bitical::String("value".to_string())));
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
        let qry = q.query();
        let label = Label::Other(
            TagType::from("qtest"),
            Bitical::String("hello".to_string()),
        );
        let normalized = qry.normalize_label(&label).unwrap();
        assert_eq!(normalized.as_str(), "HELLO");
    }

    // --- Phase 3: normalize_label ---

    #[test]
    fn test_size_normalize_label_string_converts() {
        let q = SizeFn;
        let label = Label::Other(
            TagType::from("size"),
            Bitical::String("1MB".to_string()),
        );
        let normalized = q.query().normalize_label(&label).unwrap();
        assert_eq!(normalized, Label::Size(1_048_576));
    }

    #[test]
    fn test_size_normalize_label_glob_pattern_becomes_size_typed_other() {
        let q = SizeFn;
        // 比較右辺のリテラルは Label::from(&str) と同じ形（TagType::Custom("")）で来る
        let label = Label::from("*MB");
        let normalized = q.query().normalize_label(&label).unwrap();
        assert_eq!(
            normalized,
            Label::Other(
                TagType::from(SType::Size),
                Bitical::String("*MB".to_string())
            )
        );
    }

    #[test]
    fn test_rank_normalize_label_numeric_string_converts() {
        let q = RankFn;
        let label = Label::Other(
            TagType::from("rank"),
            Bitical::String("3".to_string()),
        );
        let normalized = q.query().normalize_label(&label).unwrap();
        assert_eq!(normalized, Label::Rank(3.into()));
    }

    #[test]
    fn test_rank_expand_unparseable_is_error() {
        let q = RankFn;
        let label = Label::Other(
            TagType::from("rank"),
            Bitical::String("abc".to_string()),
        );
        let tag = TypedTag::retag(SType::Rank, &label);
        let lens = crate::query::lens_schema::Lens::base_standard();
        let result =
            q.query().expand(&TagType::from("rank"), &label, &tag, &lens);
        assert!(result.is_err(), "rank:abc must not be silently passed through");
    }

    // --- ExtensionFn::normalize_label の記号のみ判定 ---

    #[test]
    fn test_extension_normalize_label_glob_star_not_treated_as_extension() {
        let q = ExtensionFn;
        let label = Label::Other(
            TagType::from("extension"),
            Bitical::String("*".to_string()),
        );
        let normalized = q.query().normalize_label(&label).unwrap();
        assert_eq!(normalized, label);
    }

    #[test]
    fn test_extension_normalize_label_glob_question_mark_not_treated_as_extension(
    ) {
        let q = ExtensionFn;
        let label = Label::Other(
            TagType::from("extension"),
            Bitical::String("?".to_string()),
        );
        let normalized = q.query().normalize_label(&label).unwrap();
        assert_eq!(normalized, label);
    }

    #[test]
    fn test_extension_normalize_label_allowed_symbol_only_stays_extension() {
        let q = ExtensionFn;
        let label = Label::Other(
            TagType::from("extension"),
            Bitical::String("~".to_string()),
        );
        let normalized = q.query().normalize_label(&label).unwrap();
        assert_eq!(normalized, Label::Extension("~".to_string()));
    }

    #[test]
    fn test_mtime_normalize_label_date_string() {
        let q = MtimeFn;
        let label = Label::Other(
            TagType::from("mtime"),
            Bitical::String("2026-02-01".to_string()),
        );
        let normalized = q.query().normalize_label(&label).unwrap();
        match normalized {
            Label::Date(crate::types::DateTime::Date(d)) => {
                assert_eq!(
                    d,
                    chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()
                );
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
            Bitical::String("2026".to_string()),
        );
        let normalized = q.query().normalize_label(&label).unwrap();
        assert_eq!(
            normalized, label,
            "bare YYYY should be unchanged by normalize_label"
        );
    }

    #[test]
    fn test_mtime_normalize_label_non_date_unchanged() {
        let q = MtimeFn;
        let label = Label::Other(
            TagType::from("mtime"),
            Bitical::String("not-a-date".to_string()),
        );
        let normalized = q.query().normalize_label(&label).unwrap();
        assert_eq!(normalized, label);
    }

    #[test]
    fn test_mtime_normalize_label_year_month_string() {
        let q = MtimeFn;
        let label = Label::Other(
            TagType::from("mtime"),
            Bitical::String("2026-02".to_string()),
        );
        let normalized = q.query().normalize_label(&label).unwrap();
        assert_eq!(
            normalized,
            Label::Date(crate::types::DateTime::YearMonth {
                year: 2026,
                month: 2
            })
        );
    }

    #[test]
    fn test_mtime_normalize_label_natural_language_unchanged() {
        // 自然言語（"today" 等）は normalize_label の対象外のまま
        // （expand/expand_comparison の parse_datetime 経由で処理される。
        // DateTime::from_str へのパーサ統合後も normalize_label の受理範囲
        // 自体は変えない = 構造化日付（YYYY-MM-DD/YYYY-MM）のみ）
        let q = MtimeFn;
        let label = Label::Other(
            TagType::from("mtime"),
            Bitical::String("today".to_string()),
        );
        let normalized = q.query().normalize_label(&label).unwrap();
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

    // --- Display: 優先 Order ---

    // TypeFn は Display で優先 Order（item_id 降順）を宣言する
    #[test]
    fn typefn_display_declares_item_id_order() {
        use crate::types::{Order, SType};
        let display =
            TypeFn.display().expect("TypeFn should declare a Display");
        assert_eq!(display.preferred_order(), vec![Order::desc(SType::ItemId)]);
    }

    // 優先 Order の既定は空（SizeFn は Display を持つが宣言しない）
    #[test]
    fn display_preferred_order_defaults_to_empty() {
        let display = SizeFn.display().expect("SizeFn declares a Display");
        assert!(display.preferred_order().is_empty());
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

    // --- Phase 6: MtimeFn::expand ---

    #[test]
    fn test_mtime_expand_date_label_becomes_range() {
        use crate::query::ast::BasicOp;
        use crate::types::{DateTime, Label, SType, TagType, TypedTag};
        use chrono::NaiveDate;
        let date = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let dt = DateTime::Date(date);
        let label = Label::Date(dt.clone());
        let tag_type = TagType::from(SType::Mtime);
        let typed_tag = TypedTag::retag(SType::Mtime, &label);
        let result = MtimeFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &crate::query::lens_schema::Lens::base_standard(),
        ).unwrap();
        // Date Eq → QueryNode::DateTimeRange { op: Eq, range: floor..ceiling }
        let QueryNode::DateTimeRange { first, op, range } = result else {
            panic!("expected DateTimeRange, got {:?}", result)
        };
        assert_eq!(
            first,
            crate::query::ast::Operand::TypeRef(TagType::from(SType::Mtime))
        );
        assert_eq!(op, BasicOp::Eq);
        assert_eq!(
            range.as_interval(),
            dt.to_interval().unwrap().as_interval()
        );
    }

    // --- SizeFn::expand の glob 翻訳（数値部のフィールド単位 glob） ---

    /// (first, op) -> bytes のペアを取り出す。first の形は問わない
    /// （TypeRef=単純な範囲 / Calculation=剰余の周期条件）。
    fn extract_size_bounds(
        result: &QueryNode,
    ) -> (Option<i64>, Option<i64>, crate::query::ast::Operand) {
        use crate::query::ast::{BasicOp, ComparisonOp, Operand};
        use crate::types::Label;
        let QueryNode::And(nodes) = result else {
            panic!("expected And, got {:?}", result)
        };
        assert_eq!(nodes.len(), 2);
        let mut ge = None;
        let mut le = None;
        let mut first = None;
        for n in nodes {
            let QueryNode::Comparison(c) = n else {
                panic!("expected Comparison, got {:?}", n)
            };
            first = Some(c.first.clone());
            let (op, val) = &c.rest[0];
            let Operand::Literal(Label::Size(bytes)) = val else {
                panic!("expected Label::Size literal, got {:?}", val)
            };
            match op {
                ComparisonOp::Label(BasicOp::Ge) => ge = Some(*bytes),
                ComparisonOp::Label(BasicOp::Le) => le = Some(*bytes),
                other => panic!("unexpected op {:?}", other),
            }
        }
        (ge, le, first.unwrap())
    }

    #[test]
    fn test_size_expand_glob_star_mb_matches_all_sizes() {
        use crate::query::ast::Operand;
        use crate::types::{Bitical, Label, SType, TagType, TypedTag};
        let tag_type = TagType::from(SType::Size);
        let label =
            Label::Other(tag_type.clone(), Bitical::String("*MB".to_string()));
        let typed_tag = TypedTag::retag(SType::Size, &label);
        let result = SizeFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &crate::query::lens_schema::Lens::base_standard(),
        ).unwrap();
        let (ge, le, first) = extract_size_bounds(&result);
        assert_eq!(ge, Some(0));
        assert_eq!(le, Some(i64::MAX));
        assert!(matches!(first, Operand::TypeRef(_)));
    }

    #[test]
    fn test_size_expand_glob_star_b_matches_all_sizes() {
        use crate::query::ast::Operand;
        use crate::types::{Bitical, Label, SType, TagType, TypedTag};
        let tag_type = TagType::from(SType::Size);
        let label =
            Label::Other(tag_type.clone(), Bitical::String("*B".to_string()));
        let typed_tag = TypedTag::retag(SType::Size, &label);
        let result = SizeFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &crate::query::lens_schema::Lens::base_standard(),
        ).unwrap();
        let (ge, le, first) = extract_size_bounds(&result);
        assert_eq!(ge, Some(0));
        assert_eq!(le, Some(i64::MAX));
        assert!(matches!(first, Operand::TypeRef(_)));
    }

    #[test]
    fn test_size_expand_glob_decimal_free_integer_literal_translates_to_single_range(
    ) {
        use crate::query::ast::Operand;
        use crate::types::{Bitical, Label, SType, TagType, TypedTag};
        let tag_type = TagType::from(SType::Size);
        let label = Label::Other(
            tag_type.clone(),
            Bitical::String("2.*MB".to_string()),
        );
        let typed_tag = TypedTag::retag(SType::Size, &label);
        let result = SizeFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &crate::query::lens_schema::Lens::base_standard(),
        ).unwrap();
        let (ge, le, first) = extract_size_bounds(&result);
        // 整数部が2で小数部が自由 → [2.00MB, 3.00MB)。
        assert_eq!(ge, Some(2_097_152));
        assert_eq!(le, Some(3_145_727));
        assert!(matches!(first, Operand::TypeRef(_)));
    }

    #[test]
    fn test_size_expand_glob_integer_free_decimal_literal_translates_to_periodic_condition(
    ) {
        use crate::query::ast::{ArithmeticOp, Operand};
        use crate::types::{Bitical, Label, SType, TagType, TypedTag};
        let tag_type = TagType::from(SType::Size);
        let label = Label::Other(
            tag_type.clone(),
            Bitical::String("*.5MB".to_string()),
        );
        let typed_tag = TypedTag::retag(SType::Size, &label);
        let result = SizeFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &crate::query::lens_schema::Lens::base_standard(),
        ).unwrap();
        let (ge, le, first) = extract_size_bounds(&result);
        // 小数第1位が5・第2位は自由 → 1MBごとの剰余が [0.50MB, 0.60MB) に入る周期条件。
        assert_eq!(ge, Some(524_288));
        assert_eq!(le, Some(629_145));
        let Operand::Calculation(calc) = first else {
            panic!("expected Calculation (modulo), got {:?}", first)
        };
        assert_eq!(calc.op, ArithmeticOp::Mod);
        assert!(matches!(calc.left, Operand::TypeRef(_)));
        assert_eq!(calc.right, Operand::Literal(Label::Size(1024 * 1024)));
    }

    #[test]
    fn test_size_expand_glob_two_digit_decimal_narrows_to_second_place() {
        use crate::query::ast::{ArithmeticOp, Operand};
        use crate::types::{Bitical, Label, SType, TagType, TypedTag};
        let tag_type = TagType::from(SType::Size);
        let label = Label::Other(
            tag_type.clone(),
            Bitical::String("*.55MB".to_string()),
        );
        let typed_tag = TypedTag::retag(SType::Size, &label);
        let result = SizeFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &crate::query::lens_schema::Lens::base_standard(),
        ).unwrap();
        let (ge, le, first) = extract_size_bounds(&result);
        // 2桁とも指定されたので [0.55MB, 0.56MB) に狭まる。
        assert_eq!(ge, Some(576_717));
        assert_eq!(le, Some(587_202));
        let Operand::Calculation(calc) = first else {
            panic!("expected Calculation (modulo), got {:?}", first)
        };
        assert_eq!(calc.op, ArithmeticOp::Mod);
    }

    #[test]
    fn test_size_expand_glob_three_digit_decimal_narrows_to_third_place() {
        use crate::query::ast::{ArithmeticOp, Operand};
        use crate::types::{Bitical, Label, SType, TagType, TypedTag};
        let tag_type = TagType::from(SType::Size);
        let label = Label::Other(
            tag_type.clone(),
            Bitical::String("*.555MB".to_string()),
        );
        let typed_tag = TypedTag::retag(SType::Size, &label);
        let result = SizeFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &crate::query::lens_schema::Lens::base_standard(),
        ).unwrap();
        let (ge, le, first) = extract_size_bounds(&result);
        // 桁数に上限はない。3桁とも指定されたので [0.555MB, 0.556MB) に狭まる。
        assert_eq!(ge, Some(581_960));
        assert_eq!(le, Some(583_008));
        let Operand::Calculation(calc) = first else {
            panic!("expected Calculation (modulo), got {:?}", first)
        };
        assert_eq!(calc.op, ArithmeticOp::Mod);
    }

    #[test]
    fn test_size_expand_glob_digit_prefix_partial_glob_is_error() {
        use crate::types::{Bitical, Label, SType, TagType, TypedTag};
        let tag_type = TagType::from(SType::Size);
        let label =
            Label::Other(tag_type.clone(), Bitical::String("1*".to_string()));
        let typed_tag = TypedTag::retag(SType::Size, &label);
        let result = SizeFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &crate::query::lens_schema::Lens::base_standard(),
        );
        // フィールド内の部分 glob は整数部/小数部いずれの形式でも解釈できないので、
        // 元のタグ一致へ黙って通さずエラーにする。
        assert!(result.is_err());
    }

    // --- SizeFn::expand_comparison の glob 翻訳（B・C・D・F の size 列） ---

    /// 単一の Comparison ノードから (first, op, バイト値) を取り出す。
    fn extract_single_size_comparison(
        result: &QueryNode,
    ) -> (crate::query::ast::Operand, crate::query::ast::BasicOp, i64) {
        use crate::query::ast::{ComparisonOp, Operand};
        use crate::types::Label;
        let QueryNode::Comparison(c) = result else {
            panic!("expected Comparison, got {:?}", result)
        };
        assert_eq!(c.rest.len(), 1);
        let (op, val) = &c.rest[0];
        let basic_op = match op {
            ComparisonOp::Label(b) | ComparisonOp::Scalar(b) => *b,
        };
        let Operand::Literal(Label::Size(bytes)) = val else {
            panic!("expected Label::Size literal, got {:?}", val)
        };
        (c.first.clone(), basic_op, *bytes)
    }

    fn size_glob_comparison_node(
        op: crate::query::ast::BasicOp,
        pattern: &str,
    ) -> crate::query::ast::ComparisonNode {
        use crate::query::ast::{ComparisonOp, Operand};
        use crate::types::{Bitical, SType, TagType};
        crate::query::ast::ComparisonNode {
            first: Operand::TypeRef(TagType::from(SType::Size)),
            rest: vec![(
                ComparisonOp::Label(op),
                Operand::Literal(Label::Other(
                    TagType::from(SType::Size),
                    Bitical::String(pattern.to_string()),
                )),
            )],
        }
    }

    /// 整数部リテラル・小数部自由（範囲形）は、順序演算子ごとに区間の境界へ翻訳される。
    /// Gt/Le は上限側、Ge/Lt は下限側の境界を使う（日付の区間形と同じ行列）。
    #[test]
    fn test_size_expand_comparison_range_glob_order_ops_use_interval_bounds() {
        use crate::query::ast::{BasicOp, Operand};
        use crate::types::SType;
        let size_ref = Operand::TypeRef(SType::Size.into());

        let gt = SizeFn
            .query()
            .expand_comparison(size_glob_comparison_node(BasicOp::Gt, "2.*MB")).unwrap();
        let (first, op, val) = extract_single_size_comparison(&gt);
        assert_eq!(first, size_ref);
        assert_eq!(op, BasicOp::Gt);
        assert_eq!(val, 3_145_727);

        let ge = SizeFn
            .query()
            .expand_comparison(size_glob_comparison_node(BasicOp::Ge, "2.*MB")).unwrap();
        let (_, op, val) = extract_single_size_comparison(&ge);
        assert_eq!(op, BasicOp::Ge);
        assert_eq!(val, 2_097_152);

        let lt = SizeFn
            .query()
            .expand_comparison(size_glob_comparison_node(BasicOp::Lt, "2.*MB")).unwrap();
        let (_, op, val) = extract_single_size_comparison(&lt);
        assert_eq!(op, BasicOp::Lt);
        assert_eq!(val, 2_097_152);

        let le = SizeFn
            .query()
            .expand_comparison(size_glob_comparison_node(BasicOp::Le, "2.*MB")).unwrap();
        let (_, op, val) = extract_single_size_comparison(&le);
        assert_eq!(op, BasicOp::Le);
        assert_eq!(val, 3_145_727);
    }

    /// 範囲形の Eq/Ne は、素の値と同じ And/Or の2条件に翻訳される。
    #[test]
    fn test_size_expand_comparison_range_glob_eq_and_ne() {
        use crate::query::ast::{BasicOp, ComparisonOp, Operand};
        use crate::types::Label;

        let eq = SizeFn
            .query()
            .expand_comparison(size_glob_comparison_node(BasicOp::Eq, "2.*MB")).unwrap();
        let (ge, le, _) = extract_size_bounds(&eq);
        assert_eq!(ge, Some(2_097_152));
        assert_eq!(le, Some(3_145_727));

        let ne = SizeFn
            .query()
            .expand_comparison(size_glob_comparison_node(BasicOp::Ne, "2.*MB")).unwrap();
        let QueryNode::Or(nodes) = &ne else {
            panic!("expected Or, got {:?}", ne)
        };
        assert_eq!(nodes.len(), 2);
        for n in nodes {
            let QueryNode::Comparison(c) = n else {
                panic!("expected Comparison, got {:?}", n)
            };
            let (op, val) = &c.rest[0];
            let Operand::Literal(Label::Size(bytes)) = val else {
                panic!("expected Label::Size literal, got {:?}", val)
            };
            match op {
                ComparisonOp::Label(BasicOp::Lt) => {
                    assert_eq!(*bytes, 2_097_152)
                }
                ComparisonOp::Label(BasicOp::Gt) => {
                    assert_eq!(*bytes, 3_145_727)
                }
                other => panic!("unexpected op {:?}", other),
            }
        }
    }

    /// 整数部自由・小数部リテラル（周期形）は first を剰余（Calculation）へ包んだうえで
    /// 同じ区間の境界行列を適用する。
    #[test]
    fn test_size_expand_comparison_periodic_glob_order_ops_wrap_modulo() {
        use crate::query::ast::{ArithmeticOp, BasicOp, Operand};

        let gt = SizeFn
            .query()
            .expand_comparison(size_glob_comparison_node(BasicOp::Gt, "*.5MB")).unwrap();
        let (first, op, val) = extract_single_size_comparison(&gt);
        assert_eq!(op, BasicOp::Gt);
        assert_eq!(val, 629_145);
        let Operand::Calculation(calc) = first else {
            panic!("expected Calculation (modulo), got {:?}", gt)
        };
        assert_eq!(calc.op, ArithmeticOp::Mod);
        assert!(matches!(calc.left, Operand::TypeRef(_)));

        let ge = SizeFn
            .query()
            .expand_comparison(size_glob_comparison_node(BasicOp::Ge, "*.5MB")).unwrap();
        let (_, op, val) = extract_single_size_comparison(&ge);
        assert_eq!(op, BasicOp::Ge);
        assert_eq!(val, 524_288);
    }

    /// first が単純な TypeRef でない場合（例: max(size:) :> *.5MB）でも、first の構造が
    /// 剰余の Calculation の内側にそのまま保持される必要がある（Mtime と同じ回帰防止）。
    #[test]
    fn test_size_expand_comparison_aggregation_wrapped_size_preserves_first() {
        use crate::query::ast::{
            AggregationNode, ArithmeticAggOp, ArithmeticOp, BasicOp,
            ComparisonNode, ComparisonOp, Operand,
        };
        use crate::types::{Bitical, SType, TagType};
        let agg = Operand::Aggregation(Box::new(AggregationNode::Arithmetic {
            op: ArithmeticAggOp::Max,
            inner: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from(SType::Size),
            ))),
        }));
        let node = ComparisonNode {
            first: agg.clone(),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Gt),
                Operand::Literal(Label::Other(
                    TagType::from(SType::Size),
                    Bitical::String("*.5MB".to_string()),
                )),
            )],
        };
        let result = SizeFn.query().expand_comparison(node).unwrap();
        let (first, op, _) = extract_single_size_comparison(&result);
        assert_eq!(op, BasicOp::Gt);
        let Operand::Calculation(calc) = first else {
            panic!("expected Calculation (modulo), got {:?}", result)
        };
        assert_eq!(calc.op, ArithmeticOp::Mod);
        assert_eq!(calc.left, agg, "first（集約）が保持されているべき");
    }

    /// フィールド内の部分 glob は解釈できないので、比較の右辺でもエラーにする。
    #[test]
    fn test_size_expand_comparison_digit_prefix_partial_glob_is_error() {
        use crate::query::ast::BasicOp;
        let result = SizeFn
            .query()
            .expand_comparison(size_glob_comparison_node(BasicOp::Gt, "1*"));
        assert!(result.is_err(), "got: {:?}", result);
    }

    // --- MtimeFn::expand の glob 翻訳 ---

    #[test]
    fn test_mtime_expand_glob_month_day_slot_translates_to_slots_range() {
        use crate::query::ast::BasicOp;
        use crate::types::{
            Bitical, DateTimeRange, Label, SType, TagType, TypedTag,
        };
        let tag_type = TagType::from(SType::Mtime);
        let label = Label::Other(
            tag_type.clone(),
            Bitical::String("*-02-01".to_string()),
        );
        let typed_tag = TypedTag::retag(SType::Mtime, &label);
        let result = MtimeFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &crate::query::lens_schema::Lens::base_standard(),
        ).unwrap();
        let QueryNode::DateTimeRange { first, op, range } = result else {
            panic!("expected DateTimeRange, got {:?}", result)
        };
        assert_eq!(first, crate::query::ast::Operand::TypeRef(tag_type));
        assert_eq!(op, BasicOp::Eq);
        assert_eq!(range, DateTimeRange::parse_slot_glob("*-02-01").unwrap());
    }

    // --- ItemIdFn::expand の glob 翻訳 ---

    #[test]
    fn test_item_id_expand_glob_origin_star_translates_to_block_range() {
        use crate::query::ast::{BasicOp, ComparisonOp, Operand};
        use crate::types::{Bitical, Label, Origin, SType, TagType, TypedTag};
        let tag_type = TagType::from(SType::ItemId);
        let label = Label::Other(
            tag_type.clone(),
            Bitical::String("File(*)".to_string()),
        );
        let typed_tag = TypedTag::retag(SType::ItemId, &label);
        let result = ItemIdFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &crate::query::lens_schema::Lens::base_standard(),
        ).unwrap();
        let QueryNode::And(nodes) = result else {
            panic!("expected And, got {:?}", result)
        };
        assert_eq!(nodes.len(), 2);
        let mut ge = None;
        let mut le = None;
        for n in &nodes {
            let QueryNode::Comparison(c) = n else {
                panic!("expected Comparison, got {:?}", n)
            };
            let (op, val) = &c.rest[0];
            let Operand::Literal(Label::ItemId(id)) = val else {
                panic!("expected Label::ItemId literal, got {:?}", val)
            };
            match op {
                ComparisonOp::Label(BasicOp::Ge) => ge = Some(*id),
                ComparisonOp::Label(BasicOp::Le) => le = Some(*id),
                other => panic!("unexpected op {:?}", other),
            }
        }
        assert_eq!(ge, Some(Origin::File.block_lo()));
        assert_eq!(le, Some(Origin::File.block_hi() - 1));
    }

    // --- IsDirFn::expand の glob 翻訳 ---

    #[test]
    fn test_isdir_expand_glob_star_translates_to_true_or_false() {
        use crate::query::ast::{BasicOp, ComparisonOp, Operand};
        use crate::types::{Bitical, Label, SType, TagType, TypedTag};
        let tag_type = TagType::from(SType::IsDir);
        let label =
            Label::Other(tag_type.clone(), Bitical::String("*".to_string()));
        let typed_tag = TypedTag::retag(SType::IsDir, &label);
        let result = IsDirFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &crate::query::lens_schema::Lens::base_standard(),
        ).unwrap();
        let QueryNode::Or(nodes) = result else {
            panic!("expected Or, got {:?}", result)
        };
        assert_eq!(nodes.len(), 2);
        let bools: Vec<bool> = nodes
            .iter()
            .map(|n| {
                let QueryNode::Comparison(c) = n else {
                    panic!("expected Comparison, got {:?}", n)
                };
                assert_eq!(c.rest[0].0, ComparisonOp::Label(BasicOp::Eq));
                let Operand::Literal(Label::IsDir(b)) = &c.rest[0].1 else {
                    panic!(
                        "expected Label::IsDir literal, got {:?}",
                        c.rest[0].1
                    )
                };
                *b
            })
            .collect();
        assert!(bools.contains(&true));
        assert!(bools.contains(&false));
    }

    // --- IsDirFn::expand_comparison の glob 翻訳 ---

    #[test]
    fn test_isdir_expand_comparison_glob_eq_enumerates_matching_boolean() {
        use crate::query::ast::{
            BasicOp, ComparisonNode, ComparisonOp, Operand,
        };
        use crate::types::{Bitical, Label, SType, TagType};
        let node = ComparisonNode {
            first: Operand::TypeRef(TagType::from(SType::IsDir)),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Eq),
                Operand::Literal(Label::Other(
                    TagType::from(SType::IsDir),
                    Bitical::String("tr*".to_string()),
                )),
            )],
        };
        let result = IsDirFn.query().expand_comparison(node).unwrap();
        let QueryNode::Comparison(c) = result else {
            panic!("expected Comparison, got {:?}", result)
        };
        assert_eq!(c.rest[0].0, ComparisonOp::Label(BasicOp::Eq));
        assert_eq!(c.rest[0].1, Operand::Literal(Label::IsDir(true)));
    }

    #[test]
    fn test_isdir_expand_comparison_glob_ne_enumerates_non_matching_boolean() {
        use crate::query::ast::{
            BasicOp, ComparisonNode, ComparisonOp, Operand,
        };
        use crate::types::{Bitical, Label, SType, TagType};
        let node = ComparisonNode {
            first: Operand::TypeRef(TagType::from(SType::IsDir)),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Ne),
                Operand::Literal(Label::Other(
                    TagType::from(SType::IsDir),
                    Bitical::String("tr*".to_string()),
                )),
            )],
        };
        let result = IsDirFn.query().expand_comparison(node).unwrap();
        let QueryNode::Comparison(c) = result else {
            panic!("expected Comparison, got {:?}", result)
        };
        assert_eq!(c.rest[0].0, ComparisonOp::Label(BasicOp::Eq));
        assert_eq!(c.rest[0].1, Operand::Literal(Label::IsDir(false)));
    }

    #[test]
    fn test_isdir_expand_comparison_full_match_glob_ne_becomes_false() {
        use crate::query::ast::{
            BasicOp, ComparisonNode, ComparisonOp, Operand,
        };
        use crate::types::{Bitical, Label, SType, TagType};
        let node = ComparisonNode {
            first: Operand::TypeRef(TagType::from(SType::IsDir)),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Ne),
                Operand::Literal(Label::Other(
                    TagType::from(SType::IsDir),
                    Bitical::String("*".to_string()),
                )),
            )],
        };
        let result = IsDirFn.query().expand_comparison(node).unwrap();
        assert_eq!(result, QueryNode::Or(vec![]));
    }

    #[test]
    fn test_isdir_expand_comparison_full_match_glob_order_ops_follow_domain_rule(
    ) {
        use crate::query::ast::{
            BasicOp, ComparisonNode, ComparisonOp, Operand,
        };
        use crate::types::{Bitical, Label, SType, TagType};
        let case = |op: BasicOp| {
            let node = ComparisonNode {
                first: Operand::TypeRef(TagType::from(SType::IsDir)),
                rest: vec![(
                    ComparisonOp::Label(op),
                    Operand::Literal(Label::Other(
                        TagType::from(SType::IsDir),
                        Bitical::String("*".to_string()),
                    )),
                )],
            };
            IsDirFn.query().expand_comparison(node).unwrap()
        };
        // Gt/Lt は全域 glob に対して 0 件（FALSE）
        assert_eq!(case(BasicOp::Gt), QueryNode::Or(vec![]));
        assert_eq!(case(BasicOp::Lt), QueryNode::Or(vec![]));
        // Ge/Le は全域 glob に対して全件（TRUE）
        assert_eq!(case(BasicOp::Ge), QueryNode::And(vec![]));
        assert_eq!(case(BasicOp::Le), QueryNode::And(vec![]));
    }

    // --- Phase 5: MtimeFn::expand_comparison ---

    #[test]
    fn test_mtime_expand_comparison_date_label_gt() {
        use crate::query::ast::{
            BasicOp, ComparisonNode, ComparisonOp, Operand,
        };
        use crate::types::{DateTime, Label, SType, TagType};
        use chrono::NaiveDate;
        let date = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let dt = DateTime::Date(date);
        let node = ComparisonNode {
            first: Operand::TypeRef(TagType::from(SType::Mtime)),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Gt),
                Operand::Literal(Label::Date(dt.clone())),
            )],
        };
        let result = MtimeFn.query().expand_comparison(node).unwrap();
        let QueryNode::DateTimeRange { first, op, range } = result else {
            panic!("expected DateTimeRange, got {:?}", result)
        };
        assert_eq!(first, Operand::TypeRef(TagType::from(SType::Mtime)));
        assert_eq!(op, BasicOp::Gt);
        assert_eq!(
            range.as_interval(),
            dt.to_interval().unwrap().as_interval()
        );
    }

    #[test]
    fn test_mtime_expand_comparison_date_label_eq_is_date_time_range() {
        use crate::query::ast::{
            BasicOp, ComparisonNode, ComparisonOp, Operand,
        };
        use crate::types::{DateTime, Label, SType, TagType};
        use chrono::NaiveDate;
        let date = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let dt = DateTime::Date(date);
        let node = ComparisonNode {
            first: Operand::TypeRef(TagType::from(SType::Mtime)),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Eq),
                Operand::Literal(Label::Date(dt)),
            )],
        };
        let result = MtimeFn.query().expand_comparison(node).unwrap();
        let QueryNode::DateTimeRange { op, .. } = result else {
            panic!("expected DateTimeRange for Eq, got {:?}", result)
        };
        assert_eq!(op, BasicOp::Eq);
    }

    #[test]
    fn test_mtime_expand_comparison_aggregation_wrapped_date_preserves_first() {
        // first が単純な TypeRef でない場合（例: max(mtime:) < 2026-02-01）でも、
        // DateTimeRange.first に集約の構造がそのまま保持される必要がある
        // （集約が消えて単純タグ一致に化ける回帰を test_fetch_boolean が実データで検出した）。
        use crate::query::ast::{
            AggregationNode, ArithmeticAggOp, BasicOp, ComparisonNode,
            ComparisonOp, Operand,
        };
        use crate::types::{DateTime, Label, SType, TagType};
        use chrono::NaiveDate;
        let date = NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let dt = DateTime::Date(date);
        let agg = Operand::Aggregation(Box::new(AggregationNode::Arithmetic {
            op: ArithmeticAggOp::Max,
            inner: Box::new(QueryNode::Projection(Operand::TypeRef(
                TagType::from(SType::Mtime),
            ))),
        }));
        let node = ComparisonNode {
            first: agg.clone(),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Lt),
                Operand::Literal(Label::Date(dt.clone())),
            )],
        };
        let result = MtimeFn.query().expand_comparison(node).unwrap();
        let QueryNode::DateTimeRange { first, op, range } = result else {
            panic!(
                "expected DateTimeRange preserving the aggregation first, got {:?}",
                result
            )
        };
        assert_eq!(first, agg, "first（集約）が保持されているべき");
        assert_eq!(op, BasicOp::Lt);
        assert_eq!(
            range.as_interval(),
            dt.to_interval().unwrap().as_interval()
        );
    }

    #[test]
    fn test_mtime_expand_comparison_slot_glob_direct_tag_becomes_date_time_range(
    ) {
        use crate::query::ast::{
            BasicOp, ComparisonNode, ComparisonOp, Operand,
        };
        use crate::types::{Bitical, DateTimeRange, SType, TagType};
        let node = ComparisonNode {
            first: Operand::TypeRef(TagType::from(SType::Mtime)),
            rest: vec![(
                ComparisonOp::Label(BasicOp::Gt),
                Operand::Literal(Label::Other(
                    TagType::from(SType::Mtime),
                    Bitical::String("*-02-01".to_string()),
                )),
            )],
        };
        let result = MtimeFn.query().expand_comparison(node).unwrap();
        let QueryNode::DateTimeRange { first, op, range } = result else {
            panic!("expected DateTimeRange, got {:?}", result)
        };
        assert_eq!(first, Operand::TypeRef(TagType::from(SType::Mtime)));
        assert_eq!(op, BasicOp::Gt);
        assert_eq!(range, DateTimeRange::parse_slot_glob("*-02-01").unwrap());
    }

    // --- TypeFn::expand: Literal=完全一致検索 / String=glob検索 の振り分け ---

    fn expand_type_tag(label: Label) -> QueryNode {
        use crate::query::lens_schema::Lens;
        use crate::types::{SType, TagType, TypedTag};
        let tag_type = TagType::from(SType::Type);
        let tag = TypedTag {
            label: label.clone(),
        };
        let registry = TagRegistry::with_standard();
        let lens = Lens::from_registry(&registry);
        TypeFn.query().expand(&tag_type, &label, &tag, &lens).unwrap()
    }

    #[test]
    fn test_type_expand_string_pattern_bakes_registered_candidates() {
        use crate::types::ItemId;
        // `type:*`（unquoted String パターン）は glob検索。
        // schema の登録型名＋default_rank＋固定 Sys id が candidates になる。
        let result = expand_type_tag(Label::from("*"));
        let QueryNode::DefinitionRef(DefinitionRef { candidates, .. }) = result
        else {
            panic!("expected DefinitionRef, got {:?}", result)
        };
        let registry = TagRegistry::with_standard();
        let hash_rank = registry.get("hash").unwrap().default_rank();
        let filename_rank = registry.get("filename").unwrap().default_rank();
        let hash_sys_id = Origin::Builtin.block_lo()
            + registry.builtin_offset("hash").unwrap() as i64;
        let filename_sys_id = Origin::Builtin.block_lo()
            + registry.builtin_offset("filename").unwrap() as i64;
        assert!(
            candidates.contains(&Candidate {
                name: "hash".to_string(),
                rank: hash_rank,
                id: ItemId::Stored(hash_sys_id)
            }),
            "candidates should include unused registered type 'hash' with its fixed Sys id, got {:?}",
            candidates
        );
        assert!(
            candidates.contains(&Candidate {
                name: "filename".to_string(),
                rank: filename_rank,
                id: ItemId::Stored(filename_sys_id)
            }),
            "candidates should include registered type 'filename' with its fixed Sys id, got {:?}",
            candidates
        );
    }

    // プラグイン登録型（register_plugin）は candidates に載るが固定 Sys id は持たない。
    #[test]
    fn test_type_expand_string_pattern_plugin_candidate_has_no_sys_id() {
        use crate::query::lens_schema::Lens;
        use crate::types::{Bitical, ItemId, SType, TagType, TypedTag};
        let mut registry = TagRegistry::with_standard();
        registry.register_plugin(QueryTag);
        let lens = Lens::from_registry(&registry);
        let tag_type = TagType::from(SType::Type);
        let label =
            Label::resolve(tag_type.clone(), Bitical::String("*".to_string()));
        let tag = TypedTag::retag(SType::Type, &label);
        let result = TypeFn.query().expand(&tag_type, &label, &tag, &lens).unwrap();
        let QueryNode::DefinitionRef(DefinitionRef { candidates, .. }) = result
        else {
            panic!("expected DefinitionRef, got {:?}", result)
        };
        let qtest_rank = registry.get("qtest").unwrap().default_rank();
        assert!(
            candidates.iter().any(|c| {
                c.name == "qtest"
                    && c.rank == qtest_rank
                    && matches!(c.id, ItemId::Settling(Origin::Plugin, _))
            }),
            "plugin-registered candidate should have no fixed Sys id, got {:?}",
            candidates
        );
    }

    #[test]
    fn test_type_expand_literal_is_exact_match_lookup_with_registry_candidates()
    {
        use crate::types::{ItemId, SType, TagType};
        // `type:"filename"`（quoted Literal）は完全一致検索だが、Stored/Volatile
        // の区別（固定 Sys id を持つ組み込みかどうか）のため、registry 由来の
        // candidates は glob検索と同様に含める。
        let result = expand_type_tag(Label::Literal(
            TagType::from(SType::Type),
            "filename".to_string(),
        ));
        let QueryNode::DefinitionRef(DefinitionRef { candidates, .. }) = result
        else {
            panic!("expected DefinitionRef, got {:?}", result)
        };
        let registry = TagRegistry::with_standard();
        let filename_rank = registry.get("filename").unwrap().default_rank();
        let filename_sys_id = Origin::Builtin.block_lo()
            + registry.builtin_offset("filename").unwrap() as i64;
        assert!(
            candidates.contains(&Candidate {
                name: "filename".to_string(),
                rank: filename_rank,
                id: ItemId::Stored(filename_sys_id)
            }),
            "exact-match lookup should still recognize a registered built-in's fixed Sys id, got {:?}",
            candidates
        );
    }

    #[test]
    fn test_typed_tag_expand_string_pattern_has_no_registry_candidates() {
        use crate::query::lens_schema::Lens;
        use crate::types::{Bitical, SType, TagType, TypedTag};
        // `tag:*` のソースは Stored 定義行と使用中ペアのみ。
        // registry は型のソースなので tag: の candidates にはならない。
        let tag_type = TagType::from(SType::TypedTag);
        let label =
            Label::resolve(tag_type.clone(), Bitical::String("*".to_string()));
        let tag = TypedTag::retag(SType::TypedTag, &label);
        let registry = TagRegistry::with_standard();
        let lens = Lens::from_registry(&registry);
        let result = TypedTagFn.query().expand(&tag_type, &label, &tag, &lens).unwrap();
        let QueryNode::DefinitionRef(DefinitionRef { candidates, .. }) = result
        else {
            panic!("expected DefinitionRef, got {:?}", result)
        };
        assert!(
            candidates.is_empty(),
            "tag: has no registry source, got {:?}",
            candidates
        );
    }

    // --- OriginFn::expand ---

    #[test]
    fn test_origin_expand_system_alias_becomes_or_of_small_classification() {
        use crate::query::lens_schema::Lens;
        use crate::types::{Bitical, SType, TagType, TypedTag};
        let tag_type = TagType::from(SType::Origin);
        let label = Label::resolve(
            tag_type.clone(),
            Bitical::String("system".to_string()),
        );
        let typed_tag = TypedTag::retag(SType::Origin, &label);
        let result = OriginFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &Lens::base_standard(),
        ).unwrap();
        let QueryNode::Or(nodes) = result else {
            panic!("expected Or, got {:?}", result)
        };
        let mut labels = Vec::new();
        for node in &nodes {
            match node {
                QueryNode::ColumnMatch { label, .. } => {
                    labels.push(label.as_str().to_string());
                }
                QueryNode::Or(sub_nodes) => {
                    for sub in sub_nodes {
                        if let QueryNode::ColumnMatch { label, .. } = sub {
                            labels.push(label.as_str().to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        assert_eq!(labels.len(), 3);
        assert!(labels.contains(&"builtin".to_string()));
        assert!(labels.contains(&"file".to_string()));
        assert!(labels.contains(&"plugin".to_string()));
    }

    #[test]
    fn test_origin_expand_non_alias_label_is_direct_column_match() {
        use crate::query::lens_schema::Lens;
        use crate::types::{Bitical, SType, TagType, TypedTag};
        let tag_type = TagType::from(SType::Origin);
        let label = Label::resolve(
            tag_type.clone(),
            Bitical::String("user".to_string()),
        );
        let typed_tag = TypedTag::retag(SType::Origin, &label);
        let result = OriginFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &Lens::base_standard(),
        ).unwrap();
        let QueryNode::ColumnMatch { tag, label: l } = result else {
            panic!("expected ColumnMatch, got {:?}", result)
        };
        assert_eq!(tag, SType::Origin);
        assert_eq!(l.as_str(), "user");
    }

    #[test]
    fn test_origin_expand_glob_generates_definition_ref_and_column_match() {
        use crate::query::lens_schema::Lens;
        use crate::types::{Bitical, SType, TagType, TypedTag};
        let tag_type = TagType::from(SType::Origin);
        let label =
            Label::resolve(tag_type.clone(), Bitical::String("b*".to_string()));
        let typed_tag = TypedTag::retag(SType::Origin, &label);
        let result = OriginFn.query().expand(
            &tag_type,
            &label,
            &typed_tag,
            &Lens::base_standard(),
        ).unwrap();
        let QueryNode::Or(nodes) = result else {
            panic!("expected Or, got {:?}", result)
        };
        assert_eq!(nodes.len(), 2);

        let has_column_match = nodes
            .iter()
            .any(|n| matches!(n, QueryNode::ColumnMatch { .. }));
        let has_definition_ref = nodes
            .iter()
            .any(|n| matches!(n, QueryNode::DefinitionRef(_)));
        assert!(has_column_match, "should contain ColumnMatch");
        assert!(has_definition_ref, "should contain DefinitionRef");
    }
}
