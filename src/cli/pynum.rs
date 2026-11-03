//! Python `int(str)` and `float(str)` (base 10, surrounding whitespace,
//! a sign, and single underscores between digits).

use num_bigint::BigInt;

/// Python `str.strip()` with no arguments.
pub(crate) fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Python `str.isspace()` for one character.
pub(crate) fn is_py_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{b}'
            | '\u{c}'
            | '\r'
            | '\u{1c}'..='\u{1f}'
            | ' '
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

/// Drop underscores that sit between two digits; `None` for any other
/// underscore.
fn strip_underscores(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            let prev = i > 0 && chars[i - 1].is_ascii_digit();
            let next = chars.get(i + 1).is_some_and(char::is_ascii_digit);
            if !prev || !next {
                return None;
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// `int(s)`.
pub(crate) fn py_int(s: &str) -> Option<BigInt> {
    let t = py_strip(s);
    let (sign, digits) = match t.as_bytes().first() {
        Some(b'+') => ("", &t[1..]),
        Some(b'-') => ("-", &t[1..]),
        _ => ("", t),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit() || c == '_') {
        return None;
    }
    let cleaned = strip_underscores(digits)?;
    format!("{sign}{cleaned}").parse::<BigInt>().ok()
}

/// `float(s)`.
pub(crate) fn py_float(s: &str) -> Option<f64> {
    let t = py_strip(s);
    let lower = t.to_ascii_lowercase();
    let unsigned = lower.trim_start_matches(['+', '-']);
    if lower.len() - unsigned.len() > 1 {
        return None;
    }
    if matches!(unsigned, "nan" | "inf" | "infinity") {
        return lower.parse::<f64>().ok();
    }
    if !unsigned
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '_' | '.' | 'e' | '+' | '-'))
    {
        return None;
    }
    let cleaned = strip_underscores(t)?;
    cleaned.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_and_float() {
        assert_eq!(py_int(" 1_000 "), Some(BigInt::from(1000)));
        assert_eq!(py_int("-007"), Some(BigInt::from(-7)));
        assert_eq!(py_int("1__0"), None);
        assert_eq!(py_int("1.0"), None);
        assert_eq!(py_float("1e3"), Some(1000.0));
        assert_eq!(py_float(" -Infinity"), Some(f64::NEG_INFINITY));
        assert_eq!(py_float("1_0.5"), Some(10.5));
        assert_eq!(py_float("abc"), None);
    }
}
