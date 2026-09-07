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
        page_start: usize,
        offset: usize,
        total_count: Option<usize>,
        has_more: bool,
        col_offsets: Vec<usize>,
        next_col_offset: Option<usize>,
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
        self.to_searched_at_page(
            query,
            cid,
            0,
            fetched_len,
            total_count,
            has_more,
        );
    }

    pub fn to_searched_at_page(
        &mut self,
        query: String,
        cid: Option<String>,
        page_start: usize,
        fetched_len: usize,
        total_count: Option<usize>,
        has_more: bool,
    ) {
        *self = State::Searched {
            query,
            cid,
            page_start,
            offset: page_start + fetched_len,
            total_count,
            has_more,
            col_offsets: vec![0],
            next_col_offset: None,
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
            State::Init => String::new(),
            State::Searched { query, .. } => {
                format!("[{query}] ")
            }
        }
    }

    pub fn current_col_offset(&self) -> usize {
        match self {
            State::Searched { col_offsets, .. } => {
                *col_offsets.last().unwrap_or(&0)
            }
            _ => 0,
        }
    }

    pub fn push_col_offset(&mut self, next: usize) {
        if let State::Searched { col_offsets, .. } = self {
            col_offsets.push(next);
        }
    }

    pub fn pop_col_offset(&mut self) -> Option<usize> {
        if let State::Searched { col_offsets, .. } = self {
            if col_offsets.len() > 1 {
                return col_offsets.pop();
            }
        }
        None
    }

    pub fn can_prev_cols(&self) -> bool {
        match self {
            State::Searched { col_offsets, .. } => col_offsets.len() > 1,
            _ => false,
        }
    }

    pub fn set_next_col_offset(&mut self, next: Option<usize>) {
        if let State::Searched {
            next_col_offset, ..
        } = self
        {
            *next_col_offset = next;
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
        assert_eq!(s.prompt_string(), "");
        assert_eq!(s.last_query(), None);

        s.to_searched(
            "extension:rs".to_string(),
            Some("cid-1".to_string()),
            20,
            None,
            true,
        );
        assert_eq!(s.prompt_string(), "[extension:rs] ");
        assert_eq!(s.last_query(), Some("extension:rs"));

        s.to_searched("extension:rs".to_string(), None, 5, Some(5), false);
        assert_eq!(s.prompt_string(), "[extension:rs] ");

        s.clear();
        assert_eq!(s, State::Init);
    }

    #[test]
    fn test_state_col_offset_navigation_and_reset() {
        let mut s = State::new();
        assert_eq!(s.current_col_offset(), 0);
        assert!(!s.can_prev_cols());

        s.to_searched_at_page("ext:rs".to_string(), None, 0, 20, None, true);
        assert_eq!(s.current_col_offset(), 0);
        assert!(!s.can_prev_cols());

        s.set_next_col_offset(Some(3));
        s.push_col_offset(3);
        assert_eq!(s.current_col_offset(), 3);
        assert!(s.can_prev_cols());

        s.set_next_col_offset(Some(5));
        s.push_col_offset(5);
        assert_eq!(s.current_col_offset(), 5);

        assert_eq!(s.pop_col_offset(), Some(5));
        assert_eq!(s.current_col_offset(), 3);
        assert!(s.can_prev_cols());

        assert_eq!(s.pop_col_offset(), Some(3));
        assert_eq!(s.current_col_offset(), 0);
        assert!(!s.can_prev_cols());
        assert_eq!(s.pop_col_offset(), None);

        // Reset on to_searched_at_page
        s.push_col_offset(4);
        s.to_searched_at_page("ext:rs".to_string(), None, 20, 20, None, true);
        assert_eq!(s.current_col_offset(), 0);
        assert!(!s.can_prev_cols());
    }
}
