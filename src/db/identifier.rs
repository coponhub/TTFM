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

//! item_id の採番と入力正規化を所有する抽象。
//!
//! 区画レイアウト（Origin → 区画 index / lo / hi / short ラベル）は
//! `types::Origin` が唯一の定義点。このモジュールは DB アクセスが必要な
//! 採番（attach）と文字列入力の正規化（parse）のみを持つ。
//! 逆引き・表示は `Origin::within` / `Origin::short` / `Origin::block_lo` を直接使う。

use crate::db::{Col, DuckDbFunc, Store, TargetTable, Tbl};
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
        let origin =
            Origin::iter().find(|o| o.short() == label).ok_or_else(|| {
                anyhow::anyhow!("unknown item_id origin: {label}")
            })?;
        return origin.id_at_offset(offset).ok_or_else(|| {
            anyhow::anyhow!("item_id offset outside {label} block: {s}")
        });
    }
    s.parse()
        .map_err(|_| anyhow::anyhow!("invalid item_id: {s}"))
}

/// origin の区画を採番ソースとして読む (TargetTable, 別名) の組。
fn source_tables(origin: Origin) -> &'static [(TargetTable, Tbl)] {
    match origin {
        Origin::File => &[
            (TargetTable::FileReferences, Tbl::FileReferences),
            (TargetTable::RemovedFiles, Tbl::RemovedFiles),
        ],
        Origin::Builtin | Origin::User | Origin::Plugin => {
            &[(TargetTable::ItemReferences, Tbl::ItemReferences)]
        }
    }
}

/// origin の区画 `[lo, hi)` 内の現在の最大 item_id を読む。
/// 区画内に行が無ければ `lo - 1` を返す（→ 採番は lo から始まる）。
fn max_in_block(store: &Store, origin: Origin) -> Result<i64> {
    let lo = origin.block_lo();
    let hi = origin.block_hi();
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
pub fn next(store: &Store, origin: Origin, count: usize) -> Result<Vec<i64>> {
    let start = max_in_block(store, origin)? + 1;
    Ok((0..count as i64).map(|i| start + i).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 区画幅 B = 2^58。テスト側でも独立に算出して定数を裏取りする。
    const B: i64 = 1 << 58;

    fn fmt_id(id: i64) -> String {
        let o = Origin::within(id);
        format!("{}({})", o.short(), id - o.block_lo())
    }

    #[test]
    fn block_builtin_owns_negative_side() {
        // Builtin: index -1 → lo = -B, hi = 0 (User の lo)
        assert_eq!(Origin::Builtin.block_lo(), -B);
        assert_eq!(Origin::Builtin.block_hi(), 0);
    }

    #[test]
    fn block_user_owns_zero_to_file() {
        assert_eq!(Origin::User.block_lo(), 0);
        assert_eq!(Origin::User.block_hi(), 8 * B);
    }

    #[test]
    fn block_file_owns_8b_to_16b() {
        assert_eq!(Origin::File.block_lo(), 8 * B);
        assert_eq!(Origin::File.block_hi(), 16 * B);
    }

    #[test]
    fn block_plugin_owns_16b_to_i64_max() {
        assert_eq!(Origin::Plugin.block_lo(), 16 * B);
        assert_eq!(Origin::Plugin.block_hi(), i64::MAX);
    }

    #[test]
    fn blocks_tile_without_overlap() {
        let (slo, shi) =
            (Origin::Builtin.block_lo(), Origin::Builtin.block_hi());
        let (ulo, uhi) = (Origin::User.block_lo(), Origin::User.block_hi());
        let (flo, fhi) = (Origin::File.block_lo(), Origin::File.block_hi());
        let (plo, _) = (Origin::Plugin.block_lo(), Origin::Plugin.block_hi());
        assert_eq!(shi, ulo); // Builtin → User 隙間なし
        assert_eq!(uhi, flo); // User → File 隙間なし
        assert_eq!(fhi, plo); // File → Plugin 隙間なし
        let _ = slo; // -B は定義上の下端
    }

    #[test]
    fn short_labels() {
        assert_eq!(Origin::Builtin.short(), "Sys");
        assert_eq!(Origin::User.short(), "User");
        assert_eq!(Origin::File.short(), "File");
        assert_eq!(Origin::Plugin.short(), "Plg");
    }

    #[test]
    fn parse_rejects_offset_outside_the_block() {
        assert!(
            parse(&format!("File({})", i64::MAX)).is_err(),
            "an offset that overflows must error, not panic"
        );
        assert!(parse("File(-1)").is_err());
        assert!(parse(&format!("File({})", 8 * B)).is_err());
        assert_eq!(parse(&format!("File({})", 8 * B - 1)).unwrap(), 16 * B - 1);
    }

    #[test]
    fn within_inside_builtin_block() {
        // Builtin: [-B, 0)
        assert_eq!(Origin::within(-B), Origin::Builtin);
        assert_eq!(Origin::within(-5), Origin::Builtin);
        assert_eq!(Origin::within(-1), Origin::Builtin);
    }

    #[test]
    fn within_inside_user_block() {
        assert_eq!(Origin::within(0), Origin::User);
        assert_eq!(Origin::within(10), Origin::User);
        assert_eq!(Origin::within(B - 1), Origin::User);
    }

    #[test]
    fn within_inside_file_block() {
        assert_eq!(Origin::within(8 * B), Origin::File);
        assert_eq!(Origin::within(8 * B + 10), Origin::File);
        assert_eq!(Origin::within(15 * B), Origin::File);
    }

    #[test]
    fn within_inside_plugin_block() {
        assert_eq!(Origin::within(16 * B), Origin::Plugin);
        assert_eq!(Origin::within(16 * B + 10), Origin::Plugin);
        assert_eq!(Origin::within(30 * B), Origin::Plugin);
    }

    #[test]
    fn within_below_all_blocks_falls_to_builtin() {
        // -B 未満は全区画より下 → lo 最小 (Builtin=-B) に縮退
        assert_eq!(Origin::within(-B - 1), Origin::Builtin);
    }

    #[test]
    fn within_gap_maps_to_block_directly_below() {
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
        assert_eq!(fmt_id(16 * B), "Plg(0)");
        assert_eq!(fmt_id(16 * B + 10), "Plg(10)");
    }

    #[test]
    fn parse_local_form() {
        assert_eq!(parse("Sys(10)").unwrap(), -B + 10);
        assert_eq!(parse("Sys(0)").unwrap(), -B);
        assert_eq!(parse("User(10)").unwrap(), 10);
        assert_eq!(parse("User(0)").unwrap(), 0);
        assert_eq!(parse("File(0)").unwrap(), 8 * B);
        assert_eq!(parse("File(10)").unwrap(), 8 * B + 10);
        assert_eq!(parse("Plg(0)").unwrap(), 16 * B);
        assert_eq!(parse("Plg(10)").unwrap(), 16 * B + 10);
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
        for id in [
            0_i64,
            10,
            B - 1,
            8 * B,
            8 * B + 10,
            -B,
            -B + 5,
            16 * B,
            16 * B + 10,
        ] {
            assert_eq!(parse(&fmt_id(id)).unwrap(), id);
        }
    }
}
