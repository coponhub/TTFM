use std::ops::RangeInclusive;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Atom {
    Ch(char),
    Any,
    One,
    Class {
        set: Vec<RangeInclusive<char>>,
        negated: bool,
    },
}

impl Atom {
    pub(crate) fn is_metachar(&self) -> bool {
        matches!(self, Atom::Any | Atom::One | Atom::Class { .. })
    }
}

pub(crate) fn lex(pattern: &str) -> Vec<Atom> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut atoms = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                atoms.push(Atom::Any);
                i += 1;
            }
            '?' => {
                atoms.push(Atom::One);
                i += 1;
            }
            '\\' if i + 1 < chars.len() => {
                atoms.push(Atom::Ch(chars[i + 1]));
                i += 2;
            }
            '[' => match lex_class(&chars, i) {
                Some((atom, next)) => {
                    atoms.push(atom);
                    i = next;
                }
                None => {
                    // 閉じない `[` を含むパターンは DuckDB GLOB ではどんな文字列
                    // にも一致しない。残りを1つの空クラスとして畳み、その挙動に
                    // 合わせる。
                    atoms.push(Atom::Class {
                        set: vec![],
                        negated: false,
                    });
                    i = chars.len();
                }
            },
            c => {
                atoms.push(Atom::Ch(c));
                i += 1;
            }
        }
    }
    atoms
}

/// `chars[open]` は `[`。クラスと閉じ `]` の次の位置を返す。
/// 有効な閉じ `]` が無い場合は None。
fn lex_class(chars: &[char], open: usize) -> Option<(Atom, usize)> {
    let mut idx = open + 1;
    let negated = chars.get(idx) == Some(&'!');
    if negated {
        idx += 1;
    }

    let mut set = Vec::new();
    let mut first = true;
    loop {
        let &c = chars.get(idx)?;
        if c == ']' && !first {
            return Some((Atom::Class { set, negated }, idx + 1));
        }
        first = false;
        match (chars.get(idx + 1), chars.get(idx + 2)) {
            (Some('-'), Some(&hi)) if hi != ']' => {
                set.push(c..=hi);
                idx += 3;
            }
            (Some('-'), _) => return None,
            _ => {
                set.push(c..=c);
                idx += 1;
            }
        }
    }
}

pub(crate) fn glob_captures(pattern: &str, text: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = text.chars().collect();
    match_atoms(&lex(pattern), &chars)
}

fn match_atoms(atoms: &[Atom], text: &[char]) -> Option<Vec<String>> {
    let Some((atom, rest)) = atoms.split_first() else {
        return text.is_empty().then(Vec::new);
    };
    match atom {
        Atom::Ch(c) => match text.split_first() {
            Some((t, tail)) if t == c => match_atoms(rest, tail),
            _ => None,
        },
        Atom::One => bind_one(text, rest, |_| true),
        Atom::Class { set, negated } => bind_one(text, rest, |t| {
            set.iter().any(|r| r.contains(&t)) != *negated
        }),
        Atom::Any => (0..=text.len()).find_map(|take| {
            let (bound, tail) = text.split_at(take);
            match_atoms(rest, tail)
                .map(|caps| prepend(bound.iter().collect(), caps))
        }),
    }
}

fn bind_one(
    text: &[char],
    rest: &[Atom],
    accepts: impl Fn(char) -> bool,
) -> Option<Vec<String>> {
    let (&t, tail) = text.split_first()?;
    accepts(t)
        .then(|| match_atoms(rest, tail))
        .flatten()
        .map(|caps| prepend(t.to_string(), caps))
}

fn prepend(head: String, mut caps: Vec<String>) -> Vec<String> {
    caps.insert(0, head);
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captures(pattern: &str, text: &str) -> Option<Vec<String>> {
        glob_captures(pattern, text)
    }

    #[test]
    fn shortest_split_on_first_separator() {
        assert_eq!(
            captures("*=*", "a=b=c"),
            Some(vec!["a".to_string(), "b=c".to_string()])
        );
    }

    #[test]
    fn shortest_keeps_tail_for_repeated_marker() {
        assert_eq!(
            captures("*_draft*", "a_draft_b_draft"),
            Some(vec!["a".to_string(), "_b_draft".to_string()])
        );
    }

    #[test]
    fn question_and_class_capture_one_char() {
        assert_eq!(captures("a?c", "abc"), Some(vec!["b".to_string()]));
        assert_eq!(captures("a[bx]c", "abc"), Some(vec!["b".to_string()]));
    }

    #[test]
    fn bang_class_is_the_only_negation() {
        assert_eq!(captures("[!a]", "b"), Some(vec!["b".to_string()]));
        assert_eq!(captures("[!a]", "a"), None);
    }

    #[test]
    fn caret_in_class_is_a_member_not_negation() {
        // [^a] is a class containing the literal members '^' and 'a', not negation.
        assert_eq!(captures("[^a]", "^"), Some(vec!["^".to_string()]));
        assert_eq!(captures("[^a]", "a"), Some(vec!["a".to_string()]));
        assert_eq!(captures("[^a]", "b"), None);
    }

    #[test]
    fn leading_caret_is_matched_literally() {
        assert_eq!(captures("[^a]", "^"), Some(vec!["^".to_string()]));
    }

    #[test]
    fn bracketed_star_is_a_literal_star() {
        assert_eq!(captures("[*]", "*"), Some(vec!["*".to_string()]));
        assert_eq!(captures("[*]", "x"), None);
    }

    #[test]
    fn escaped_metachar_matches_and_yields_no_capture() {
        assert_eq!(captures("a\\*b", "a*b"), Some(vec![]));
        assert_eq!(captures("a\\*b", "aXb"), None);
    }

    #[test]
    fn backslash_before_plain_char_is_consumed() {
        assert_eq!(captures("\\a", "a"), Some(vec![]));
    }

    #[test]
    fn unclosed_bracket_never_matches() {
        assert_eq!(captures("[a", "[a"), None);
        assert_eq!(captures("[a", "a"), None);
    }

    #[test]
    fn multibyte_counts_as_one_char() {
        assert_eq!(captures("?", "あ"), Some(vec!["あ".to_string()]));
        assert_eq!(captures("あ*", "あい"), Some(vec!["い".to_string()]));
    }

    #[test]
    fn no_match_returns_none() {
        assert_eq!(captures("abc", "abd"), None);
    }

    const ASCII_CASES: &[(&str, &str)] = &[
        ("b", "[!a]"),
        ("!", "[!a]"),
        ("a", "[!a]"),
        ("b", "[^a]"),
        ("^", "[^a]"),
        ("a", "[^a]"),
        ("*", "[*]"),
        ("x", "[*]"),
        ("*", "\\*"),
        ("a", "\\a"),
        ("[", "["),
        ("a", "[a"),
        ("b", "[a-c]"),
        ("-", "[a-c]"),
        ("d", "[a-c]"),
        ("a", "[abc]"),
        ("z", "[abc]"),
        ("\\", "\\\\"),
        ("a*b", "a\\*b"),
        ("aXb", "a\\*b"),
        ("a", "a"),
        ("A", "a"),
        ("ab", "a?"),
        ("a", "a?"),
        ("", "*"),
        ("anything", "*"),
        ("c", "[a-c-e]"),
        ("-", "[a-c-e]"),
        ("e", "[a-c-e]"),
        ("d", "[a-c-e]"),
        ("c", "[ac-]"),
        ("a", "[ac-]"),
        ("-", "[-ac]"),
        ("a", "[-ac]"),
        ("a]", "[a]]"),
        ("]", "[]]"),
        ("a", "[]a]"),
    ];

    #[test]
    fn agrees_with_duckdb_glob_on_ascii_table() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        for (text, pat) in ASCII_CASES {
            let sql_hit: bool = conn
                .query_row(
                    "SELECT ? GLOB ?",
                    duckdb::params![text, pat],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                sql_hit,
                glob_captures(pat, text).is_some(),
                "pattern={pat:?} text={text:?}"
            );
        }
    }

    #[test]
    fn multibyte_divergence_from_duckdb_is_pinned() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let sql_hit: bool = conn
            .query_row("SELECT ? GLOB ?", duckdb::params!["あ", "?"], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!sql_hit, "DuckDB GLOB matches ? against bytes, not chars");
        assert!(glob_captures("?", "あ").is_some());
    }
}
