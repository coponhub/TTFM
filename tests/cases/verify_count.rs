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

#[cfg(test)]
mod tests {
    use std::fs;
    use tempfile::tempdir;
    use ttfm::search;
    use ttfm::SearchOptions;

    #[test]
    fn test_verify_count_logic() {
        let db_dir = tempdir().unwrap();
        let index_dir = tempdir().unwrap();

        let registry = ttfm::tag::TagRegistry::with_standard();
        let store = ttfm::db::Store::open(db_dir.path()).unwrap();
        ttfm::indexing::Indexer::new(&store, &registry).initialize_tables().unwrap();

        for i in 1..=5 {
            fs::write(
                index_dir.path().join(format!("file{}.txt", i)),
                "content",
            )
            .unwrap();
        }

        ttfm::indexing::Indexer::new(&store, &registry)
            .run_single(index_dir.path(), None::<&fn(usize)>, false)
            .unwrap();

        let res_comp = search::search_nowarn(&store, &registry, "count()", SearchOptions::default()).unwrap();
        println!("count() result: {}", res_comp.results[0].raw_repr());

        let res_wild =
            search::search_nowarn(&store, &registry, "count(*:*)", SearchOptions::default()).unwrap();
        println!("count(*:*) result: {}", res_wild.results[0].raw_repr());

        let items = search::search_nowarn(&store, &registry, "path:", SearchOptions::default()).unwrap();
        println!(
            "Total items found by path: projection: {}",
            items.results.len()
        );
    }
}
