//! The shared config file parser: a port of `botocore/configloader.py`
//! (`raw_config_parse`, `_parse_nested`, `build_profile_map`,
//! `multi_file_load_config`) on top of a port of Python's
//! `configparser.RawConfigParser` reading rules (`_read_inner`,
//! `_handle_continuation_line`, `_handle_rest`, `_handle_header`,
//! `_handle_option`, `_join_multiline_values`) and `shlex.split`.

use std::path::Path;

use indexmap::IndexMap;

/// A value in a config section: a plain string, or an indented
/// sub-section (`s3 =\n  addressing_style = path`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IniValue {
    Str(String),
    Nested(IndexMap<String, String>),
}

impl IniValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            IniValue::Str(s) => Some(s),
            IniValue::Nested(_) => None,
        }
    }

    pub fn as_nested(&self) -> Option<&IndexMap<String, String>> {
        match self {
            IniValue::Nested(m) => Some(m),
            IniValue::Str(_) => None,
        }
    }
}

pub type Section = IndexMap<String, IniValue>;

/// The parsed sections of one file (`raw_config_parse`).
pub type RawConfig = IndexMap<String, Section>;

/// Any `configparser.Error` (or a nested value without `=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError;

/// The error `raw_config_parse` raises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// `ConfigNotFound`: the path is not a regular file.
    NotFound(String),
    /// `ConfigParseError`: "Unable to parse config file: <path>".
    Parse(String),
}

/// `build_profile_map`: profiles, sso-sessions, services, and the other
/// top-level sections.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FullConfig {
    pub profiles: IndexMap<String, Section>,
    pub sso_sessions: IndexMap<String, Section>,
    pub services: IndexMap<String, Section>,
    pub other: IndexMap<String, Section>,
}

/// `os.path.expandvars` followed by `os.path.expanduser` (POSIX
/// `posixpath` semantics).
pub fn expand_path(path: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    expand_user(&expand_vars(path, env), env)
}

/// `posixpath.expandvars`: `$name` and `${name}` with `name` matching
/// `[\w]+`; unknown variables are left unchanged.
fn expand_vars(path: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    if !path.contains('$') {
        return path.to_string();
    }
    let chars: Vec<char> = path.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '$' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let braced = i + 1 < chars.len() && chars[i + 1] == '{';
        if braced {
            if let Some(close) = chars[i + 2..].iter().position(|c| *c == '}') {
                let name: String = chars[i + 2..i + 2 + close].iter().collect();
                // `_varprog` is `\$(\w+|\{[^}]*\})`.
                if let Some(v) = env(&name) {
                    out.push_str(&v);
                } else {
                    out.extend(&chars[i..i + 3 + close]);
                }
                i += 3 + close;
                continue;
            }
            out.push('$');
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        if j == i + 1 {
            out.push('$');
            i += 1;
            continue;
        }
        let name: String = chars[i + 1..j].iter().collect();
        match env(&name) {
            Some(v) => out.push_str(&v),
            None => out.extend(&chars[i..j]),
        }
        i = j;
    }
    out
}

/// `posixpath.expanduser` (only `~` and `~/...`, using `$HOME`; `~user`
/// falls back to the password database).
fn expand_user(path: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    if !path.starts_with('~') {
        return path.to_string();
    }
    let end = path.find('/').unwrap_or(path.len());
    let user = &path[1..end];
    let home = if user.is_empty() {
        env("HOME").or_else(|| home_from_passwd(None))
    } else {
        home_from_passwd(Some(user))
    };
    match home {
        Some(home) => {
            let home = if home == "/" {
                String::new()
            } else {
                home.trim_end_matches('/').to_string()
            };
            let rest = &path[end..];
            let joined = format!("{home}{rest}");
            if joined.is_empty() {
                "/".to_string()
            } else {
                joined
            }
        }
        None => path.to_string(),
    }
}

fn home_from_passwd(user: Option<&str>) -> Option<String> {
    let text = std::fs::read_to_string("/etc/passwd").ok()?;
    let uid = if user.is_none() {
        Some(
            std::fs::read_to_string("/proc/self/status")
                .ok()?
                .lines()
                .find_map(|l| l.strip_prefix("Uid:"))?
                .split_whitespace()
                .next()?
                .to_string(),
        )
    } else {
        None
    };
    text.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 6 {
            return None;
        }
        let matches = match (user, &uid) {
            (Some(u), _) => fields[0] == u,
            (None, Some(uid)) => fields[2] == uid,
            _ => false,
        };
        matches.then(|| fields[5].to_string())
    })
}

/// `raw_config_parse(config_filename)` with `parse_subsections=True`.
/// `path` is already expanded.
pub fn raw_config_parse(path: &str) -> Result<RawConfig, LoadError> {
    let p = Path::new(path);
    if !p.is_file() {
        return Err(LoadError::NotFound(path.to_string()));
    }
    let bytes = std::fs::read(p).map_err(|_| LoadError::Parse(path.to_string()))?;
    let text = String::from_utf8(bytes).map_err(|_| LoadError::Parse(path.to_string()))?;
    parse_ini_text(&text).map_err(|ParseError| LoadError::Parse(path.to_string()))
}

/// `load_config`: `raw_config_parse` + `build_profile_map`.
pub fn load_config(path: &str) -> Result<FullConfig, LoadError> {
    raw_config_parse(path).map(build_profile_map)
}

/// The body of `raw_config_parse` after reading the file.
pub fn parse_ini_text(text: &str) -> Result<RawConfig, ParseError> {
    let parser = RawConfigParser::read(text)?;
    let mut config = RawConfig::new();
    for (name, options) in &parser.sections {
        let mut section = Section::new();
        // `cp.options(section)`: the section's options, then the
        // `[DEFAULT]` ones it does not override.
        let mut keys: Vec<&String> = options.keys().collect();
        for k in parser.defaults.keys() {
            if !options.contains_key(k) {
                keys.push(k);
            }
        }
        for key in keys {
            let value = options
                .get(key)
                .or_else(|| parser.defaults.get(key))
                .cloned()
                .unwrap_or_default();
            let value = if value.starts_with('\n') {
                IniValue::Nested(parse_nested(&value)?)
            } else {
                IniValue::Str(value)
            };
            section.insert(key.clone(), value);
        }
        config.insert(name.clone(), section);
    }
    Ok(config)
}

/// `raw_config_parse(path, parse_subsections=False)`: the options of
/// every section as written (multi-line values joined, not parsed).
pub fn raw_config_parse_flat(
    path: &str,
) -> Result<IndexMap<String, IndexMap<String, String>>, LoadError> {
    let p = Path::new(path);
    if !p.is_file() {
        return Err(LoadError::NotFound(path.to_string()));
    }
    let parse = || -> Result<_, ParseError> {
        let bytes = std::fs::read(p).map_err(|_| ParseError)?;
        let text = String::from_utf8(bytes).map_err(|_| ParseError)?;
        let parser = RawConfigParser::read(&text)?;
        let mut config = IndexMap::new();
        for (name, options) in &parser.sections {
            let mut section: IndexMap<String, String> = options.clone();
            for (k, v) in &parser.defaults {
                if !section.contains_key(k) {
                    section.insert(k.clone(), v.clone());
                }
            }
            config.insert(name.clone(), section);
        }
        Ok(config)
    };
    parse().map_err(|ParseError| LoadError::Parse(path.to_string()))
}

/// `_parse_nested`.
fn parse_nested(value: &str) -> Result<IndexMap<String, String>, ParseError> {
    let mut parsed = IndexMap::new();
    for line in py_splitlines(value) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (k, v) = line.split_once('=').ok_or(ParseError)?;
        parsed.insert(k.trim().to_string(), v.trim().to_string());
    }
    Ok(parsed)
}

/// `str.splitlines()`.
fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut iter = s.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        let is_break = matches!(
            c,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if is_break {
            out.push(&s[start..i]);
            let mut next = i + c.len_utf8();
            if c == '\r'
                && let Some((_, '\n')) = iter.peek()
            {
                iter.next();
                next += 1;
            }
            start = next;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// `_parse_section`: `shlex.split(key)`, keeping the second word when
/// there are exactly two.
fn parse_section_name(key: &str) -> Option<String> {
    let parts = shlex_split(key)?;
    (parts.len() == 2).then(|| parts[1].clone())
}

/// `build_profile_map`.
pub fn build_profile_map(parsed: RawConfig) -> FullConfig {
    let mut full = FullConfig::default();
    for (key, values) in parsed {
        if key.starts_with("profile") {
            if let Some(name) = parse_section_name(&key) {
                full.profiles.insert(name, values);
            }
        } else if key.starts_with("sso-session") {
            if let Some(name) = parse_section_name(&key) {
                full.sso_sessions.insert(name, values);
            }
        } else if key.starts_with("services") {
            if let Some(name) = parse_section_name(&key) {
                full.services.insert(name, values);
            }
        } else if key == "default" {
            full.profiles.insert(key, values);
        } else {
            full.other.insert(key, values);
        }
    }
    full
}

/// `shlex.split(s)` (POSIX mode, no comments). `None` where Python
/// raises `ValueError`.
pub fn shlex_split(s: &str) -> Option<Vec<String>> {
    shlex_split_checked(s).ok()
}

/// [`shlex_split`] with the message of the `ValueError` Python raises.
pub fn shlex_split_checked(s: &str) -> Result<Vec<String>, &'static str> {
    const NO_CLOSING: &str = "No closing quotation";
    const NO_ESCAPED: &str = "No escaped character";
    let mut out = Vec::new();
    let mut token = String::new();
    let mut in_token = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                if in_token {
                    out.push(std::mem::take(&mut token));
                    in_token = false;
                }
            }
            '\\' => {
                token.push(chars.next().ok_or(NO_ESCAPED)?);
                in_token = true;
            }
            '\'' => {
                in_token = true;
                loop {
                    match chars.next().ok_or(NO_CLOSING)? {
                        '\'' => break,
                        ch => token.push(ch),
                    }
                }
            }
            '"' => {
                in_token = true;
                loop {
                    match chars.next().ok_or(NO_CLOSING)? {
                        '"' => break,
                        '\\' => {
                            let next = chars.next().ok_or(NO_ESCAPED)?;
                            if next != '"' && next != '\\' {
                                token.push('\\');
                            }
                            token.push(next);
                        }
                        ch => token.push(ch),
                    }
                }
            }
            ch => {
                token.push(ch);
                in_token = true;
            }
        }
    }
    if in_token {
        out.push(token);
    }
    Ok(out)
}

/// The state `RawConfigParser._read` builds: sections in file order and
/// the `[DEFAULT]` section, with multi-line values joined.
struct RawConfigParser {
    sections: IndexMap<String, IndexMap<String, String>>,
    defaults: IndexMap<String, String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Cursor {
    None,
    Default,
    Section(usize),
}

impl RawConfigParser {
    fn read(text: &str) -> Result<RawConfigParser, ParseError> {
        let mut sections: IndexMap<String, IndexMap<String, Vec<String>>> = IndexMap::new();
        let mut defaults: IndexMap<String, Vec<String>> = IndexMap::new();
        let mut cursect = Cursor::None;
        let mut sectname = String::new();
        let mut optname: Option<String> = None;
        let mut indent_level = 0usize;
        let mut elements_added: std::collections::HashSet<(String, Option<String>)> =
            std::collections::HashSet::new();
        let mut errors = false;

        // Text mode with universal newlines.
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut lines: Vec<&str> = normalized.split_inclusive('\n').collect();
        if lines.is_empty() && !normalized.is_empty() {
            lines.push(&normalized);
        }

        for line in lines {
            let trimmed = line.trim();
            let has_comment_prefix = trimmed.starts_with('#') || trimmed.starts_with(';');
            let clean = if has_comment_prefix { "" } else { trimmed };
            let has_comments = trimmed != clean;
            if clean.is_empty() {
                if !has_comments
                    && cursect != Cursor::None
                    && let Some(name) = optname.as_deref().filter(|n| !n.is_empty())
                {
                    let target = match cursect {
                        Cursor::Default => defaults.get_mut(name),
                        Cursor::Section(i) => sections[i].get_mut(name),
                        Cursor::None => None,
                    };
                    if let Some(values) = target {
                        values.push(String::new());
                    }
                }
                continue;
            }
            let cur_indent_level = line
                .char_indices()
                .find(|(_, c)| !c.is_whitespace())
                .map(|(i, _)| line[..i].chars().count())
                .unwrap_or(0);

            // _handle_continuation_line
            let is_continue = cursect != Cursor::None
                && optname.as_deref().is_some_and(|n| !n.is_empty())
                && cur_indent_level > indent_level;
            if is_continue {
                let name = optname.as_deref().unwrap();
                let target = match cursect {
                    Cursor::Default => defaults.get_mut(name),
                    Cursor::Section(i) => sections[i].get_mut(name),
                    Cursor::None => None,
                };
                if let Some(values) = target {
                    values.push(clean.to_string());
                }
                continue;
            }

            // _handle_rest
            indent_level = cur_indent_level;
            let header = section_header(clean);
            if header.is_none() && cursect == Cursor::None {
                return Err(ParseError);
            }
            if let Some(name) = header {
                // _handle_header
                sectname = name.to_string();
                if let Some(idx) = sections.get_index_of(&sectname) {
                    if elements_added.contains(&(sectname.clone(), None)) {
                        return Err(ParseError);
                    }
                    cursect = Cursor::Section(idx);
                    elements_added.insert((sectname.clone(), None));
                } else if sectname == "DEFAULT" {
                    cursect = Cursor::Default;
                } else {
                    sections.insert(sectname.clone(), IndexMap::new());
                    cursect = Cursor::Section(sections.len() - 1);
                    elements_added.insert((sectname.clone(), None));
                }
                optname = None;
                continue;
            }

            // _handle_option
            let Some(pos) = clean.find(['=', ':']) else {
                errors = true;
                continue;
            };
            let raw_name = clean[..pos].trim_end();
            if raw_name.is_empty() {
                errors = true;
            }
            let name = raw_name.to_lowercase();
            let key = (sectname.clone(), Some(name.clone()));
            if elements_added.contains(&key) {
                return Err(ParseError);
            }
            elements_added.insert(key);
            let value = clean[pos + 1..].trim().to_string();
            match cursect {
                Cursor::Default => {
                    defaults.insert(name.clone(), vec![value]);
                }
                Cursor::Section(i) => {
                    sections[i].insert(name.clone(), vec![value]);
                }
                Cursor::None => unreachable!(),
            }
            optname = Some(name);
        }

        let join = |values: Vec<String>| values.join("\n").trim_end().to_string();
        let sections = sections
            .into_iter()
            .map(|(k, opts)| (k, opts.into_iter().map(|(n, v)| (n, join(v))).collect()))
            .collect();
        let defaults = defaults.into_iter().map(|(n, v)| (n, join(v))).collect();
        if errors {
            return Err(ParseError);
        }
        Ok(RawConfigParser { sections, defaults })
    }
}

/// `SECTCRE.match`: `\[(?P<header>.+)\]` anchored at the start, with the
/// greedy `.+` ending at the last `]`.
fn section_header(clean: &str) -> Option<&str> {
    let rest = clean.strip_prefix('[')?;
    let close = rest.rfind(']')?;
    (close > 0).then(|| &rest[..close])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> RawConfig {
        parse_ini_text(text).unwrap()
    }

    #[test]
    fn nested_sections_and_comments() {
        let c = parse(
            "# comment\n[default]\nregion = us-west-2\ns3 =\n  addressing_style = path\n  \
             signature_version=s3v4\n; another\n[profile dev]\nOutput: text\n",
        );
        let d = &c["default"];
        assert_eq!(d["region"], IniValue::Str("us-west-2".into()));
        let s3 = d["s3"].as_nested().unwrap();
        assert_eq!(s3["addressing_style"], "path");
        assert_eq!(s3["signature_version"], "s3v4");
        assert_eq!(c["profile dev"]["output"], IniValue::Str("text".into()));
    }

    #[test]
    fn parse_errors() {
        assert!(parse_ini_text("region=us-east-1\n").is_err());
        assert!(parse_ini_text("[default]\nregion\n").is_err());
        assert!(parse_ini_text("[a]\nx=1\n[a]\n").is_err());
        assert!(parse_ini_text("[a]\nx=1\nX=2\n").is_err());
        assert!(parse_ini_text("[a]\ns3 =\n  novalue\n").is_err());
    }

    #[test]
    fn default_section_is_inherited() {
        let c = parse("[DEFAULT]\na = 1\n[x]\nb = 2\n[y]\na = 3\n");
        assert_eq!(c.keys().collect::<Vec<_>>(), ["x", "y"]);
        assert_eq!(c["x"].keys().collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(c["y"]["a"], IniValue::Str("3".into()));
    }

    #[test]
    fn profile_map() {
        let full = build_profile_map(parse(
            "[default]\na=1\n[profile \"my prof\"]\nb=2\n[profile x y]\n[profilefoo]\n\
             [sso-session s]\nc=3\n[services dev]\nd=4\n[preview]\ne=5\n",
        ));
        assert_eq!(
            full.profiles.keys().collect::<Vec<_>>(),
            ["default", "my prof"]
        );
        assert!(full.sso_sessions.contains_key("s"));
        assert!(full.services.contains_key("dev"));
        assert!(full.other.contains_key("preview"));
    }

    #[test]
    fn expand() {
        let env = |k: &str| match k {
            "HOME" => Some("/home/u".to_string()),
            "X" => Some("x".to_string()),
            _ => None,
        };
        assert_eq!(
            expand_path("~/.aws/$X/${X}/$Y", &env),
            "/home/u/.aws/x/x/$Y"
        );
    }
}
