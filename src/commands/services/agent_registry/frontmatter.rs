//! The `name` in a SKILL.md frontmatter (`install_skill.py`
//! `_frontmatter_name_from_bytes`, `_no_frontmatter`), read with
//! `ruamel.yaml.YAML(typ='base', pure=True)`: every scalar is a string,
//! tags are ignored, and a repeated key is an error.

use std::collections::HashSet;

use crate::commands::services::cloudformation::yamlhelper::{Node, parse_nodes};

/// `_MAX_SKILL_MD_BYTES`.
pub(crate) const MAX_SKILL_MD_BYTES: usize = 100 * 1024;
const FENCE: &[u8] = b"---";

/// `bytes.rstrip()` (ASCII whitespace).
fn rstrip(b: &[u8]) -> &[u8] {
    let end = b
        .iter()
        .rposition(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c'))
        .map_or(0, |i| i + 1);
    &b[..end]
}

fn strip_is_empty(b: &[u8]) -> bool {
    rstrip(b).is_empty()
}

/// `_no_frontmatter(lines, what)`.
fn no_frontmatter(lines: &[&[u8]], what: &str) -> String {
    let first: &[u8] = lines.first().copied().unwrap_or(b"");
    if first.starts_with(b"\xef\xbb\xbf") {
        return format!("{what} begins with a byte order mark, not frontmatter");
    }
    if rstrip(first).contains(&b'\r') {
        return format!(
            "{what} breaks lines with carriage returns alone, so it does not begin with a frontmatter line"
        );
    }
    let first_text = lines.iter().position(|l| !strip_is_empty(l));
    if let Some(i) = first_text
        && i != 0
        && rstrip(lines[i]) == FENCE
    {
        return format!("{what} has blank lines before its frontmatter");
    }
    format!("{what} does not begin with frontmatter")
}

/// The value of a base-loader node, as far as the name lookup needs it:
/// `Err` when constructing it fails (an unhashable or repeated key).
enum Built {
    Str(String),
    Other,
}

/// A mapping key as a hashable value, or `None` if it is unhashable.
fn key_of(node: &Node) -> Option<String> {
    match node {
        Node::Scalar { value, .. } => Some(format!("s{value}")),
        Node::Seq { items, .. } => {
            let parts: Option<Vec<String>> = items.iter().map(key_of).collect();
            Some(format!("t[{}]", parts?.join("\u{0}")))
        }
        Node::Map { .. } => None,
    }
}

/// `construct_document` of the base constructor: only the top-level
/// `name` is kept.
fn construct(node: &Node) -> Result<Option<Built>, ()> {
    fn check(node: &Node) -> Result<(), ()> {
        match node {
            Node::Scalar { .. } => Ok(()),
            Node::Seq { items, .. } => items.iter().try_for_each(check),
            Node::Map { pairs, .. } => {
                let mut seen = HashSet::new();
                for (k, v) in pairs {
                    check(k)?;
                    let key = key_of(k).ok_or(())?;
                    if !seen.insert(key) {
                        return Err(());
                    }
                    check(v)?;
                }
                Ok(())
            }
        }
    }
    check(node)?;
    let Node::Map { pairs, .. } = node else {
        return Ok(None);
    };
    for (k, v) in pairs {
        if let Node::Scalar { value, .. } = k
            && value == "name"
        {
            return Ok(Some(match v {
                Node::Scalar { value, .. } => Built::Str(value.clone()),
                _ => Built::Other,
            }));
        }
    }
    Ok(None)
}

/// `_frontmatter_name_from_bytes(data, what)`: `Ok(name)` or `Err(why)`.
pub(crate) fn frontmatter_name(data: &[u8], what: &str) -> Result<String, String> {
    let head = &data[..data.len().min(MAX_SKILL_MD_BYTES)];
    let mut lines: Vec<&[u8]> = head.split(|&b| b == b'\n').collect();
    if data.len() > MAX_SKILL_MD_BYTES {
        lines.pop();
    }
    if lines.is_empty() || rstrip(lines[0]) != FENCE {
        return Err(no_frontmatter(&lines, what));
    }
    let Some(end) = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, l)| rstrip(l) == FENCE)
        .map(|(i, _)| i)
    else {
        return Err(format!(
            "the frontmatter of {what} does not end within its first {MAX_SKILL_MD_BYTES} bytes"
        ));
    };
    let invalid = || {
        crate::debuglog::debug(
            "awscli.customizations.agentregistry.install_skill",
            "Could not parse the SKILL.md frontmatter: YAMLError",
        );
        format!("the frontmatter of {what} is not valid UTF-8 YAML")
    };
    let text = String::from_utf8(lines[1..end].join(&b'\n')).map_err(|_| invalid())?;
    let node = parse_nodes(&text).map_err(|_| invalid())?;
    let name = match node {
        Some(n) => construct(&n).map_err(|_| invalid())?,
        None => None,
    };
    match name {
        Some(Built::Str(n)) if !n.is_empty() => Ok(n),
        _ => Err(format!("the frontmatter of {what} has no name")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let what = "the record's SKILL.md";
        let cases: [(&[u8], Result<&str, &str>); 9] = [
            (b"---\nname: pdf-tool\n---\nbody", Ok("pdf-tool")),
            (b"---\nname: 123\n---\n", Ok("123")),
            (b"---\nname: !!int 5\n---\n", Ok("5")),
            (
                b"---\nname: a\nname: b\n---\n",
                Err("the frontmatter of the record's SKILL.md is not valid UTF-8 YAML"),
            ),
            (
                b"---\nname:\n---\n",
                Err("the frontmatter of the record's SKILL.md has no name"),
            ),
            (
                b"name: x\n",
                Err("the record's SKILL.md does not begin with frontmatter"),
            ),
            (
                b"\n---\nname: x\n---\n",
                Err("the record's SKILL.md has blank lines before its frontmatter"),
            ),
            (
                b"\xef\xbb\xbf---\nname: x\n---\n",
                Err("the record's SKILL.md begins with a byte order mark, not frontmatter"),
            ),
            (
                b"---\nname: x\n",
                Err(
                    "the frontmatter of the record's SKILL.md does not end within its first 102400 bytes",
                ),
            ),
        ];
        for (data, expected) in cases {
            assert_eq!(
                frontmatter_name(data, what),
                expected.map(str::to_string).map_err(str::to_string),
                "{}",
                String::from_utf8_lossy(data)
            );
        }
    }
}
