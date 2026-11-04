//! A cache of reference runs. The reference CLI takes most of the time
//! of a case, and its run depends only on the reference build and the
//! prepared case, so a run is stored under a digest of both and replayed
//! instead of starting the reference again. The case directory and the
//! stub's `host:port` are stored as placeholders and replaced with the
//! current ones when the run is loaded.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::case::{Case, Prepared};
use crate::run::{Env, RunOut};
use crate::server::Recorded;

const DIR_PLACEHOLDER: &[u8] = b"\x00casedir\x00";
const HOST_PLACEHOLDER: &[u8] = b"\x00hostport\x00";

#[derive(Serialize, Deserialize)]
struct Entry {
    run: RunOut,
    requests: Vec<Recorded>,
}

#[derive(Debug, Clone)]
pub struct RefCache {
    dir: PathBuf,
    /// Run the reference CLI again and replace the stored runs.
    refresh: bool,
    /// The resolved reference CLI (a Nix store path names its build).
    reference: PathBuf,
}

/// The values of one run that the placeholders stand for.
pub struct Site<'a> {
    pub dir: &'a str,
    pub host_port: &'a str,
}

impl RefCache {
    pub fn new(dir: PathBuf, reference: &Path, refresh: bool) -> RefCache {
        let reference = which(reference)
            .and_then(|p| std::fs::canonicalize(p).ok())
            .unwrap_or_else(|| reference.to_path_buf());
        RefCache {
            dir,
            refresh,
            reference,
        }
    }

    pub fn key(&self, case: &Case, p: &Prepared, env: &Env, host: &str) -> String {
        let text = format!(
            "{}\n{case:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{env:?}\n{host}",
            self.reference.display(),
            p.argv,
            p.files,
            p.responses,
            p.outfile,
        );
        hex::encode(Sha1::digest(text.as_bytes()))
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(&key[..2]).join(format!("{key}.cbor"))
    }

    pub fn load(&self, key: &str, site: &Site<'_>) -> Option<(RunOut, Vec<Recorded>)> {
        if self.refresh {
            return None;
        }
        let data = std::fs::read(self.path(key)).ok()?;
        let mut entry: Entry = ciborium::de::from_reader(data.as_slice()).ok()?;
        let subst = |b: &mut Vec<u8>| {
            *b = replace(b, DIR_PLACEHOLDER, site.dir.as_bytes());
            *b = replace(b, HOST_PLACEHOLDER, site.host_port.as_bytes());
        };
        map_bytes(&mut entry, subst);
        Some((entry.run, entry.requests))
    }

    pub fn store(
        &self,
        key: &str,
        site: &Site<'_>,
        run: &RunOut,
        requests: &[Recorded],
    ) -> Result<()> {
        if run.timed_out {
            return Ok(());
        }
        let mut entry = Entry {
            run: run.clone(),
            requests: requests.to_vec(),
        };
        let subst = |b: &mut Vec<u8>| {
            *b = replace(b, site.dir.as_bytes(), DIR_PLACEHOLDER);
            *b = replace(b, site.host_port.as_bytes(), HOST_PLACEHOLDER);
        };
        map_bytes(&mut entry, subst);
        let path = self.path(key);
        std::fs::create_dir_all(path.parent().expect("cache subdirectory"))?;
        let mut data = Vec::new();
        ciborium::ser::into_writer(&entry, &mut data)?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, data)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

fn map_bytes(entry: &mut Entry, f: impl Fn(&mut Vec<u8>)) {
    f(&mut entry.run.stdout);
    f(&mut entry.run.stderr);
    if let Some(o) = &mut entry.run.outfile {
        f(o);
    }
    for r in &mut entry.requests {
        let mut target = r.target.clone().into_bytes();
        f(&mut target);
        r.target = String::from_utf8_lossy(&target).into_owned();
        for (_, v) in &mut r.headers {
            f(v);
        }
        f(&mut r.body);
    }
}

fn replace(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

fn which(bin: &Path) -> Option<PathBuf> {
    if bin.components().count() > 1 {
        return Some(bin.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}
