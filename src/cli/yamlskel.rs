//! `--generate-cli-skeleton yaml-input`
//! (`awscli/customizations/generatecliskeleton.py` `YAMLArgumentGenerator`
//! and `_generate_yaml_input_skeleton`): the skeleton as ruamel.yaml's
//! round-trip dumper writes it, with the end-of-line comments
//! `get_shape_doc_overview` (`awscli/customizations/utils.py`) builds.

use std::sync::OnceLock;

use regex::Regex;

use crate::model::Shape;

/// A node of the skeleton.
enum Node {
    /// A `CommentedMap`: (key, value, end-of-line comment) in order.
    Commented(Vec<(String, Node, Option<String>)>),
    /// A plain `dict` (`_generate_type_map`).
    Dict(Vec<(String, Node)>),
    List(Vec<Node>),
    Str(String),
    Int,
    Float,
    Bool,
    Timestamp,
    /// `_Bytes`: `!!binary ''`.
    Binary,
    /// `None` (types the generator has no case for).
    Null,
}

fn generate(shape: &Shape<'_>, stack: &mut Vec<String>) -> Node {
    if shape.type_name() == "blob" {
        return Node::Binary;
    }
    stack.push(shape.name.to_string());
    let node = match shape.type_name() {
        "structure" => {
            if stack.iter().filter(|n| *n == shape.name).count() > 1 {
                Node::Commented(Vec::new())
            } else {
                let required = shape.required();
                let mut entries = Vec::new();
                for (member, member_shape) in shape.members() {
                    let value = generate(&member_shape, stack);
                    let mut parts = Vec::new();
                    if required.contains(&member) {
                        parts.push("[REQUIRED]".to_string());
                    }
                    parts.push(doc_overview(
                        member_shape.get_str("documentation").unwrap_or(""),
                    ));
                    let enums = super::skeleton::enum_values(&member_shape);
                    if !enums.is_empty() {
                        parts.push(format!("Valid values are: {}.", enums.join(", ")));
                    }
                    let comment = parts.join(" ");
                    let comment = (!comment.is_empty()
                        && !comment.chars().all(super::pynum::is_py_space))
                    .then_some(comment);
                    entries.push((member.to_string(), value, comment));
                }
                Node::Commented(entries)
            }
        }
        "list" => {
            let member = shape.list_member().expect("list member");
            Node::List(vec![generate(&member, stack)])
        }
        "map" => {
            let value = shape.map_value().expect("map value");
            Node::Dict(vec![("KeyName".to_string(), generate(&value, stack))])
        }
        "string" => Node::Str(
            super::skeleton::enum_values(shape)
                .first()
                .map_or(String::new(), |e| e.to_string()),
        ),
        "integer" | "long" => Node::Int,
        "float" | "double" => Node::Float,
        "boolean" => Node::Bool,
        "timestamp" => Node::Timestamp,
        _ => Node::Null,
    };
    stack.pop();
    node
}

/// `get_shape_doc_overview`.
fn doc_overview(documentation: &str) -> String {
    let content = strip_xml(documentation);
    static SENTENCE: OnceLock<Regex> = OnceLock::new();
    let re = SENTENCE.get_or_init(|| Regex::new("[.:]+").expect("regex"));
    let mut first = re.split(&content).next().unwrap_or_default().to_string();
    if !first.is_empty() {
        first.push('.');
    }
    first.replace(['\n', '\u{2028}'], " ")
}

/// `_strip_xml_from_documentation`: the text nodes of
/// `<doc>{documentation}</doc>` when it is well-formed XML, the
/// documentation itself otherwise.
fn strip_xml(documentation: &str) -> String {
    let doc = format!("<doc>{documentation}</doc>");
    xml_text(&doc).unwrap_or_else(|| documentation.to_string())
}

fn is_name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == ':' || (c as u32) > 0x7f && !c.is_whitespace()
}

fn is_name_char(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == '-' || c == '.' || c == '\u{b7}'
}

fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || (c >= ' ' && c != '\u{fffe}' && c != '\u{ffff}')
}

/// Parse a well-formed XML document the way expat accepts it and return
/// the concatenated text nodes (`minidom` keeps CDATA sections and
/// comments out of `TEXT_NODE`s).
fn xml_text(doc: &str) -> Option<String> {
    let s: Vec<char> = doc
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .collect();
    let mut i = 0;
    let mut out = String::new();
    let mut stack: Vec<String> = Vec::new();
    let mut seen_root = false;
    let name = |i: &mut usize| -> Option<String> {
        let start = *i;
        if !s.get(*i).copied().is_some_and(is_name_start) {
            return None;
        }
        while s.get(*i).copied().is_some_and(is_name_char) {
            *i += 1;
        }
        Some(s[start..*i].iter().collect())
    };
    let reference = |i: &mut usize| -> Option<char> {
        *i += 1;
        let start = *i;
        while *i < s.len() && s[*i] != ';' {
            *i += 1;
        }
        if *i >= s.len() {
            return None;
        }
        let r: String = s[start..*i].iter().collect();
        *i += 1;
        let c = match r.as_str() {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = if let Some(h) = r.strip_prefix("#x") {
                    u32::from_str_radix(h, 16).ok()?
                } else {
                    r.strip_prefix('#')?.parse::<u32>().ok()?
                };
                char::from_u32(code).filter(|&c| is_xml_char(c))?
            }
        };
        Some(c)
    };
    let starts = |i: usize, lit: &str| -> bool {
        let lit: Vec<char> = lit.chars().collect();
        s.len() >= i + lit.len() && s[i..i + lit.len()] == lit[..]
    };
    while i < s.len() {
        let c = s[i];
        if !is_xml_char(c) {
            return None;
        }
        if c == '<' {
            if starts(i, "<!--") {
                i += 4;
                loop {
                    if i + 1 >= s.len() {
                        return None;
                    }
                    if s[i] == '-' && s[i + 1] == '-' {
                        if s.get(i + 2) == Some(&'>') {
                            i += 3;
                            break;
                        }
                        return None;
                    }
                    i += 1;
                }
                continue;
            }
            if starts(i, "<![CDATA[") {
                if stack.is_empty() {
                    return None;
                }
                i += 9;
                while !starts(i, "]]>") {
                    if i >= s.len() {
                        return None;
                    }
                    i += 1;
                }
                i += 3;
                continue;
            }
            if starts(i, "<?") {
                i += 2;
                let target = name(&mut i)?;
                if target.eq_ignore_ascii_case("xml") {
                    return None;
                }
                while !starts(i, "?>") {
                    if i >= s.len() {
                        return None;
                    }
                    i += 1;
                }
                i += 2;
                continue;
            }
            if starts(i, "</") {
                i += 2;
                let n = name(&mut i)?;
                while s.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
                    i += 1;
                }
                if s.get(i) != Some(&'>') || stack.pop().as_deref() != Some(n.as_str()) {
                    return None;
                }
                i += 1;
                continue;
            }
            i += 1;
            if stack.is_empty() && seen_root {
                return None;
            }
            let n = name(&mut i)?;
            let mut attrs: Vec<String> = Vec::new();
            loop {
                let had_space = s.get(i).is_some_and(|c| c.is_ascii_whitespace());
                while s.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
                    i += 1;
                }
                match s.get(i) {
                    Some('>') => {
                        i += 1;
                        stack.push(n);
                        seen_root = true;
                        break;
                    }
                    Some('/') if s.get(i + 1) == Some(&'>') => {
                        i += 2;
                        seen_root = true;
                        break;
                    }
                    Some(_) if had_space => {
                        let a = name(&mut i)?;
                        if attrs.contains(&a) {
                            return None;
                        }
                        attrs.push(a);
                        while s.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
                            i += 1;
                        }
                        if s.get(i) != Some(&'=') {
                            return None;
                        }
                        i += 1;
                        while s.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
                            i += 1;
                        }
                        let quote = *s.get(i)?;
                        if quote != '"' && quote != '\'' {
                            return None;
                        }
                        i += 1;
                        while s.get(i) != Some(&quote) {
                            match s.get(i) {
                                None | Some('<') => return None,
                                Some('&') => {
                                    reference(&mut i)?;
                                }
                                Some(_) => i += 1,
                            }
                        }
                        i += 1;
                    }
                    _ => return None,
                }
            }
            continue;
        }
        if stack.is_empty() {
            if !c.is_ascii_whitespace() {
                return None;
            }
            i += 1;
            continue;
        }
        if c == '&' {
            out.push(reference(&mut i)?);
            continue;
        }
        if starts(i, "]]>") {
            return None;
        }
        out.push(c);
        i += 1;
    }
    if !stack.is_empty() || !seen_root {
        return None;
    }
    Some(out)
}

/// The YAML 1.2 implicit resolvers of ruamel.yaml: whether a plain scalar
/// would load as something other than a string.
fn resolves_to_non_string(s: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(concat!(
            r"^(?:",
            r"true|True|TRUE|false|False|FALSE",
            r"|~|null|Null|NULL",
            r"|[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+]?[0-9]+)?",
            r"|[-+]?(?:[0-9][0-9_]*)(?:[eE][-+]?[0-9]+)",
            r"|\.[0-9_]+(?:[eE][-+][0-9]+)?",
            r"|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN)",
            r"|[-+]?0b[0-1_]+|[-+]?0o?[0-7_]+|[-+]?[0-9_]+|[-+]?0x[0-9a-fA-F_]+",
            r"|<<|=",
            r"|[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]",
            r"|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?(?:[Tt]|[ \t]+)[0-9][0-9]?:[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?",
            r")$"
        ))
        .expect("regex")
    });
    re.is_match(s)
}

/// `Emitter.analyze_scalar(...).allow_block_plain` for a non-empty scalar.
fn allow_block_plain(scalar: &str) -> bool {
    let chars: Vec<char> = scalar.chars().collect();
    let ws = |c: char| {
        matches!(
            c,
            '\0' | ' ' | '\t' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}'
        )
    };
    let mut block_indicators = scalar.starts_with("---") || scalar.starts_with("...");
    let mut special = false;
    let mut line_breaks = false;
    let mut preceded = true;
    let mut followed = chars.len() == 1 || ws(chars[1]);
    let (mut leading_space, mut trailing_space, mut leading_break, mut trailing_break) =
        (false, false, false, false);
    let (mut previous_space, mut previous_break, mut break_space, mut space_break) =
        (false, false, false, false);
    for (index, &ch) in chars.iter().enumerate() {
        if index == 0 {
            if "#,[]{}&*!|>'\"%@`".contains(ch) {
                block_indicators = true;
            }
            if "?:".contains(ch) && followed {
                block_indicators = true;
            }
            if ch == '-' && followed {
                block_indicators = true;
            }
        } else {
            if ch == ':' && followed {
                block_indicators = true;
            }
            if ch == '#' && preceded {
                block_indicators = true;
            }
        }
        if matches!(ch, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            line_breaks = true;
        }
        if !(ch == '\n' || (' '..='\u{7e}').contains(&ch)) {
            let printable = (ch == '\u{85}'
                || ('\u{a0}'..='\u{d7ff}').contains(&ch)
                || ('\u{e000}'..='\u{fffd}').contains(&ch)
                || ch >= '\u{10000}')
                && ch != '\u{feff}';
            if !printable {
                special = true;
            }
        }
        if ch == ' ' {
            leading_space |= index == 0;
            trailing_space |= index == chars.len() - 1;
            break_space |= previous_break;
            previous_space = true;
            previous_break = false;
        } else if matches!(ch, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            leading_break |= index == 0;
            trailing_break |= index == chars.len() - 1;
            space_break |= previous_space;
            previous_space = false;
            previous_break = true;
        } else {
            previous_space = false;
            previous_break = false;
        }
        preceded = ws(ch);
        followed = index + 2 >= chars.len() || ws(chars[index + 2]);
    }
    !(leading_space
        || leading_break
        || trailing_space
        || trailing_break
        || break_space
        || special
        || space_break
        || line_breaks
        || block_indicators)
}

/// A string scalar: plain when ruamel would emit it plain, single-quoted
/// otherwise.
fn scalar(s: &str) -> String {
    if !s.is_empty() && !resolves_to_non_string(s) && allow_block_plain(s) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

/// The inline text of a scalar or empty collection, or `None` for a block
/// collection.
fn inline(node: &Node) -> Option<String> {
    Some(match node {
        Node::Commented(e) if e.is_empty() => "{}".to_string(),
        Node::Dict(e) if e.is_empty() => "{}".to_string(),
        Node::List(l) if l.is_empty() => "[]".to_string(),
        Node::Commented(_) | Node::Dict(_) | Node::List(_) => return None,
        Node::Str(s) => scalar(s),
        Node::Int => "0".to_string(),
        Node::Float => "0.0".to_string(),
        Node::Bool => "true".to_string(),
        Node::Timestamp => "1970-01-01 00:00:00".to_string(),
        Node::Binary => "!!binary ''".to_string(),
        Node::Null => String::new(),
    })
}

/// `CommentedMap._yaml_get_column` for the key at `index` (the comments
/// are added as the keys are inserted, so the map ends at `index`):
/// whether a column is found.
fn has_column(entries: &[(String, Node, Option<String>)], index: usize) -> bool {
    let commented = |i: usize| entries[i].2.is_some();
    if index > 0 && commented(index - 1) {
        return true;
    }
    let key = &entries[index].0;
    for (i, (k, _, _)) in entries[..=index].iter().enumerate() {
        if k >= key {
            break;
        }
        if commented(i) {
            return true;
        }
    }
    false
}

/// Emit a mapping whose first key goes at `first_prefix` (e.g. `- ` of a
/// sequence entry) and whose other keys go at `indent`.
fn emit_map(
    entries: Vec<(String, Node, Option<String>)>,
    indent: usize,
    first_prefix: &str,
    out: &mut String,
) {
    let columns: Vec<bool> = (0..entries.len())
        .map(|i| has_column(&entries, i))
        .collect();
    for (i, (key, value, comment)) in entries.into_iter().enumerate() {
        if i == 0 {
            out.push_str(first_prefix);
        } else {
            out.push_str(&" ".repeat(indent));
        }
        out.push_str(&scalar(&key));
        out.push(':');
        let comment = comment.map(|c| {
            if columns[i] {
                format!(" # {c}")
            } else {
                format!("  # {c}")
            }
        });
        match inline(&value) {
            Some(text) => {
                if !text.is_empty() {
                    let line_start = out.rfind('\n').map_or(0, |p| p + 1);
                    let column = out[line_start..].chars().count();
                    if is_plain(&value) {
                        write_plain(&text, column, indent + 2, out);
                    } else {
                        out.push(' ');
                        out.push_str(&text);
                    }
                }
                if let Some(c) = comment {
                    out.push_str(&c);
                }
                out.push('\n');
            }
            None => {
                if let Some(c) = comment {
                    out.push_str(&c);
                }
                out.push('\n');
                emit_block(value, indent, true, out);
            }
        }
    }
}

fn is_plain(node: &Node) -> bool {
    match node {
        Node::Str(s) => !scalar(s).starts_with('\''),
        Node::Timestamp => true,
        _ => false,
    }
}

/// `Emitter.write_plain` for a mapping value written at `column` (after
/// the `:`), wrapping at the 80-column `best_width` to `indent`.
fn write_plain(text: &str, mut column: usize, indent: usize, out: &mut String) {
    const BEST_WIDTH: usize = 80;
    out.push(' ');
    column += 1;
    let chars: Vec<char> = text.chars().collect();
    let write_indent = |column: &mut usize, out: &mut String| {
        out.push('\n');
        out.push_str(&" ".repeat(indent));
        *column = indent;
    };
    let mut spaces = false;
    let (mut start, mut end) = (0, 0);
    while end <= chars.len() {
        let ch = chars.get(end).copied();
        if spaces {
            if ch != Some(' ') {
                if start + 1 == end && column >= BEST_WIDTH {
                    write_indent(&mut column, out);
                } else {
                    out.extend(&chars[start..end]);
                    column += end - start;
                }
                start = end;
            }
        } else if ch.is_none() || ch == Some(' ') {
            let len = end - start;
            if len + column > BEST_WIDTH && column > indent {
                write_indent(&mut column, out);
            }
            out.extend(&chars[start..end]);
            column += len;
            start = end;
        }
        if let Some(c) = ch {
            spaces = c == ' ';
        }
        end += 1;
    }
}

/// Emit a block collection that is the value of a mapping key at
/// `indent` (`in_map`) or an item of a sequence.
fn emit_block(node: Node, indent: usize, in_map: bool, out: &mut String) {
    match node {
        Node::Commented(entries) => {
            let child = if in_map { indent + 2 } else { indent };
            let prefix = " ".repeat(child);
            emit_map(entries, child, &prefix, out);
        }
        Node::Dict(entries) => {
            let child = if in_map { indent + 2 } else { indent };
            let prefix = " ".repeat(child);
            emit_map(
                entries.into_iter().map(|(k, v)| (k, v, None)).collect(),
                child,
                &prefix,
                out,
            );
        }
        Node::List(items) => emit_list(items, indent, out),
        _ => {}
    }
}

/// A block sequence whose dashes go at `indent`.
fn emit_list(items: Vec<Node>, indent: usize, out: &mut String) {
    let dash = format!("{}- ", " ".repeat(indent));
    for item in items {
        match inline(&item) {
            Some(text) => {
                out.push_str(dash.trim_end_matches(' '));
                if !text.is_empty() {
                    out.push(' ');
                    out.push_str(&text);
                }
                out.push('\n');
            }
            None => match item {
                Node::Commented(entries) => emit_map(entries, indent + 2, &dash, out),
                Node::Dict(entries) => emit_map(
                    entries.into_iter().map(|(k, v)| (k, v, None)).collect(),
                    indent + 2,
                    &dash,
                    out,
                ),
                Node::List(inner) => {
                    out.push_str(&dash);
                    let mut nested = String::new();
                    emit_list(inner, indent + 2, &mut nested);
                    out.push_str(nested.trim_start_matches(' '));
                }
                _ => {}
            },
        }
    }
}

/// `_generate_yaml_input_skeleton`.
pub(crate) fn input_yaml(shape: Option<&Shape<'_>>) -> String {
    let node = match shape {
        Some(s) => generate(s, &mut Vec::new()),
        None => Node::Commented(Vec::new()),
    };
    let mut out = String::new();
    match inline(&node) {
        Some(text) => {
            out.push_str(&text);
            out.push('\n');
        }
        None => emit_block(node, 0, false, &mut out),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview() {
        assert_eq!(doc_overview("<p>The name. More.</p>"), "The name.");
        assert_eq!(
            doc_overview("<p>Unclosed &nbsp; tag.</p>"),
            "<p>Unclosed &nbsp; tag."
        );
        assert_eq!(doc_overview("<p>a<code>b</code>c: d</p>"), "abc.");
        assert_eq!(doc_overview(""), "");
    }

    #[test]
    fn scalars() {
        assert_eq!(scalar(""), "''");
        assert_eq!(scalar("EQ"), "EQ");
        assert_eq!(scalar("NULL"), "'NULL'");
        assert_eq!(scalar("true"), "'true'");
        assert_eq!(scalar("a: b"), "'a: b'");
        assert_eq!(scalar("*"), "'*'");
        assert_eq!(scalar("application/json"), "application/json");
    }
}
