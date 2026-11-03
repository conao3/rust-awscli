//! A test case and its preparation: the generated input, the command
//! line, and the canned responses the stub server returns.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Result, anyhow, bail};
use awsr::model::{Operation, ServiceModel, Shape};
use serde_json::Value as Json;

use crate::response::{Resp, Ser};
use crate::rng::Rng;
use crate::table::{OpEntry, Table};
use crate::values::{Dir, G, Generator, Proto};
use crate::wire::{self, J, JsonStyle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Everything through `--cli-input-json file://...`.
    Json,
    /// Top-level scalar members as `--member-name value`, the rest
    /// through `--cli-input-json`.
    Args,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorMode {
    None,
    Modeled,
    Unmodeled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Case {
    pub service: String,
    pub operation: String,
    pub seed: u64,
    pub format: String,
    pub input: InputMode,
    pub error: ErrorMode,
    /// Run with stdout on a pseudo terminal of this many columns.
    pub pty: Option<u16>,
    pub extra: Vec<String>,
}

impl Case {
    pub fn new(service: &str, operation: &str, seed: u64, format: &str) -> Case {
        Case {
            service: service.to_string(),
            operation: operation.to_string(),
            seed,
            format: format.to_string(),
            input: InputMode::Json,
            error: ErrorMode::None,
            pty: None,
            extra: vec![],
        }
    }

    /// Parse `service Operation seed format [input=args] [error=modeled|unmodeled]
    /// [pty=COLS] [-- extra args...]`.
    pub fn parse(line: &str) -> Result<Case> {
        let (head, extra) = match line.split_once(" -- ") {
            Some((h, e)) => (h, e.split_whitespace().map(str::to_string).collect()),
            None => (line, vec![]),
        };
        let words: Vec<&str> = head.split_whitespace().collect();
        if words.len() < 4 {
            bail!("expected `service Operation seed format [options]`: {line}");
        }
        let mut case = Case::new(words[0], words[1], words[2].parse()?, words[3]);
        case.extra = extra;
        for opt in &words[4..] {
            match opt.split_once('=') {
                Some(("input", "args")) => case.input = InputMode::Args,
                Some(("input", "json")) => case.input = InputMode::Json,
                Some(("error", "modeled")) => case.error = ErrorMode::Modeled,
                Some(("error", "unmodeled")) => case.error = ErrorMode::Unmodeled,
                Some(("error", "none")) => case.error = ErrorMode::None,
                Some(("pty", cols)) => case.pty = Some(cols.parse()?),
                _ => bail!("unknown case option {opt:?} in: {line}"),
            }
        }
        Ok(case)
    }

    pub fn file_stem(&self) -> String {
        self.to_string()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }
}

impl fmt::Display for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {}",
            self.service, self.operation, self.seed, self.format
        )?;
        if self.input == InputMode::Args {
            write!(f, " input=args")?;
        }
        match self.error {
            ErrorMode::None => {}
            ErrorMode::Modeled => write!(f, " error=modeled")?,
            ErrorMode::Unmodeled => write!(f, " error=unmodeled")?,
        }
        if let Some(c) = self.pty {
            write!(f, " pty={c}")?;
        }
        if !self.extra.is_empty() {
            write!(f, " -- {}", self.extra.join(" "))?;
        }
        Ok(())
    }
}

/// A file the case writes into its directory before running.
#[derive(Debug, Clone)]
pub struct CaseFile {
    pub name: String,
    pub content: Vec<u8>,
}

/// Placeholders in [`Prepared::argv`] the runner substitutes.
pub const DIR_MARK: &str = "{casedir}";
pub const ENDPOINT_MARK: &str = "{endpoint}";

#[derive(Debug, Clone)]
pub struct Prepared {
    pub argv: Vec<String>,
    pub files: Vec<CaseFile>,
    pub responses: Vec<Resp>,
    pub expect_exit: i32,
    /// A file the CLI writes (streaming output) that is compared too.
    pub outfile: Option<String>,
    /// Host to put in `--endpoint-url` (`localhost` when the operation
    /// has a host prefix, so `prefix.localhost` still resolves).
    pub needs_wildcard_host: bool,
    /// The input value, for reports.
    pub input_json: String,
}

/// Why a case cannot be prepared.
#[derive(Debug)]
pub struct Skip(pub String);

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Skip {}

fn skip(reason: &str) -> anyhow::Error {
    anyhow!(Skip(reason.to_string()))
}

static MODELS: Mutex<Option<HashMap<String, Weak<ServiceModel>>>> = Mutex::new(None);

/// Load a service model, sharing it with other in-flight cases.
pub fn model(service: &str) -> Result<Arc<ServiceModel>> {
    let mut guard = MODELS.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(m) = map.get(service).and_then(Weak::upgrade) {
        return Ok(m);
    }
    map.retain(|_, w| w.strong_count() > 0);
    let m = Arc::new(ServiceModel::load(service)?);
    map.insert(service.to_string(), Arc::downgrade(&m));
    Ok(m)
}

fn has_event_stream(shape: Option<Shape<'_>>) -> bool {
    shape.is_some_and(|s| s.members().iter().any(|(_, m)| m.is_event_stream()))
}

fn token_list(v: Option<&Json>) -> Vec<String> {
    match v {
        Some(Json::String(s)) => vec![s.clone()],
        Some(Json::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => vec![],
    }
}

fn simple_path(p: &str) -> Option<Vec<&str>> {
    let p = p.trim();
    if p.is_empty()
        || !p
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
    {
        return None;
    }
    Some(p.split('.').collect())
}

fn shape_at<'a>(root: &Shape<'a>, path: &[&str]) -> Option<Shape<'a>> {
    let mut s = *root;
    for p in path {
        s = s.member_shape(p)?;
    }
    Some(s)
}

fn remove_path(g: &mut G, path: &[&str]) {
    match path {
        [] => {}
        [last] => {
            g.remove(last);
        }
        [first, rest @ ..] => {
            if let Some(child) = g.get_mut(first) {
                remove_path(child, rest);
            }
        }
    }
}

fn set_path(g: &mut G, path: &[&str], v: G) {
    match path {
        [] => {}
        [last] => g.set(last, v),
        [first, rest @ ..] => {
            if g.get(first).is_none() {
                g.set(first, G::Struct(vec![]));
            }
            if let Some(child) = g.get_mut(first) {
                set_path(child, rest, v);
            }
        }
    }
}

pub fn prepare(case: &Case, table: &Table) -> Result<Prepared> {
    let model = model(&case.service)?;
    let op = model
        .operation(&case.operation)
        .ok_or_else(|| anyhow!("unknown operation {} {}", case.service, case.operation))?;
    let proto = Proto::parse(model.protocol())
        .ok_or_else(|| anyhow!("unknown protocol {}", model.protocol()))?;
    let svc = table
        .get(&case.service)
        .ok_or_else(|| skip("service is not a CLI command"))?;
    let entry = svc
        .operations
        .get(&case.operation)
        .ok_or_else(|| skip("operation is not a CLI command"))?;
    if has_event_stream(op.input()) || has_event_stream(op.output()) {
        return Err(skip("event stream"));
    }
    for a in &entry.args {
        if a.required()
            && a.member().is_none()
            && !matches!(
                a.class(),
                "StreamingOutputArgument" | "QueryOutFileArgument"
            )
        {
            return Err(skip("custom required argument"));
        }
    }
    let root = Rng::new(case.seed).fork(&format!("{}.{}", case.service, case.operation));

    let (input, input_files) = gen_input(&op, entry, proto, &root)?;
    let mut argv = vec![svc.cli.clone(), entry.cli.clone()];
    let mut files = input_files.files;
    argv.extend(input_files.args);
    let input_json = cli_input_text(&op, &input, &root);
    let (json_input, scalar_args) = match (entry.cli_input_json, case.input) {
        (true, InputMode::Json) => (input.clone(), vec![]),
        (true, InputMode::Args) => split_args(&op, entry, &input, &root, false),
        (false, _) => split_args(&op, entry, &input, &root, true),
    };
    argv.extend(scalar_args);
    let has_json = !matches!(&json_input, G::Struct(m) if m.is_empty());
    if entry.cli_input_json && (case.input == InputMode::Json || has_json) {
        files.push(CaseFile {
            name: "input.json".into(),
            content: cli_input_text(&op, &json_input, &root).into_bytes(),
        });
        argv.push("--cli-input-json".into());
        argv.push(format!("file://{DIR_MARK}/input.json"));
    }
    let mut outfile = None;
    for a in &entry.args {
        match a.class() {
            "StreamingOutputArgument" => {
                argv.push(format!("{DIR_MARK}/outfile.bin"));
                outfile = Some("outfile.bin".to_string());
            }
            "QueryOutFileArgument" if a.required() => {
                argv.push(format!("--{}", a.name()));
                argv.push(format!("{DIR_MARK}/outfile.bin"));
                outfile = Some("outfile.bin".to_string());
            }
            _ => {}
        }
    }
    argv.push("--endpoint-url".into());
    argv.push(ENDPOINT_MARK.into());
    argv.push("--output".into());
    argv.push(case.format.clone());
    argv.extend(case.extra.iter().cloned());

    let responses = if case.error == ErrorMode::None {
        gen_pages(&model, &op, proto, &root)
    } else {
        vec![gen_error(&model, &op, proto, &root, case.error)]
    };
    let host_prefix = op
        .get("endpoint")
        .and_then(|e| e.get("hostPrefix"))
        .is_some();
    Ok(Prepared {
        argv,
        files,
        responses,
        expect_exit: if case.error == ErrorMode::None {
            0
        } else {
            254
        },
        outfile,
        needs_wildcard_host: host_prefix,
        input_json,
    })
}

struct InputFiles {
    args: Vec<String>,
    files: Vec<CaseFile>,
}

fn paginator_input_skips(op: &Operation<'_>) -> Vec<String> {
    let Some(p) = op.model.paginator(op.name) else {
        return vec![];
    };
    let required = op.input().map(|s| s.required()).unwrap_or_default();
    let mut out = token_list(p.get("input_token"));
    out.extend(token_list(p.get("limit_key")));
    out.retain(|m| !required.contains(&m.as_str()));
    out
}

fn gen_input(
    op: &Operation<'_>,
    entry: &OpEntry,
    proto: Proto,
    root: &Rng,
) -> Result<(G, InputFiles)> {
    let mut rng = root.fork("input");
    let mut files = InputFiles {
        args: vec![],
        files: vec![],
    };
    let Some(shape) = op.input() else {
        return Ok((G::Struct(vec![]), files));
    };
    let skips = paginator_input_skips(op);
    let skips: Vec<&str> = skips.iter().map(String::as_str).collect();
    let mut generator = Generator {
        rng: &mut rng,
        dir: Dir::Input,
        proto,
        max_depth: 4,
    };
    let g = generator.structure(&shape, 0, &skips);
    for (name, m) in shape.members() {
        if !m.is_streaming() || m.is_event_stream() {
            continue;
        }
        let Some(arg) = entry.args.iter().find(|a| a.member() == Some(name)) else {
            continue;
        };
        let content = format!("streaming body for {} {name}\n日本語\n", op.name).into_bytes();
        files.files.push(CaseFile {
            name: "body.bin".into(),
            content,
        });
        files.args.push(format!("--{}", arg.name()));
        files.args.push(format!("{DIR_MARK}/body.bin"));
    }
    Ok((g, files))
}

/// The `--cli-input-json` document for `g`.
fn cli_input_text(op: &Operation<'_>, g: &G, root: &Rng) -> String {
    let mut rng = root.fork("cli-input-style");
    let j = match op.input() {
        Some(s) => cli_json(&s, g),
        None => J::Obj(vec![]),
    };
    let style = JsonStyle {
        ascii: rng.chance(1, 4),
        spaced: true,
    };
    wire::json_text(&j, style)
}

/// A value as the CLI reads it from `--cli-input-json`: member names as
/// keys, blobs base64-encoded (`cli_binary_format = base64`), timestamps
/// as ISO 8601 strings or epoch numbers.
fn cli_json(s: &Shape<'_>, g: &G) -> J {
    match g {
        G::Null => J::Null,
        G::Bool(b) => J::Bool(*b),
        G::Int(i) => J::Num(i.clone()),
        G::Float(f) => J::Num(f.text.clone()),
        G::Str(v) => J::Str(v.clone()),
        G::Blob(b) => J::Str(wire::b64(b)),
        G::Ts(ts) => match ts.form % 3 {
            0 => J::Num(ts.secs.to_string()),
            _ => J::Str(wire::iso8601(*ts)),
        },
        G::Doc(d) => J::from_serde(d),
        G::List(items) => J::Arr(
            items
                .iter()
                .map(|v| match s.list_member() {
                    Some(m) => cli_json(&m, v),
                    None => J::Null,
                })
                .collect(),
        ),
        G::Map(entries) => J::Obj(
            entries
                .iter()
                .map(|(k, v)| {
                    let jv = match s.map_value() {
                        Some(m) => cli_json(&m, v),
                        None => J::Null,
                    };
                    (k.clone(), jv)
                })
                .collect(),
        ),
        G::Struct(members) => J::Obj(
            members
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), cli_json(&s.member_shape(k)?, v))))
                .collect(),
        ),
    }
}

/// Move top-level members onto the command line: scalars always, other
/// values as JSON text when `all` (operations without `--cli-input-json`).
/// Returns the members left for `--cli-input-json` and the arguments.
fn split_args(
    op: &Operation<'_>,
    entry: &OpEntry,
    g: &G,
    root: &Rng,
    all: bool,
) -> (G, Vec<String>) {
    let mut rng = root.fork("args");
    let (G::Struct(members), Some(shape)) = (g, op.input()) else {
        return (g.clone(), vec![]);
    };
    let mut rest = vec![];
    let mut args = vec![];
    for (name, v) in members {
        let arg = entry
            .args
            .iter()
            .find(|a| a.member() == Some(name.as_str()) && !a.name().starts_with("no-"));
        let Some(m) = shape.member_shape(name) else {
            continue;
        };
        let Some(arg) = arg else {
            rest.push((name.clone(), v.clone()));
            continue;
        };
        let text = match v {
            G::Str(s) if m.type_name() == "string" && !m.get_bool("jsonvalue") => s.clone(),
            G::Int(i) => i.clone(),
            G::Float(f) => f.text.clone(),
            G::Ts(ts) => wire::iso8601(*ts),
            G::Blob(b) if all => wire::b64(b),
            G::Bool(b) => {
                let negative = format!("no-{}", arg.name());
                if arg.class() == "BooleanArgument" {
                    if *b {
                        args.push(format!("--{}", arg.name()));
                        continue;
                    }
                    if entry.args.iter().any(|a| a.name() == negative) {
                        args.push(format!("--{negative}"));
                        continue;
                    }
                    rest.push((name.clone(), v.clone()));
                    continue;
                }
                b.to_string()
            }
            other if all => wire::json_text(&cli_json(&m, other), JsonStyle::default()),
            _ => {
                rest.push((name.clone(), v.clone()));
                continue;
            }
        };
        if text.starts_with('-') || rng.chance(1, 5) {
            args.push(format!("--{}={text}", arg.name()));
        } else {
            args.push(format!("--{}", arg.name()));
            args.push(text);
        }
    }
    (G::Struct(rest), args)
}

fn gen_output(op: &Operation<'_>, proto: Proto, rng: &mut Rng) -> Option<G> {
    let shape = op.output()?;
    let mut generator = Generator {
        rng,
        dir: Dir::Output,
        proto,
        max_depth: 5,
    };
    Some(generator.structure(&shape, 0, &[]))
}

fn gen_pages(model: &ServiceModel, op: &Operation<'_>, proto: Proto, root: &Rng) -> Vec<Resp> {
    let mut rng = root.fork("pages");
    let paginator = model.paginator(op.name);
    let pages = if paginator.is_some() {
        1 + rng.below(3) as usize
    } else {
        1
    };
    let mut out = vec![];
    for i in 0..pages {
        let mut page_rng = root.fork(&format!("page{i}"));
        let mut g = gen_output(op, proto, &mut page_rng);
        if let (Some(p), Some(g), Some(shape)) = (paginator, g.as_mut(), op.output()) {
            paginate(p, g, &shape, i, pages, proto, &mut page_rng);
        }
        let mut ser = Ser {
            model,
            proto,
            rng: root.fork(&format!("ser{i}")),
        };
        out.push(ser.success(op, g.as_ref()));
    }
    out
}

fn paginate(
    p: &serde_json::Map<String, Json>,
    g: &mut G,
    shape: &Shape<'_>,
    page: usize,
    pages: usize,
    proto: Proto,
    rng: &mut Rng,
) {
    let last = page + 1 == pages;
    let tokens = token_list(p.get("output_token"));
    for (ti, expr) in tokens.iter().enumerate() {
        for (ai, alt) in expr.split("||").enumerate() {
            let Some(path) = simple_path(alt) else {
                continue;
            };
            if last {
                remove_path(g, &path);
            } else if ti == 0 && ai == 0 {
                let Some(ts) = shape_at(shape, &path) else {
                    continue;
                };
                let value = if ts.type_name() == "string" {
                    let min = ts.get("min").and_then(Json::as_u64).unwrap_or(0) as usize;
                    let max = ts.get("max").and_then(Json::as_u64).unwrap_or(1024) as usize;
                    let mut t = format!("token-{page}-{}", rng.below(1_000_000));
                    while t.len() < min {
                        t.push('x');
                    }
                    t.truncate(max.max(1));
                    G::Str(t)
                } else {
                    let mut generator = Generator {
                        rng,
                        dir: Dir::Output,
                        proto,
                        max_depth: 2,
                    };
                    let v = generator.member(path.last().unwrap(), &ts, 3);
                    match (v, ts.map_value()) {
                        // A distinct, non-empty token on every page.
                        (G::Map(mut m), Some(vs)) => {
                            let entry = generator.any(&vs, 3);
                            m.push((format!("page{page}"), entry));
                            G::Map(m)
                        }
                        (other, _) => other,
                    }
                };
                set_path(g, &path, value);
            }
        }
    }
    for path in token_list(p.get("more_results")) {
        if let Some(path) = simple_path(&path) {
            set_path(g, &path, G::Bool(!last));
        }
    }
}

const UNMODELED_CODES: &[(&str, u16)] = &[
    ("UnmodeledServiceError", 400),
    ("AccessDenied", 403),
    ("InvalidParameterValue", 400),
    ("NotFound", 404),
    ("Conflict", 409),
];

const MESSAGES: &[&str] = &[
    "Something went wrong",
    "1 validation error detected: Value at 'name' failed to satisfy constraint",
    "パラメータが不正です",
    "Ａｃｃｅｓｓ ｄｅｎｉｅｄ ±",
    "",
    "line one\nline two",
    "Message with <xml> & \"quotes\"",
];

fn gen_error(
    model: &ServiceModel,
    op: &Operation<'_>,
    proto: Proto,
    root: &Rng,
    mode: ErrorMode,
) -> Resp {
    let mut rng = root.fork("error");
    let mut message = rng.pick(MESSAGES).to_string();
    if message.is_empty() && matches!(proto, Proto::Query | Proto::Ec2 | Proto::RestXml) {
        // `<Message></Message>` parses to None, which the CLI cannot print.
        message = MESSAGES[0].to_string();
    }
    let mut ser = Ser {
        model,
        proto,
        rng: root.fork("error-ser"),
    };
    let errors = op.errors();
    if mode == ErrorMode::Modeled && !errors.is_empty() {
        let shape = errors[rng.below(errors.len() as u64) as usize];
        let info = shape.shape_get("error");
        let code = info
            .and_then(|e| e.get("code"))
            .and_then(Json::as_str)
            .filter(|_| matches!(proto, Proto::Query | Proto::Ec2 | Proto::RestXml))
            .unwrap_or(shape.name)
            .to_string();
        let status = info
            .and_then(|e| e.get("httpStatusCode"))
            .and_then(Json::as_u64)
            .unwrap_or(400) as u16;
        let mut generator = Generator {
            rng: &mut rng,
            dir: Dir::Output,
            proto,
            max_depth: 2,
        };
        let members = generator.structure(&shape, 0, &[]);
        return ser.error(Some(&shape), &members, &code, status, &message);
    }
    let (code, status) = *rng.pick(UNMODELED_CODES);
    ser.error(None, &G::Struct(vec![]), code, status, &message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_lines_round_trip() {
        let line = "s3 ListObjectsV2 3 table input=args error=modeled pty=120 -- --no-paginate";
        assert_eq!(Case::parse(line).unwrap().to_string(), line);
    }
}
