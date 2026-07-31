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

use super::nest::filter;
use super::{
    build_column_match_sql, build_label_set_op_pick_sql,
    build_resolved_and_sql, build_resolved_date_time_match_sql,
    build_resolved_diff_sql, build_resolved_match_sql, build_resolved_or_sql,
    build_resolved_tag_tag_match_sql, build_scalar_match_sql,
};
use crate::db::Src;
use crate::query::lens_resolver::ResolvedNode;
use sea_query::SelectStatement;

pub(super) fn try_dispatch_common(
    src: &Src,
    node: &ResolvedNode,
    child_sqls: Vec<SelectStatement>,
) -> Result<SelectStatement, Vec<SelectStatement>> {
    match node {
        ResolvedNode::And(_) => Ok(build_resolved_and_sql(src, child_sqls)),
        ResolvedNode::Or(_) => Ok(build_resolved_or_sql(src, child_sqls)),
        ResolvedNode::Difference(_, _) => {
            let [l, r]: [SelectStatement; 2] = child_sqls.try_into().unwrap();
            Ok(build_resolved_diff_sql(l, r))
        }
        ResolvedNode::LabelSetOp { op, .. } => {
            Ok(build_label_set_op_pick_sql(op, child_sqls))
        }
        ResolvedNode::Nest { keys, .. } => {
            Ok(filter(src, keys, child_sqls.into_iter().next()))
        }
        ResolvedNode::ColumnMatch { tag, label } => {
            Ok(build_column_match_sql(src, *tag, label))
        }
        ResolvedNode::DefinitionRef {
            def, default_rank, ..
        } => Ok(crate::query::lens_builder::filter_definitions(
            src,
            def,
            *default_rank,
        )),
        ResolvedNode::Match {
            storage,
            bitical_type,
            op,
            label,
            ..
        } => Ok(build_resolved_match_sql(
            src,
            storage,
            *bitical_type,
            *op,
            label,
        )),
        ResolvedNode::TagTagMatch {
            left_storage,
            left_sql_type,
            op,
            right_storage,
            right_sql_type,
        } => Ok(build_resolved_tag_tag_match_sql(
            src,
            left_storage,
            *left_sql_type,
            *op,
            right_storage,
            *right_sql_type,
        )),
        ResolvedNode::ScalarMatch { left, op, right } => {
            Ok(build_scalar_match_sql(src, left, *op, right))
        }
        ResolvedNode::DateTimeMatch { storage, op, range, .. } => {
            Ok(build_resolved_date_time_match_sql(src, storage, *op, range))
        }
        _ => Err(child_sqls),
    }
}
