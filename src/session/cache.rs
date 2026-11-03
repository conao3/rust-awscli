//! `botocore.utils.JSONFileCache`: JSON values stored as
//! `<dir>/<key>.json` (mode 0600), written with `json.dumps` defaults.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use crate::value::Value;

#[derive(Debug, Clone)]
pub struct JsonFileCache {
    dir: PathBuf,
}

impl JsonFileCache {
    pub fn new(dir: impl Into<PathBuf>) -> JsonFileCache {
        JsonFileCache { dir: dir.into() }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// `key in cache`.
    pub fn contains(&self, key: &str) -> bool {
        self.path(key).is_file()
    }

    /// `cache[key]`; `None` for a missing or unreadable entry (`KeyError`).
    pub fn get(&self, key: &str) -> Option<Value> {
        let text = std::fs::read_to_string(self.path(key)).ok()?;
        Value::parse_json(&text).ok()
    }

    /// `cache[key] = value`.
    pub fn set(&self, key: &str, value: &Value) -> std::io::Result<()> {
        let text = crate::pyfmt::json_dumps_default(value);
        std::fs::create_dir_all(&self.dir)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.path(key))?;
        f.write_all(text.as_bytes())
    }

    /// `del cache[key]`.
    pub fn remove(&self, key: &str) -> std::io::Result<()> {
        std::fs::remove_file(self.path(key))
    }
}

/// `os.path.expanduser(os.path.join('~', ...))` with the session's
/// environment.
pub fn home_path(config: &crate::config::Config, parts: &[&str]) -> PathBuf {
    let home = crate::config::ini::expand_path("~", &|k| config.env(k).map(str::to_string));
    let mut p = PathBuf::from(home);
    for part in parts {
        p.push(part);
    }
    p
}

/// `~/.aws/cli/cache` (`awscli.customizations.assumerole.CACHE_DIR`).
pub fn cli_cache(config: &crate::config::Config) -> JsonFileCache {
    JsonFileCache::new(home_path(config, &[".aws", "cli", "cache"]))
}

/// `~/.aws/sso/cache`.
pub fn sso_cache(config: &crate::config::Config) -> JsonFileCache {
    JsonFileCache::new(home_path(config, &[".aws", "sso", "cache"]))
}

/// `get_login_token_cache_directory()`.
pub fn login_cache(config: &crate::config::Config) -> JsonFileCache {
    match config.env("AWS_LOGIN_CACHE_DIRECTORY") {
        Some(dir) => JsonFileCache::new(crate::config::ini::expand_path(dir, &|k| {
            config.env(k).map(str::to_string)
        })),
        None => JsonFileCache::new(home_path(config, &[".aws", "login", "cache"])),
    }
}
