//! Executing one case against the reference and the candidate CLI.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use crate::case::{self, Case, DIR_MARK, ENDPOINT_MARK, Prepared, Skip};
use crate::compare::{self, Side};
use crate::run::{self, Env, RunOut};
use crate::server::Stub;
use crate::table::Table;

#[derive(Debug, Clone)]
pub struct Config {
    pub reference: PathBuf,
    pub candidate: PathBuf,
    pub reference_only: bool,
    pub env: Env,
    pub endpoint_host: String,
    pub work_dir: PathBuf,
    pub keep_dirs: bool,
}

#[derive(Debug)]
pub enum Outcome {
    Pass { detail: String },
    Skip(String),
    Fail { cause: String, detail: String },
}

pub async fn exec(case: Case, table: Arc<Table>, cfg: Arc<Config>) -> Outcome {
    let c = case.clone();
    let prepared = tokio::task::spawn_blocking(move || case::prepare(&c, &table)).await;
    let prepared = match prepared {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            return match e.downcast_ref::<Skip>() {
                Some(s) => Outcome::Skip(s.0.clone()),
                None => Outcome::Fail {
                    cause: format!("harness: {e}"),
                    detail: format!("{e:#}"),
                },
            };
        }
        Err(e) => {
            return Outcome::Fail {
                cause: "harness: panic while preparing".into(),
                detail: e.to_string(),
            };
        }
    };
    match run_prepared(&case, &prepared, &cfg).await {
        Ok(o) => o,
        Err(e) => Outcome::Fail {
            cause: format!("harness: {e}"),
            detail: format!("{e:#}"),
        },
    }
}

async fn run_prepared(case: &Case, p: &Prepared, cfg: &Config) -> anyhow::Result<Outcome> {
    std::fs::create_dir_all(&cfg.work_dir)?;
    let tmp = tempfile::Builder::new()
        .prefix("case-")
        .tempdir_in(&cfg.work_dir)?;
    let dir = tmp.path().to_path_buf();
    run::write_home(&dir)?;
    for f in &p.files {
        std::fs::write(dir.join(&f.name), &f.content)?;
    }
    let stub = Stub::start(p.responses.clone()).await?;
    let host = if p.needs_wildcard_host {
        "localhost"
    } else {
        cfg.endpoint_host.as_str()
    };
    let endpoint = format!("http://{host}:{}", stub.port);
    let dir_text = dir.display().to_string();
    let argv: Vec<String> = p
        .argv
        .iter()
        .map(|a| {
            a.replace(ENDPOINT_MARK, &endpoint)
                .replace(DIR_MARK, &dir_text)
        })
        .collect();

    let reference = run::run(
        &cfg.reference,
        &argv,
        &dir,
        &cfg.env,
        case.pty,
        p.outfile.as_deref(),
    )
    .await?;
    let ref_requests = stub.take_log();

    let mut detail = header(case, p, &argv);
    let _ = writeln!(detail, "== reference exit: {:?}", reference.code);
    let _ = writeln!(
        detail,
        "== reference stdout\n{}",
        compare::visible(&reference.stdout)
    );
    let _ = writeln!(
        detail,
        "== reference stderr\n{}",
        compare::visible(&reference.stderr)
    );
    let outcome = if cfg.reference_only {
        let _ = writeln!(
            detail,
            "== requests\n{}",
            compare::render_requests(&ref_requests)
        );
        detail.push_str(&responses_text(p));
        judge_reference(&reference, p, ref_requests.is_empty(), detail)
    } else {
        let candidate = run::run(
            &cfg.candidate,
            &argv,
            &dir,
            &cfg.env,
            case.pty,
            p.outfile.as_deref(),
        )
        .await?;
        let cand_requests = stub.take_log();
        let diffs = compare::compare(
            &Side {
                run: &reference,
                requests: &ref_requests,
            },
            &Side {
                run: &candidate,
                requests: &cand_requests,
            },
        );
        if diffs.is_empty() {
            Outcome::Pass { detail }
        } else {
            let parts: Vec<&str> = diffs.iter().map(|(n, _)| n.as_str()).collect();
            let cause = format!("differs: {}", parts.join(", "));
            for (name, d) in &diffs {
                let _ = writeln!(detail, "== {name}\n{d}");
            }
            detail.push_str(&responses_text(p));
            Outcome::Fail { cause, detail }
        }
    };
    if cfg.keep_dirs {
        let _ = tmp.keep();
    }
    Ok(outcome)
}

fn header(case: &Case, p: &Prepared, argv: &[String]) -> String {
    let mut out = format!("case: {case}\n");
    let _ = writeln!(out, "argv: aws {}", shell_words(argv));
    let _ = writeln!(out, "== input\n{}", p.input_json);
    out
}

fn shell_words(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_=/:.,{}@%+".contains(c))
                && !a.is_empty()
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn responses_text(p: &Prepared) -> String {
    let mut out = String::new();
    for (i, r) in p.responses.iter().enumerate() {
        let _ = writeln!(out, "== response {} (status {})", i + 1, r.status);
        for (k, v) in &r.headers {
            let _ = writeln!(out, "{k}: {v}");
        }
        out.push('\n');
        if r.headers.iter().any(|(_, v)| v.contains("cbor")) {
            out.push_str(&compare::render_request(&crate::server::Recorded {
                method: String::new(),
                target: String::new(),
                headers: vec![("content-type".into(), b"application/cbor".to_vec())],
                body: r.body.clone(),
            }));
        } else {
            out.push_str(&compare::visible(&r.body));
        }
    }
    out
}

fn judge_reference(r: &RunOut, p: &Prepared, no_requests: bool, detail: String) -> Outcome {
    if r.timed_out {
        return Outcome::Fail {
            cause: "reference timed out".into(),
            detail,
        };
    }
    if r.code == Some(p.expect_exit) && (p.expect_exit != 0 || r.stderr.is_empty()) {
        return Outcome::Pass { detail };
    }
    let mut cause = classify(&r.stderr);
    if r.code == Some(0) && p.expect_exit != 0 {
        cause = "reference succeeded on an error response".into();
    }
    if no_requests {
        cause = format!("no request sent: {cause}");
    }
    Outcome::Fail {
        cause: format!("exit {:?}: {cause}", r.code),
        detail,
    }
}

/// A short, value-independent summary of an error message, used to group
/// failures.
pub fn classify(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out = String::new();
    let mut quote = None;
    for c in line.chars() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                out.push('…');
                out.push(c);
            }
            Some(_) => {}
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                out.push(c);
            }
            None if c.is_ascii_digit() => {
                if !out.ends_with('N') {
                    out.push('N');
                }
            }
            None => out.push(c),
        }
    }
    out.chars().take(160).collect()
}
