//! Python `json.loads` (`json/decoder.py` with the `_json` C scanner of
//! CPython 3.14), including its `JSONDecodeError` messages, which the AWS
//! CLI shows to the user (`awscli/argprocess.py` `_unpack_json_cli_arg`).

use crate::value::{Map, Value, number_from_lexeme};

/// `json.loads(s)`; the error is `str(JSONDecodeError)`.
pub(crate) fn loads(s: &str) -> Result<Value, String> {
    let chars: Vec<char> = s.chars().collect();
    let d = Decoder { s: &chars };
    if chars.first() == Some(&'\u{feff}') {
        return Err(d.error("Unexpected UTF-8 BOM (decode using utf-8-sig)", 0));
    }
    let start = d.ws(0);
    let (value, end) = match d.scan_once(start)? {
        Some(r) => r,
        None => return Err(d.error("Expecting value", start)),
    };
    let end = d.ws(end);
    if end != chars.len() {
        return Err(d.error("Extra data", end));
    }
    Ok(value)
}

struct Decoder<'a> {
    s: &'a [char],
}

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

impl Decoder<'_> {
    fn error(&self, msg: &str, pos: usize) -> String {
        let before = &self.s[..pos.min(self.s.len())];
        let lineno = before.iter().filter(|&&c| c == '\n').count() + 1;
        let colno = match before.iter().rposition(|&c| c == '\n') {
            Some(nl) => pos - nl,
            None => pos + 1,
        };
        format!("{msg}: line {lineno} column {colno} (char {pos})")
    }

    fn at(&self, i: usize) -> Option<char> {
        self.s.get(i).copied()
    }

    /// `WHITESPACE.match(s, i).end()`.
    fn ws(&self, mut i: usize) -> usize {
        while self.at(i).is_some_and(is_ws) {
            i += 1;
        }
        i
    }

    fn starts_with(&self, i: usize, lit: &str) -> bool {
        let lit: Vec<char> = lit.chars().collect();
        self.s.len() >= i + lit.len() && self.s[i..i + lit.len()] == lit[..]
    }

    /// `scan_once`; `Ok(None)` is `StopIteration(idx)`.
    fn scan_once(&self, idx: usize) -> Result<Option<(Value, usize)>, String> {
        let Some(c) = self.at(idx) else {
            return Ok(None);
        };
        match c {
            '"' => {
                let (s, end) = self.scanstring(idx + 1)?;
                return Ok(Some((Value::Str(s), end)));
            }
            '{' => return self.object(idx + 1).map(Some),
            '[' => return self.array(idx + 1).map(Some),
            'n' if self.starts_with(idx, "null") => return Ok(Some((Value::Null, idx + 4))),
            't' if self.starts_with(idx, "true") => return Ok(Some((Value::Bool(true), idx + 4))),
            'f' if self.starts_with(idx, "false") => {
                return Ok(Some((Value::Bool(false), idx + 5)));
            }
            _ => {}
        }
        if let Some((lexeme, end)) = self.number(idx) {
            return Ok(Some((number_from_lexeme(&lexeme), end)));
        }
        if c == 'N' && self.starts_with(idx, "NaN") {
            return Ok(Some((Value::Float(f64::NAN), idx + 3)));
        }
        if c == 'I' && self.starts_with(idx, "Infinity") {
            return Ok(Some((Value::Float(f64::INFINITY), idx + 8)));
        }
        if c == '-' && self.starts_with(idx, "-Infinity") {
            return Ok(Some((Value::Float(f64::NEG_INFINITY), idx + 9)));
        }
        Ok(None)
    }

    /// `NUMBER_RE = r'(-?(?:0|[1-9][0-9]*))(\.[0-9]+)?([eE][-+]?[0-9]+)?'`.
    fn number(&self, idx: usize) -> Option<(String, usize)> {
        let mut i = idx;
        let digit = |i: usize| self.at(i).is_some_and(|c| c.is_ascii_digit());
        if self.at(i) == Some('-') {
            i += 1;
        }
        match self.at(i) {
            Some('0') => i += 1,
            Some('1'..='9') => {
                while digit(i) {
                    i += 1;
                }
            }
            _ => return None,
        }
        if self.at(i) == Some('.') && digit(i + 1) {
            i += 1;
            while digit(i) {
                i += 1;
            }
        }
        if matches!(self.at(i), Some('e' | 'E')) {
            let mut j = i + 1;
            if matches!(self.at(j), Some('+' | '-')) {
                j += 1;
            }
            if digit(j) {
                while digit(j) {
                    j += 1;
                }
                i = j;
            }
        }
        Some((self.s[idx..i].iter().collect(), i))
    }

    fn hex4(&self, pos: usize) -> Result<u32, String> {
        let digits: Option<String> = (1..=4)
            .map(|k| self.at(pos + k).filter(char::is_ascii_hexdigit))
            .collect();
        digits
            .and_then(|d| u32::from_str_radix(&d, 16).ok())
            .ok_or_else(|| self.error("Invalid \\uXXXX escape", pos))
    }

    /// `scanstring` (C version messages).
    fn scanstring(&self, mut end: usize) -> Result<(String, usize), String> {
        let begin = end - 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.at(end) else {
                return Err(self.error("Unterminated string starting at", begin));
            };
            match c {
                '"' => return Ok((out, end + 1)),
                '\\' => {
                    let Some(esc) = self.at(end + 1) else {
                        return Err(self.error("Unterminated string starting at", begin));
                    };
                    let ch = match esc {
                        '"' => '"',
                        '\\' => '\\',
                        '/' => '/',
                        'b' => '\u{8}',
                        'f' => '\u{c}',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        'u' => {
                            let mut uni = self.hex4(end + 1)?;
                            let mut next = end + 6;
                            if (0xd800..=0xdbff).contains(&uni)
                                && self.at(next) == Some('\\')
                                && self.at(next + 1) == Some('u')
                            {
                                let uni2 = self.hex4(next + 1)?;
                                if (0xdc00..=0xdfff).contains(&uni2) {
                                    uni = 0x10000 + (((uni - 0xd800) << 10) | (uni2 - 0xdc00));
                                    next += 6;
                                }
                            }
                            out.push(char::from_u32(uni).unwrap_or('\u{fffd}'));
                            end = next;
                            continue;
                        }
                        _ => return Err(self.error("Invalid \\escape", end)),
                    };
                    out.push(ch);
                    end += 2;
                }
                c if (c as u32) < 0x20 => {
                    return Err(self.error("Invalid control character at", end));
                }
                c => {
                    out.push(c);
                    end += 1;
                }
            }
        }
    }

    /// `JSONObject`.
    fn object(&self, mut end: usize) -> Result<(Value, usize), String> {
        let mut pairs = Map::new();
        let mut next = self.at(end);
        if next != Some('"') {
            if next.is_some_and(is_ws) {
                end = self.ws(end);
                next = self.at(end);
            }
            if next == Some('}') {
                return Ok((Value::Map(pairs), end + 1));
            } else if next != Some('"') {
                return Err(self.error("Expecting property name enclosed in double quotes", end));
            }
        }
        end += 1;
        loop {
            let (key, e) = self.scanstring(end)?;
            end = e;
            if self.at(end) != Some(':') {
                end = self.ws(end);
                if self.at(end) != Some(':') {
                    return Err(self.error("Expecting ':' delimiter", end));
                }
            }
            end += 1;
            end = self.ws(end);
            let (value, e) = match self.scan_once(end)? {
                Some(r) => r,
                None => return Err(self.error("Expecting value", end)),
            };
            end = e;
            pairs.insert(key, value);
            let mut next = self.at(end);
            if next.is_some_and(is_ws) {
                end = self.ws(end + 1);
                next = self.at(end);
            }
            end += 1;
            if next == Some('}') {
                break;
            } else if next != Some(',') {
                return Err(self.error("Expecting ',' delimiter", end - 1));
            }
            let comma = end - 1;
            end = self.ws(end);
            let next = self.at(end);
            end += 1;
            if next != Some('"') {
                if next == Some('}') {
                    return Err(self.error("Illegal trailing comma before end of object", comma));
                }
                return Err(
                    self.error("Expecting property name enclosed in double quotes", end - 1)
                );
            }
        }
        Ok((Value::Map(pairs), end))
    }

    /// `JSONArray`.
    fn array(&self, mut end: usize) -> Result<(Value, usize), String> {
        let mut values = Vec::new();
        let mut next = self.at(end);
        if next.is_some_and(is_ws) {
            end = self.ws(end + 1);
            next = self.at(end);
        }
        if next == Some(']') {
            return Ok((Value::List(values), end + 1));
        }
        loop {
            let (value, e) = match self.scan_once(end)? {
                Some(r) => r,
                None => return Err(self.error("Expecting value", end)),
            };
            end = e;
            values.push(value);
            let mut next = self.at(end);
            if next.is_some_and(is_ws) {
                end = self.ws(end + 1);
                next = self.at(end);
            }
            end += 1;
            if next == Some(']') {
                break;
            } else if next != Some(',') {
                return Err(self.error("Expecting ',' delimiter", end - 1));
            }
            let comma = end - 1;
            end = self.ws(end);
            if self.at(end) == Some(']') {
                return Err(self.error("Illegal trailing comma before end of array", comma));
            }
        }
        Ok((Value::List(values), end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (input, `str(JSONDecodeError)` or `None` when it parses), from
    /// CPython 3.14 `json.loads`.
    const CASES: &[(&str, Option<&str>)] = &[
        ("", Some("Expecting value: line 1 column 1 (char 0)")),
        (" ", Some("Expecting value: line 1 column 2 (char 1)")),
        (
            "{",
            Some("Expecting property name enclosed in double quotes: line 1 column 2 (char 1)"),
        ),
        (
            "{\"a\"",
            Some("Expecting ':' delimiter: line 1 column 5 (char 4)"),
        ),
        ("{\"a\":", Some("Expecting value: line 1 column 6 (char 5)")),
        (
            "{\"a\":1",
            Some("Expecting ',' delimiter: line 1 column 7 (char 6)"),
        ),
        (
            "{\"a\":1,",
            Some("Expecting property name enclosed in double quotes: line 1 column 8 (char 7)"),
        ),
        (
            "{\"a\":1,}",
            Some("Illegal trailing comma before end of object: line 1 column 7 (char 6)"),
        ),
        (
            "[1,]",
            Some("Illegal trailing comma before end of array: line 1 column 3 (char 2)"),
        ),
        (
            "[1",
            Some("Expecting ',' delimiter: line 1 column 3 (char 2)"),
        ),
        ("[", Some("Expecting value: line 1 column 2 (char 1)")),
        (
            "{a:1}",
            Some("Expecting property name enclosed in double quotes: line 1 column 2 (char 1)"),
        ),
        (
            "\"abc",
            Some("Unterminated string starting at: line 1 column 1 (char 0)"),
        ),
        (
            "\"a\nb\"",
            Some("Invalid control character at: line 1 column 3 (char 2)"),
        ),
        (
            "\"\\x\"",
            Some("Invalid \\escape: line 1 column 2 (char 1)"),
        ),
        (
            "\"\\u12\"",
            Some("Invalid \\uXXXX escape: line 1 column 3 (char 2)"),
        ),
        (
            "[1 2]",
            Some("Expecting ',' delimiter: line 1 column 4 (char 3)"),
        ),
        (
            "{\"a\" 1}",
            Some("Expecting ':' delimiter: line 1 column 6 (char 5)"),
        ),
        ("1 2", Some("Extra data: line 1 column 3 (char 2)")),
        ("tru", Some("Expecting value: line 1 column 1 (char 0)")),
        ("-", Some("Expecting value: line 1 column 1 (char 0)")),
        ("01", Some("Extra data: line 1 column 2 (char 1)")),
        ("1.", Some("Extra data: line 1 column 2 (char 1)")),
        (
            "\u{feff}1",
            Some("Unexpected UTF-8 BOM (decode using utf-8-sig): line 1 column 1 (char 0)"),
        ),
        (
            "{\"a\":1}\n x",
            Some("Extra data: line 2 column 2 (char 9)"),
        ),
        (
            "[\n1,\n\n2",
            Some("Expecting ',' delimiter: line 4 column 2 (char 7)"),
        ),
        (
            "[1,\t]",
            Some("Illegal trailing comma before end of array: line 1 column 3 (char 2)"),
        ),
        ("{ }", None),
        ("[ ]", None),
        ("{\"a\" : 1 , \"b\" :2 }", None),
        ("[\"\\uD834\\uDD1E\"]", None),
    ];

    #[test]
    fn decode_errors_match_cpython() {
        for (input, expected) in CASES {
            match (loads(input), expected) {
                (Ok(_), None) => {}
                (Err(e), Some(x)) => assert_eq!(e, *x, "{input:?}"),
                (r, x) => panic!("{input:?}: {r:?} vs {x:?}"),
            }
        }
        assert_eq!(
            loads("[\"\\uD834\\uDD1E\", 1.5, 10, NaN]")
                .unwrap()
                .as_list()
                .unwrap()[0],
            Value::str("\u{1d11e}")
        );
    }
}
