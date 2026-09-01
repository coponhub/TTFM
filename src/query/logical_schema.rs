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
use crate::types::{Bitical, ItemId, Label, Rank, TagType};

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

    /// この論理型がマップされる物理型。Float は DOUBLE へ、
    /// 型が定まらない Any は文字列（VARCHAR）へ収束する。
    pub fn to_bitical(&self) -> crate::db::BiticalType {
        use crate::db::BiticalType;
        match self {
            Self::Integer => BiticalType::Integer,
            Self::Float => BiticalType::Double,
            Self::String => BiticalType::String,
            Self::Boolean => BiticalType::Boolean,
            Self::Any => BiticalType::String,
        }
    }
}

impl Bitical {
    /// この値の変種が表す LogicalType。表記の解釈は行わない
    /// （`"1MB"` のような文字列の読み取りは `OperandFormat` の担当）。
    pub(crate) fn infer_logical_type(&self) -> LogicalType {
        match self {
            Bitical::Integer(_) => LogicalType::Integer,
            Bitical::Boolean(_) => LogicalType::Boolean,
            Bitical::Double(_) => LogicalType::Float,
            Bitical::Uuid(_) | Bitical::String(_) => LogicalType::String,
        }
    }
}

/// 論理的なスキーマ情報を提供するインターフェース。
pub trait LogicalSchema {
    fn get_logical_type(&self, tag: &TagType) -> LogicalType;
    /// 値がその型として解釈できない場合はエラーを返す。
    fn expand_tag(
        &self,
        tag_type: &TagType,
        label: &Label,
    ) -> anyhow::Result<QueryNode>;
    fn expand_projection(&self, tag_type: &TagType) -> QueryNode;
    /// 比較ノードを展開する（日付範囲化・ラベル正規化等）。
    /// 値がその型として解釈できない場合はエラーを返す。
    fn expand_comparison(
        &self,
        node: ComparisonNode,
    ) -> anyhow::Result<QueryNode>;
    /// 登録済みタグの型名と default_rank を列挙する。
    /// 3要素目は出所を表す ItemId: 組み込み（`with_standard` の Rust
    /// TagFunction）は固定 Sys id を持つ `Stored`、プラグイン登録型は
    /// `Settling(Origin::Plugin, _)`（未確定の counter はダミー値で構わない。
    /// このリストは SQL 生成の使い捨てで write の実解決には流れない）。
    fn iter_all_for_rank(&self) -> Vec<(TagType, Rank, ItemId)>;
    /// その型が定義アイテムを指すなら、その ItemKind を返す。
    fn item_kind(&self, tag_type: &TagType) -> Option<crate::types::ItemKind>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::BiticalType;

    #[test]
    fn test_to_bitical_mapping() {
        assert_eq!(LogicalType::Integer.to_bitical(), BiticalType::Integer);
        assert_eq!(LogicalType::Float.to_bitical(), BiticalType::Double);
        assert_eq!(LogicalType::String.to_bitical(), BiticalType::String);
        assert_eq!(LogicalType::Boolean.to_bitical(), BiticalType::Boolean);
        assert_eq!(LogicalType::Any.to_bitical(), BiticalType::String);
    }
}
