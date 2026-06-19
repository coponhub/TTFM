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

//! item_id の採番と入力正規化を所有する抽象。
//!
//! 区画レイアウト（Origin → 区画 index / lo / hi / short ラベル）は
//! `types::Origin` が唯一の定義点。このモジュールは DB アクセスが必要な
//! 採番（attach）と文字列入力の正規化（parse）のみを持つ。
//! 逆引き・表示は `Origin::within` / `Origin::short` / `Origin::space_lo` を直接使う。

use crate::db::{Col, DuckDbFunc, Store, Tbl, TargetTable};
use crate::types::Origin;
use crate::util;
use anyhow::{bail, Result};
use sea_query::{Expr, Func, PostgresQueryBuilder, Query};
use strum::IntoEnumIterator;

/// TTQL `item_id:` の値を raw id へ正規化する。
/// 生 id（`"123"` / 負値）もローカル形式（`"Sys(10)"` / `"User(10)"`）も受理する。
pub fn parse(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Some(open) = s.find('(') {
        let Some(rest) = s.strip_suffix(')') else {
            bail!("malformed item_id local form: {s}");
        };
        let label = &s[..open];
        let offset: i64 = rest[open + 1..]
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid item_id offset: {s}"))?;
        let origin = Origin::iter()
            .find(|o| o.short() == label)
            .ok_or_else(|| anyhow::anyhow!("unknown item_id origin: {label}"))?;
        return Ok(origin.space_lo() + offset);
    }
    s.parse()
        .map_err(|_| anyhow::anyhow!("invalid item_id: {s}"))
}

/// origin の区画を採番ソースとして読む (TargetTable, 別名) の組。
fn source_tables(origin: Origin) -> &'static [(TargetTable, Tbl)] {
    match origin {
        Origin::File => &[(TargetTable::FileReferences, Tbl::FileReferences)],
        Origin::System | Origin::User => &[(TargetTable::ItemReferences, Tbl::ItemReferences)],
    }
}

/// origin の区画 `[lo, hi)` 内の現在の最大 item_id を読む。
/// 区画内に行が無ければ `lo - 1` を返す（→ 採番は lo から始まる）。
fn max_in_space(store: &Store, origin: Origin) -> Result<i64> {
    let lo = origin.space_lo();
    let hi = origin.space_hi();
    let mut max = lo - 1;
    for (target, alias) in source_tables(origin) {
        let path = store.path_for_target(*target);
        if !path.exists() {
            continue;
        }
        let path_str = path.to_string_lossy();
        let sql = Query::select()
            .expr(Func::cust(DuckDbFunc::Coalesce).args([
                Expr::col(Col::ItemId).max().into(),
                Expr::val(lo - 1).into(),
            ]))
            .from_subquery(util::parquet_query(&path_str), *alias)
            .and_where(Expr::col(Col::ItemId).gte(lo))
            .and_where(Expr::col(Col::ItemId).lt(hi))
            .to_string(PostgresQueryBuilder);
        let m: i64 = store.conn.query_row(&sql, [], |r| r.get(0))?;
        max = max.max(m);
    }
    Ok(max)
}

/// origin の区画から次の count 個の item_id を採番する。
/// 区画内 MAX から **必ず +1 ずつ昇順**に連番（減算は無い）。区画が空なら
/// 下端 lo から。公称幅を超えても直上 origin の手前まで採番でき、エラーにしない。
pub fn attach(store: &Store, origin: Origin, count: usize) -> Result<Vec<i64>> {
    let start = max_in_space(store, origin)? + 1;
    Ok((0..count as i64).map(|i| start + i).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 区画幅 B = 2^58。テスト側でも独立に算出して定数を裏取りする。
    const B: i64 = 1 << 58;

    fn fmt_id(id: i64) -> String {
        let o = Origin::within(id);
        format!("{}({})", o.short(), id - o.space_lo())
    }

    #[test]
    fn space_system_owns_negative_side() {
        // System: index -1 → lo = -B, hi = 0 (User の lo)
        assert_eq!(Origin::System.space_lo(), -B);
        assert_eq!(Origin::System.space_hi(), 0);
    }

    #[test]
    fn space_user_owns_zero_to_file() {
        assert_eq!(Origin::User.space_lo(), 0);
        assert_eq!(Origin::User.space_hi(), 8 * B);
    }

    #[test]
    fn space_file_owns_up_to_i64_max() {
        assert_eq!(Origin::File.space_lo(), 8 * B);
        assert_eq!(Origin::File.space_hi(), i64::MAX);
    }

    #[test]
    fn spaces_tile_without_overlap() {
        let (slo, shi) = (Origin::System.space_lo(), Origin::System.space_hi());
        let (ulo, uhi) = (Origin::User.space_lo(), Origin::User.space_hi());
        let (flo, _)   = (Origin::File.space_lo(), Origin::File.space_hi());
        assert_eq!(shi, ulo); // System → User 隙間なし
        assert_eq!(uhi, flo); // User → File 隙間なし
        let _ = slo; // -B は定義上の下端
    }

    #[test]
    fn short_labels() {
        assert_eq!(Origin::System.short(), "Sys");
        assert_eq!(Origin::User.short(), "User");
        assert_eq!(Origin::File.short(), "File");
    }

    #[test]
    fn within_inside_system_space() {
        // System: [-B, 0)
        assert_eq!(Origin::within(-B), Origin::System);
        assert_eq!(Origin::within(-5), Origin::System);
        assert_eq!(Origin::within(-1), Origin::System);
    }

    #[test]
    fn within_inside_user_space() {
        assert_eq!(Origin::within(0), Origin::User);
        assert_eq!(Origin::within(10), Origin::User);
        assert_eq!(Origin::within(B - 1), Origin::User);
    }

    #[test]
    fn within_inside_file_space() {
        assert_eq!(Origin::within(8 * B), Origin::File);
        assert_eq!(Origin::within(8 * B + 10), Origin::File);
        assert_eq!(Origin::within(30 * B), Origin::File);
    }

    #[test]
    fn within_below_all_spaces_falls_to_system() {
        // -B 未満は全区画より下 → lo 最小 (System=-B) に縮退
        assert_eq!(Origin::within(-B - 1), Origin::System);
    }

    #[test]
    fn within_gap_maps_to_space_directly_below() {
        // gap [B, 8B) は User (lo=0 が直下)
        assert_eq!(Origin::within(B), Origin::User);
        assert_eq!(Origin::within(4 * B), Origin::User);
        assert_eq!(Origin::within(8 * B - 1), Origin::User);
    }

    #[test]
    fn display_uses_local_offset_form() {
        assert_eq!(fmt_id(10), "User(10)");
        assert_eq!(fmt_id(0), "User(0)");
        assert_eq!(fmt_id(-B), "Sys(0)");
        assert_eq!(fmt_id(-B + 10), "Sys(10)");
        assert_eq!(fmt_id(8 * B), "File(0)");
        assert_eq!(fmt_id(8 * B + 10), "File(10)");
    }

    #[test]
    fn parse_local_form() {
        assert_eq!(parse("Sys(10)").unwrap(), -B + 10);
        assert_eq!(parse("Sys(0)").unwrap(), -B);
        assert_eq!(parse("User(10)").unwrap(), 10);
        assert_eq!(parse("User(0)").unwrap(), 0);
        assert_eq!(parse("File(0)").unwrap(), 8 * B);
        assert_eq!(parse("File(10)").unwrap(), 8 * B + 10);
    }

    #[test]
    fn parse_raw_id() {
        assert_eq!(parse("123").unwrap(), 123);
        assert_eq!(parse("-5").unwrap(), -5);
        assert_eq!(parse(&(8 * B + 10).to_string()).unwrap(), 8 * B + 10);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse("Sys(abc)").is_err());
        assert!(parse("Bogus(10)").is_err());
        assert!(parse("hello").is_err());
        assert!(parse("Sys(10").is_err());
    }

    #[test]
    fn display_parse_roundtrip() {
        for id in [0_i64, 10, B - 1, 8 * B, 8 * B + 10, -B, -B + 5] {
            assert_eq!(parse(&fmt_id(id)).unwrap(), id);
        }
    }
}
