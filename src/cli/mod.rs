//! The command line: global options, the service / operation command
//! tables derived from the models, argument parsing with the AWS CLI's
//! messages, parameter values (shorthand, JSON, `file://`), and parameter
//! validation (`awscli/clidriver.py`, `awscli/argparser.py`,
//! `awscli/arguments.py`, `awscli/argprocess.py`, `awscli/shorthand.py`,
//! `awscli/paramfile.py`, the generic customizations, and
//! `botocore/validate.py`).

pub(crate) mod argparse;
pub(crate) mod argprocess;
pub mod arguments;
mod commands;
mod difflib;
mod driver;
#[cfg(test)]
mod fingerprint_tests;
#[cfg(test)]
mod fingerprints;
mod globals;
#[cfg(test)]
mod golden_tests;
#[cfg(test)]
mod goldens;
mod operation;
pub(crate) mod paramfile;
pub(crate) mod pyjson;
mod pynum;
pub(crate) mod render;
pub(crate) mod shorthand;
mod skeleton;
mod tables;
pub(crate) mod validate;
mod yamlskel;

use crate::config::ConfigLookup;
use crate::error::CliError;
use crate::paginate::PaginationConfig;
use crate::value::Value;

/// The global options (`awscli/data/cli.json`).
#[derive(Debug, Clone, Default)]
pub struct GlobalArgs {
    pub debug: bool,
    pub endpoint_url: Option<String>,
    pub no_verify_ssl: bool,
    pub no_paginate: bool,
    pub output: Option<String>,
    pub query: Option<String>,
    pub profile: Option<String>,
    pub region: Option<String>,
    pub version: bool,
    pub color: Option<String>,
    pub no_sign_request: bool,
    pub ca_bundle: Option<String>,
    pub cli_read_timeout: Option<String>,
    pub cli_connect_timeout: Option<String>,
    pub cli_binary_format: Option<String>,
    pub no_cli_pager: bool,
    pub cli_auto_prompt: bool,
    pub no_cli_auto_prompt: bool,
    pub cli_error_format: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OperationInvocation {
    /// The command name as typed (e.g. `s3api`).
    pub cli_service: String,
    /// The botocore service name (e.g. `s3`).
    pub service: String,
    /// The botocore operation name (e.g. `ListObjectsV2`).
    pub operation: String,
    /// The botocore parameters (the dict passed to the client method),
    /// already validated.
    pub params: Value,
    /// Whether to page through all results (`--no-paginate` unset and the
    /// operation has a paginator).
    pub paginate: bool,
    pub pagination: PaginationConfig,
    /// The `outfile` positional argument of operations with streaming
    /// output.
    pub outfile: Option<String>,
    /// `--generate-cli-skeleton output`: the generated sample response to
    /// display instead of calling the service (the reference stubs the
    /// client with it, so the parameters are still validated and the
    /// response goes through the normal output path).
    pub stub_response: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct WaitInvocation {
    pub cli_service: String,
    pub service: String,
    /// The waiter name (e.g. `InstanceRunning`).
    pub waiter: String,
    pub params: Value,
}

#[derive(Debug, Clone)]
pub enum Command {
    /// `--version`.
    Version,
    /// Text to print to stdout and exit 0 (help, `--generate-cli-skeleton`).
    Print(String),
    Operation(OperationInvocation),
    Wait(WaitInvocation),
    /// A command implemented by `crate::commands` (high-level commands such
    /// as `s3` or `configure`, commands a customization adds to a service,
    /// and operations whose arguments a customization reshapes): the
    /// command path (`["s3", ...]` or `[service, subcommand]`) and the
    /// arguments after it.
    Custom {
        path: Vec<String>,
        args: Vec<String>,
    },
}

/// Split the global options out of `args` (`args[0]` is the first argument
/// after the program name). Errors carry the exact stderr text and exit
/// code of the AWS CLI.
///
/// The second element is the command name followed by the arguments the
/// main parser did not consume (`parsed_args.command` + `remaining` of
/// `CLIDriver.main`). With `--version` (which exits as soon as argparse
/// reaches it) the globals have `version` set and the rest is empty.
/// Errors are rendered in the `enhanced` error format; use
/// [`parse_globals_with_config`] to honor `cli_error_format` from the
/// environment and config file.
pub fn parse_globals(args: &[String]) -> Result<(GlobalArgs, Vec<String>), CliError> {
    globals::parse(args, None)
}

/// [`parse_globals`], resolving the error format of parse errors from
/// `error_config` (`cli_error_format`) the way the error handler falls back
/// to the session configuration.
pub fn parse_globals_with_config(
    args: &[String],
    error_config: &dyn ConfigLookup,
) -> Result<(GlobalArgs, Vec<String>), CliError> {
    globals::parse(args, Some(error_config))
}

/// Parse the remaining arguments into a command.
///
/// `rest` is the second element returned by [`parse_globals`].
pub fn parse_command(
    rest: &[String],
    globals: &GlobalArgs,
    config: &dyn ConfigLookup,
) -> Result<Command, CliError> {
    let ctx = Ctx { globals, config };
    driver::dispatch(rest, &ctx).map_err(|f| render::render(f, Some(globals), Some(config)))
}

pub(crate) use arguments::{Arg, ArgKind, ArgTable, CustomSpec, custom_action};
pub(crate) use operation::{
    OperationHooks, format_unknown_options_error, option_suggestion_candidates,
};

/// Parse the operation command `cli_service name` (a model operation of
/// the service table, after its renames) with a service customization's
/// handlers, the way [`parse_command`] parses one without them.
pub(crate) fn parse_operation_with(
    cli_service: &str,
    name: &str,
    args: &[String],
    globals: &GlobalArgs,
    config: &dyn ConfigLookup,
    hooks: &mut dyn OperationHooks,
) -> Result<Command, CliError> {
    let ctx = Ctx { globals, config };
    driver::customized_operation(cli_service, name, args, &ctx, hooks)
        .map_err(|f| render::render(f, Some(globals), Some(config)))
}

/// Render a parse failure of a custom command the way [`parse_command`]
/// renders one.
pub(crate) fn render_fail(fail: Fail, globals: &GlobalArgs, config: &dyn ConfigLookup) -> CliError {
    render::render(fail, Some(globals), Some(config))
}

/// What a parse step can fail with, by the reference's exception handler.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Fail {
    /// `ParamValidationErrorsHandler` (`ParamError`, `ParamSyntaxError`,
    /// `ArgParseException`, both `ParamValidationError`s): `str(exception)`.
    ParamValidation(String),
    /// `UnknownArgumentErrorHandler`.
    Unknown(String),
    /// `GeneralExceptionHandler` for a `ValueError` / `TypeError`.
    Python(String),
    /// `GeneralExceptionHandler` for any other exception.
    General(String),
}

/// The parsed globals and configuration a command is parsed with.
pub(crate) struct Ctx<'a> {
    pub globals: &'a GlobalArgs,
    pub config: &'a dyn ConfigLookup,
}

thread_local! {
    static CAPTURED_WARNINGS: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Write a warning to stderr the way `uni_print(..., sys.stderr)` does
/// (captured instead in tests).
pub(crate) fn warn(text: &str) {
    let captured = CAPTURED_WARNINGS.with(|c| {
        if let Some(v) = c.borrow_mut().as_mut() {
            v.push(text.to_string());
            true
        } else {
            false
        }
    });
    if !captured {
        eprint!("{text}");
    }
}

#[cfg(test)]
pub(crate) fn capture_warnings<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    CAPTURED_WARNINGS.with(|c| *c.borrow_mut() = Some(Vec::new()));
    let r = f();
    let w = CAPTURED_WARNINGS.with(|c| c.borrow_mut().take().unwrap_or_default());
    (r, w)
}
