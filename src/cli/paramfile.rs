//! Parameter values loaded from local files (`awscli/paramfile.py`
//! `get_paramfile` with `LOCAL_PREFIX_MAP`).

use std::io::ErrorKind;
use std::sync::OnceLock;

use regex::Regex;

use crate::pyfmt::str_repr;
use crate::value::Value;

/// `ResourceLoadingError`: the message.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParamfileError(pub String);

/// `get_paramfile(path, LOCAL_PREFIX_MAP)`: `Ok(None)` when `path` has
/// neither prefix.
pub(crate) fn get_paramfile(path: &str) -> Result<Option<Value>, ParamfileError> {
    if let Some(rest) = path.strip_prefix("file://") {
        return get_file(path, rest, false).map(Some);
    }
    if let Some(rest) = path.strip_prefix("fileb://") {
        return get_file(path, rest, true).map(Some);
    }
    Ok(None)
}

/// `get_file`.
fn get_file(path: &str, rest: &str, binary: bool) -> Result<Value, ParamfileError> {
    let file_path = expandvars(&expanduser(rest));
    let bytes = match std::fs::metadata(&file_path) {
        Ok(m) if m.is_dir() => Err(std::io::Error::from(ErrorKind::IsADirectory)),
        _ => std::fs::read(&file_path),
    };
    let bytes = bytes.map_err(|e| {
        ParamfileError(format!(
            "Unable to load paramfile {path}: {}",
            os_error(&e, &file_path)
        ))
    })?;
    if binary {
        return Ok(Value::Bytes(bytes));
    }
    match String::from_utf8(bytes) {
        Ok(s) => Ok(Value::Str(universal_newlines(&s))),
        Err(_) => Err(ParamfileError(format!(
            "Unable to load paramfile ({file_path}), text contents could not be decoded.  If this is a binary file, please use the fileb:// prefix instead of the file:// prefix."
        ))),
    }
}

/// Text-mode newline translation (`\r\n` and `\r` become `\n`).
fn universal_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// `str(OSError)` for a failed `open()`.
pub(crate) fn os_error(e: &std::io::Error, filename: &str) -> String {
    let (errno, text) = match e.kind() {
        ErrorKind::NotFound => (2, "No such file or directory".to_string()),
        ErrorKind::PermissionDenied => (13, "Permission denied".to_string()),
        ErrorKind::IsADirectory => (21, "Is a directory".to_string()),
        ErrorKind::NotADirectory => (20, "Not a directory".to_string()),
        _ => {
            let msg = e.to_string();
            let text = match msg.rfind(" (os error ") {
                Some(i) => msg[..i].to_string(),
                None => msg,
            };
            (e.raw_os_error().unwrap_or(0), text)
        }
    };
    format!("[Errno {errno}] {text}: {}", str_repr(filename))
}

/// `os.path.expanduser` for `~` and `~/...` (other users' homes are left
/// unexpanded).
pub(crate) fn expanduser(path: &str) -> String {
    if !path.starts_with('~') {
        return path.to_string();
    }
    let i = path[1..].find('/').map_or(path.len(), |p| p + 1);
    if i != 1 {
        return path.to_string();
    }
    let Ok(home) = std::env::var("HOME") else {
        return path.to_string();
    };
    let home = home.trim_end_matches('/');
    let out = format!("{home}{}", &path[i..]);
    if out.is_empty() { "/".to_string() } else { out }
}

/// `os.path.expandvars` (POSIX): `$name` and `${name}` with set
/// variables replaced.
pub(crate) fn expandvars(path: &str) -> String {
    if !path.contains('$') {
        return path.to_string();
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\$([A-Za-z0-9_]+|\{[^}]*\})").expect("regex"));
    let mut out = String::new();
    let mut last = 0;
    let mut i = 0;
    while let Some(m) = re.captures_at(path, i) {
        let whole = m.get(0).expect("match");
        let mut name = m.get(1).expect("group").as_str();
        if name.starts_with('{') && name.ends_with('}') {
            name = &name[1..name.len() - 1];
        }
        match std::env::var(name) {
            Ok(value) => {
                out.push_str(&path[last..whole.start()]);
                out.push_str(&value);
                last = whole.end();
                i = whole.end();
            }
            Err(_) => i = whole.end(),
        }
    }
    out.push_str(&path[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file() {
        let e = get_paramfile("file:///nonexist").unwrap_err();
        assert_eq!(
            e.0,
            "Unable to load paramfile file:///nonexist: [Errno 2] No such file or directory: '/nonexist'"
        );
        assert_eq!(get_paramfile("foo").unwrap(), None);
    }

    #[test]
    fn text_and_binary() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, b"a\r\nb\rc\xff").unwrap();
        let text = get_paramfile(&format!("file://{}", p.display())).unwrap_err();
        assert!(text.0.contains("text contents could not be decoded"));
        let bin = get_paramfile(&format!("fileb://{}", p.display()))
            .unwrap()
            .unwrap();
        assert_eq!(bin, Value::Bytes(b"a\r\nb\rc\xff".to_vec()));
        std::fs::write(&p, b"a\r\nb\rc").unwrap();
        let text = get_paramfile(&format!("file://{}", p.display()))
            .unwrap()
            .unwrap();
        assert_eq!(text, Value::str("a\nb\nc"));
    }
}
