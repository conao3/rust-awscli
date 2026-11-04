//! The main parser and the global options (`awscli/clidriver.py`
//! `CLIDriver.main` / `_build_argument_table`, `awscli/argparser.py`
//! `MainArgParser`, `awscli/data/cli.json`,
//! `awscli/customizations/globalargs.py`).

use crate::bjson::{Node, Obj};

use super::argparse::{Action, Kind, Nargs, NsVal, Parsed, Parser};
use super::commands::main_choices;
use super::pynum::py_int;
use super::render::{argparse_error, render};
use super::{Fail, GlobalArgs};
use crate::config::ConfigLookup;
use crate::error::CliError;
use crate::pyfmt::str_repr;

/// The global options of `cli.json` in order (option name, data).
pub(crate) fn cli_options() -> Vec<(String, &'static Obj)> {
    crate::model::cli_data()["options"]
        .as_object()
        .expect("cli.json options")
        .iter()
        .map(|(k, v)| (k.to_string(), v.as_object().expect("option")))
        .collect()
}

/// `CustomArgument(...).add_to_parser` for a `cli.json` option.
fn option_action(name: &str, data: &Obj) -> Action {
    let cli_name = format!("--{name}");
    let action = data.get("action").and_then(Node::as_str);
    let mut a = match action {
        Some("store_true") => Action::flag(&cli_name, Kind::StoreTrue),
        Some("store_false") => Action::flag(&cli_name, Kind::StoreFalse),
        Some("version") => Action::flag(&cli_name, Kind::Version),
        _ => Action::option(&cli_name),
    };
    if let Some(dest) = data.get("dest").and_then(Node::as_str) {
        a.dest = dest.to_string();
    }
    if let Some(default) = data.get("default").and_then(Node::as_str) {
        a.default = NsVal::Str(default.to_string());
    }
    if let Some(choices) = data.get("choices").and_then(Node::as_array) {
        a.choices = Some(
            choices
                .iter()
                .filter_map(|c| c.as_str().map(String::from))
                .collect(),
        );
    }
    if let Some(required) = data.get("required").and_then(Node::as_bool) {
        a.required = required;
    }
    a
}

/// `MainArgParser`.
fn main_parser() -> Parser {
    let mut p = Parser::new();
    for (name, data) in cli_options() {
        p.add(option_action(&name, data));
    }
    p.add(Action::flag("--version", Kind::Version));
    let mut command = Action::positional("command", Nargs::One);
    command.choices = Some(main_choices());
    p.add(command);
    p
}

fn ns_str(ns: &indexmap::IndexMap<String, NsVal>, key: &str) -> Option<String> {
    match ns.get(key) {
        Some(NsVal::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

fn ns_bool(ns: &indexmap::IndexMap<String, NsVal>, key: &str) -> bool {
    ns.get(key).is_some_and(NsVal::is_truthy)
}

/// `urllib.parse.urlparse(value).scheme` is non-empty.
fn has_scheme(url: &str) -> bool {
    let url = url.trim_matches(|c: char| c <= ' ');
    let url: String = url
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let Some(i) = url.find(':') else { return false };
    if i == 0 {
        return false;
    }
    let first = url.chars().next().expect("non-empty");
    if !(first.is_ascii() && first.is_ascii_alphabetic()) {
        return false;
    }
    url[..i]
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// `FirstPassGlobalArgParser` (`create_clidriver`), whose errors carry its
/// own generated usage.
fn first_pass(args: &[String]) -> Result<(), Fail> {
    let mut p = Parser::new();
    p.add(Action::option("--profile"));
    p.add(Action::flag("--debug", Kind::StoreTrue));
    match p.parse_known_args(args) {
        Ok(_) => Ok(()),
        Err(e) => Err(Fail::ParamValidation(format!(
            "{e}\n\nusage: aws [--profile PROFILE] [--debug]\n"
        ))),
    }
}

/// `resolve_auto_prompt_mode`'s mutual exclusion check, which runs before
/// any parsing.
fn validate_auto_prompt(args: &[String]) -> Result<(), Fail> {
    let has = |o: &str| args.iter().any(|a| a == o);
    if has("--cli-auto-prompt") && has("--no-cli-auto-prompt") {
        return Err(Fail::ParamValidation(
            "Both --cli-auto-prompt and --no-cli-auto-prompt cannot be specified at the same time."
                .to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn parse(
    args: &[String],
    error_config: Option<&dyn ConfigLookup>,
) -> Result<(GlobalArgs, Vec<String>), CliError> {
    if let Err(f) = first_pass(args).and_then(|()| validate_auto_prompt(args)) {
        return Err(render(f, None, None));
    }
    if let Err(f) = super::alias::get(&[]) {
        return Err(render(f, None, None));
    }
    let parsed = main_parser()
        .parse_known_args(args)
        .map_err(|e| render(argparse_error(&e.to_string()), None, error_config))?;
    let (ns, extras) = match parsed {
        Parsed::Version => {
            return Ok((
                GlobalArgs {
                    version: true,
                    ..GlobalArgs::default()
                },
                Vec::new(),
            ));
        }
        Parsed::Args { ns, extras } => (ns, extras),
    };
    let globals = globals_from_ns(&ns);
    if let Err(f) = top_level_args_parsed(&globals) {
        return Err(render(f, Some(&globals), error_config));
    }
    let command = ns_str(&ns, "command").expect("command");
    let mut rest = vec![command];
    rest.extend(extras);
    Ok((globals, rest))
}

fn globals_from_ns(ns: &indexmap::IndexMap<String, NsVal>) -> GlobalArgs {
    GlobalArgs {
        debug: ns_bool(ns, "debug"),
        endpoint_url: ns_str(ns, "endpoint_url"),
        no_verify_ssl: ns.get("verify_ssl") == Some(&NsVal::Bool(false)),
        no_paginate: ns.get("paginate") == Some(&NsVal::Bool(false)),
        output: ns_str(ns, "output"),
        query: ns_str(ns, "query"),
        profile: ns_str(ns, "profile"),
        region: ns_str(ns, "region"),
        version: false,
        color: ns_str(ns, "color"),
        no_sign_request: ns.get("sign_request") == Some(&NsVal::Bool(false)),
        ca_bundle: ns_str(ns, "ca_bundle"),
        cli_read_timeout: ns_str(ns, "read_timeout"),
        cli_connect_timeout: ns_str(ns, "connect_timeout"),
        cli_binary_format: ns_str(ns, "cli_binary_format"),
        no_cli_pager: ns_bool(ns, "no_cli_pager"),
        cli_auto_prompt: ns_bool(ns, "cli_auto_prompt"),
        no_cli_auto_prompt: ns_bool(ns, "no_cli_auto_prompt"),
        cli_error_format: ns_str(ns, "cli_error_format"),
    }
}

/// `MainArgParser.parse_known_args` of an alias value
/// (`awscli/alias.py`): the globals it gives and the command name
/// followed by the arguments the parser did not consume.
pub(crate) fn parse_alias(args: &[String]) -> Result<(GlobalArgs, Vec<String>), Fail> {
    let parsed = main_parser()
        .parse_known_args(args)
        .map_err(|e| argparse_error(&e.to_string()))?;
    let (ns, extras) = match parsed {
        Parsed::Version => {
            return Ok((
                GlobalArgs {
                    version: true,
                    ..GlobalArgs::default()
                },
                Vec::new(),
            ));
        }
        Parsed::Args { ns, extras } => (ns, extras),
    };
    let command = ns_str(&ns, "command").expect("command");
    let mut rest = vec![command];
    rest.extend(extras);
    Ok((globals_from_ns(&ns), rest))
}

/// `emit_top_level_args_parsed_event` for globals parsed from an alias.
pub(crate) fn validate(globals: &GlobalArgs) -> Result<(), Fail> {
    top_level_args_parsed(globals)
}

/// The `top-level-args-parsed` handlers of `globalargs.py`.
fn top_level_args_parsed(globals: &GlobalArgs) -> Result<(), Fail> {
    if let Some(query) = &globals.query
        && let Err(e) = crate::jmes::Expression::compile(query)
    {
        return Err(Fail::ParamValidation(format!(
            "Bad value for --query {query}: {e}"
        )));
    }
    if let Some(url) = &globals.endpoint_url
        && !has_scheme(url)
    {
        return Err(Fail::ParamValidation(format!(
            "Bad value for --endpoint-url \"{url}\": scheme is missing.  Must be of the form http://<hostname>/ or https://<hostname>/"
        )));
    }
    for timeout in [&globals.cli_read_timeout, &globals.cli_connect_timeout]
        .into_iter()
        .flatten()
    {
        if py_int(timeout).is_none() {
            return Err(Fail::Python(format!(
                "invalid literal for int() with base 10: {}",
                str_repr(timeout)
            )));
        }
    }
    Ok(())
}
