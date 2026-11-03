//! Wire encodings used by the stub server and the CLI input writer:
//! a JSON tree that keeps number lexemes, an XML element writer, a CBOR
//! encoder, and the textual timestamp forms.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value as Json;

use crate::values::Ts;

pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

// ---------------------------------------------------------------- JSON

/// A JSON tree whose numbers are raw lexemes (so `1.50`, `1e2`, `NaN`
/// reach the client exactly as written).
#[derive(Debug, Clone)]
pub enum J {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn from_serde(v: &Json) -> J {
        match v {
            Json::Null => J::Null,
            Json::Bool(b) => J::Bool(*b),
            Json::Number(n) => J::Num(n.to_string()),
            Json::String(s) => J::Str(s.clone()),
            Json::Array(a) => J::Arr(a.iter().map(J::from_serde).collect()),
            Json::Object(o) => J::Obj(
                o.iter()
                    .map(|(k, v)| (k.clone(), J::from_serde(v)))
                    .collect(),
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JsonStyle {
    /// Escape every non-ASCII character as `\uXXXX`.
    pub ascii: bool,
    /// Put a space after `:` and `,`.
    pub spaced: bool,
}

pub fn json_text(j: &J, style: JsonStyle) -> String {
    let mut out = String::new();
    write_json(&mut out, j, style);
    out
}

fn write_json(out: &mut String, j: &J, style: JsonStyle) {
    let (colon, comma) = if style.spaced {
        (": ", ", ")
    } else {
        (":", ",")
    };
    match j {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Num(n) => out.push_str(n),
        J::Str(s) => json_string(out, s, style.ascii),
        J::Arr(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(comma);
                }
                write_json(out, v, style);
            }
            out.push(']');
        }
        J::Obj(entries) => {
            out.push('{');
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push_str(comma);
                }
                json_string(out, k, style.ascii);
                out.push_str(colon);
                write_json(out, v, style);
            }
            out.push('}');
        }
    }
}

pub fn json_string(out: &mut String, s: &str, ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii && !c.is_ascii() => {
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

// ----------------------------------------------------------------- XML

pub fn xml_escape(s: &str, attr: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            '\n' if attr => out.push_str("&#10;"),
            '\t' if attr => out.push_str("&#9;"),
            c => out.push(c),
        }
    }
    out
}

/// A minimal XML element tree.
#[derive(Debug, Clone, Default)]
pub struct El {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: Option<String>,
    pub children: Vec<El>,
}

impl El {
    pub fn new(name: impl Into<String>) -> El {
        El {
            name: name.into(),
            ..El::default()
        }
    }

    pub fn text(name: impl Into<String>, text: impl Into<String>) -> El {
        El {
            name: name.into(),
            text: Some(text.into()),
            ..El::default()
        }
    }

    pub fn child(mut self, c: El) -> El {
        self.children.push(c);
        self
    }

    pub fn attr(mut self, k: impl Into<String>, v: impl Into<String>) -> El {
        self.attrs.push((k.into(), v.into()));
        self
    }

    pub fn write(&self, out: &mut String) {
        out.push('<');
        out.push_str(&self.name);
        for (k, v) in &self.attrs {
            out.push(' ');
            out.push_str(k);
            out.push_str("=\"");
            out.push_str(&xml_escape(v, true));
            out.push('"');
        }
        if self.text.is_none() && self.children.is_empty() {
            out.push_str("/>");
            return;
        }
        out.push('>');
        if let Some(t) = &self.text {
            out.push_str(&xml_escape(t, false));
        }
        for c in &self.children {
            c.write(out);
        }
        out.push_str("</");
        out.push_str(&self.name);
        out.push('>');
    }

    pub fn document(&self, declaration: bool) -> String {
        let mut out = String::new();
        if declaration {
            out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        }
        self.write(&mut out);
        out
    }
}

// ---------------------------------------------------------------- CBOR

/// A CBOR data item.
#[derive(Debug, Clone)]
pub enum C {
    Uint(u64),
    Nint(u64),
    Bytes(Vec<u8>),
    Text(String),
    Arr(Vec<C>, bool),
    Map(Vec<(C, C)>, bool),
    Tag(u64, Box<C>),
    Bool(bool),
    Null,
    F32(f32),
    F64(f64),
}

impl C {
    pub fn int(v: i128) -> C {
        if v >= 0 {
            C::Uint(v as u64)
        } else {
            C::Nint((-1 - v) as u64)
        }
    }
}

fn cbor_head(out: &mut Vec<u8>, major: u8, v: u64) {
    let m = major << 5;
    if v < 24 {
        out.push(m | v as u8);
    } else if v <= 0xff {
        out.push(m | 24);
        out.push(v as u8);
    } else if v <= 0xffff {
        out.push(m | 25);
        out.extend_from_slice(&(v as u16).to_be_bytes());
    } else if v <= 0xffff_ffff {
        out.push(m | 26);
        out.extend_from_slice(&(v as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&v.to_be_bytes());
    }
}

pub fn cbor_encode(out: &mut Vec<u8>, c: &C) {
    match c {
        C::Uint(v) => cbor_head(out, 0, *v),
        C::Nint(v) => cbor_head(out, 1, *v),
        C::Bytes(b) => {
            cbor_head(out, 2, b.len() as u64);
            out.extend_from_slice(b);
        }
        C::Text(s) => {
            cbor_head(out, 3, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        C::Arr(items, indefinite) => {
            if *indefinite {
                out.push(0x9f);
            } else {
                cbor_head(out, 4, items.len() as u64);
            }
            for i in items {
                cbor_encode(out, i);
            }
            if *indefinite {
                out.push(0xff);
            }
        }
        C::Map(entries, indefinite) => {
            if *indefinite {
                out.push(0xbf);
            } else {
                cbor_head(out, 5, entries.len() as u64);
            }
            for (k, v) in entries {
                cbor_encode(out, k);
                cbor_encode(out, v);
            }
            if *indefinite {
                out.push(0xff);
            }
        }
        C::Tag(t, inner) => {
            cbor_head(out, 6, *t);
            cbor_encode(out, inner);
        }
        C::Bool(false) => out.push(0xf4),
        C::Bool(true) => out.push(0xf5),
        C::Null => out.push(0xf6),
        C::F32(f) => {
            out.push(0xfa);
            out.extend_from_slice(&f.to_be_bytes());
        }
        C::F64(f) => {
            out.push(0xfb);
            out.extend_from_slice(&f.to_be_bytes());
        }
    }
}

pub fn cbor_from_json(v: &Json) -> C {
    match v {
        Json::Null => C::Null,
        Json::Bool(b) => C::Bool(*b),
        Json::Number(n) => {
            let s = n.to_string();
            if let Ok(i) = s.parse::<i128>() {
                C::int(i)
            } else {
                C::F64(s.parse().unwrap_or(0.0))
            }
        }
        Json::String(s) => C::Text(s.clone()),
        Json::Array(a) => C::Arr(a.iter().map(cbor_from_json).collect(), false),
        Json::Object(o) => C::Map(
            o.iter()
                .map(|(k, v)| (C::Text(k.clone()), cbor_from_json(v)))
                .collect(),
            false,
        ),
    }
}

// ---------------------------------------------------------- timestamps

/// `(year, month, day)` of a day count since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

struct Parts {
    y: i64,
    mo: u32,
    d: u32,
    h: i64,
    mi: i64,
    s: i64,
    wday: usize,
}

fn parts(secs: i64) -> Parts {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, mo, d) = civil(days);
    Parts {
        y,
        mo,
        d,
        h: rem / 3600,
        mi: rem / 60 % 60,
        s: rem % 60,
        wday: (days + 4).rem_euclid(7) as usize,
    }
}

/// An ISO 8601 rendering of `ts`; `ts.form` picks fractional digits and
/// the offset (`Z`, `+09:00`, `-05:30`, `+00:00`, or none).
pub fn iso8601(ts: Ts) -> String {
    let (offset_secs, suffix) = match ts.form % 7 {
        3 => (9 * 3600, "+09:00"),
        4 => (0, ""),
        5 => (-(5 * 3600 + 1800), "-05:30"),
        6 => (0, "+00:00"),
        _ => (0, "Z"),
    };
    let p = parts(ts.secs + offset_secs);
    let frac = match (ts.form / 7) % 3 {
        _ if ts.micros == 0 => String::new(),
        0 => String::new(),
        1 => format!(".{:03}", ts.micros / 1000),
        _ => format!(".{:06}", ts.micros),
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{frac}{suffix}",
        p.y, p.mo, p.d, p.h, p.mi, p.s
    )
}

/// An epoch-seconds JSON number lexeme.
pub fn epoch(ts: Ts) -> String {
    match ts.form % 5 {
        1 if ts.micros != 0 => format!("{}.{:03}", ts.secs, ts.micros / 1000),
        2 if ts.micros != 0 => format!("{}.{:06}", ts.secs, ts.micros),
        3 if ts.secs != 0 => format!("{:e}", ts.secs as f64),
        4 => format!("{}.0", ts.secs),
        _ => ts.secs.to_string(),
    }
}

/// An RFC 822 / HTTP date.
pub fn rfc822(ts: Ts) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let p = parts(ts.secs);
    let zone = if ts.form % 4 == 3 { "+0000" } else { "GMT" };
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} {zone}",
        DAYS[p.wday],
        p.d,
        MONTHS[p.mo as usize - 1],
        p.y,
        p.h,
        p.mi,
        p.s
    )
}

pub fn cbor_timestamp(ts: Ts) -> C {
    let inner = if ts.micros != 0 && ts.form % 2 == 1 {
        C::F64(ts.secs as f64 + f64::from(ts.micros) / 1e6)
    } else {
        C::int(i128::from(ts.secs))
    };
    C::Tag(1, Box::new(inner))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        let ts = Ts {
            secs: 1_422_172_800,
            micros: 0,
            form: 0,
        };
        assert_eq!(iso8601(ts), "2015-01-25T08:00:00Z");
        assert_eq!(rfc822(ts), "Sun, 25 Jan 2015 08:00:00 GMT");
        let ts = Ts {
            secs: 1_422_172_800,
            micros: 123_456,
            form: 3 + 7,
        };
        assert_eq!(iso8601(ts), "2015-01-25T17:00:00.123+09:00");
    }

    #[test]
    fn cbor_heads() {
        let mut out = vec![];
        cbor_encode(&mut out, &C::int(-1));
        cbor_encode(&mut out, &C::Uint(500));
        assert_eq!(out, [0x20, 0x19, 0x01, 0xf4]);
    }
}
