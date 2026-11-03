//! Hand-written scenarios for commands the models do not describe (the
//! commands and argument customizations of `awscli/customizations/`):
//! a command line, the files of the working directory, and the canned
//! responses of the stub, run with both CLIs and compared like a case
//! (stdout, stderr, exit code, the recorded requests, and output files).
//!
//! A scenario file is a JSON array of objects:
//!
//! ```json
//! {"name": "ecr get-login-password",
//!  "argv": ["ecr", "get-login-password", "--endpoint-url", "{endpoint}"],
//!  "files": {"key.pem": "text", "a.zip": "base64:UEsDBA=="},
//!  "responses": [{"status": 200, "headers": {"Content-Type": "application/x-amz-json-1.1"},
//!                 "body": "{}"}],
//!  "outputs": ["cert.pem"]}
//! ```
//!
//! `{endpoint}` and `{casedir}` in `argv` and file contents are replaced
//! with the stub URL and the scenario's working directory. A body that
//! starts with `base64:` is decoded.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine as _;
use serde::Deserialize;

use crate::case::{DIR_MARK, ENDPOINT_MARK};
use crate::compare::{self, Side};
use crate::response::Resp;
use crate::run::{self, Env, RunOut};
use crate::server::Stub;

#[derive(Debug, Clone, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub responses: Vec<RespDef>,
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Regular expressions for volatile values (random request tokens,
    /// times) replaced with `<masked>` in the outputs and the requests.
    #[serde(default)]
    pub masks: Vec<String>,
    /// Regular expressions whose matches in request targets and bodies
    /// are masked (values that depend on the time of the run).
    #[serde(default)]
    pub mask: Vec<String>,
    /// Request headers left out of the comparison (lower case), for
    /// known gaps outside the command under test.
    #[serde(default)]
    pub ignore_headers: Vec<String>,
}

fn apply_masks(masks: &[regex::Regex], out: &mut RunOut, reqs: &mut [crate::server::Recorded]) {
    let mask = |b: &[u8]| -> Vec<u8> {
        let mut text = String::from_utf8_lossy(b).into_owned();
        for re in masks {
            text = re.replace_all(&text, "<masked>").into_owned();
        }
        text.into_bytes()
    };
    if masks.is_empty() {
        return;
    }
    out.stdout = mask(&out.stdout);
    out.stderr = mask(&out.stderr);
    for r in reqs {
        if std::str::from_utf8(&r.body).is_ok() {
            r.body = mask(&r.body);
            let len = r.body.len().to_string();
            for (k, v) in &mut r.headers {
                if k.eq_ignore_ascii_case("content-length") {
                    *v = len.clone().into_bytes();
                }
            }
        }
        r.target = String::from_utf8_lossy(&mask(r.target.as_bytes())).into_owned();
        for (k, v) in &mut r.headers {
            if !k.eq_ignore_ascii_case("content-length") {
                *v = mask(v);
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RespDef {
    #[serde(default = "ok")]
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: String,
}

fn ok() -> u16 {
    200
}

fn bytes_of(text: &str) -> Result<Vec<u8>> {
    match text.strip_prefix("base64:") {
        Some(b) => Ok(base64::engine::general_purpose::STANDARD.decode(b.trim())?),
        None => Ok(text.as_bytes().to_vec()),
    }
}

pub fn load(path: &Path) -> Result<Vec<Scenario>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn fixture_mtime() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)
}

pub struct Outcome {
    pub pass: bool,
    pub detail: String,
}

type SideOut = (
    RunOut,
    Vec<crate::server::Recorded>,
    Vec<Option<Vec<u8>>>,
    Vec<String>,
);

async fn run_side(
    bin: &Path,
    s: &Scenario,
    stub: &Stub,
    work: &Path,
    env: &Env,
) -> Result<SideOut> {
    let tmp = tempfile::Builder::new()
        .prefix("scenario-")
        .tempdir_in(work)?;
    let dir = tmp.path().to_path_buf();
    run::write_home(&dir)?;
    let endpoint = format!("http://difftest.localhost:{}", stub.port);
    let dir_text = dir.display().to_string();
    let subst = |a: &str| {
        a.replace(ENDPOINT_MARK, &endpoint)
            .replace(DIR_MARK, &dir_text)
    };
    for (name, content) in &s.files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = match content.strip_prefix("base64:") {
            Some(_) => bytes_of(content)?,
            None => subst(content).into_bytes(),
        };
        std::fs::write(&path, data)?;
        std::fs::File::options()
            .write(true)
            .open(&path)?
            .set_modified(fixture_mtime())?;
    }
    let argv: Vec<String> = s.argv.iter().map(|a| subst(a)).collect();
    let mut out = run::run(bin, &argv, &dir, env, None, None).await?;
    let requests = stub.take_log();
    let outputs = s
        .outputs
        .iter()
        .map(|f| std::fs::read(dir.join(f)).ok())
        .collect();
    // Paths of the working directory differ between the two runs.
    out.stdout = String::from_utf8_lossy(&out.stdout)
        .replace(&dir_text, DIR_MARK)
        .replace(&endpoint, ENDPOINT_MARK)
        .into_bytes();
    out.stderr = String::from_utf8_lossy(&out.stderr)
        .replace(&dir_text, DIR_MARK)
        .replace(&endpoint, ENDPOINT_MARK)
        .into_bytes();
    Ok((out, requests, outputs, argv))
}

pub async fn exec(
    s: &Scenario,
    reference: &Path,
    candidate: &Path,
    work: &Path,
    env: &Env,
) -> Result<Outcome> {
    std::fs::create_dir_all(work)?;
    let responses: Vec<Resp> = if s.responses.is_empty() {
        vec![Resp {
            status: 200,
            headers: vec![],
            body: vec![],
        }]
    } else {
        s.responses
            .iter()
            .map(|r| {
                Ok(Resp {
                    status: r.status,
                    headers: r.headers.clone().into_iter().collect(),
                    body: bytes_of(&r.body)?,
                })
            })
            .collect::<Result<_>>()?
    };
    let stub = Stub::start(responses).await?;
    let (mut ra, mut qa, oa, argv) = run_side(reference, s, &stub, work, env).await?;
    let (mut rb, mut qb, ob, _) = run_side(candidate, s, &stub, work, env).await?;
    let masks = s
        .masks
        .iter()
        .map(|m| regex::Regex::new(m))
        .collect::<Result<Vec<_>, _>>()?;
    apply_masks(&masks, &mut ra, &mut qa);
    apply_masks(&masks, &mut rb, &mut qb);
    for r in qa.iter_mut().chain(qb.iter_mut()) {
        r.headers
            .retain(|(k, _)| !s.ignore_headers.contains(&k.to_ascii_lowercase()));
    }
    for pattern in &s.mask {
        let re = regex::bytes::Regex::new(pattern)?;
        for r in qa.iter_mut().chain(qb.iter_mut()) {
            r.body = re.replace_all(&r.body, &b"<masked>"[..]).into_owned();
            r.target =
                String::from_utf8_lossy(&re.replace_all(r.target.as_bytes(), &b"<masked>"[..]))
                    .into_owned();
        }
    }
    let mut detail = format!("scenario: {}\nargv: aws {}\n", s.name, argv.join(" "));
    let _ = writeln!(detail, "== reference exit: {:?}", ra.code);
    let _ = writeln!(
        detail,
        "== reference stdout\n{}",
        compare::visible(&ra.stdout)
    );
    let _ = writeln!(
        detail,
        "== reference stderr\n{}",
        compare::visible(&ra.stderr)
    );
    let mut diffs = compare::compare(
        &Side {
            run: &ra,
            requests: &qa,
        },
        &Side {
            run: &rb,
            requests: &qb,
        },
    );
    for (i, name) in s.outputs.iter().enumerate() {
        if oa[i] != ob[i] {
            let a = oa[i]
                .as_deref()
                .map(compare::visible)
                .unwrap_or("<missing>\n".into());
            let b = ob[i]
                .as_deref()
                .map(compare::visible)
                .unwrap_or("<missing>\n".into());
            diffs.push((
                format!("output {name}"),
                format!("reference:\n{a}candidate:\n{b}"),
            ));
        }
    }
    let _ = writeln!(
        detail,
        "== reference requests\n{}",
        compare::render_requests(&qa)
    );
    for (name, d) in &diffs {
        let _ = writeln!(detail, "== {name}\n{d}");
    }
    Ok(Outcome {
        pass: diffs.is_empty(),
        detail,
    })
}
