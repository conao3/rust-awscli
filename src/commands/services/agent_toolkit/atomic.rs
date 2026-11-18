//! Skill directory changes that an interruption never leaves half done
//! (`awscli/customizations/agenttoolkit/atomic.py`), and the lock that
//! serializes them across processes (`filelock.py`, `utils.skills_lock`).

use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

const KINDS: [&str; 3] = ["staging", "backup", "trash"];
const BACKUP: &str = "backup";
const TRASH: &str = "trash";

/// `utils.SKILLS_LOCK_FILENAME`.
const SKILLS_LOCK_FILENAME: &str = ".aws-cli-skills.lock";

/// `filelock.DEFAULT_TIMEOUT`.
pub(super) const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a change failed: `str(exception)` of the `OSError` (or
/// `TimeoutError`) it raised.
pub(super) struct Failure(pub String);

fn os_failure(e: &std::io::Error, path: &str) -> Failure {
    Failure(crate::cli::paramfile::os_error(e, path))
}

/// `str(OSError)` of a failed `os.rename(src, dst)`.
fn rename_failure(e: &std::io::Error, src: &str, dst: &str) -> Failure {
    Failure(format!(
        "{} -> {}",
        crate::cli::paramfile::os_error(e, src),
        crate::pyfmt::str_repr(dst)
    ))
}

/// `os.path.lexists`.
pub(super) fn lexists(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// `os.path.islink`.
pub(super) fn islink(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// `os.path.split`.
pub(super) fn split(path: &str) -> (String, String) {
    match path.rfind('/') {
        Some(i) => {
            let head = &path[..=i];
            let trimmed = head.trim_end_matches('/');
            let head = if trimmed.is_empty() { head } else { trimmed };
            (head.to_string(), path[i + 1..].to_string())
        }
        None => (String::new(), path.to_string()),
    }
}

/// `os.path.join(parent, name)`.
pub(super) fn join(parent: &str, name: &str) -> String {
    if name.starts_with('/') || parent.is_empty() {
        name.to_string()
    } else if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// `filelock.file_lock`: an exclusive `flock` on `path`, polled until
/// `timeout`. Released when dropped.
pub(super) struct FileLock(std::fs::File);

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor stays open for the lifetime of `self.0`.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn file_lock(path: &str, timeout: Duration) -> Result<FileLock, Failure> {
    let (parent, _) = split(path);
    if !parent.is_empty() {
        std::fs::create_dir_all(&parent).map_err(|e| os_failure(&e, &parent))?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| os_failure(&e, path))?;
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: `file` is an open descriptor.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(FileLock(file));
        }
        if Instant::now() >= deadline {
            return Err(Failure(format!(
                "timed out after {} seconds waiting for {path}",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `utils.skills_lock`: the lock held while changing skill directories in
/// `parent`.
pub(super) fn skills_lock(parent: &str, timeout: Duration) -> Result<FileLock, Failure> {
    file_lock(&join(parent, SKILLS_LOCK_FILENAME), timeout)
}

fn rename(src: &str, dst: &str) -> Result<(), Failure> {
    std::fs::rename(src, dst).map_err(|e| rename_failure(&e, src, dst))
}

/// `_remove_tree`: best effort, like `shutil.rmtree` with an error handler
/// that ignores every failure.
fn remove_tree(path: &str) {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {}
        _ => return,
    }
    remove_tree_contents(Path::new(path));
}

fn remove_tree_contents(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => remove_tree_contents(&path),
                _ => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// `_sibling`: `<parent>/.<name>.aws-cli-<kind>-<16 hex digits>`.
fn sibling(live_dir: &str, kind: &str) -> String {
    let (parent, name) = split(live_dir);
    let token = uuid::Uuid::new_v4().simple().to_string();
    join(&parent, &format!(".{name}.aws-cli-{kind}-{}", &token[..16]))
}

/// `remove_dir`: move `live_dir` aside, then delete it.
pub(super) fn remove_dir(live_dir: &str) -> Result<(), Failure> {
    let trash_dir = sibling(live_dir, TRASH);
    rename(live_dir, &trash_dir)?;
    remove_tree(&trash_dir);
    Ok(())
}

/// `_LEFTOVER_RE`: the skill name and kind of a leftover entry.
fn parse_leftover(entry: &str) -> Option<(&str, &str)> {
    let rest = entry.strip_prefix('.')?;
    if rest.len() < 17 {
        return None;
    }
    let (head, hex) = rest.split_at(rest.len() - 16);
    if !hex
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let head = head.strip_suffix('-')?;
    KINDS.iter().find_map(|kind| {
        let name = head.strip_suffix(kind)?.strip_suffix(".aws-cli-")?;
        (!name.is_empty()).then_some((name, *kind))
    })
}

/// A skill directory's leftovers: the staging and trash directories, and
/// the backups.
#[derive(Default)]
struct Leftovers {
    to_delete: Vec<String>,
    backups: Vec<String>,
}

/// `_find_leftovers`, in directory order.
fn find_leftovers(parent: &str) -> Vec<(String, Leftovers)> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut found: Vec<(String, Leftovers)> = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some((name, kind)) = parse_leftover(&file_name) else {
            continue;
        };
        let path = join(parent, &file_name);
        if !Path::new(&path).is_dir() || islink(&path) {
            continue;
        }
        let i = match found.iter().position(|(n, _)| n == name) {
            Some(i) => i,
            None => {
                found.push((name.to_string(), Leftovers::default()));
                found.len() - 1
            }
        };
        if kind == BACKUP {
            found[i].1.backups.push(path);
        } else {
            found[i].1.to_delete.push(path);
        }
    }
    found
}

/// `has_leftovers`.
pub(super) fn has_leftovers(parent: &str, name: Option<&str>) -> bool {
    let found = find_leftovers(parent);
    match name {
        Some(name) => found.iter().any(|(n, _)| n == name),
        None => !found.is_empty(),
    }
}

/// `recover`: restore `live_dir` from its backup if it is missing, and
/// delete its other leftovers.
pub(super) fn recover(live_dir: &str) -> Result<(), Failure> {
    let (parent, name) = split(live_dir);
    let leftovers = find_leftovers(&parent)
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, l)| l)
        .unwrap_or_default();
    recover_from(live_dir, leftovers)
}

/// `recover_all`.
pub(super) fn recover_all(parent: &str) -> Result<(), Failure> {
    for (name, leftovers) in find_leftovers(parent) {
        recover_from(&join(parent, &name), leftovers)?;
    }
    Ok(())
}

fn recover_from(live_dir: &str, mut leftovers: Leftovers) -> Result<(), Failure> {
    if !lexists(live_dir)
        && let Some(backup) = leftovers.backups.pop()
    {
        rename(&backup, live_dir)?;
    }
    for path in leftovers.backups.iter().chain(&leftovers.to_delete) {
        remove_tree(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leftover_names() {
        assert_eq!(
            parse_leftover(".s.aws-cli-backup-0123456789abcdef"),
            Some(("s", "backup"))
        );
        assert_eq!(
            parse_leftover(".a.b.aws-cli-trash-0123456789abcdef"),
            Some(("a.b", "trash"))
        );
        assert_eq!(parse_leftover(".aws-cli-trash-0123456789abcdef"), None);
        assert_eq!(parse_leftover(".s.aws-cli-backup-0123456789abcdeF"), None);
        assert_eq!(parse_leftover(".s.aws-cli-other-0123456789abcdef"), None);
    }

    #[test]
    fn split_and_join() {
        assert_eq!(split("/a/b"), ("/a".to_string(), "b".to_string()));
        assert_eq!(split("/b"), ("/".to_string(), "b".to_string()));
        assert_eq!(join("/a", "b"), "/a/b");
        assert_eq!(join("/a/", "b"), "/a/b");
    }
}
