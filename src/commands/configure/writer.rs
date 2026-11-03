//! `awscli/customizations/configure/writer.py` `ConfigFileWriter`: update
//! one section of an INI file in place, keeping every other line (and
//! comments) untouched.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::OnceLock;

use indexmap::IndexMap;
use regex::Regex;

use crate::pyfmt::str_repr;

/// A value in `new_values`: a string, a nested section (`s3 =` with
/// indented keys), or `None` (remove the line).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WriterValue {
    Str(String),
    Nested(IndexMap<String, String>),
    None,
}

impl From<&str> for WriterValue {
    fn from(s: &str) -> WriterValue {
        WriterValue::Str(s.to_string())
    }
}

impl From<String> for WriterValue {
    fn from(s: String) -> WriterValue {
        WriterValue::Str(s)
    }
}

pub(crate) type NewValues = IndexMap<String, WriterValue>;

/// An error `update_config` raises: the `ValueError` /
/// `UnboundLocalError` / `OSError` text (all go to the general handler).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WriterError(pub String);

fn section_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\[(?P<header>[^\]]+)\]").expect("regex"))
}

fn option_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?P<option>[^:=\n][^:=\n]*)\s*[:=]\s*[^\n]*\n?\z").expect("regex")
    })
}

/// `OPTION_REGEX.search(line)`: the `option` group (`.` stops at `\n`
/// and `$` matches at the end or before a final `\n`). Lines scanned
/// here are always single lines of the file.
fn option_match(line: &str) -> Option<String> {
    let caps = option_regex().captures(line)?;
    caps.name("option").map(|m| m.as_str().to_string())
}

/// `str.strip()` / `str.lstrip()` with Python's whitespace set.
fn py_strip(s: &str) -> &str {
    s.trim_matches(char::is_whitespace)
}

fn indent_of(option: &str) -> usize {
    option.chars().count()
        - option
            .trim_start_matches(char::is_whitespace)
            .chars()
            .count()
}

fn validate(value: &str, label: &str, msg_override: Option<String>) -> Result<(), WriterError> {
    if value.contains('\n') || value.contains('\r') {
        return Err(WriterError(msg_override.unwrap_or_else(|| {
            format!(
                "Invalid {label}: newline characters and carriage returns are not allowed: {}",
                str_repr(value)
            )
        })));
    }
    Ok(())
}

/// `ConfigFileWriter.update_config`. `section` is `__section__`
/// (`default` when absent). Returns the stderr warning text from
/// `warn_if_permissive` when `check_permissions` is set.
pub(crate) fn update_config(
    section: Option<&str>,
    new_values: &NewValues,
    config_filename: &str,
    check_permissions: bool,
) -> Result<(), WriterError> {
    let section_name = section.unwrap_or("default");
    validate(section_name, "section name", None)?;
    for (k, v) in new_values {
        validate(k, "key", None)?;
        let msg = || {
            Some(format!(
                "Invalid value for key {k}: newline characters and carriage returns are not allowed."
            ))
        };
        match v {
            WriterValue::Str(s) => validate(s, "value", msg())?,
            WriterValue::Nested(m) => {
                for (sk, sv) in m {
                    validate(sk, "key", None)?;
                    validate(sv, "value", msg())?;
                }
            }
            WriterValue::None => {}
        }
    }
    let path = std::path::Path::new(config_filename);
    if !path.is_file() {
        create_file(config_filename)?;
        write_new_section(section_name, new_values, config_filename)?;
        return Ok(());
    }
    let text = std::fs::read(path).map_err(|e| io_error(&e, config_filename))?;
    let text = String::from_utf8_lossy(&text)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut contents = readlines(&text);
    match update_section_contents(&mut contents, section_name, new_values)? {
        true => {
            std::fs::write(path, contents.concat()).map_err(|e| io_error(&e, config_filename))?;
        }
        false => write_new_section(section_name, new_values, config_filename)?,
    }
    if check_permissions {
        super::warn_if_permissive(config_filename);
    }
    Ok(())
}

fn io_error(e: &std::io::Error, filename: &str) -> WriterError {
    WriterError(crate::cli::paramfile::os_error(e, filename))
}

/// `f.readlines()`.
fn readlines(text: &str) -> Vec<String> {
    text.split_inclusive('\n').map(str::to_string).collect()
}

fn create_file(config_filename: &str) -> Result<(), WriterError> {
    let dirname = match config_filename.rfind('/') {
        Some(0) => "/",
        Some(i) => &config_filename[..i],
        None => "",
    };
    if !std::path::Path::new(dirname).is_dir() {
        std::fs::create_dir_all(if dirname.is_empty() { "" } else { dirname })
            .map_err(|e| io_error(&e, dirname))?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(config_filename)
        .map_err(|e| io_error(&e, config_filename))?;
    Ok(())
}

fn check_file_needs_newline(filename: &str) -> Result<bool, WriterError> {
    let bytes = std::fs::read(filename).map_err(|e| io_error(&e, filename))?;
    Ok(bytes.last().is_some_and(|b| *b != b'\n'))
}

fn write_new_section(
    section_name: &str,
    new_values: &NewValues,
    config_filename: &str,
) -> Result<(), WriterError> {
    let needs_newline = check_file_needs_newline(config_filename)?;
    let mut out = String::new();
    if needs_newline {
        out.push('\n');
    }
    out.push_str(&format!("[{section_name}]\n"));
    let mut contents = Vec::new();
    let mut values = new_values.clone();
    insert_new_values(0, &mut contents, &mut values, "");
    out.push_str(&contents.concat());
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(config_filename)
        .map_err(|e| io_error(&e, config_filename))?;
    f.write_all(out.as_bytes())
        .map_err(|e| io_error(&e, config_filename))?;
    Ok(())
}

fn find_section_start(contents: &[String], section_name: &str) -> Option<usize> {
    for (i, line) in contents.iter().enumerate() {
        let stripped = py_strip(line);
        if stripped.starts_with('#') || stripped.starts_with(';') {
            continue;
        }
        if let Some(m) = section_regex().find(line)
            && matches_section(m.as_str(), section_name)
        {
            return Some(i);
        }
    }
    None
}

/// `_update_section_contents`; `Ok(false)` is `SectionNotFoundError`.
fn update_section_contents(
    contents: &mut Vec<String>,
    section_name: &str,
    new_values: &NewValues,
) -> Result<bool, WriterError> {
    let mut new_values = new_values.clone();
    let Some(section_start) = find_section_start(contents, section_name) else {
        return Ok(false);
    };
    let mut last_matching_line = section_start;
    let mut j = last_matching_line + 1;
    while j < contents.len() {
        let line = contents[j].clone();
        if section_regex().is_match(&line) {
            insert_new_values(last_matching_line, contents, &mut new_values, "");
            return Ok(true);
        }
        if let Some(option) = option_match(&line) {
            last_matching_line = j;
            let key_name = py_strip(&option).to_string();
            if let Some(value) = new_values.get(&key_name).cloned() {
                match value {
                    WriterValue::Nested(sub) => {
                        let mut sub = sub;
                        update_subattributes(j, contents, &mut sub, indent_of(&option))?;
                        return Ok(true);
                    }
                    WriterValue::None => {
                        contents[j] = String::new();
                        new_values.shift_remove(&key_name);
                    }
                    WriterValue::Str(v) => {
                        contents[j] = format!("{key_name} = {v}\n");
                        new_values.shift_remove(&key_name);
                    }
                }
            }
        }
        j += 1;
    }
    if !new_values.is_empty() {
        if !contents.last().is_some_and(|l| l.ends_with('\n')) {
            contents.push("\n".to_string());
        }
        insert_new_values(last_matching_line + 1, contents, &mut new_values, "");
    }
    Ok(true)
}

fn unbound() -> WriterError {
    WriterError(
        "cannot access local variable 'current_indent' where it is not associated with a value"
            .to_string(),
    )
}

/// `_update_subattributes`.
fn update_subattributes(
    index: usize,
    contents: &mut Vec<String>,
    values: &mut IndexMap<String, String>,
    starting_indent: usize,
) -> Result<usize, WriterError> {
    let index = index + 1;
    let mut current_indent: Option<usize> = None;
    let mut i = index;
    let mut broke = false;
    let len = contents.len();
    let mut last_i: Option<usize> = None;
    while i < len {
        last_i = Some(i);
        let line = contents[i].clone();
        if let Some(option) = option_match(&line) {
            let indent = indent_of(&option);
            current_indent = Some(indent);
            let key_name = py_strip(&option).to_string();
            if let Some(v) = values.shift_remove(&key_name) {
                contents[i] = format!("{}{key_name} = {v}\n", " ".repeat(indent));
            }
        }
        let cur = current_indent.ok_or_else(unbound)?;
        if starting_indent == cur || section_regex().is_match(&line) {
            let mut nested: NewValues = values
                .drain(..)
                .map(|(k, v)| (k, WriterValue::Str(v)))
                .collect();
            // Python's `i - 1` is `-1` for `i == 0`, which inserts before
            // the last element; `i` is always at least 1 here.
            insert_new_values_at(i as isize - 1, contents, &mut nested, "    ");
            broke = true;
            break;
        }
        i += 1;
    }
    if !broke {
        let cur = current_indent.ok_or_else(unbound)?;
        let Some(i) = last_i else {
            return Err(unbound());
        };
        if starting_indent != cur {
            let mut nested: NewValues = values
                .drain(..)
                .map(|(k, v)| (k, WriterValue::Str(v)))
                .collect();
            insert_new_values(i, contents, &mut nested, "    ");
        }
        return Ok(i);
    }
    Ok(i)
}

fn insert_new_values(
    line_number: usize,
    contents: &mut Vec<String>,
    new_values: &mut NewValues,
    indent: &str,
) {
    insert_new_values_at(line_number as isize, contents, new_values, indent);
}

/// `_insert_new_values`: `contents.insert(line_number + 1, ...)` with
/// Python's `list.insert` index clamping.
fn insert_new_values_at(
    line_number: isize,
    contents: &mut Vec<String>,
    new_values: &mut NewValues,
    indent: &str,
) {
    let mut new_contents = String::new();
    for (key, value) in new_values.drain(..) {
        match value {
            WriterValue::Nested(sub) => {
                let subindent = format!("{indent}    ");
                new_contents.push_str(&format!("{indent}{key} =\n"));
                for (sk, sv) in sub {
                    new_contents.push_str(&format!("{subindent}{sk} = {sv}\n"));
                }
            }
            WriterValue::Str(v) => new_contents.push_str(&format!("{indent}{key} = {v}\n")),
            WriterValue::None => {}
        }
    }
    if !new_contents.is_empty() {
        let len = contents.len() as isize;
        let mut idx = line_number + 1;
        if idx < 0 {
            idx = (idx + len).max(0);
        }
        let idx = (idx.min(len)) as usize;
        contents.insert(idx, new_contents);
    }
}

fn matches_section(matched: &str, section_name: &str) -> bool {
    let parts: Vec<&str> = section_name.split(' ').collect();
    let unquoted = matched == format!("[{section_name}]");
    if parts.len() > 1 {
        let quoted = matched == format!("[{} \"{}\"]", parts[0], parts[1..].join(" "));
        return unquoted || quoted;
    }
    unquoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(initial: Option<&str>, section: Option<&str>, values: &[(&str, WriterValue)]) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        if let Some(text) = initial {
            std::fs::write(&path, text).unwrap();
        }
        let nv: NewValues = values
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        update_config(section, &nv, path.to_str().unwrap(), false).unwrap();
        std::fs::read_to_string(&path).unwrap()
    }

    fn nested(pairs: &[(&str, &str)]) -> WriterValue {
        WriterValue::Nested(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn new_file() {
        assert_eq!(
            run(None, None, &[("region", "us-west-2".into())]),
            "[default]\nregion = us-west-2\n"
        );
    }

    #[test]
    fn update_and_insert() {
        let initial = "# comment\n[default]\nregion = a\n# keep = me\n\n[profile x]\nfoo = bar";
        assert_eq!(
            run(
                Some(initial),
                None,
                &[("region", "b".into()), ("output", "json".into())]
            ),
            "# comment\n[default]\nregion = b\n# keep = me\noutput = json\n\n[profile x]\nfoo = bar"
        );
        assert_eq!(
            run(Some(initial), Some("profile x"), &[("a", "1".into())]),
            "# comment\n[default]\nregion = a\n# keep = me\n\n[profile x]\nfoo = bar\na = 1\n"
        );
        assert_eq!(
            run(Some(initial), Some("profile y"), &[("a", "1".into())]),
            "# comment\n[default]\nregion = a\n# keep = me\n\n[profile x]\nfoo = bar\n[profile y]\na = 1\n"
        );
        assert_eq!(
            run(
                Some("[default]\nregion = a\nx = y\n"),
                None,
                &[("region", WriterValue::None)]
            ),
            "[default]\nx = y\n"
        );
    }

    #[test]
    fn nested_values() {
        assert_eq!(
            run(
                Some("[default]\ns3 =\n    a = 1\nregion = x\n"),
                None,
                &[("s3", nested(&[("a", "2"), ("b", "3")]))]
            ),
            "[default]\ns3 =\n    a = 2\n    b = 3\nregion = x\n"
        );
        assert_eq!(
            run(
                Some("[default]\ns3 =\n    a = 1\n"),
                None,
                &[("s3", nested(&[("b", "3")]))]
            ),
            "[default]\ns3 =\n    a = 1\n    b = 3\n"
        );
        assert_eq!(
            run(
                Some("[default]\nregion = x\n"),
                None,
                &[("s3", nested(&[("b", "3")]))]
            ),
            "[default]\nregion = x\ns3 =\n    b = 3\n"
        );
    }
}
