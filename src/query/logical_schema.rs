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

//! # 論理スキーマ（LogicalSchema）
//!
//! クエリエンジンの論理レイヤーが依存するスキーマインターフェース。
//! タグの型情報・展開ロジック・ラベル正規化を抽象化する。
//!
//! ## 実装
//!
//! - `Lens` (`lens_schema.rs`): 標準実装。TagRegistry から構築。
//! - テスト用モックなど任意の実装も可能。

use crate::query::ast::{ComparisonNode, QueryNode};
use crate::types::{Label, TagType};

/// クエリエンジンの論理レイヤーで扱う型。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LogicalType {
    Integer,
    Float,
    String,
    Boolean,
    Any,
}

impl LogicalType {
    pub fn is_numeric(&self) -> bool {
        matches!(self, Self::Integer | Self::Float | Self::Boolean)
    }
}

/// 論理的なスキーマ情報を提供するインターフェース。
pub trait LogicalSchema {
    fn get_logical_type(&self, tag: &TagType) -> LogicalType;
    fn expand_tag(&self, tag_type: &TagType, label: &Label) -> QueryNode;
    fn expand_projection(&self, tag_type: &TagType) -> QueryNode;
    /// リテラル値（サイズ単位・日付文字列等）を正規化する。Literal は変換しない。
    fn normalize_label_any(&self, label: &Label) -> Label;
    /// 比較ノードを展開する（日付範囲化・ラベル正規化等）。
    fn expand_comparison(&self, node: ComparisonNode) -> QueryNode;
}
