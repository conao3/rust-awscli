//! Running `aws s3` scenarios: each scenario sets up local files, S3
//! state, and configuration, runs the reference and the candidate CLI
//! against the stateful S3 stub, and compares stdout, stderr, the exit
//! code, the requests (as a multiset: transfers run concurrently), and the
//! final local and S3 state.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use regex::Regex;
use similar::TextDiff;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::compare::visible;
use crate::s3scenarios::Scenario;
use crate::s3stub::{Bucket, Object, S3Stub, md5_hex};
use crate::server::Recorded;

pub struct Side {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
    pub requests: Vec<String>,
    pub s3: String,
    pub local: String,
}

const MASKED: &[&str] = &[
    "authorization",
    "x-amz-date",
    "x-amz-content-sha256",
    "x-amz-security-token",
    "user-agent",
    "x-amz-user-agent",
    "amz-sdk-invocation-id",
];

fn render(r: &Recorded) -> String {
    let mut out = format!("{} {}\n", r.method, r.target);
    let mut headers: Vec<(String, String)> = r
        .headers
        .iter()
        .map(|(k, v)| {
            let k = k.to_ascii_lowercase();
            let v = if MASKED.contains(&k.as_str()) {
                "<masked>".to_string()
            } else {
                String::from_utf8_lossy(v).into_owned()
            };
            (k, v)
        })
        .collect();
    headers.sort();
    for (k, v) in headers {
        let _ = writeln!(out, "{k}: {v}");
    }
    match std::str::from_utf8(&r.body) {
        Ok(s) if r.body.len() <= 4096 => out.push_str(&visible(s.as_bytes())),
        _ => {
            let _ = writeln!(out, "<{} bytes md5={}>", r.body.len(), md5_hex(&r.body));
        }
    }
    out
}

fn progress_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("Completed [^\r\n]*[\r\n]").unwrap())
}

fn trailing_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(" +([\r\n])").unwrap())
}

fn presign_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("(X-Amz-Date|X-Amz-Signature)=[0-9A-Za-z]+").unwrap())
}

/// Mask the per-run parts of the output: the case directory, the progress
/// lines (they carry the transfer speed) with the padding they leave, and
/// the time-dependent parts of presigned URLs.
fn normalize(bytes: &[u8], dir: &Path) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let text = text.replace(&dir.display().to_string(), "<DIR>");
    let text = progress_re().replace_all(&text, "");
    let text = trailing_re().replace_all(&text, "$1");
    let text = text.replace("\r\n", "\n").replace('\r', "");
    let text = presign_re().replace_all(&text, "$1=<masked>");
    visible(text.as_bytes())
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let rel = p.strip_prefix(root).unwrap_or(&p).display().to_string();
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&p).unwrap_or_default();
            out.insert(rel, format!("-> {}", target.display()));
        } else if meta.is_dir() {
            out.insert(format!("{rel}/"), String::new());
            walk(root, &p, out);
        } else {
            use std::os::unix::fs::MetadataExt;
            let data = std::fs::read(&p).unwrap_or_default();
            out.insert(
                rel,
                format!(
                    "size={} md5={} mtime={}",
                    data.len(),
                    md5_hex(&data),
                    meta.mtime()
                ),
            );
        }
    }
}

fn local_dump(work: &Path) -> String {
    let mut m = BTreeMap::new();
    walk(work, work, &mut m);
    let mut out = String::new();
    for (k, v) in m {
        let _ = writeln!(out, "{k} {v}");
    }
    out
}

fn set_mtime(path: &Path, secs: i64) -> Result<()> {
    use nix::sys::stat::utimes;
    use nix::sys::time::TimeVal;
    let tv = TimeVal::new(secs, 0);
    utimes(path, &tv, &tv).with_context(|| format!("utimes {}", path.display()))?;
    Ok(())
}

fn setup(sc: &Scenario, dir: &Path) -> Result<PathBuf> {
    let home = dir.join("home");
    let aws = home.join(".aws");
    std::fs::create_dir_all(&aws)?;
    std::fs::write(
        aws.join("config"),
        format!("[default]\nregion = us-east-1\n{}", sc.config),
    )?;
    std::fs::write(
        aws.join("credentials"),
        "[default]\naws_access_key_id = AKIDEXAMPLE\naws_secret_access_key = difftest-fake-secret-key\n",
    )?;
    let work = dir.join("work");
    std::fs::create_dir_all(&work)?;
    for f in &sc.local {
        let p = work.join(&f.path);
        if f.path.ends_with('/') {
            std::fs::create_dir_all(&p)?;
            continue;
        }
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(target) = &f.symlink {
            std::os::unix::fs::symlink(target, &p)?;
            continue;
        }
        std::fs::write(&p, &f.content)?;
        set_mtime(&p, f.mtime)?;
    }
    // Directory mtimes do not take part in the comparison; files do.
    Ok(work)
}

fn initial_buckets(sc: &Scenario) -> BTreeMap<String, Bucket> {
    let mut out = BTreeMap::new();
    for b in &sc.buckets {
        let mut bucket = Bucket {
            created: 1_700_000_000,
            ..Default::default()
        };
        for o in &b.objects {
            let mut obj = Object::new(o.content.clone(), o.mtime);
            for (k, v) in &o.headers {
                obj.headers.insert(k.to_string(), v.to_string());
            }
            for (k, v) in &o.tags {
                obj.tags.push((k.to_string(), v.to_string()));
            }
            bucket.objects.insert(o.key.clone(), obj);
        }
        out.insert(b.name.clone(), bucket);
    }
    out
}

pub async fn run_side(bin: &Path, sc: &Scenario, stub: &S3Stub, base: &Path) -> Result<Side> {
    let tmp = tempfile::Builder::new().prefix("s3-").tempdir_in(base)?;
    let dir = tmp.path().canonicalize()?;
    let work = setup(sc, &dir)?;
    stub.set_state(initial_buckets(sc));
    stub.take_log();
    let home = dir.join("home");
    let endpoint = format!("http://difftest.localhost:{}", stub.port);
    let mut argv: Vec<String> = vec![];
    if !sc.no_endpoint {
        argv.push("--endpoint-url".into());
        argv.push(endpoint);
    }
    argv.extend(sc.args.iter().cloned());
    let mut cmd = Command::new(bin);
    cmd.args(&argv)
        .current_dir(&work)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("AWS_CONFIG_FILE", home.join(".aws/config"))
        .env("AWS_SHARED_CREDENTIALS_FILE", home.join(".aws/credentials"))
        .env("AWS_PAGER", "")
        .env("LC_ALL", "C.UTF-8")
        .env("TZ", sc.tz)
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("AWS_MAX_ATTEMPTS", "1")
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
        .stdin(if sc.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &sc.env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    if let Some(input) = sc.stdin.clone() {
        let mut stdin = child.stdin.take().unwrap();
        tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
        });
    }
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let so = tokio::spawn(async move {
        let mut b = vec![];
        let _ = out.read_to_end(&mut b).await;
        b
    });
    let se = tokio::spawn(async move {
        let mut b = vec![];
        let _ = err.read_to_end(&mut b).await;
        b
    });
    let code = match tokio::time::timeout(Duration::from_secs(120), child.wait()).await {
        Ok(s) => s?.code(),
        Err(_) => {
            let _ = child.kill().await;
            Some(-1)
        }
    };
    let stdout = so.await.unwrap_or_default();
    let stderr = se.await.unwrap_or_default();
    let mut requests: Vec<String> = stub.take_log().iter().map(render).collect();
    requests.sort();
    let side = Side {
        stdout: normalize(&stdout, &dir),
        stderr: normalize(&stderr, &dir),
        code,
        requests,
        s3: stub.dump(),
        local: local_dump(&work),
    };
    Ok(side)
}

fn diff(name: &str, a: &str, b: &str, out: &mut Vec<(String, String)>) {
    if a != b {
        let d = TextDiff::from_lines(a, b)
            .unified_diff()
            .context_radius(3)
            .header("reference", "candidate")
            .to_string();
        out.push((name.to_string(), d));
    }
}

fn sorted_lines(s: &str) -> Vec<&str> {
    let mut v: Vec<&str> = s.split_inclusive('\n').collect();
    v.sort_unstable();
    v
}

pub fn compare(a: &Side, b: &Side, compare_requests: bool) -> Vec<(String, String)> {
    let mut out = vec![];
    diff(
        "exit",
        &format!("{:?}\n", a.code),
        &format!("{:?}\n", b.code),
        &mut out,
    );
    // Concurrent transfers finish in any order: lines may come in any
    // order as long as they are the same lines.
    if sorted_lines(&a.stdout) != sorted_lines(&b.stdout) {
        diff("stdout", &a.stdout, &b.stdout, &mut out);
    }
    if sorted_lines(&a.stderr) != sorted_lines(&b.stderr) {
        diff("stderr", &a.stderr, &b.stderr, &mut out);
    }
    if compare_requests {
        diff(
            "requests",
            &a.requests.join("---\n"),
            &b.requests.join("---\n"),
            &mut out,
        );
    }
    diff("s3 state", &a.s3, &b.s3, &mut out);
    diff("local state", &a.local, &b.local, &mut out);
    out
}

pub fn report(sc: &Scenario, a: &Side, diffs: &[(String, String)]) -> String {
    let mut s = format!("scenario: {}\nargv: aws {}\n", sc.name, sc.args.join(" "));
    let _ = writeln!(s, "== reference exit: {:?}", a.code);
    let _ = writeln!(s, "== reference stdout\n{}", a.stdout);
    let _ = writeln!(s, "== reference stderr\n{}", a.stderr);
    for (n, d) in diffs {
        let _ = writeln!(s, "== {n}\n{d}");
    }
    if diffs.iter().any(|(n, _)| n == "requests") {
        let _ = writeln!(s, "== reference requests\n{}", a.requests.join("---\n"));
    }
    s
}
