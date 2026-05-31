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

use super::{default_scope, inject_path_scope};

define_cases! {
    date_slash_format: {
        setup: |dir| {
            let file_path = dir.join("date_test.txt");
            std::fs::write(&file_path, "date test")?;
            let status = std::process::Command::new("touch")
                .args(["-d", "2024-01-01 12:00:00", file_path.to_str().unwrap()])
                .status()?;
            anyhow::ensure!(status.success(), "touch command failed");
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "mtime:2024/01/01",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Should match file with slash date");
            assert_eq!(res.results[0].raw_repr(), "date_test.txt");
            Ok(())
        },
    },
    date_hyphen_format: {
        setup: |dir| {
            let file_path = dir.join("date_test.txt");
            std::fs::write(&file_path, "date test")?;
            let status = std::process::Command::new("touch")
                .args(["-d", "2024-01-01 12:00:00", file_path.to_str().unwrap()])
                .status()?;
            anyhow::ensure!(status.success(), "touch command failed");
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "mtime:2024-01-01",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Should match file with hyphen date");
            assert_eq!(res.results[0].raw_repr(), "date_test.txt");
            Ok(())
        },
    },
    date_max_mtime: {
        setup: |dir| {
            let file_path = dir.join("date_test.txt");
            std::fs::write(&file_path, "date test")?;
            let status = std::process::Command::new("touch")
                .args(["-d", "2024-01-01 12:00:00", file_path.to_str().unwrap()])
                .status()?;
            anyhow::ensure!(status.success(), "touch command failed");
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "max(mtime:)",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            // raw_repr() is now mtime-formatted ("YYYY-MM-DD HH:MM"); use value tag for numeric check
            let value_strs = res.results[0].get_all_values("value");
            let val: f64 = value_strs[0].parse()?;
            assert!(val > 0.0);
            Ok(())
        },
    },
    date_min_mtime: {
        setup: |dir| {
            let file_path = dir.join("date_test.txt");
            std::fs::write(&file_path, "date test")?;
            let status = std::process::Command::new("touch")
                .args(["-d", "2024-01-01 12:00:00", file_path.to_str().unwrap()])
                .status()?;
            anyhow::ensure!(status.success(), "touch command failed");
            Ok(())
        },
        modify: None,
        format_query: inject_path_scope,
        query: "min(mtime:)",
        assert: |res, _dir| {
            assert!(!res.results.is_empty());
            // raw_repr() is now mtime-formatted ("YYYY-MM-DD HH:MM"); use value tag for numeric check
            let value_strs = res.results[0].get_all_values("value");
            let val: f64 = value_strs[0].parse()?;
            assert!(val > 0.0);
            Ok(())
        },
    },
    date_unquoted_time_query: {
        setup: |dir| {
            let file_path = dir.join("time_test.txt");
            std::fs::write(&file_path, "time test")?;
            let now = chrono::Local::now();
            let target_time = now.date_naive().and_hms_opt(12, 0, 0).unwrap();
            let status = std::process::Command::new("touch")
                .args(["-d", &target_time.format("%Y-%m-%d %H:%M:%S").to_string(), file_path.to_str().unwrap()])
                .status()?;
            anyhow::ensure!(status.success(), "touch command failed");
            Ok(())
        },
        modify: None,
        format_query: default_scope,
        query: "mtime:12:00",
        assert: |res, _dir| {
            assert!(!res.results.is_empty(), "Should match file with unquoted time '12:00'");
            assert_eq!(res.results[0].raw_repr(), "time_test.txt");
            Ok(())
        },
    },
}
