use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::HelpRequest;

fn identity(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    Some(format!(
        "{}:{}:{}:{}:{}",
        path.display(),
        meta.dev(),
        meta.ino(),
        meta.size(),
        meta.mtime_nsec() + meta.mtime() * 1_000_000_000
    ))
}

fn dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME").filter(|h| !h.is_empty())?).join(".cache"),
    };
    Some(base.join("awsr").join("help"))
}

pub(super) fn path(req: &HelpRequest, device: &str, formatter: &str) -> Option<PathBuf> {
    let mut key = format!("{req:?}\n{device}\n");
    key.push_str(&identity(&std::env::current_exe().ok()?)?);
    key.push('\n');
    key.push_str(&identity(&super::render::find_on_path(formatter)?)?);
    key.push('\n');
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k.starts_with("GROFF") || k.starts_with("LC_") || k == "LANG")
        .collect();
    env.sort();
    for (k, v) in env {
        let _ = writeln!(key, "{k}={v}");
    }
    Some(dir()?.join(hex::encode(Sha256::digest(key.as_bytes()))))
}

pub(super) fn read(path: &Path) -> Option<(Vec<u8>, Vec<u8>)> {
    let data = std::fs::read(path).ok()?;
    let len = u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as usize;
    let stderr = data.get(4..4 + len)?.to_vec();
    Some((stderr, data[4 + len..].to_vec()))
}

pub(super) fn write(path: &Path, stderr: &[u8], output: &[u8]) {
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut data = Vec::with_capacity(4 + stderr.len() + output.len());
    data.extend_from_slice(&(stderr.len() as u32).to_le_bytes());
    data.extend_from_slice(stderr);
    data.extend_from_slice(output);
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, &data)
        .and_then(|_| std::fs::rename(&tmp, path))
        .is_err()
    {
        let _ = std::fs::remove_file(&tmp);
    }
}
