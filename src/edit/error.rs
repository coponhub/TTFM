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

//! Error messages and validation functions for the edit module.

pub const LABEL_GROUPING_IN_CAPTURE_EDITING: &str =
    "Label Grouping pattern projection cannot be used in capture editing";

pub fn label_grouping_in_capture_editing_err() -> anyhow::Error {
    anyhow::anyhow!(LABEL_GROUPING_IN_CAPTURE_EDITING)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_label_grouping_in_capture_editing_err() {
        let err = label_grouping_in_capture_editing_err();
        assert_eq!(err.to_string(), LABEL_GROUPING_IN_CAPTURE_EDITING);
    }
}
