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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Init,
    Searched {
        query: String,
        cid: Option<String>,
        offset: usize,
        total_count: Option<usize>,
        has_more: bool,
    },
}

impl State {
    pub fn new() -> Self {
        State::Init
    }

    pub fn to_searched(
        &mut self,
        query: String,
        cid: Option<String>,
        fetched_len: usize,
        total_count: Option<usize>,
        has_more: bool,
    ) {
        *self = State::Searched {
            query,
            cid,
            offset: fetched_len,
            total_count,
            has_more,
        };
    }

    pub fn clear(&mut self) {
        *self = State::Init;
    }

    pub fn last_query(&self) -> Option<&str> {
        match self {
            State::Init => None,
            State::Searched { query, .. } => Some(query.as_str()),
        }
    }

    pub fn prompt_string(&self) -> String {
        match self {
            State::Init => "Command (m for help): ".to_string(),
            State::Searched {
                query,
                offset,
                total_count,
                has_more,
                ..
            } => {
                let count_str = if let Some(total) = total_count {
                    format!("{total}")
                } else if *has_more {
                    format!("{offset}+")
                } else {
                    format!("{offset}")
                };
                format!("[{query} ({count_str} items)] Command (m for help): ")
            }
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_transitions_and_prompt() {
        let mut s = State::new();
        assert_eq!(s.prompt_string(), "Command (m for help): ");
        assert_eq!(s.last_query(), None);

        s.to_searched(
            "extension:rs".to_string(),
            Some("cid-1".to_string()),
            20,
            None,
            true,
        );
        assert_eq!(
            s.prompt_string(),
            "[extension:rs (20+ items)] Command (m for help): "
        );
        assert_eq!(s.last_query(), Some("extension:rs"));

        s.to_searched("extension:rs".to_string(), None, 5, Some(5), false);
        assert_eq!(
            s.prompt_string(),
            "[extension:rs (5 items)] Command (m for help): "
        );

        s.clear();
        assert_eq!(s, State::Init);
    }
}
