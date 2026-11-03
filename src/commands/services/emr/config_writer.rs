//! `awscli/customizations/configure/writer.py` `ConfigFileWriter`, for the
//! configuration `emr create-default-roles` writes
//! (`configutils.ConfigWriter`).

use std::sync::OnceLock;

use indexmap::IndexMap;
use regex::Regex;

/// A value of `new_values`: a string, or a nested section.
#[derive(Debug, Clone)]
pub(crate) enum IniValue {
    Str(String),
    Nested(IndexMap<String, String>),
}

fn section_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*\[([^\]]+)\]").expect("regex"))
}

fn option_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([^:=][^:=]*)\s*([:=])\s*(.*)$").expect("regex"))
}

/// `OPTION_REGEX.search(line)`: the `option` group (Python's `$` also
/// matches before a final newline).
fn option_match(line: &str) -> Option<String> {
    let line = line.strip_suffix('\n').unwrap_or(line);
    option_re()
        .captures(line)
        .map(|c| c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default())
}

fn indent_of(option: &str) -> usize {
    option.len() - option.trim_start().len()
}

/// `ConfigFileWriter.update_config`.
pub(crate) fn update_config(
    section_name: &str,
    new_values: IndexMap<String, IniValue>,
    filename: &str,
) -> std::io::Result<()> {
    let path = std::path::Path::new(filename);
    if !path.is_file() {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
            && !dir.is_dir()
        {
            std::fs::create_dir_all(dir)?;
        }
        create_file(path)?;
        return write_new_section(section_name, new_values, path);
    }
    let text = std::fs::read_to_string(path)?;
    let mut contents: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    let mut values = new_values.clone();
    match update_section_contents(&mut contents, section_name, &mut values) {
        Some(()) => std::fs::write(path, contents.concat()),
        None => write_new_section(section_name, new_values, path),
    }
}

fn create_file(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map(|_| ())
}

fn write_new_section(
    section_name: &str,
    mut new_values: IndexMap<String, IniValue>,
    path: &std::path::Path,
) -> std::io::Result<()> {
    let existing = std::fs::read(path).unwrap_or_default();
    let mut out = String::new();
    if !existing.is_empty() && existing.last() != Some(&b'\n') {
        out.push('\n');
    }
    out.push_str(&format!("[{section_name}]\n"));
    let mut contents = Vec::new();
    insert_new_values(-1, &mut contents, &mut new_values, "");
    out.push_str(&contents.concat());
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
    f.write_all(out.as_bytes())
}

fn matches_section(header: &str, section_name: &str) -> bool {
    let parts: Vec<&str> = section_name.split(' ').collect();
    let unquoted = header == format!("[{section_name}]");
    if parts.len() > 1 {
        let quoted = header == format!("[{} \"{}\"]", parts[0], parts[1..].join(" "));
        return unquoted || quoted;
    }
    unquoted
}

fn find_section_start(contents: &[String], section_name: &str) -> Option<usize> {
    for (i, line) in contents.iter().enumerate() {
        let t = line.trim();
        if t.starts_with('#') || t.starts_with(';') {
            continue;
        }
        if let Some(m) = section_re().find(line)
            && matches_section(m.as_str(), section_name)
        {
            return Some(i);
        }
    }
    None
}

fn update_section_contents(
    contents: &mut Vec<String>,
    section_name: &str,
    new_values: &mut IndexMap<String, IniValue>,
) -> Option<()> {
    let start = find_section_start(contents, section_name)?;
    let mut last_matching_line = start;
    let mut j = start + 1;
    while j < contents.len() {
        let line = contents[j].clone();
        if section_re().is_match(&line) {
            insert_new_values(last_matching_line as isize, contents, new_values, "");
            return Some(());
        }
        if let Some(option) = option_match(&line) {
            last_matching_line = j;
            let key = option.trim().to_string();
            match new_values.get(&key).cloned() {
                Some(IniValue::Str(v)) => {
                    contents[j] = format!("{key} = {v}\n");
                    new_values.shift_remove(&key);
                }
                Some(IniValue::Nested(sub)) => {
                    update_subattributes(j, contents, sub, indent_of(&option));
                    return Some(());
                }
                None => {}
            }
        }
        j += 1;
    }
    if !new_values.is_empty() {
        if !contents.last().is_some_and(|l| l.ends_with('\n')) {
            contents.push("\n".to_string());
        }
        insert_new_values(last_matching_line as isize + 1, contents, new_values, "");
    }
    Some(())
}

fn update_subattributes(
    index: usize,
    contents: &mut Vec<String>,
    values: IndexMap<String, String>,
    starting_indent: usize,
) {
    let mut values: IndexMap<String, IniValue> = values
        .into_iter()
        .map(|(k, v)| (k, IniValue::Str(v)))
        .collect();
    let mut current_indent: Option<usize> = None;
    let mut i = index + 1;
    let mut broke = false;
    while i < contents.len() {
        let line = contents[i].clone();
        if let Some(option) = option_match(&line) {
            let indent = indent_of(&option);
            current_indent = Some(indent);
            let key = option.trim().to_string();
            if let Some(IniValue::Str(v)) = values.get(&key).cloned() {
                contents[i] = format!("{}{key} = {v}\n", " ".repeat(indent));
                values.shift_remove(&key);
            }
        }
        if current_indent == Some(starting_indent) || section_re().is_match(&line) {
            insert_new_values(i as isize - 1, contents, &mut values, "    ");
            broke = true;
            break;
        }
        i += 1;
    }
    if !broke && current_indent != Some(starting_indent) {
        let last = contents.len().saturating_sub(1).max(index + 1);
        insert_new_values(last as isize, contents, &mut values, "    ");
    }
}

fn insert_new_values(
    line_number: isize,
    contents: &mut Vec<String>,
    new_values: &mut IndexMap<String, IniValue>,
    indent: &str,
) {
    let mut new_contents = String::new();
    for (key, value) in std::mem::take(new_values) {
        match value {
            IniValue::Nested(sub) => {
                new_contents.push_str(&format!("{indent}{key} =\n"));
                for (sk, sv) in sub {
                    new_contents.push_str(&format!("{indent}    {sk} = {sv}\n"));
                }
            }
            IniValue::Str(v) => new_contents.push_str(&format!("{indent}{key} = {v}\n")),
        }
    }
    if !new_contents.is_empty() {
        let at = ((line_number + 1).max(0) as usize).min(contents.len());
        contents.insert(at, new_contents);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_section_written_into_existing_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "[default]\nregion = us-east-1\n").unwrap();
        let p = path.to_str().unwrap();
        let mut sub = IndexMap::new();
        sub.insert("service_role".to_string(), "EMR_DefaultRole".to_string());
        let mut v = IndexMap::new();
        v.insert("emr".to_string(), IniValue::Nested(sub));
        update_config("default", v, p).unwrap();
        let mut sub = IndexMap::new();
        sub.insert(
            "instance_profile".to_string(),
            "EMR_EC2_DefaultRole".to_string(),
        );
        let mut v = IndexMap::new();
        v.insert("emr".to_string(), IniValue::Nested(sub));
        update_config("default", v, p).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[default]\nregion = us-east-1\nemr =\n    service_role = EMR_DefaultRole\n    instance_profile = EMR_EC2_DefaultRole\n"
        );
    }
}
