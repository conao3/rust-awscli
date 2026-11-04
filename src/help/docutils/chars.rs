//! Python `str` character predicates and string helpers that operate on
//! code points (`str.isspace`, `str.isalnum`, `str.strip`, slicing,
//! `unicodedata.east_asian_width`, `unicodedata.combining`).

use super::unicode_tables::{COMBINING, DECIMAL, SPACE, WIDE, WORD};

fn in_table(table: &[(u32, u32)], c: char) -> bool {
    let c = c as u32;
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                std::cmp::Ordering::Less
            } else if lo > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// `str.isspace()` (and `\s` of a `str` pattern).
pub(super) fn is_space(c: char) -> bool {
    if c.is_ascii() {
        return matches!(
            c,
            ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c' | '\x1c'..='\x1f'
        );
    }
    in_table(SPACE, c)
}

/// `\w` of a `str` pattern.
pub(super) fn is_word(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_alphanumeric() || c == '_';
    }
    in_table(WORD, c)
}

/// `\d` of a `str` pattern.
pub(super) fn is_decimal(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_digit();
    }
    in_table(DECIMAL, c)
}

/// `unicodedata.east_asian_width(c) in 'WF'`.
pub(super) fn is_wide(c: char) -> bool {
    in_table(WIDE, c)
}

/// `unicodedata.combining(c) != 0`.
pub(super) fn is_combining(c: char) -> bool {
    in_table(COMBINING, c)
}

/// `docutils.utils.column_width`.
pub(super) fn column_width(text: &str) -> usize {
    let mut w = 0isize;
    for c in text.chars() {
        w += if is_wide(c) { 2 } else { 1 };
        if is_combining(c) {
            w -= 1;
        }
    }
    w.max(0) as usize
}

/// `str.strip()`.
pub(super) fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `str.lstrip()`.
pub(super) fn lstrip(s: &str) -> &str {
    s.trim_start_matches(is_space)
}

/// `str.rstrip()`.
pub(super) fn rstrip(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

/// `len(s)` in code points.
pub(super) fn clen(s: &str) -> usize {
    s.chars().count()
}

fn byte_index(s: &str, i: usize) -> usize {
    s.char_indices().nth(i).map_or(s.len(), |(b, _)| b)
}

/// `s[a:b]` with non-negative code point bounds clamped to the string.
pub(super) fn slice(s: &str, a: usize, b: usize) -> &str {
    if a >= b {
        return "";
    }
    let start = byte_index(s, a);
    let end = byte_index(s, b);
    if start >= end { "" } else { &s[start..end] }
}

/// `s[a:]`.
pub(super) fn slice_from(s: &str, a: usize) -> &str {
    &s[byte_index(s, a)..]
}

/// `s[:b]`.
pub(super) fn slice_to(s: &str, b: usize) -> &str {
    &s[..byte_index(s, b)]
}

/// `str.split()` (no arguments).
pub(super) fn split_ws(s: &str) -> Vec<&str> {
    s.split(is_space).filter(|p| !p.is_empty()).collect()
}

/// `s.split(None, maxsplit)`.
pub(super) fn split_ws_max(s: &str, maxsplit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = lstrip(s);
    while !rest.is_empty() {
        if out.len() == maxsplit {
            out.push(rest.to_string());
            break;
        }
        match rest.find(is_space) {
            Some(i) => {
                out.push(rest[..i].to_string());
                rest = lstrip(&rest[i..]);
            }
            None => {
                out.push(rest.to_string());
                break;
            }
        }
    }
    out
}

/// `str.splitlines()`.
pub(super) fn splitlines(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            '\n' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}'
            | '\u{2029}' => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `str.expandtabs(tabsize)`.
pub(super) fn expandtabs(s: &str, tabsize: usize) -> String {
    let mut out = String::new();
    let mut col = 0;
    for c in s.chars() {
        match c {
            '\t' => {
                let n = tabsize - col % tabsize;
                for _ in 0..n {
                    out.push(' ');
                }
                col += n;
            }
            '\n' | '\r' => {
                out.push(c);
                col = 0;
            }
            _ => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}
