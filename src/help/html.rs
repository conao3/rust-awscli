//! Python's `html.parser.HTMLParser` (with `convert_charrefs=True`) and
//! `html.unescape`, over code points so positions match Python's `str`
//! indices.

use super::entities::{HTML5, INVALID_CHARREFS, INVALID_CODEPOINTS};

/// The handler methods `HTMLParser` calls. An `Err` is an exception raised
/// by a handler: it propagates out of `feed` / `close` and leaves the
/// parser's buffered data as it was.
pub(super) trait Handler {
    fn handle_starttag(&mut self, tag: &str, attrs: &[(String, Option<String>)]) -> Result<(), ()>;
    fn handle_endtag(&mut self, tag: &str) -> Result<(), ()>;
    fn handle_data(&mut self, data: &str) -> Result<(), ()>;
}

const CDATA_CONTENT_ELEMENTS: &[&str] =
    &["script", "style", "xmp", "iframe", "noembed", "noframes"];
const RCDATA_CONTENT_ELEMENTS: &[&str] = &["textarea", "title"];

fn is_ws(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\x0c' | ' ')
}

/// Python's `str.isspace` for `re`'s `\s` on `str` patterns.
fn is_re_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

fn collect(s: &[char]) -> String {
    s.iter().collect()
}

/// `html.parser.HTMLParser`.
#[derive(Debug, Default)]
pub(super) struct HtmlParser {
    rawdata: Vec<char>,
    cdata_elem: Option<String>,
    escapable: bool,
    pending: Vec<String>,
    pending_len: usize,
    parse_threshold: usize,
}

impl HtmlParser {
    pub(super) fn new() -> HtmlParser {
        HtmlParser {
            escapable: true,
            parse_threshold: 1,
            ..HtmlParser::default()
        }
    }

    /// `HTMLParser.feed`.
    pub(super) fn feed(&mut self, data: &str, h: &mut dyn Handler) -> Result<(), ()> {
        self.pending_len += data.chars().count();
        if self.pending_len < self.parse_threshold {
            self.pending.push(data.to_string());
            return Ok(());
        }
        if self.pending.is_empty() {
            self.rawdata.extend(data.chars());
        } else {
            self.pending.push(data.to_string());
            for p in std::mem::take(&mut self.pending) {
                self.rawdata.extend(p.chars());
            }
        }
        self.pending_len = 0;
        let n = self.rawdata.len();
        self.goahead(false, h)?;
        self.parse_threshold = if self.rawdata.len() < n {
            1
        } else {
            self.rawdata.len()
        };
        Ok(())
    }

    /// `HTMLParser.close`.
    pub(super) fn close(&mut self, h: &mut dyn Handler) -> Result<(), ()> {
        if !self.pending.is_empty() {
            for p in std::mem::take(&mut self.pending) {
                self.rawdata.extend(p.chars());
            }
            self.pending_len = 0;
        }
        self.goahead(true, h)
    }

    fn set_cdata_mode(&mut self, elem: &str, escapable: bool) {
        self.cdata_elem = Some(elem.to_lowercase());
        self.escapable = escapable;
    }

    fn clear_cdata_mode(&mut self) {
        self.cdata_elem = None;
        self.escapable = true;
    }

    /// `self.interesting.search(rawdata, i)` in CDATA mode:
    /// `</elem(?=[\t\n\r\f />])` case-insensitively (or `\z` for
    /// `plaintext`).
    fn interesting_search(&self, raw: &[char], i: usize) -> Option<usize> {
        let elem: Vec<char> = self.cdata_elem.as_deref().unwrap_or("").chars().collect();
        if elem == ['p', 'l', 'a', 'i', 'n', 't', 'e', 'x', 't'] {
            return Some(raw.len());
        }
        let len = 2 + elem.len();
        (i..raw.len()).find(|&j| {
            j + len < raw.len()
                && raw[j] == '<'
                && raw[j + 1] == '/'
                && raw[j + 2..j + len]
                    .iter()
                    .zip(&elem)
                    .all(|(a, b)| a.to_ascii_lowercase() == *b)
                && matches!(raw[j + len], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>')
        })
    }

    fn emit_data(&self, data: &[char], h: &mut dyn Handler) -> Result<(), ()> {
        let s = collect(data);
        if self.escapable {
            h.handle_data(&unescape(&s))
        } else {
            h.handle_data(&s)
        }
    }

    /// `HTMLParser.goahead` (the `convert_charrefs` paths).
    fn goahead(&mut self, end: bool, h: &mut dyn Handler) -> Result<(), ()> {
        let raw = self.rawdata.clone();
        let n = raw.len();
        let mut i = 0;
        while i < n {
            let j = if self.cdata_elem.is_none() {
                match raw[i..].iter().position(|&c| c == '<') {
                    Some(p) => i + p,
                    None => {
                        let from = i.max(n.saturating_sub(34));
                        let amppos = raw[from..]
                            .iter()
                            .rposition(|&c| c == '&')
                            .map(|p| from + p);
                        if let Some(a) = amppos
                            && !raw[a..]
                                .iter()
                                .any(|&c| matches!(c, '\t' | '\n' | '\r' | '\x0c' | ' ' | ';'))
                        {
                            break;
                        }
                        n
                    }
                }
            } else {
                match self.interesting_search(&raw, i) {
                    Some(j) => j,
                    None => break,
                }
            };
            if i < j {
                self.emit_data(&raw[i..j], h)?;
            }
            i = j;
            if i == n {
                break;
            }
            let starts = |k: usize, s: &str| {
                let s: Vec<char> = s.chars().collect();
                raw.len() >= k + s.len() && raw[k..k + s.len()] == s[..]
            };
            if raw[i] == '<' {
                let mut k = if starttagopen(&raw, i) {
                    self.parse_starttag(&raw, i, h)?
                } else if starts(i, "</") {
                    self.parse_endtag(&raw, i, h)?
                } else if starts(i, "<!--") {
                    parse_comment(&raw, i)
                } else if starts(i, "<?") {
                    raw[i + 2..]
                        .iter()
                        .position(|&c| c == '>')
                        .map_or(-1, |p| (i + 2 + p + 1) as isize)
                } else if starts(i, "<!") {
                    parse_html_declaration(&raw, i)
                } else if i + 1 < n || end {
                    h.handle_data("<")?;
                    (i + 1) as isize
                } else {
                    break;
                };
                if k < 0 {
                    if !end {
                        break;
                    }
                    if !starttagopen(&raw, i) && starts(i, "</") && i + 2 == n {
                        h.handle_data("</")?;
                    }
                    k = n as isize;
                }
                i = k as usize;
            } else {
                unreachable!("interesting.search() lied");
            }
        }
        if end && i < n {
            self.emit_data(&raw[i..n], h)?;
            i = n;
        }
        self.rawdata = raw[i..].to_vec();
        Ok(())
    }

    /// `HTMLParser.parse_starttag`.
    fn parse_starttag(&mut self, raw: &[char], i: usize, h: &mut dyn Handler) -> Result<isize, ()> {
        let endpos = locatetagend(raw, i + 1);
        if raw[endpos - 1] != '>' {
            return Ok(-1);
        }
        let (name_end, mut k) = tagfind_tolerant(raw, i + 1);
        let tag = collect(&raw[i + 1..name_end]).to_lowercase();
        let mut attrs = Vec::new();
        while k < endpos {
            let Some(m) = attrfind_tolerant(raw, k) else {
                break;
            };
            let name = collect(&raw[m.name.0..m.name.1]);
            let mut value = match m.value {
                None => None,
                Some((s, e)) => {
                    let v = &raw[s..e];
                    let quoted = v.len() >= 2
                        && ((v[0] == '\'' && v[v.len() - 1] == '\'')
                            || (v[0] == '"' && v[v.len() - 1] == '"'));
                    let quoted_one = v.len() == 1 && (v[0] == '\'' || v[0] == '"');
                    if quoted {
                        Some(collect(&v[1..v.len() - 1]))
                    } else if quoted_one {
                        Some(String::new())
                    } else {
                        Some(collect(v))
                    }
                }
            };
            if let Some(v) = &value
                && !v.is_empty()
            {
                value = Some(unescape_attrvalue(v));
            }
            attrs.push((name.to_lowercase(), value));
            k = m.end;
        }
        let rest = collect(&raw[k..endpos]);
        let rest = rest.trim_matches(is_re_space);
        if rest != ">" && rest != "/>" {
            h.handle_data(&collect(&raw[i..endpos]))?;
            return Ok(endpos as isize);
        }
        if rest.ends_with("/>") {
            h.handle_starttag(&tag, &attrs)?;
            h.handle_endtag(&tag)?;
        } else {
            h.handle_starttag(&tag, &attrs)?;
            if CDATA_CONTENT_ELEMENTS.contains(&tag.as_str()) || tag == "plaintext" {
                self.set_cdata_mode(&tag, false);
            } else if RCDATA_CONTENT_ELEMENTS.contains(&tag.as_str()) {
                self.set_cdata_mode(&tag, true);
            }
        }
        Ok(endpos as isize)
    }

    /// `HTMLParser.parse_endtag`.
    fn parse_endtag(&mut self, raw: &[char], i: usize, h: &mut dyn Handler) -> Result<isize, ()> {
        if !raw[i + 2..].contains(&'>') {
            return Ok(-1);
        }
        if !(raw.len() > i + 2 && raw[i + 2].is_ascii_alphabetic()) {
            if raw.get(i + 2) == Some(&'>') {
                return Ok((i + 3) as isize);
            }
            let pos = raw[i + 2..]
                .iter()
                .position(|&c| c == '>')
                .expect("checked");
            return Ok((i + 2 + pos + 1) as isize);
        }
        let j = locatetagend(raw, i + 2);
        if raw[j - 1] != '>' {
            return Ok(-1);
        }
        let (name_end, _) = tagfind_tolerant(raw, i + 2);
        let tag = collect(&raw[i + 2..name_end]).to_lowercase();
        h.handle_endtag(&tag)?;
        self.clear_cdata_mode();
        Ok(j as isize)
    }
}

fn starttagopen(raw: &[char], i: usize) -> bool {
    raw.len() > i + 1 && raw[i] == '<' && raw[i + 1].is_ascii_alphabetic()
}

/// `parse_comment`: `commentabruptclose` (`-?>`) at `i + 4`, else the
/// first `--!?>`.
fn parse_comment(raw: &[char], i: usize) -> isize {
    let s = i + 4;
    if raw.get(s) == Some(&'>') {
        return (s + 1) as isize;
    }
    if raw.get(s) == Some(&'-') && raw.get(s + 1) == Some(&'>') {
        return (s + 2) as isize;
    }
    let mut j = s;
    while j + 2 < raw.len() + 1 {
        if raw.get(j) == Some(&'-') && raw.get(j + 1) == Some(&'-') {
            if raw.get(j + 2) == Some(&'>') {
                return (j + 3) as isize;
            }
            if raw.get(j + 2) == Some(&'!') && raw.get(j + 3) == Some(&'>') {
                return (j + 4) as isize;
            }
        }
        j += 1;
    }
    -1
}

/// `parse_html_declaration`.
fn parse_html_declaration(raw: &[char], i: usize) -> isize {
    let rest = collect(&raw[i..raw.len().min(i + 9)]);
    if rest.starts_with("<!--") {
        return parse_comment(raw, i);
    }
    let find = |from: usize, pat: &str| {
        let pat: Vec<char> = pat.chars().collect();
        (from..raw.len()).find(|&k| raw[k..].starts_with(&pat))
    };
    if rest == "<![CDATA[" {
        return find(i + 9, "]]>").map_or(-1, |j| (j + 3) as isize);
    }
    if rest.to_lowercase() == "<!doctype" {
        return find(i + 9, ">").map_or(-1, |j| (j + 1) as isize);
    }
    find(i + 2, ">").map_or(-1, |j| (j + 1) as isize)
}

/// `tagfind_tolerant` at `i`: (end of group 1, end of match).
fn tagfind_tolerant(raw: &[char], i: usize) -> (usize, usize) {
    let mut k = i + 1;
    while k < raw.len() && !matches!(raw[k], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>') {
        k += 1;
    }
    let name_end = k;
    while k < raw.len() && (is_ws(raw[k]) || (raw[k] == '/' && raw.get(k + 1) != Some(&'>'))) {
        k += 1;
    }
    (name_end, k)
}

fn attr_name_end(raw: &[char], k: usize) -> Option<usize> {
    if k == 0 || k >= raw.len() {
        return None;
    }
    if !matches!(
        raw[k - 1],
        '\'' | '"' | '\t' | '\n' | '\r' | '\x0c' | ' ' | '/'
    ) {
        return None;
    }
    if matches!(raw[k], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>') {
        return None;
    }
    let mut e = k + 1;
    while e < raw.len() && !matches!(raw[e], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '=' | '>') {
        e += 1;
    }
    Some(e)
}

/// The optional value part `[\t\n\r\f ]*=[\t\n\r\f ]*(value)`: (value start,
/// value end) when it matches at `k`.
fn attr_value(raw: &[char], k: usize) -> Option<(usize, usize)> {
    let mut p = k;
    while p < raw.len() && is_ws(raw[p]) {
        p += 1;
    }
    if raw.get(p) != Some(&'=') {
        return None;
    }
    p += 1;
    while p < raw.len() && is_ws(raw[p]) {
        p += 1;
    }
    let s = p;
    match raw.get(p) {
        Some(&q @ ('\'' | '"')) => raw[p + 1..]
            .iter()
            .position(|&c| c == q)
            .map(|e| (s, p + 1 + e + 1)),
        _ => {
            while p < raw.len() && !matches!(raw[p], '>' | '\t' | '\n' | '\r' | '\x0c' | ' ') {
                p += 1;
            }
            Some((s, p))
        }
    }
}

/// `locatetagend` at `i`: the end of the match.
fn locatetagend(raw: &[char], i: usize) -> usize {
    let mut k = i + 1;
    while k < raw.len() && !matches!(raw[k], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/' | '>') {
        k += 1;
    }
    while k < raw.len() && matches!(raw[k], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/') {
        k += 1;
    }
    while let Some(e) = attr_name_end(raw, k) {
        k = e;
        if let Some((_, ve)) = attr_value(raw, k) {
            k = ve;
        }
        while k < raw.len() && matches!(raw[k], '\t' | '\n' | '\r' | '\x0c' | ' ' | '/') {
            k += 1;
        }
    }
    if raw.get(k) == Some(&'>') {
        k += 1;
    }
    k
}

struct AttrMatch {
    name: (usize, usize),
    value: Option<(usize, usize)>,
    end: usize,
}

/// `attrfind_tolerant` at `k`.
fn attrfind_tolerant(raw: &[char], k: usize) -> Option<AttrMatch> {
    let e = attr_name_end(raw, k)?;
    let mut end = e;
    let value = attr_value(raw, e);
    if let Some((_, ve)) = value {
        end = ve;
    }
    while end < raw.len()
        && (is_ws(raw[end]) || (raw[end] == '/' && raw.get(end + 1) != Some(&'>')))
    {
        end += 1;
    }
    Some(AttrMatch {
        name: (k, e),
        value,
        end,
    })
}

fn html5(name: &str) -> Option<&'static str> {
    HTML5
        .binary_search_by(|(n, _)| (*n).cmp(name))
        .ok()
        .map(|i| HTML5[i].1)
}

/// `html._replace_charref` for the text after `&`.
fn replace_charref(s: &str) -> String {
    if let Some(num) = s.strip_prefix('#') {
        let (digits, radix) = match num.strip_prefix(['x', 'X']) {
            Some(hex) => (hex, 16),
            None => (num, 10),
        };
        let digits = digits.trim_end_matches(';');
        let value = digits
            .chars()
            .try_fold(0u64, |acc, c| {
                acc.checked_mul(radix)?
                    .checked_add(c.to_digit(radix as u32)? as u64)
            })
            .unwrap_or(u64::MAX);
        if let Ok(i) = INVALID_CHARREFS.binary_search_by(|(n, _)| (*n as u64).cmp(&value)) {
            return INVALID_CHARREFS[i].1.to_string();
        }
        if (0xD800..=0xDFFF).contains(&value) || value > 0x10FFFF {
            return "\u{FFFD}".to_string();
        }
        if INVALID_CODEPOINTS.binary_search(&(value as u32)).is_ok() {
            return String::new();
        }
        return char::from_u32(value as u32)
            .map(String::from)
            .unwrap_or_default();
    }
    if let Some(v) = html5(s) {
        return v.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    for x in (2..chars.len()).rev() {
        let prefix = collect(&chars[..x]);
        if let Some(v) = html5(&prefix) {
            return format!("{v}{}", collect(&chars[x..]));
        }
    }
    format!("&{s}")
}

/// `html.unescape`.
pub(super) fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '&' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        if chars.get(start) == Some(&'#') {
            let hex = matches!(chars.get(start + 1), Some('x' | 'X'));
            let digits_start = if hex { start + 2 } else { start + 1 };
            let mut k = digits_start;
            while k < chars.len()
                && (if hex {
                    chars[k].is_ascii_hexdigit()
                } else {
                    chars[k].is_ascii_digit()
                })
            {
                k += 1;
            }
            if k > digits_start {
                end = k;
                if chars.get(end) == Some(&';') {
                    end += 1;
                }
            }
        } else {
            let mut k = start;
            while k < chars.len()
                && k - start < 32
                && !matches!(chars[k], '\t' | '\n' | '\x0c' | ' ' | '<' | '&' | '#' | ';')
            {
                k += 1;
            }
            if k > start {
                end = k;
                if chars.get(end) == Some(&';') {
                    end += 1;
                }
            }
        }
        if end == start {
            out.push('&');
            i += 1;
            continue;
        }
        out.push_str(&replace_charref(&collect(&chars[start..end])));
        i = end;
    }
    out
}

/// `html.parser._unescape_attrvalue`.
fn unescape_attrvalue(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '&' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut k = start;
        if chars.get(k) == Some(&'#') {
            let hex = matches!(chars.get(k + 1), Some('x' | 'X'));
            let ds = if hex { k + 2 } else { k + 1 };
            let mut e = ds;
            while e < chars.len()
                && (if hex {
                    chars[e].is_ascii_hexdigit()
                } else {
                    chars[e].is_ascii_digit()
                })
            {
                e += 1;
            }
            k = if e > ds { e } else { start };
        } else if chars.get(k).is_some_and(char::is_ascii_alphabetic) {
            k += 1;
            while k < chars.len() && chars[k].is_ascii_alphanumeric() {
                k += 1;
            }
        }
        if k == start {
            out.push('&');
            i += 1;
            continue;
        }
        let mut end = k;
        if matches!(chars.get(end), Some(';' | '=')) {
            end += 1;
        }
        let r = collect(&chars[i..end]);
        let replaced = if r.starts_with("&#") || (!r.ends_with('=') && html5(&r[1..]).is_some()) {
            unescape(&r)
        } else {
            r
        };
        out.push_str(&replaced);
        i = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Rec(Vec<String>);

    impl Handler for Rec {
        fn handle_starttag(
            &mut self,
            tag: &str,
            attrs: &[(String, Option<String>)],
        ) -> Result<(), ()> {
            self.0.push(format!("start {tag} {attrs:?}"));
            Ok(())
        }
        fn handle_endtag(&mut self, tag: &str) -> Result<(), ()> {
            self.0.push(format!("end {tag}"));
            Ok(())
        }
        fn handle_data(&mut self, data: &str) -> Result<(), ()> {
            self.0.push(format!("data {data:?}"));
            Ok(())
        }
    }

    fn parse(s: &str) -> Vec<String> {
        let mut p = HtmlParser::new();
        let mut r = Rec::default();
        p.feed(s, &mut r).unwrap();
        p.close(&mut r).unwrap();
        r.0
    }

    #[test]
    fn tags_and_entities() {
        assert_eq!(
            parse("<p>a &amp; b <a href='x?a=1&amp;b'>link</a></p>"),
            vec![
                "start p []",
                "data \"a & b \"",
                "start a [(\"href\", Some(\"x?a=1&b\"))]",
                "data \"link\"",
                "end a",
                "end p",
            ]
        );
    }

    #[test]
    fn unescape_matches_python() {
        assert_eq!(unescape("&lt;&gt;&notit; &#x41;&#128;&amp"), "<>¬it; A€&");
    }
}
