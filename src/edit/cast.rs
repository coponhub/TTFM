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

use crate::db::{Col, Store, TargetTable, Tbl};
use crate::types::{Bitical, BiticalType, TagType};
use crate::util;
use anyhow::{bail, Result};
use sea_query::{Expr, PostgresQueryBuilder, Query};
use std::str::FromStr;

impl Bitical {
    pub(crate) fn parse_as(s: &str, target_type: BiticalType) -> Option<Self> {
        match target_type {
            BiticalType::String => Some(Bitical::String(s.to_string())),
            BiticalType::Integer => s.parse::<i64>().ok().map(Bitical::Integer),
            BiticalType::Double => s.parse::<f64>().ok().map(Bitical::Double),
            BiticalType::Boolean => match s.to_lowercase().as_str() {
                "true" => Some(Bitical::Boolean(true)),
                "false" => Some(Bitical::Boolean(false)),
                _ => None,
            },
            BiticalType::Uuid => {
                uuid::Uuid::from_str(s).ok().map(Bitical::Uuid)
            }
        }
    }
}

pub fn pre_validate_cast(
    store: &Store,
    tag_type: &TagType,
    target_type: BiticalType,
) -> Result<usize> {
    let ut_path = store.path_for_target(TargetTable::UserTags);
    if !ut_path.exists() {
        return Ok(0);
    }

    let ut_sub = util::parquet_query(&ut_path.to_string_lossy());
    let query = Query::select()
        .expr(BiticalType::label_coalesce_expr())
        .from_subquery(ut_sub, Tbl::UserTags)
        .and_where(Expr::col(Col::Type).eq(tag_type.as_str()))
        .to_string(PostgresQueryBuilder);

    let mut stmt = store.conn.prepare(&query)?;
    let rows = stmt.query_map([], |r| {
        let val: Option<String> = r.get(0)?;
        Ok(val)
    })?;

    let mut invalid_labels = Vec::new();
    let mut count = 0;

    for row in rows {
        let Some(raw_str) = row? else { continue };
        count += 1;
        if Bitical::parse_as(&raw_str, target_type).is_none() {
            invalid_labels.push(raw_str);
        }
    }

    if !invalid_labels.is_empty() {
        let msg = invalid_labels.join(", ");
        bail!("Cast aborted due to invalid labels: [{}]", msg);
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bitical_parse_as() {
        assert_eq!(
            Bitical::parse_as("100", BiticalType::Integer),
            Some(Bitical::Integer(100))
        );
        assert_eq!(Bitical::parse_as("invalid", BiticalType::Integer), None);
        assert_eq!(
            Bitical::parse_as("true", BiticalType::Boolean),
            Some(Bitical::Boolean(true))
        );
        assert_eq!(
            Bitical::parse_as("hello", BiticalType::String),
            Some(Bitical::String("hello".to_string()))
        );
    }

    #[test]
    fn test_pre_validate_cast_empty_db() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let count = pre_validate_cast(
            &store,
            &TagType::from("score"),
            BiticalType::Integer,
        )
        .unwrap();
        assert_eq!(count, 0);
    }
}
