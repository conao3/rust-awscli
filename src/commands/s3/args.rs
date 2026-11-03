//! The argument tables of the `aws s3` subcommands
//! (`awscli/customizations/s3/subcommands.py`, the sync strategies'
//! `ARGUMENT`s) and `BasicCommand.__call__`'s parsing and unpacking
//! (`awscli/customizations/commands.py`).

use indexmap::IndexMap;

use crate::cli::Fail;
use crate::cli::argparse::{Action, Kind, Nargs, NsVal, Parsed, Parser};
use crate::cli::argprocess::{self, ArgContext};
use crate::cli::render::argparse_error;
use crate::model::ServiceModel;
use crate::value::{Map, Value};

/// The `argument_model` of a `CustomArgument`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Model {
    /// `nargs` was given: no model.
    None,
    String,
    Integer,
    Boolean,
    Blob,
    /// `METADATA`'s `{'type': 'map', 'key': string, 'value': string}`.
    StringMap,
}

#[derive(Debug, Clone)]
pub(crate) struct ArgDef {
    pub name: &'static str,
    pub action: Action,
    pub model: Model,
    pub positional: bool,
}

impl ArgDef {
    pub(crate) fn cli_name(&self) -> String {
        if self.positional {
            self.name.to_string()
        } else {
            format!("--{}", self.name)
        }
    }
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn opt(name: &'static str) -> ArgDef {
    ArgDef {
        name,
        action: Action::option(&format!("--{name}")),
        model: Model::String,
        positional: false,
    }
}

fn flag(name: &'static str) -> ArgDef {
    ArgDef {
        name,
        action: Action::flag(&format!("--{name}"), Kind::StoreTrue),
        model: Model::Boolean,
        positional: false,
    }
}

fn with_dest(mut a: ArgDef, dest: &str) -> ArgDef {
    a.action.dest = dest.to_string();
    a
}

fn choices(mut a: ArgDef, c: &[&str]) -> ArgDef {
    a.action.choices = Some(strs(c));
    a
}

/// `nargs='?'` with a `const`.
fn optional_const(mut a: ArgDef, c: &str) -> ArgDef {
    a.action.nargs = Nargs::Optional;
    a.action.const_ = Some(c.to_string());
    a.model = Model::None;
    a
}

fn integer(mut a: ArgDef) -> ArgDef {
    a.model = Model::Integer;
    a
}

fn positional(name: &'static str, nargs: Option<Nargs>) -> ArgDef {
    let action = Action::positional(name, nargs.unwrap_or(Nargs::One));
    ArgDef {
        name,
        action,
        model: if nargs.is_some() {
            Model::None
        } else {
            Model::String
        },
        positional: true,
    }
}

fn recursive() -> ArgDef {
    with_dest(flag("recursive"), "dir_op")
}

fn page_size() -> ArgDef {
    integer(opt("page-size"))
}

fn request_payer() -> ArgDef {
    optional_const(choices(opt("request-payer"), &["requester"]), "requester")
}

fn filter(name: &'static str) -> ArgDef {
    let mut a = opt(name);
    a.action.kind = Kind::AppendFilter;
    a.action.nargs = Nargs::Exact(1);
    a.action.dest = "filters".to_string();
    a.model = Model::None;
    a
}

fn transfer_args() -> Vec<ArgDef> {
    let mut follow = flag("follow-symlinks");
    follow.action.default = NsVal::Bool(true);
    let mut no_follow = with_dest(flag("no-follow-symlinks"), "follow_symlinks");
    no_follow.action.kind = Kind::StoreFalse;
    no_follow.action.default = NsVal::Bool(true);
    let mut no_guess = with_dest(flag("no-guess-mime-type"), "guess_mime_type");
    no_guess.action.kind = Kind::StoreFalse;
    no_guess.action.default = NsVal::Bool(true);
    let mut grants = opt("grants");
    grants.action.nargs = Nargs::OneOrMore;
    grants.model = Model::None;
    let mut sse_c_key = opt("sse-c-key");
    sse_c_key.model = Model::Blob;
    let mut sse_c_copy_source_key = opt("sse-c-copy-source-key");
    sse_c_copy_source_key.model = Model::Blob;
    let mut no_progress = with_dest(flag("no-progress"), "progress");
    no_progress.action.kind = Kind::StoreFalse;
    no_progress.action.default = NsVal::Bool(true);
    let mut progress_frequency = integer(opt("progress-frequency"));
    progress_frequency.action.default = NsVal::Int(0.into());
    vec![
        flag("dryrun"),
        flag("quiet"),
        filter("include"),
        filter("exclude"),
        choices(
            opt("acl"),
            &[
                "private",
                "public-read",
                "public-read-write",
                "authenticated-read",
                "aws-exec-read",
                "bucket-owner-read",
                "bucket-owner-full-control",
                "log-delivery-write",
            ],
        ),
        follow,
        no_follow,
        no_guess,
        optional_const(choices(opt("sse"), &["AES256", "aws:kms"]), "AES256"),
        optional_const(choices(opt("sse-c"), &["AES256"]), "AES256"),
        sse_c_key,
        opt("sse-kms-key-id"),
        optional_const(choices(opt("sse-c-copy-source"), &["AES256"]), "AES256"),
        sse_c_copy_source_key,
        choices(
            opt("storage-class"),
            &[
                "STANDARD",
                "REDUCED_REDUNDANCY",
                "STANDARD_IA",
                "ONEZONE_IA",
                "INTELLIGENT_TIERING",
                "GLACIER",
                "DEEP_ARCHIVE",
                "GLACIER_IR",
            ],
        ),
        grants,
        opt("website-redirect"),
        opt("content-type"),
        opt("cache-control"),
        opt("content-disposition"),
        opt("content-encoding"),
        opt("content-language"),
        opt("expires"),
        opt("source-region"),
        flag("only-show-errors"),
        no_progress,
        with_dest(progress_frequency, "progress_frequency"),
        with_dest(flag("progress-multiline"), "progress_multiline"),
        page_size(),
        flag("ignore-glacier-warnings"),
        flag("force-glacier-transfer"),
        request_payer(),
        choices(opt("checksum-mode"), &["ENABLED"]),
        choices(
            opt("checksum-algorithm"),
            &[
                "CRC64NVME",
                "CRC32",
                "SHA256",
                "SHA1",
                "CRC32C",
                "SHA512",
                "XXHASH64",
                "XXHASH3",
                "XXHASH128",
            ],
        ),
    ]
}

fn metadata() -> ArgDef {
    let mut a = opt("metadata");
    a.model = Model::StringMap;
    a
}

fn copy_props() -> ArgDef {
    let mut a = choices(
        opt("copy-props"),
        &["none", "metadata-directive", "default", "all"],
    );
    a.action.default = NsVal::Str("default".into());
    a
}

fn case_conflict() -> ArgDef {
    let mut a = choices(opt("case-conflict"), &["ignore", "skip", "warn", "error"]);
    a.action.default = NsVal::Str("ignore".into());
    a
}

fn paths2() -> ArgDef {
    positional("paths", Some(Nargs::Exact(2)))
}

/// The argument table of `aws s3 <name>`, in `ARG_TABLE` order.
pub(crate) fn arg_table(name: &str) -> Vec<ArgDef> {
    match name {
        "ls" => {
            let mut p = positional("paths", Some(Nargs::Optional));
            p.action.default = NsVal::Str("s3://".into());
            vec![
                p,
                recursive(),
                page_size(),
                flag("human-readable"),
                flag("summarize"),
                request_payer(),
                opt("bucket-name-prefix"),
                opt("bucket-region"),
            ]
        }
        "website" => vec![
            positional("paths", Some(Nargs::Exact(1))),
            opt("index-document"),
            opt("error-document"),
        ],
        "presign" => {
            let mut e = integer(opt("expires-in"));
            e.action.default = NsVal::Int(3600.into());
            vec![positional("path", None), e]
        }
        "cp" | "mv" | "sync" => {
            let mut t = vec![paths2()];
            t.extend(transfer_args());
            t.push(metadata());
            t.push(copy_props());
            t.push(choices(opt("metadata-directive"), &["COPY", "REPLACE"]));
            if name == "cp" {
                t.push(opt("expected-size"));
            }
            if name != "sync" {
                t.push(recursive());
            }
            if name == "mv" {
                t.push(flag("validate-same-s3-paths"));
            }
            t.push(case_conflict());
            t.push(flag("no-overwrite"));
            if name == "sync" {
                // `register_sync_strategies`: SizeOnlySync, ExactTimestampsSync,
                // DeleteSync, NoOverwriteSync (already in the table).
                t.push(flag("size-only"));
                t.push(flag("exact-timestamps"));
                t.push(flag("delete"));
            }
            t
        }
        "rm" => vec![
            positional("paths", Some(Nargs::Exact(1))),
            flag("dryrun"),
            flag("quiet"),
            recursive(),
            request_payer(),
            filter("include"),
            filter("exclude"),
            flag("only-show-errors"),
            page_size(),
        ],
        "mb" => {
            let mut tags = opt("tags");
            tags.action.kind = Kind::Append;
            tags.action.nargs = Nargs::Exact(2);
            tags.model = Model::None;
            vec![positional("path", None), tags]
        }
        "rb" => vec![positional("path", None), flag("force")],
        _ => vec![],
    }
}

/// The parsed and unpacked arguments (`vars(parsed_args)`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Args(pub IndexMap<String, Value>);

impl Args {
    pub(crate) fn get(&self, k: &str) -> Option<&Value> {
        self.0.get(k).filter(|v| !matches!(v, Value::Null))
    }

    pub(crate) fn str(&self, k: &str) -> Option<&str> {
        self.get(k).and_then(Value::as_str)
    }

    /// Python truthiness of `params.get(k)`.
    pub(crate) fn truthy(&self, k: &str) -> bool {
        self.get(k).is_some_and(Value::is_truthy)
    }

    pub(crate) fn set(&mut self, k: &str, v: Value) {
        self.0.insert(k.to_string(), v);
    }

    pub(crate) fn int(&self, k: &str) -> Option<i64> {
        self.get(k).and_then(Value::as_i64)
    }
}

fn ns_to_value(v: NsVal) -> Value {
    match v {
        NsVal::None => Value::Null,
        NsVal::NotSpecified => Value::Null,
        NsVal::Bool(b) => Value::Bool(b),
        NsVal::Str(s) => Value::Str(s),
        NsVal::Int(i) => Value::Int(i),
        NsVal::Float(f) => Value::Float(f),
        NsVal::List(l) => Value::List(l.into_iter().map(ns_to_value).collect()),
    }
}

/// The outcome of parsing a subcommand's arguments.
pub(crate) enum ParseOutcome {
    Args(Args),
    Help,
}

pub(crate) fn build_parser(table: &[ArgDef]) -> Parser {
    let mut p = Parser::new();
    for a in table {
        p.add(a.action.clone());
    }
    p
}

/// `BasicCommand.__call__` without subcommands: parse, unpack each value
/// (`load-cli-arg` file references, then the argument model), and reject
/// leftovers.
pub(crate) fn parse(command: &str, args: &[String]) -> Result<ParseOutcome, Fail> {
    if args.len() == 1 && args[0] == "help" {
        return Ok(ParseOutcome::Help);
    }
    let table = arg_table(command);
    let parser = build_parser(&table);
    let (ns, extras) = match parser
        .parse_known_args(args)
        .map_err(|e| argparse_error(&e.to_string()))?
    {
        Parsed::Args { ns, extras } => (ns, extras),
        Parsed::Version => unreachable!("no version action"),
    };
    let mut out = Args::default();
    for (key, value) in ns {
        let mut value = ns_to_value(value);
        let xformed = key.replace('_', "-");
        let def = table.iter().find(|a| a.name == xformed);
        let cli_name = def.map_or_else(|| "anonymous".to_string(), ArgDef::cli_name);
        if let Some(loaded) = argprocess::load_cli_arg(&cli_name, &value)? {
            value = loaded;
        }
        if let Some(def) = def
            && def.model != Model::None
            && !matches!(value, Value::Null)
        {
            value = unpack(command, def, &value)?;
        }
        out.0.insert(key, value);
    }
    if !extras.is_empty() {
        let mut candidates: Vec<String> = table
            .iter()
            .filter(|a| !a.positional)
            .map(|a| a.name.to_string())
            .collect();
        candidates.extend(crate::cli::operation::option_suggestion_candidates(None));
        return Err(Fail::ParamValidation(
            crate::cli::operation::format_unknown_options_error(&extras, &candidates),
        ));
    }
    Ok(ParseOutcome::Args(out))
}

/// The `process-cli-arg` override (`ParamShorthandParser`), then
/// `unpack_cli_arg` and `_validate_value_against_schema`.
fn unpack(command: &str, def: &ArgDef, value: &Value) -> Result<Value, Fail> {
    match def.model {
        Model::Integer => crate::cli::shorthand::int_of(value).map_err(Fail::Python),
        Model::Boolean => Ok(match value {
            Value::Str(s) => Value::Bool(s.to_lowercase() == "true"),
            v => Value::Bool(v.is_truthy()),
        }),
        Model::StringMap => {
            let model = ServiceModel::load("s3").map_err(|e| Fail::General(e.to_string()))?;
            let shape = model.shape("Metadata").expect("Metadata shape");
            let cli_name = def.cli_name();
            let ctx = ArgContext {
                name: def.name,
                cli_name: &cli_name,
                shape: &shape,
                service: "custom",
                operation: command,
            };
            let v = argprocess::unpack_argument(&ctx, value)?;
            if let Some(msg) = crate::cli::validate::validate(&v, &shape) {
                return Err(Fail::ParamValidation(msg));
            }
            Ok(v)
        }
        Model::String | Model::Blob | Model::None => Ok(value.clone()),
    }
}

/// `{'Key': k, 'Value': v}` maps of `--tags k v`.
pub(crate) fn tag_list(args: &Args) -> Vec<Value> {
    let Some(Value::List(tags)) = args.get("tags") else {
        return vec![];
    };
    tags.iter()
        .filter_map(|t| {
            let l = t.as_list()?;
            let mut m = Map::new();
            m.insert("Key".into(), l.first()?.clone());
            m.insert("Value".into(), l.get(1)?.clone());
            Some(Value::Map(m))
        })
        .collect()
}
