//! `awscli/customizations/commands.py` `BasicCommand.__call__`: the
//! subcommand lookup (`SubCommandArgParser`), the argument table parser
//! (`ArgTableArgParser` with `CustomArgument.add_to_parser`), the
//! `load-cli-arg` paramfile handling, the scalar type conversion
//! (`unpack_cli_arg`), and the unknown-option error.

use indexmap::IndexMap;

use crate::cli::Fail;
use crate::cli::argparse::{Action, Kind, Nargs, NsVal, Parsed, Parser};
use crate::cli::operation::format_unknown_options_error;
use crate::cli::render::argparse_error;
use crate::session::error::py_int;
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CliType {
    String,
    Integer,
    Boolean,
}

/// One `ARG_TABLE` entry.
#[derive(Debug, Clone)]
pub(crate) struct ArgSpec {
    pub name: &'static str,
    pub positional: bool,
    pub store_true: bool,
    pub required: bool,
    pub choices: Option<&'static [&'static str]>,
    pub default: Option<&'static str>,
    pub dest: Option<&'static str>,
    pub cli_type: CliType,
    pub no_paramfile: bool,
}

impl ArgSpec {
    pub(crate) const fn option(name: &'static str) -> ArgSpec {
        ArgSpec {
            name,
            positional: false,
            store_true: false,
            required: false,
            choices: None,
            default: None,
            dest: None,
            cli_type: CliType::String,
            no_paramfile: false,
        }
    }

    pub(crate) const fn positional(name: &'static str) -> ArgSpec {
        let mut a = ArgSpec::option(name);
        a.positional = true;
        a
    }

    pub(crate) const fn flag(name: &'static str) -> ArgSpec {
        let mut a = ArgSpec::option(name);
        a.store_true = true;
        a.cli_type = CliType::Boolean;
        a
    }

    fn cli_name(&self) -> String {
        if self.positional {
            self.name.to_string()
        } else {
            format!("--{}", self.name)
        }
    }

    fn action(&self, force_optional: bool) -> Action {
        let mut action = if self.positional {
            let mut a = Action::positional(self.name, Nargs::One);
            if force_optional {
                a.required = false;
            }
            a
        } else if self.store_true {
            Action::flag(&self.cli_name(), Kind::StoreTrue)
        } else {
            Action::option(&self.cli_name())
        };
        if let Some(d) = self.dest {
            action.dest = d.to_string();
        }
        if let Some(d) = self.default {
            action.default = NsVal::Str(d.to_string());
        }
        if self.store_true {
            action.default = NsVal::Bool(false);
        }
        action.choices = self
            .choices
            .map(|c| c.iter().map(|s| s.to_string()).collect());
        if !self.positional {
            action.required = self.required && !force_optional;
        }
        action
    }
}

/// A parsed argument value after `unpack_cli_arg`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ArgValue {
    None,
    Str(String),
    Int(i64),
    Bool(bool),
}

impl ArgValue {
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            ArgValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn is_true(&self) -> bool {
        matches!(self, ArgValue::Bool(true))
    }

    pub(crate) fn as_int(&self) -> Option<i64> {
        match self {
            ArgValue::Int(i) => Some(*i),
            _ => None,
        }
    }
}

pub(crate) type Args = IndexMap<String, ArgValue>;

pub(crate) trait ArgsExt {
    fn str_arg(&self, dest: &str) -> Option<String>;
    fn flag(&self, dest: &str) -> bool;
}

impl ArgsExt for Args {
    fn str_arg(&self, dest: &str) -> Option<String> {
        self.get(dest)
            .and_then(ArgValue::as_str)
            .map(str::to_string)
    }

    fn flag(&self, dest: &str) -> bool {
        self.get(dest).is_some_and(ArgValue::is_true)
    }
}

/// The outcome of parsing a command's arguments.
pub(crate) enum Outcome {
    /// `aws ... help`.
    Help,
    Run(Args),
}

fn parse_err(e: crate::cli::argparse::ArgumentError) -> Fail {
    argparse_error(&e.to_string())
}

fn add_subcommand(parser: &mut Parser, subcommands: &[&str]) {
    if subcommands.is_empty() {
        return;
    }
    let mut sub = Action::positional("subcommand", Nargs::Optional);
    sub.choices = Some(subcommands.iter().map(|s| s.to_string()).collect());
    parser.add(sub);
}

/// `BasicCommand._parse_potential_subcommand`: the subcommand and the
/// arguments without it, when one is given.
pub(crate) fn parse_subcommand(
    args: &[String],
    table: &[ArgSpec],
    subcommands: &[&str],
) -> Result<Option<(String, Vec<String>)>, Fail> {
    if subcommands.is_empty() {
        return Ok(None);
    }
    if args.len() == 1 && args[0] == "help" {
        return Ok(None);
    }
    let mut parser = Parser::new();
    for spec in table {
        parser.add(spec.action(true));
    }
    add_subcommand(&mut parser, subcommands);
    let ns = match parser.parse_known_args(args).map_err(parse_err)? {
        Parsed::Args { ns, .. } => ns,
        Parsed::Version => return Ok(None),
    };
    match ns.get("subcommand") {
        Some(NsVal::Str(name)) => {
            let mut rest = args.to_vec();
            if let Some(pos) = rest.iter().position(|a| a == name) {
                rest.remove(pos);
            }
            Ok(Some((name.clone(), rest)))
        }
        _ => Ok(None),
    }
}

/// `ArgTableArgParser.parse_known_args` and the value processing of
/// `BasicCommand.__call__`. `command_name` is used for the unknown
/// option suggestions.
pub(crate) fn parse_args(
    args: &[String],
    table: &[ArgSpec],
    subcommands: &[&str],
) -> Result<Outcome, Fail> {
    if args.len() == 1 && args[0] == "help" {
        return Ok(Outcome::Help);
    }
    let mut parser = Parser::new();
    for spec in table {
        parser.add(spec.action(false));
    }
    add_subcommand(&mut parser, subcommands);
    let (ns, extras) = match parser.parse_known_args(args).map_err(parse_err)? {
        Parsed::Args { ns, extras } => (ns, extras),
        Parsed::Version => return Ok(Outcome::Help),
    };
    let mut out = Args::new();
    for (key, value) in ns {
        let spec = table
            .iter()
            .find(|s| s.dest.unwrap_or(s.name).replace('-', "_") == key);
        let value = match (spec, value) {
            (_, NsVal::None) => ArgValue::None,
            (_, NsVal::Bool(b)) => ArgValue::Bool(b),
            (Some(spec), NsVal::Str(s)) => {
                let s = if spec.no_paramfile {
                    s
                } else {
                    match crate::cli::argprocess::load_cli_arg(
                        &spec.cli_name(),
                        &Value::Str(s.clone()),
                    )? {
                        Some(Value::Str(loaded)) => loaded,
                        Some(Value::Bytes(b)) => String::from_utf8_lossy(&b).into_owned(),
                        _ => s,
                    }
                };
                match spec.cli_type {
                    CliType::Integer => {
                        ArgValue::Int(py_int(&s).map_err(|e| Fail::Python(e.message.clone()))?)
                    }
                    _ => ArgValue::Str(s),
                }
            }
            (None, NsVal::Str(s)) => ArgValue::Str(s),
            (_, other) => ArgValue::Str(other.py_str()),
        };
        out.insert(key, value);
    }
    if !matches!(out.get("subcommand"), Some(ArgValue::Str(_))) && !extras.is_empty() {
        let mut candidates: Vec<String> = table
            .iter()
            .filter(|s| !s.positional)
            .map(|s| s.name.to_string())
            .collect();
        if let Some(options) = crate::model::cli_data()["options"].as_object() {
            candidates.extend(options.keys().cloned());
        }
        return Err(Fail::ParamValidation(format_unknown_options_error(
            &extras,
            &candidates,
        )));
    }
    Ok(Outcome::Run(out))
}

/// `validate_mutually_exclusive(parsed_args, *groups)` for single-key
/// groups, checked in namespace order.
pub(crate) fn validate_mutually_exclusive(args: &Args, keys: &[&str]) -> Result<(), Fail> {
    let set: Vec<&String> = args
        .iter()
        .filter(|(k, v)| keys.contains(&k.as_str()) && **v != ArgValue::None)
        .map(|(k, _)| k)
        .collect();
    if set.len() > 1 {
        return Err(Fail::ParamValidation(format!(
            "The key \"{}\" cannot be specified when one of the following keys are also specified: {}",
            set[1], set[0]
        )));
    }
    Ok(())
}
