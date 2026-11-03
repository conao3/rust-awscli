//! Python-compatible formatting of [`Value`]s: `float.__repr__`, `str()`,
//! and `json.dumps`.

use base64::Engine;

use crate::value::Value;

mod printable_table;

/// Python `repr(float)` / `str(float)` (they are identical in Python 3):
/// shortest round-trip digits, exponent form `1e-05` / `1e+16` outside
/// `1e-4 <= |x| < 1e16`, `.0` kept for integral values, `nan`, `inf`,
/// `-inf`.
///
/// Ported from CPython `Python/pystrtod.c` `format_float_short` with
/// `format_code='r'` and `Py_DTSF_ADD_DOT_0`.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    let sign = if f.is_sign_negative() { "-" } else { "" };
    // `{:e}` gives the shortest round-trip digits: `d[.ddd]e<exp>`.
    let lower = format!("{:e}", f.abs());
    let (mantissa, exp) = lower.split_once('e').expect("exponent");
    let exp: i32 = exp.parse().expect("exponent digits");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let ndigits = digits.len() as i32;
    let decpt = exp + 1;
    let body = if decpt <= -4 || decpt > 16 {
        let mut s = String::new();
        s.push_str(&digits[..1]);
        if ndigits > 1 {
            s.push('.');
            s.push_str(&digits[1..]);
        }
        let e = decpt - 1;
        s.push('e');
        s.push(if e < 0 { '-' } else { '+' });
        s.push_str(&format!("{:02}", e.abs()));
        s
    } else if decpt <= 0 {
        format!("0.{}{}", "0".repeat((-decpt) as usize), digits)
    } else if decpt >= ndigits {
        format!("{}{}.0", digits, "0".repeat((decpt - ndigits) as usize))
    } else {
        format!(
            "{}.{}",
            &digits[..decpt as usize],
            &digits[decpt as usize..]
        )
    };
    format!("{sign}{body}")
}

/// Python `str(value)` as used by the text and table formatters
/// (`True` / `False` / `None`, `b'...'` for bytes, `repr`-style for
/// containers).
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => py_repr(other),
    }
}

/// Python `repr(value)`.
pub fn py_repr(v: &Value) -> String {
    let mut out = String::new();
    write_repr(v, &mut out);
    out
}

fn write_repr(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("None"),
        Value::Bool(true) => out.push_str("True"),
        Value::Bool(false) => out.push_str("False"),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) => out.push_str(&float_repr(*f)),
        Value::Str(s) => out.push_str(&str_repr(s)),
        Value::Bytes(b) => out.push_str(&bytes_repr(b)),
        Value::List(l) => {
            out.push('[');
            for (i, item) in l.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_repr(item, out);
            }
            out.push(']');
        }
        Value::Map(m) => {
            out.push('{');
            for (i, (k, item)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&str_repr(k));
                out.push_str(": ");
                write_repr(item, out);
            }
            out.push('}');
        }
    }
}

/// Python `str.isprintable()` for one character.
pub fn is_printable(c: char) -> bool {
    let cp = c as u32;
    printable_table::NON_PRINTABLE
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_err()
}

/// Python `repr(str)` (CPython `unicode_repr`).
pub fn str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        let cp = c as u32;
        if c == quote || c == '\\' {
            out.push('\\');
            out.push(c);
        } else if c == '\t' {
            out.push_str("\\t");
        } else if c == '\n' {
            out.push_str("\\n");
        } else if c == '\r' {
            out.push_str("\\r");
        } else if cp < 0x20 || cp == 0x7f {
            out.push_str(&format!("\\x{cp:02x}"));
        } else if cp < 0x7f || is_printable(c) {
            out.push(c);
        } else if cp <= 0xff {
            out.push_str(&format!("\\x{cp:02x}"));
        } else if cp <= 0xffff {
            out.push_str(&format!("\\u{cp:04x}"));
        } else {
            out.push_str(&format!("\\U{cp:08x}"));
        }
    }
    out.push(quote);
    out
}

/// Python `repr(bytes)` (CPython `bytes_repr`).
pub fn bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') {
        b'"'
    } else {
        b'\''
    };
    let mut out = String::with_capacity(b.len() + 3);
    out.push('b');
    out.push(quote as char);
    for &c in b {
        if c == quote || c == b'\\' {
            out.push('\\');
            out.push(c as char);
        } else if c == b'\t' {
            out.push_str("\\t");
        } else if c == b'\n' {
            out.push_str("\\n");
        } else if c == b'\r' {
            out.push_str("\\r");
        } else if !(0x20..0x7f).contains(&c) {
            out.push_str(&format!("\\x{c:02x}"));
        } else {
            out.push(c as char);
        }
    }
    out.push(quote as char);
    out
}

/// `json.dumps(v, indent=4, ensure_ascii=False, default=json_encoder)`
/// without a trailing newline. Bytes are emitted as base64 strings
/// (`awscli.utils.json_encoder`); NaN / Infinity as Python emits them.
pub fn json_dumps_pretty(v: &Value) -> String {
    let mut out = String::new();
    write_json(
        v,
        &JsonStyle {
            indent: Some(4),
            ensure_ascii: false,
        },
        0,
        &mut out,
    );
    out
}

/// `json.dumps(v)` with Python's defaults (separators `", "` and `": "`,
/// `ensure_ascii=True`), as used for pagination tokens.
pub fn json_dumps_default(v: &Value) -> String {
    let mut out = String::new();
    write_json(
        v,
        &JsonStyle {
            indent: None,
            ensure_ascii: true,
        },
        0,
        &mut out,
    );
    out
}

/// `json.dumps(v, ensure_ascii=False, default=json_encoder)` on one line
/// (separators `", "` and `": "`), as the YAML dumper uses for scalars.
pub fn json_dumps_line(v: &Value) -> String {
    let mut out = String::new();
    write_json(
        v,
        &JsonStyle {
            indent: None,
            ensure_ascii: false,
        },
        0,
        &mut out,
    );
    out
}

struct JsonStyle {
    indent: Option<usize>,
    ensure_ascii: bool,
}

/// Python `json.encoder` (`_make_iterencode`, `py_encode_basestring`,
/// `py_encode_basestring_ascii`, `floatstr`).
fn write_json(v: &Value, style: &JsonStyle, level: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) => {
            if f.is_nan() {
                out.push_str("NaN");
            } else if f.is_infinite() {
                out.push_str(if *f > 0.0 { "Infinity" } else { "-Infinity" });
            } else {
                out.push_str(&float_repr(*f));
            }
        }
        Value::Str(s) => json_string(s, style.ensure_ascii, out),
        Value::Bytes(b) => json_string(
            &base64::engine::general_purpose::STANDARD.encode(b),
            style.ensure_ascii,
            out,
        ),
        Value::List(l) => {
            if l.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in l.iter().enumerate() {
                json_separator(style, level + 1, i == 0, out);
                write_json(item, style, level + 1, out);
            }
            json_close(style, level, out);
            out.push(']');
        }
        Value::Map(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in m.iter().enumerate() {
                json_separator(style, level + 1, i == 0, out);
                json_string(k, style.ensure_ascii, out);
                out.push_str(": ");
                write_json(item, style, level + 1, out);
            }
            json_close(style, level, out);
            out.push('}');
        }
    }
}

fn json_separator(style: &JsonStyle, level: usize, first: bool, out: &mut String) {
    match style.indent {
        Some(n) => {
            if !first {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&" ".repeat(n * level));
        }
        None => {
            if !first {
                out.push_str(", ");
            }
        }
    }
}

fn json_close(style: &JsonStyle, level: usize, out: &mut String) {
    if let Some(n) = style.indent {
        out.push('\n');
        out.push_str(&" ".repeat(n * level));
    }
}

fn json_string(s: &str, ensure_ascii: bool, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ensure_ascii && (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod goldens;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::test_support::decode;

    #[test]
    fn float_repr_goldens() {
        for &(bits, expected) in goldens::FLOAT_REPR {
            assert_eq!(float_repr(f64::from_bits(bits)), expected, "bits {bits:#x}");
        }
    }

    #[test]
    fn py_str_goldens() {
        for &(input, str_expected, repr_expected) in goldens::PY_STR {
            let v = decode(input);
            assert_eq!(py_str(&v), str_expected, "str({input})");
            assert_eq!(py_repr(&v), repr_expected, "repr({input})");
        }
    }

    #[test]
    fn json_goldens() {
        for &(input, pretty, default, line) in goldens::JSON {
            let v = decode(input);
            assert_eq!(json_dumps_pretty(&v), pretty, "pretty {input}");
            assert_eq!(json_dumps_default(&v), default, "default {input}");
            assert_eq!(json_dumps_line(&v), line, "line {input}");
        }
    }
}
