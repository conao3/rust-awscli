//! The parts of `xml.dom.minidom` (CPython 3.14) that `history show`
//! uses on HTTP bodies: `parseString` (`expatbuilder.ExpatBuilderNS`),
//! `toxml`, and `toprettyxml`.

use quick_xml::events::Event;
use quick_xml::reader::Reader;

enum Node {
    Element {
        name: String,
        attrs: Vec<(String, String)>,
        children: Vec<Node>,
    },
    Text(String),
    CData(String),
    Comment(String),
    Pi {
        target: String,
        data: String,
    },
}

/// An element being parsed: its name, attributes, and children.
type Open = (String, Vec<(String, String)>, Vec<Node>);

/// `minidom.parseString(text)`: the document's nodes, or `None` when
/// expat would reject the text.
fn parse(text: &str) -> Option<Vec<Node>> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut reader = Reader::from_str(&text);
    reader.config_mut().check_end_names = true;
    let mut document: Vec<Node> = Vec::new();
    let mut stack: Vec<Open> = Vec::new();
    let mut roots = 0;
    loop {
        let event = reader.read_event().ok()?;
        match event {
            Event::Eof => break,
            Event::Decl(_) | Event::DocType(_) => {}
            Event::Start(_) | Event::Empty(_) if stack.is_empty() && roots == 1 => return None,
            Event::Start(e) => {
                let name = String::from_utf8(e.name().as_ref().to_vec()).ok()?;
                let attrs = attributes(&e)?;
                stack.push((name, attrs, Vec::new()));
            }
            Event::Empty(e) => {
                let name = String::from_utf8(e.name().as_ref().to_vec()).ok()?;
                let attrs = attributes(&e)?;
                let node = Node::Element {
                    name,
                    attrs,
                    children: Vec::new(),
                };
                match stack.last_mut() {
                    Some((_, _, children)) => children.push(node),
                    None => {
                        roots += 1;
                        document.push(node);
                    }
                }
            }
            Event::End(_) => {
                let (name, attrs, children) = stack.pop()?;
                let node = Node::Element {
                    name,
                    attrs,
                    children,
                };
                match stack.last_mut() {
                    Some((_, _, siblings)) => siblings.push(node),
                    None => {
                        roots += 1;
                        document.push(node);
                    }
                }
            }
            Event::Text(t) => {
                let raw = std::str::from_utf8(t.as_ref()).ok()?;
                let data = quick_xml::escape::unescape(raw).ok()?.into_owned();
                match stack.last_mut() {
                    Some((_, _, children)) => {
                        if let Some(Node::Text(prev)) = children.last_mut() {
                            prev.push_str(&data);
                        } else if !data.is_empty() {
                            children.push(Node::Text(data));
                        }
                    }
                    None => {
                        if !data.chars().all(|c| matches!(c, ' ' | '\t' | '\n')) {
                            return None;
                        }
                    }
                }
            }
            Event::GeneralRef(r) => {
                let name = std::str::from_utf8(r.as_ref()).ok()?;
                let data = quick_xml::escape::unescape(&format!("&{name};"))
                    .ok()?
                    .into_owned();
                let (_, _, children) = stack.last_mut()?;
                if let Some(Node::Text(prev)) = children.last_mut() {
                    prev.push_str(&data);
                } else {
                    children.push(Node::Text(data));
                }
            }
            Event::CData(c) => {
                let data = String::from_utf8(c.into_inner().into_owned()).ok()?;
                let (_, _, children) = stack.last_mut()?;
                children.push(Node::CData(data));
            }
            Event::Comment(c) => {
                let data = String::from_utf8(c.into_inner().into_owned()).ok()?;
                let node = Node::Comment(data);
                match stack.last_mut() {
                    Some((_, _, children)) => children.push(node),
                    None => document.push(node),
                }
            }
            Event::PI(p) => {
                let raw = String::from_utf8(p.into_inner().into_owned()).ok()?;
                let (target, data) = match raw.find(|c: char| c.is_ascii_whitespace()) {
                    Some(i) => (
                        raw[..i].to_string(),
                        raw[i..]
                            .trim_start_matches(|c: char| c.is_ascii_whitespace())
                            .to_string(),
                    ),
                    None => (raw.clone(), String::new()),
                };
                let node = Node::Pi { target, data };
                match stack.last_mut() {
                    Some((_, _, children)) => children.push(node),
                    None => document.push(node),
                }
            }
        }
    }
    if !stack.is_empty() || roots != 1 {
        return None;
    }
    Some(document)
}

/// The attributes of an element as `ExpatBuilderNS` sets them: the
/// namespace declarations first, then the others, each in document order,
/// with values normalized (`\t` and `\n` become spaces) and unescaped.
fn attributes(e: &quick_xml::events::BytesStart<'_>) -> Option<Vec<(String, String)>> {
    let mut ns = Vec::new();
    let mut other = Vec::new();
    for attr in e.attributes() {
        let attr = attr.ok()?;
        let key = String::from_utf8(attr.key.as_ref().to_vec()).ok()?;
        let raw = std::str::from_utf8(&attr.value)
            .ok()?
            .replace(['\t', '\n'], " ");
        let value = quick_xml::escape::unescape(&raw).ok()?.into_owned();
        if ns
            .iter()
            .chain(other.iter())
            .any(|(k, _): &(String, String)| *k == key)
        {
            return None;
        }
        if key == "xmlns" || key.starts_with("xmlns:") {
            ns.push((key, value));
        } else {
            other.push((key, value));
        }
    }
    ns.extend(other);
    Some(ns)
}

/// `_write_data`.
fn write_data(out: &mut String, text: &str, attr: bool) {
    let mut text = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    if attr {
        text = text
            .replace('"', "&quot;")
            .replace('\r', "&#13;")
            .replace('\n', "&#10;")
            .replace('\t', "&#9;");
    }
    out.push_str(&text);
}

/// `Node.writexml(writer, indent, addindent, newl)`.
fn write_node(out: &mut String, node: &Node, indent: &str, addindent: &str, newl: &str) {
    match node {
        Node::Element {
            name,
            attrs,
            children,
        } => {
            out.push_str(indent);
            out.push('<');
            out.push_str(name);
            for (k, v) in attrs {
                out.push(' ');
                out.push_str(k);
                out.push_str("=\"");
                write_data(out, v, true);
                out.push('"');
            }
            if children.is_empty() {
                out.push_str("/>");
                out.push_str(newl);
                return;
            }
            out.push('>');
            if let [only @ (Node::Text(_) | Node::CData(_))] = children.as_slice() {
                write_node(out, only, "", "", "");
            } else {
                out.push_str(newl);
                let inner = format!("{indent}{addindent}");
                for child in children {
                    write_node(out, child, &inner, addindent, newl);
                }
                out.push_str(indent);
            }
            out.push_str("</");
            out.push_str(name);
            out.push('>');
            out.push_str(newl);
        }
        Node::Text(data) => write_data(out, &format!("{indent}{data}{newl}"), false),
        Node::CData(data) => {
            out.push_str("<![CDATA[");
            out.push_str(data);
            out.push_str("]]>");
        }
        Node::Comment(data) => {
            out.push_str(indent);
            out.push_str("<!--");
            out.push_str(data);
            out.push_str("-->");
            out.push_str(newl);
        }
        Node::Pi { target, data } => {
            out.push_str(indent);
            out.push_str("<?");
            out.push_str(target);
            out.push(' ');
            out.push_str(data);
            out.push_str("?>");
            out.push_str(newl);
        }
    }
}

/// `Document.toprettyxml(indent, newl)`.
fn toprettyxml(document: &[Node], indent: &str, newl: &str) -> String {
    let mut out = String::from("<?xml version=\"1.0\" ?>");
    out.push_str(newl);
    for node in document {
        write_node(&mut out, node, "", indent, newl);
    }
    out
}

/// `str.isspace` of one character.
fn is_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

/// `str.splitlines`.
fn splitlines(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if matches!(
            c,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            lines.push(&s[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' && chars.peek().map(|(_, n)| *n) == Some('\n') {
                chars.next();
                end += 1;
            }
            start = end;
        }
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

/// `SectionValuePrettyFormatter._is_xml`.
pub(crate) fn is_xml(body: &str) -> bool {
    parse(body).is_some()
}

/// `SectionValuePrettyFormatter._get_pretty_xml`: the whitespace at the
/// ends of each line of `toxml()` stripped, parsed again, and written
/// with `toprettyxml(indent=' ' * 4, newl='\n')`.
pub(crate) fn pretty_xml(body: &str) -> Option<String> {
    let document = parse(body)?;
    let xml = toprettyxml(&document, "", "");
    let stripped: String = splitlines(&xml)
        .into_iter()
        .map(|l| l.trim_matches(is_space))
        .collect();
    let document = parse(&stripped)?;
    Some(toprettyxml(&document, "    ", "\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty() {
        assert_eq!(
            pretty_xml("<a xmlns=\"u\" x=\"1\">\n  <b>t &amp; u</b>\n  <c/>\n</a>").as_deref(),
            Some(
                "<?xml version=\"1.0\" ?>\n<a xmlns=\"u\" x=\"1\">\n    <b>t &amp; u</b>\n    <c/>\n</a>\n"
            )
        );
        assert!(!is_xml("{\"a\": 1}"));
        assert!(!is_xml("<a><b></a>"));
    }
}
